//! The single structured operation: `localize`.
//!
//! Spectrum-based fault localization (Ochiai / Tarantula) over the symbols
//! `tool-code-graph` already indexed. Input is a directory of per-test
//! LCOV `.info` files (produced by the agent's own `cargo llvm-cov` calls — this
//! tool never runs coverage itself) plus the list of failing test names. Output
//! is a ranked suspect list, with the four suspicion scores written back onto the
//! shared `symbols` table.
//!
//! `repo_path` and `coverage_dir` go through `murmur_host_roots::locate` before anything is
//! opened: a relative value opens in the work folder as written, an absolute one only through
//! the host root covering it (as `murmur-host/…`). An absolute value handed to the filesystem or
//! SQLite as written would resolve into the work folder, not onto the host. Messages and `data`
//! name each path as the call named it; `SF:` paths are never opened ([`crate::sf_paths`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use murmur_host_roots::{describe_roots, locate, HostRoot, Located, Refusal, RootAccess};
use rusqlite::{params, Connection, ErrorCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db;
use crate::lcov;
use crate::out::{errored, failed, passed, Meta};
use crate::resolve::resolve_failing_ids;
use crate::sf_paths::{self, SfPath};

/// The tool's name, as refusals name it.
const TOOL_NAME: &str = "tool-code-coverage";

// ── Input helpers ───────────────────────────────────────────────────────────────

fn str_field<'a>(op: &'a Value, key: &str) -> Option<&'a str> {
    op.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

fn opt_i64(op: &Value, key: &str, default: i64) -> i64 {
    op.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
}

// ── Paths as the call named them ────────────────────────────────────────────────

/// `repo_path` as reported: the value as written with trailing `/` trimmed (`/` itself stays
/// `/`), or `.` (the work folder) when absent or empty.
fn shown_repo(written: Option<&str>) -> String {
    match written {
        None => ".".to_string(),
        Some(value) => match value.trim_end_matches('/') {
            "" => "/".to_string(),
            trimmed => trimmed.to_string(),
        },
    }
}

/// `rel` under the repository as the call named it: `rel` alone when `shown` is the work folder
/// itself (`.`, `./`, …), else `shown/rel`.
fn display_join(shown: &str, rel: &str) -> String {
    let is_work_folder =
        !shown.starts_with('/') && shown.split('/').all(|s| s.is_empty() || s == ".");
    if is_work_folder {
        rel.to_string()
    } else if shown.ends_with('/') {
        format!("{shown}{rel}")
    } else {
        format!("{shown}/{rel}")
    }
}

/// The message for a path value `locate` refused. `field` is the input's name and `value` the
/// value as written; `repo_path`'s no-root message adds that `.` is the work folder.
fn refusal_message(field: &str, value: &str, refusal: Refusal, roots: &[HostRoot]) -> String {
    match refusal {
        Refusal::NoRoots => {
            let dot = if field == "repo_path" {
                " ('.' is the work folder itself)"
            } else {
                ""
            };
            format!(
                "localize: could not open {field} '{value}': this session opens no host folder to {TOOL_NAME}; a relative path opens in the work folder{dot}"
            )
        }
        Refusal::Uncovered => format!(
            "localize: could not open {field} '{value}': it is outside every host folder {TOOL_NAME} may open: {}. A relative path opens in the work folder",
            describe_roots(roots)
        ),
        Refusal::ReservedName { suggestion } => format!(
            "localize: '{field}' starts with murmur-host, where this tool's host folders are opened; name the host path absolutely: '{suggestion}'"
        ),
    }
}

/// Where a located value opens: as written in the work folder, or `murmur-host/…`.
fn open_path(located: &Located<'_>) -> String {
    match located {
        Located::Relative(value) => value.clone(),
        Located::Host { open, .. } => open.clone(),
    }
}

// ── Database errors ─────────────────────────────────────────────────────────────

/// `errored(message)`, unless SQLite reported the database busy while the dotfile lock
/// `<db>.lock` exists: then the message names the lock, as the call named the repository, and
/// how to clear one a cut-off call left behind.
fn db_failure(message: String, e: &rusqlite::Error, lock: &Path, shown: &str) -> Value {
    if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) && lock.exists() {
        return errored(format!(
            "localize: code graph db is locked: {} exists; a localize call that was cut off leaves it behind — if no other call is using the database, delete it and retry",
            display_join(shown, ".murmur/code-graph.db.lock")
        ));
    }
    errored(message)
}

// ── op: localize ───────────────────────────────────────────────────────────────

pub fn op_localize(op: &Value, roots: &[HostRoot]) -> Value {
    // repo_path: defaults to the work folder; must exist, be a directory, and be writable.
    let repo_written = str_field(op, "repo_path");
    let shown = shown_repo(repo_written);
    let repo_value = repo_written.unwrap_or(".");
    let repo_located = match locate(&shown, roots) {
        Ok(located) => located,
        Err(refusal) => {
            return failed(refusal_message("repo_path", repo_value, refusal, roots));
        }
    };
    if let Located::Host { root, .. } = &repo_located {
        if root.access == RootAccess::Read {
            return failed(format!(
                "localize: repo_path '{repo_value}' is in {}, which this session opens read-only; localize writes its scores into .murmur/code-graph.db there",
                root.path
            ));
        }
    }
    let repo = PathBuf::from(open_path(&repo_located));
    if !repo.exists() {
        return failed(format!("localize: repo_path does not exist: {repo_value}"));
    }
    if !repo.is_dir() {
        return failed(format!(
            "localize: repo_path is not a directory: {repo_value}"
        ));
    }

    // coverage_dir: required, must exist and be a directory. A read-only host folder is fine.
    let coverage_value = match str_field(op, "coverage_dir") {
        Some(c) => c,
        None => return failed("localize: missing required field 'coverage_dir'"),
    };
    let coverage_dir = match locate(coverage_value, roots) {
        Ok(located) => PathBuf::from(open_path(&located)),
        Err(refusal) => {
            return failed(refusal_message(
                "coverage_dir",
                coverage_value,
                refusal,
                roots,
            ))
        }
    };
    if !coverage_dir.exists() {
        return failed(format!(
            "localize: coverage_dir does not exist: {coverage_value}"
        ));
    }
    if !coverage_dir.is_dir() {
        return failed(format!(
            "localize: coverage_dir is not a directory: {coverage_value}"
        ));
    }

    // failing_tests: required, non-empty array of non-empty strings.
    let failing_tests: Vec<String> =
        match op.get("failing_tests") {
            Some(Value::Array(a)) if !a.is_empty() => {
                let names: Vec<String> = a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if names.is_empty() {
                    return failed(
                        "localize: 'failing_tests' must be a non-empty array of test-name strings",
                    );
                }
                names
            }
            _ => return failed(
                "localize: missing or empty required field 'failing_tests' (array of test names)",
            ),
        };

    let limit = opt_i64(op, "limit", 50).clamp(1, 10_000);

    // The database must already exist — this tool never creates it.
    let store = repo.join(db::STORE_DIR);
    let db_file = store.join(db::DB_FILE);
    let lock_file = store.join(format!("{}.lock", db::DB_FILE));
    if !db_file.exists() {
        return failed(format!(
            "localize: code graph db not found at {} — run tool-code-graph index_repository first",
            display_join(&shown, ".murmur/code-graph.db")
        ));
    }

    let conn = match db::open(&db_file.to_string_lossy()) {
        Ok(c) => c,
        Err(e) => {
            let mut message = format!("localize: failed to open/migrate code graph db: {e}");
            if e.sqlite_error_code() == Some(ErrorCode::NotADatabase) {
                message.push_str(
                    " (if the database is in WAL mode, which this tool cannot open, switch it back with PRAGMA journal_mode=DELETE)",
                );
            }
            return db_failure(message, &e, &lock_file, &shown);
        }
    };

    // Load the symbol table once: repo-relative file path → its symbols' ranges,
    // and symbol_id → display detail (for the suspect list); and every indexed path.
    let (path_to_syms, detail) = match load_symbols(&conn) {
        Ok(v) => v,
        Err(e) => return db_failure(format!("localize: {e}"), &e, &lock_file, &shown),
    };
    let indexed = match load_indexed_paths(&conn) {
        Ok(v) => v,
        Err(e) => return db_failure(format!("localize: {e}"), &e, &lock_file, &shown),
    };

    // Parse every `.info` file in coverage_dir first, classifying by whether its stem is a
    // failing-test name; then infer the repository's host prefix from every absolute `SF:` path
    // of the call, and map covered lines to covered symbols.
    let failing_set: HashSet<&str> = failing_tests.iter().map(|s| s.as_str()).collect();
    let mut skipped_files: Vec<String> = Vec::new();
    let mut covered_failing: HashSet<String> = HashSet::new();
    let mut ef: HashMap<String, i64> = HashMap::new();
    let mut ep: HashMap<String, i64> = HashMap::new();
    let mut passing_count: i64 = 0;

    let mut info_files = list_info_files(&coverage_dir);
    info_files.sort();
    let mut parsed: Vec<(String, Vec<lcov::FileCoverage>)> = Vec::new();
    for path in &info_files {
        let Some(stem) = info_stem(path) else {
            continue;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => {
                skipped_files.push(file_name(path));
                continue;
            }
        };
        let sections = lcov::parse(&text);
        if sections.is_empty() {
            skipped_files.push(file_name(path));
            continue;
        }
        parsed.push((stem, sections));
    }

    let absolute: HashSet<Vec<String>> = parsed
        .iter()
        .flat_map(|(_, sections)| sections)
        .filter_map(
            |section| match sf_paths::normalize_sf(&section.source_file) {
                Some(SfPath::Absolute(segments)) => Some(segments),
                _ => None,
            },
        )
        .collect();
    let absolute: Vec<Vec<String>> = absolute.into_iter().collect();
    let root = sf_paths::infer_root(&absolute, &indexed);

    for (stem, sections) in &parsed {
        // The set of symbols this single test touched.
        let covered = covered_symbols(sections, root.as_deref(), &path_to_syms);

        if failing_set.contains(stem.as_str()) {
            covered_failing.insert(stem.clone());
            for sid in &covered {
                *ef.entry(sid.clone()).or_insert(0) += 1;
            }
        } else {
            passing_count += 1;
            for sid in &covered {
                *ep.entry(sid.clone()).or_insert(0) += 1;
            }
        }
    }

    let f_total = covered_failing.len() as i64;
    let p_total = passing_count;

    // Failing tests for which no parseable coverage was found (absent file or
    // unparseable). Reported, but not fatal on their own.
    let failing_tests_without_coverage: Vec<String> = failing_tests
        .iter()
        .filter(|n| !covered_failing.contains(*n))
        .cloned()
        .collect();

    // SBFL is undefined with zero failing observations.
    if f_total == 0 {
        return failed(
            "localize: no failing-test coverage found — every failing test is absent from coverage_dir or unparseable (F == 0)",
        );
    }

    // Reset all four suspicion columns repo-wide, then write the freshly computed
    // scores, inside one transaction. The reset guarantees a prior run's scores
    // never linger on symbols this run's coverage doesn't touch.
    let scored: Vec<String> = {
        let mut set: HashSet<String> = HashSet::new();
        set.extend(ef.keys().cloned());
        set.extend(ep.keys().cloned());
        set.into_iter().collect()
    };

    if let Err(e) = write_scores(&conn, &scored, &ef, &ep, f_total, p_total) {
        return db_failure(
            format!("localize: failed writing suspicion scores: {e}"),
            &e,
            &lock_file,
            &shown,
        );
    }

    // Build the ranked suspect list (Ochiai desc, then Tarantula desc, then
    // symbol_id asc for determinism).
    let mut suspects: Vec<Value> = scored
        .iter()
        .map(|sid| {
            let ef_v = ef.get(sid).copied().unwrap_or(0);
            let ep_v = ep.get(sid).copied().unwrap_or(0);
            let ochiai = ochiai(ef_v, ep_v, f_total);
            let tarantula = tarantula(ef_v, ep_v, f_total, p_total);
            let (qn, kind, file, start_line) = detail
                .get(sid)
                .cloned()
                .unwrap_or_else(|| ("<unknown>".into(), String::new(), String::new(), 0));
            json!({
                "symbol_id": sid,
                "qualified_name": qn,
                "kind": kind,
                "file": file,
                "start_line": start_line,
                "ef": ef_v,
                "ep": ep_v,
                "suspicion_ochiai": ochiai,
                "suspicion_tarantula": tarantula,
            })
        })
        .collect();
    suspects.sort_by(|a, b| {
        let ao = a["suspicion_ochiai"].as_f64().unwrap_or(0.0);
        let bo = b["suspicion_ochiai"].as_f64().unwrap_or(0.0);
        let at = a["suspicion_tarantula"].as_f64().unwrap_or(0.0);
        let bt = b["suspicion_tarantula"].as_f64().unwrap_or(0.0);
        bo.partial_cmp(&ao)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(bt.partial_cmp(&at).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| {
                a["symbol_id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["symbol_id"].as_str().unwrap_or(""))
            })
    });

    let scored_count = suspects.len();
    let truncated = scored_count as i64 > limit;

    // Best-effort stable_id for each failing test's own symbol.
    let stable_ids = resolve_failing_ids(&conn, &failing_tests);
    let failing_test_stable_ids: Vec<Value> = stable_ids
        .into_iter()
        .map(|(name, sid)| json!({ "test_name": name, "stable_id": sid }))
        .collect();

    // Full sorted suspect list to disk when the inline view is capped.
    let full = json!({
        "repo_path": shown,
        "coverage_dir": coverage_value,
        "failing_test_count": f_total,
        "passing_test_count": p_total,
        "scored_symbols": scored_count,
        "truncated": false,
        "top_suspects": suspects,
        "failing_test_stable_ids": failing_test_stable_ids.clone(),
        "failing_tests_without_coverage": failing_tests_without_coverage.clone(),
        "skipped_files": skipped_files.clone(),
    });
    let data_path = if truncated {
        write_localize_file(&repo, &shown, &full)
    } else {
        None
    };

    let inline_suspects: Vec<Value> = suspects.iter().take(limit as usize).cloned().collect();
    let summary = format!(
        "localize: {scored_count} symbol(s) scored over {f_total} failing / {p_total} passing test(s){}",
        if truncated { format!(", top {limit} shown (full list at data_path)") } else { String::new() }
    );
    let data = json!({
        "repo_path": shown,
        "coverage_dir": coverage_value,
        "failing_test_count": f_total,
        "passing_test_count": p_total,
        "scored_symbols": scored_count,
        "truncated": truncated,
        "top_suspects": inline_suspects,
        "failing_test_stable_ids": failing_test_stable_ids,
        "failing_tests_without_coverage": failing_tests_without_coverage,
        "skipped_files": skipped_files,
        "data_path": data_path,
    });

    passed(
        summary,
        data,
        data_path,
        Some(Meta::mutate(format!("coverage:{shown}"))),
    )
}

// ── SBFL formulas ──────────────────────────────────────────────────────────────

/// Ochiai suspiciousness: `ef / sqrt(F * (ef + ep))`, `0.0` when `ef == 0`.
/// `F` is the total number of failing tests, so the denominator is never zero
/// when `ef > 0` (`ef <= F` and `ef + ep >= ef >= 1`).
fn ochiai(ef: i64, ep: i64, f_total: i64) -> f64 {
    if ef == 0 {
        return 0.0;
    }
    let denom = ((f_total * (ef + ep)) as f64).sqrt();
    ef as f64 / denom
}

/// Tarantula suspiciousness: `(ef/F) / ((ef/F) + (ep/P))`, with the `ep/P` term
/// treated as `0` when `P == 0` (no passing tests). `0.0` when `ef == 0`.
fn tarantula(ef: i64, ep: i64, f_total: i64, p_total: i64) -> f64 {
    if ef == 0 {
        return 0.0;
    }
    let fail_ratio = ef as f64 / f_total as f64;
    let pass_ratio = if p_total == 0 {
        0.0
    } else {
        ep as f64 / p_total as f64
    };
    let denom = fail_ratio + pass_ratio;
    if denom == 0.0 {
        0.0
    } else {
        fail_ratio / denom
    }
}

// ── Symbol loading / covered-symbol mapping ────────────────────────────────────

type SymRange = (String, i64, i64); // (symbol_id, start_line, end_line)
type SymDetail = (String, String, String, i64); // (qualified_name, kind, file, start_line)

/// Load the whole symbol table once: a map from repo-relative file path to that
/// file's symbol ranges (for covered-line lookup) and a map from `symbol_id` to
/// display detail (for the suspect list). One `SELECT` joining `symbols` with
/// `files`.
#[allow(clippy::type_complexity)]
fn load_symbols(
    conn: &Connection,
) -> rusqlite::Result<(HashMap<String, Vec<SymRange>>, HashMap<String, SymDetail>)> {
    let mut stmt = conn.prepare(
        "SELECT s.symbol_id, s.start_line, s.end_line, s.qualified_name, s.kind, f.path
           FROM symbols s JOIN files f ON f.id = s.file_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?, // symbol_id
            r.get::<_, i64>(1)?,    // start_line
            r.get::<_, i64>(2)?,    // end_line
            r.get::<_, String>(3)?, // qualified_name
            r.get::<_, String>(4)?, // kind
            r.get::<_, String>(5)?, // path
        ))
    })?;

    let mut path_to_syms: HashMap<String, Vec<SymRange>> = HashMap::new();
    let mut detail: HashMap<String, SymDetail> = HashMap::new();
    for row in rows {
        let (sid, start, end, qn, kind, path) = row?;
        path_to_syms
            .entry(path.clone())
            .or_default()
            .push((sid.clone(), start, end));
        detail.insert(sid, (qn, kind, path, start));
    }
    Ok((path_to_syms, detail))
}

/// Every `files.path` the code graph indexed: what an `SF:` path must map onto.
fn load_indexed_paths(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// The set of symbols one test's coverage touched: for every `SF:` section, map the source path
/// to the stored repo-relative form ([`sf_paths::map_sf`] under the call's inferred `root`) and
/// collect every symbol whose `[start_line, end_line]` contains at least one hit line. An `SF:`
/// path is never opened.
fn covered_symbols(
    sections: &[lcov::FileCoverage],
    root: Option<&[String]>,
    path_to_syms: &HashMap<String, Vec<SymRange>>,
) -> HashSet<String> {
    let mut covered: HashSet<String> = HashSet::new();
    for section in sections {
        let Some(rel) = sf_paths::map_sf(&section.source_file, root) else {
            continue;
        };
        let Some(syms) = path_to_syms.get(&rel) else {
            continue;
        };
        // Sort hit lines once so each symbol range can be probed with a single
        // binary search (first hit line >= start, then compare against end).
        let mut lines = section.hit_lines.clone();
        lines.sort_unstable();
        for (sid, start, end) in syms {
            if range_hit(&lines, *start, *end) {
                covered.insert(sid.clone());
            }
        }
    }
    covered
}

/// Whether any value in the sorted `lines` falls within `[start, end]`.
fn range_hit(lines: &[u32], start: i64, end: i64) -> bool {
    if end < start {
        return false;
    }
    // First hit line >= start; it is within range iff it is also <= end.
    let idx = lines.partition_point(|&l| (l as i64) < start);
    match lines.get(idx) {
        Some(&l) => (l as i64) <= end,
        None => false,
    }
}

// ── DB writes ──────────────────────────────────────────────────────────────────

/// Reset every symbol's four suspicion columns to `NULL`, then set the freshly
/// computed scores for each scored symbol, all in one transaction: one lock and one journal for
/// the whole write, and no half-written scores if a statement fails.
fn write_scores(
    conn: &Connection,
    scored: &[String],
    ef: &HashMap<String, i64>,
    ep: &HashMap<String, i64>,
    f_total: i64,
    p_total: i64,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "UPDATE symbols SET suspicion_ef = NULL, suspicion_ep = NULL,
                            suspicion_ochiai = NULL, suspicion_tarantula = NULL",
    )?;
    let mut stmt = tx.prepare(
        "UPDATE symbols
            SET suspicion_ef = ?1, suspicion_ep = ?2,
                suspicion_ochiai = ?3, suspicion_tarantula = ?4
          WHERE symbol_id = ?5",
    )?;
    for sid in scored {
        let ef_v = ef.get(sid).copied().unwrap_or(0);
        let ep_v = ep.get(sid).copied().unwrap_or(0);
        let ochiai = ochiai(ef_v, ep_v, f_total);
        let tarantula = tarantula(ef_v, ep_v, f_total, p_total);
        stmt.execute(params![ef_v, ep_v, ochiai, tarantula, sid])?;
    }
    drop(stmt);
    tx.commit()
}

// ── Filesystem helpers ─────────────────────────────────────────────────────────

/// Every `*.info` file directly inside `dir` (non-recursive).
fn list_info_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("info") {
                out.push(path);
            }
        }
    }
    out
}

/// The test name a `<name>.info` file represents: its filename with the trailing
/// `.info` removed (so `tests::foo.info` → `tests::foo`, preserving any dots in
/// the middle of a test name).
fn info_stem(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".info"))
        .map(|s| s.to_string())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Persist the full localize payload under `<repo>/.murmur/coverage/` and return its path as
/// the call named the repository. `repo` is where the repository opens, `shown` how the call
/// named it; the file is named after `shown`. `None` when it cannot be written.
fn write_localize_file(repo: &Path, shown: &str, full: &Value) -> Option<String> {
    let dir = repo.join(db::STORE_DIR).join("coverage");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!("localize-{}.json", sha256_hex(shown.as_bytes()));
    let body = serde_json::to_string_pretty(full).ok()?;
    std::fs::write(dir.join(&name), body).ok()?;
    Some(display_join(shown, &format!(".murmur/coverage/{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_trims_trailing_slashes_and_defaults_to_the_work_folder() {
        assert_eq!(shown_repo(None), ".");
        assert_eq!(shown_repo(Some("repo/")), "repo");
        assert_eq!(shown_repo(Some("/srv/repo//")), "/srv/repo");
        assert_eq!(shown_repo(Some("/")), "/");
        assert_eq!(shown_repo(Some("./")), ".");
    }

    #[test]
    fn display_join_names_paths_under_the_repository_as_the_call_did() {
        let rel = ".murmur/code-graph.db";
        assert_eq!(display_join(".", rel), rel);
        assert_eq!(display_join("./", rel), rel);
        assert_eq!(display_join("./.", rel), rel);
        assert_eq!(display_join("repo", rel), "repo/.murmur/code-graph.db");
        assert_eq!(
            display_join("/srv/repo", rel),
            "/srv/repo/.murmur/code-graph.db"
        );
        assert_eq!(display_join("/", rel), "/.murmur/code-graph.db");
    }

    #[test]
    fn range_hit_basic() {
        let lines = vec![3u32, 7, 10];
        assert!(range_hit(&lines, 5, 8)); // 7 in range
        assert!(range_hit(&lines, 3, 3)); // exact
        assert!(!range_hit(&lines, 4, 6)); // gap
        assert!(!range_hit(&lines, 11, 20)); // above all
        assert!(!range_hit(&lines, 1, 2)); // below all
    }

    #[test]
    fn ochiai_edge_cases() {
        // F=1, P=1, touched only by the failing test: ef=1, ep=0.
        assert_eq!(ochiai(1, 0, 1), 1.0);
        assert_eq!(ochiai(0, 3, 1), 0.0);
    }

    #[test]
    fn tarantula_edge_cases() {
        assert_eq!(tarantula(1, 0, 1, 1), 1.0);
        assert_eq!(tarantula(0, 2, 1, 1), 0.0);
        // P == 0 → pass term is 0 → 1.0 for any ef>0.
        assert_eq!(tarantula(2, 0, 3, 0), 1.0);
    }
}
