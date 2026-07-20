//! The single structured operation: `localize`.
//!
//! Spectrum-based fault localization (Ochiai / Tarantula) over the symbols
//! `murmur-tool-code-graph` already indexed. Input is a directory of per-test
//! LCOV `.info` files (produced by the agent's own `cargo llvm-cov` calls — this
//! tool never runs coverage itself) plus the list of failing test names. Output
//! is a ranked suspect list, with the four suspicion scores written back onto the
//! shared `symbols` table.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db;
use crate::lcov;
use crate::out::{errored, failed, passed, Meta};
use crate::resolve::resolve_failing_ids;

// ── Input helpers (mirrors murmur-tool-code-graph's ops.rs) ────────────────────

fn str_field<'a>(op: &'a Value, key: &str) -> Option<&'a str> {
    op.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

fn opt_i64(op: &Value, key: &str, default: i64) -> i64 {
    op.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
}

/// Resolve and validate `repo_path`; defaults to the current directory. Returns
/// a `failed` envelope if the path is missing, not found, or not a directory.
fn resolve_repo(op: &Value, operation: &str) -> Result<PathBuf, Value> {
    let raw = str_field(op, "repo_path")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| failed(format!("{operation}: could not determine repo_path")))?;
    if !raw.exists() {
        return Err(failed(format!("{operation}: repo_path does not exist: {}", raw.display())));
    }
    if !raw.is_dir() {
        return Err(failed(format!("{operation}: repo_path is not a directory: {}", raw.display())));
    }
    Ok(raw.canonicalize().unwrap_or(raw))
}

// ── op: localize ───────────────────────────────────────────────────────────────

pub fn op_localize(op: &Value) -> Value {
    let repo = match resolve_repo(op, "localize") {
        Ok(r) => r,
        Err(e) => return e,
    };

    // coverage_dir: required, must exist and be a directory.
    let coverage_dir = match str_field(op, "coverage_dir") {
        Some(c) => PathBuf::from(c),
        None => return failed("localize: missing required field 'coverage_dir'"),
    };
    if !coverage_dir.exists() {
        return failed(format!("localize: coverage_dir does not exist: {}", coverage_dir.display()));
    }
    if !coverage_dir.is_dir() {
        return failed(format!("localize: coverage_dir is not a directory: {}", coverage_dir.display()));
    }

    // failing_tests: required, non-empty array of non-empty strings.
    let failing_tests: Vec<String> = match op.get("failing_tests") {
        Some(Value::Array(a)) if !a.is_empty() => {
            let names: Vec<String> =
                a.iter().filter_map(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from).collect();
            if names.is_empty() {
                return failed("localize: 'failing_tests' must be a non-empty array of test-name strings");
            }
            names
        }
        _ => return failed("localize: missing or empty required field 'failing_tests' (array of test names)"),
    };

    let limit = opt_i64(op, "limit", 50).clamp(1, 10_000);

    // The database must already exist — this tool never creates it.
    let db_file = db::db_path(&repo);
    if !db_file.exists() {
        return failed(format!(
            "localize: code graph db not found at {} — run murmur-tool-code-graph index_repository first",
            db_file.display()
        ));
    }

    let conn = match db::open(&repo) {
        Ok(c) => c,
        Err(e) => return errored(format!("localize: failed to open/migrate code graph db: {e}")),
    };

    // Load the symbol table once: repo-relative file path → its symbols' ranges,
    // and symbol_id → display detail (for the suspect list).
    let (path_to_syms, detail) = match load_symbols(&conn) {
        Ok(v) => v,
        Err(e) => return errored(format!("localize: {e}")),
    };

    // Parse every `.info` file in coverage_dir, classifying by whether its stem
    // is a failing-test name, and mapping covered lines to covered symbols.
    let failing_set: HashSet<&str> = failing_tests.iter().map(|s| s.as_str()).collect();
    let mut skipped_files: Vec<String> = Vec::new();
    let mut covered_failing: HashSet<String> = HashSet::new();
    let mut ef: HashMap<String, i64> = HashMap::new();
    let mut ep: HashMap<String, i64> = HashMap::new();
    let mut passing_count: i64 = 0;

    let mut info_files = list_info_files(&coverage_dir);
    info_files.sort();
    for path in &info_files {
        let Some(stem) = info_stem(path) else { continue };
        let is_failing = failing_set.contains(stem.as_str());

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

        // The set of symbols this single test touched.
        let covered = covered_symbols(&repo, &sections, &path_to_syms);

        if is_failing {
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
    let failing_tests_without_coverage: Vec<String> =
        failing_tests.iter().filter(|n| !covered_failing.contains(*n)).cloned().collect();

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
        return errored(format!("localize: failed writing suspicion scores: {e}"));
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
            .then_with(|| a["symbol_id"].as_str().unwrap_or("").cmp(b["symbol_id"].as_str().unwrap_or("")))
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
        "repo_path": repo.display().to_string(),
        "coverage_dir": coverage_dir.display().to_string(),
        "failing_test_count": f_total,
        "passing_test_count": p_total,
        "scored_symbols": scored_count,
        "truncated": false,
        "top_suspects": suspects,
        "failing_test_stable_ids": failing_test_stable_ids.clone(),
        "failing_tests_without_coverage": failing_tests_without_coverage.clone(),
        "skipped_files": skipped_files.clone(),
    });
    let data_path = if truncated { write_localize_file(&repo, &full) } else { None };

    let inline_suspects: Vec<Value> = suspects.iter().take(limit as usize).cloned().collect();
    let summary = format!(
        "localize: {scored_count} symbol(s) scored over {f_total} failing / {p_total} passing test(s){}",
        if truncated { format!(", top {limit} shown (full list at data_path)") } else { String::new() }
    );
    let data = json!({
        "repo_path": repo.display().to_string(),
        "coverage_dir": coverage_dir.display().to_string(),
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

    passed(summary, data, data_path, Some(Meta::mutate(format!("coverage:{}", repo.display()))))
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
    let pass_ratio = if p_total == 0 { 0.0 } else { ep as f64 / p_total as f64 };
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
            r.get::<_, i64>(1)?,     // start_line
            r.get::<_, i64>(2)?,     // end_line
            r.get::<_, String>(3)?, // qualified_name
            r.get::<_, String>(4)?, // kind
            r.get::<_, String>(5)?, // path
        ))
    })?;

    let mut path_to_syms: HashMap<String, Vec<SymRange>> = HashMap::new();
    let mut detail: HashMap<String, SymDetail> = HashMap::new();
    for row in rows {
        let (sid, start, end, qn, kind, path) = row?;
        path_to_syms.entry(path.clone()).or_default().push((sid.clone(), start, end));
        detail.insert(sid, (qn, kind, path, start));
    }
    Ok((path_to_syms, detail))
}

/// The set of symbols one test's coverage touched: for every `SF:` section, map
/// the source path to the stored repo-relative form and collect every symbol
/// whose `[start_line, end_line]` contains at least one hit line.
fn covered_symbols(
    repo: &Path,
    sections: &[lcov::FileCoverage],
    path_to_syms: &HashMap<String, Vec<SymRange>>,
) -> HashSet<String> {
    let mut covered: HashSet<String> = HashSet::new();
    for section in sections {
        let rel = normalize_rel(repo, &section.source_file);
        let Some(syms) = path_to_syms.get(&rel) else { continue };
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
/// computed scores for each scored symbol, all in one transaction.
fn write_scores(
    conn: &Connection,
    scored: &[String],
    ef: &HashMap<String, i64>,
    ep: &HashMap<String, i64>,
    f_total: i64,
    p_total: i64,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "UPDATE symbols SET suspicion_ef = NULL, suspicion_ep = NULL,
                            suspicion_ochiai = NULL, suspicion_tarantula = NULL",
    )?;
    let mut stmt = conn.prepare(
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
    Ok(())
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
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Normalize an LCOV `SF:` path to the stored repo-relative form: forward
/// slashes, no leading `./`, absolute repo-root prefix stripped — mirrors
/// `murmur-tool-code-graph`'s `normalize_rel`/`rel_path` so a lookup always
/// agrees with what indexing wrote into `files.path`.
///
/// `repo` is already canonicalized by `resolve_repo`, so `file` is canonicalized
/// here too before stripping — otherwise a `repo_path` that resolves through a
/// symlink (e.g. macOS's `/tmp` -> `/private/tmp`) would never strip cleanly
/// against `cargo llvm-cov`'s un-resolved absolute `SF:` path, silently matching
/// zero symbols. Falls back to the literal path if the file no longer exists on
/// this machine (e.g. moved since the coverage run).
fn normalize_rel(repo: &Path, file: &str) -> String {
    let mut f = file.replace('\\', "/");
    while let Some(s) = f.strip_prefix("./") {
        f = s.to_string();
    }
    let path = Path::new(&f);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    rel_path(repo, &canonical)
}

fn rel_path(repo: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(repo).unwrap_or(file);
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Persist the full localize payload under `.murmur/coverage/` and return its
/// path, mirroring `murmur-tool-code-graph`'s `write_slice_file`/`write_impact_file`.
fn write_localize_file(repo: &Path, full: &Value) -> Option<String> {
    let dir = repo.join(db::STORE_DIR).join("coverage");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!("localize-{}.json", sha256_hex(repo.display().to_string().as_bytes()));
    let path = dir.join(name);
    let body = serde_json::to_string_pretty(full).ok()?;
    std::fs::write(&path, body).ok()?;
    Some(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
