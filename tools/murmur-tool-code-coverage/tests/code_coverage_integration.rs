//! Real-filesystem, real-SQLite integration tests for murmur-tool-code-coverage.
//!
//! Each test spawns the actual compiled binary via `std::process::Command`, feeds
//! a real JSON envelope on stdin, and inspects the real JSON on stdout — no mocks.
//! The code-graph fixture db is hand-built at test time with `rusqlite` against
//! the exact schema `murmur-tool-code-graph` documents (that tool is `[[bin]]`-
//! only, so it cannot be linked as a library), and the four `suspicion_*` columns
//! are deliberately omitted from the fixture so the migration path is exercised.
//!
//! Scenario map (16 total):
//!   1  happy path, F=1/P=1 minimal, Ochiai/Tarantula == 1.0 numerically
//!   2  untouched symbol stays NULL in all four columns
//!   3  passing-test coverage lowers a shared symbol's suspicion below 1.0
//!   4  db columns populated for scored symbols, NULL for untouched (direct read)
//!   5  reset: a second run drops scores the second run's coverage doesn't touch
//!   6  failing test with no <name>.info → failing_tests_without_coverage
//!   7  unparseable .info → skipped_files, non-fatal
//!   8  F==0 (all failing coverage absent/unparseable) → failed
//!   9  missing db → failed naming index_repository
//!   10 coverage_dir missing / not a directory → failed
//!   11 malformed input (missing operation / bad op / missing coverage_dir / empty failing_tests)
//!   12 stable_id: unique match resolves, ambiguous resolves null
//!   13 migration adds the four columns to a pre-existing db lacking them
//!   14 top_suspects truncation → truncated + data_path with full list
//!   15 P==0 (no passing tests): Tarantula defined, ep/P term treated as 0
//!   16 symlinked repo_path: SF: paths still canonicalize and join correctly

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("murmur-tool-code-coverage-it-{}-{nanos}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("temp test directory should be created");
        Self { path }
    }
    /// The canonicalized path — matches what the tool computes internally and what
    /// LCOV `SF:` absolute paths must agree with.
    fn path(&self) -> PathBuf {
        self.path.canonicalize().unwrap_or_else(|_| self.path.clone())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ── Fixture db ─────────────────────────────────────────────────────────────────

/// A symbol row: (symbol_id, module, qualified_name, kind, start_line, end_line).
type Sym<'a> = (&'a str, &'a str, &'a str, &'a str, i64, i64);

/// Build `<repo>/.murmur/code-graph.db` with the documented schema (WITHOUT the
/// four suspicion_* columns, so the tool's migration adds them) and one file
/// `src/lib.rs` holding the given symbols.
fn build_graph_db(repo: &Path, symbols: &[Sym]) {
    let dir = repo.join(".murmur");
    fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join("code-graph.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, content_hash TEXT NOT NULL,
            language TEXT NOT NULL DEFAULT 'rust'
        );
        CREATE TABLE symbols (
            id INTEGER PRIMARY KEY, symbol_id TEXT NOT NULL UNIQUE, language TEXT NOT NULL,
            package TEXT NOT NULL, module TEXT NOT NULL, qualified_name TEXT NOT NULL,
            simple_name TEXT NOT NULL, signature TEXT NOT NULL, kind TEXT NOT NULL,
            file_id INTEGER NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
            doc_comment TEXT NOT NULL DEFAULT '', visibility TEXT NOT NULL DEFAULT '',
            attributes TEXT NOT NULL DEFAULT ''
        );
        INSERT INTO files (id, path, content_hash) VALUES (1, 'src/lib.rs', 'deadbeef');
        "#,
    )
    .unwrap();
    for (sid, module, qname, kind, start, end) in symbols {
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name,
                 signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture', ?2, ?3, ?3, '(->())', ?4, 1, ?5, ?6)",
            rusqlite::params![sid, module, qname, kind, start, end],
        )
        .unwrap();
    }
}

/// The five-symbol fixture used by most tests.
const S_ADD: &str = "rust://fixture/#add(->i64)";
const S_SUB: &str = "rust://fixture/#sub(->i64)";
const S_UNUSED: &str = "rust://fixture/#unused(->i64)";
const S_TEST_ADD: &str = "rust://fixture/tests#test_add(->())";
const S_TEST_SUB: &str = "rust://fixture/tests#test_sub(->())";

fn standard_symbols() -> Vec<Sym<'static>> {
    vec![
        (S_ADD, "", "add", "function", 5, 8),
        (S_SUB, "", "sub", "function", 10, 13),
        (S_UNUSED, "", "unused", "function", 40, 45),
        (S_TEST_ADD, "tests", "test_add", "function", 20, 25),
        (S_TEST_SUB, "tests", "test_sub", "function", 30, 35),
    ]
}

// ── Coverage helpers ───────────────────────────────────────────────────────────

/// Write `<coverage_dir>/<test_name>.info` covering the given 1-based lines of
/// `<repo>/src/lib.rs` (absolute `SF:` path, as `cargo llvm-cov` emits).
fn write_info(coverage_dir: &Path, repo: &Path, test_name: &str, hit_lines: &[i64]) {
    let sf = format!("{}/src/lib.rs", repo.display());
    let mut body = format!("TN:\nSF:{sf}\n");
    for l in hit_lines {
        body.push_str(&format!("DA:{l},1\n"));
    }
    body.push_str("LF:1\nLH:1\nend_of_record\n");
    fs::write(coverage_dir.join(format!("{test_name}.info")), body).unwrap();
}

// ── Harness ────────────────────────────────────────────────────────────────────

fn run_tool(op: Value) -> Value {
    let envelope = json!({ "data": op.to_string(), "log_path": null }).to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_murmur-tool-code-coverage"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary should start");
    child.stdin.take().unwrap().write_all(envelope.as_bytes()).unwrap();
    let output = child.wait_with_output().expect("tool should complete");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Invariant: stdout is always exactly one JSON object, never partial output.
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be valid JSON ({e}): {stdout:?}"))
}

fn localize_op(repo: &Path, coverage_dir: &Path, failing: &[&str]) -> Value {
    json!({
        "operation": "localize",
        "repo_path": repo.display().to_string(),
        "coverage_dir": coverage_dir.display().to_string(),
        "failing_tests": failing,
    })
}

/// Read the four suspicion columns for `symbol_id` directly from the db.
/// Returns (ef, ep, ochiai, tarantula) as `Option`s (`None` == SQL NULL).
#[allow(clippy::type_complexity)]
fn read_scores(
    repo: &Path,
    symbol_id: &str,
) -> (Option<i64>, Option<i64>, Option<f64>, Option<f64>) {
    let db = repo.join(".murmur").join("code-graph.db");
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row(
        "SELECT suspicion_ef, suspicion_ep, suspicion_ochiai, suspicion_tarantula
           FROM symbols WHERE symbol_id = ?1",
        rusqlite::params![symbol_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

fn suspect<'a>(res: &'a Value, symbol_id: &str) -> Option<&'a Value> {
    res["data"]["top_suspects"].as_array()?.iter().find(|s| s["symbol_id"] == symbol_id)
}

// ── 1 & 4 & 15-ish. Happy path F=1/P=1, exact scores, db populated ─────────────

#[test]
fn scenario_01_happy_path_exact_scores() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());
    // Failing test_add covers add(5,6) + itself(20,21). Passing test_sub covers
    // sub(10,11) + itself(30,31).
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5, 6, 20, 21]);
    write_info(&cov.path(), &repo.path(), "tests::test_sub", &[10, 11, 30, 31]);

    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "passed", "res: {res}");
    assert_eq!(res["data"]["failing_test_count"], 1);
    assert_eq!(res["data"]["passing_test_count"], 1);

    // `add`: touched only by the failing test → ochiai=1.0, tarantula=1.0.
    let add = suspect(&res, S_ADD).expect("add present");
    assert_eq!(add["ef"], 1);
    assert_eq!(add["ep"], 0);
    assert_eq!(add["suspicion_ochiai"].as_f64().unwrap(), 1.0);
    assert_eq!(add["suspicion_tarantula"].as_f64().unwrap(), 1.0);

    // The failing test's own symbol self-covers (ef>=1) — expected, not filtered.
    let t = suspect(&res, S_TEST_ADD).expect("test_add present");
    assert_eq!(t["ef"], 1);

    // `sub`: only the passing test → ef=0, both scores 0.0 (scored, not NULL).
    let sub = suspect(&res, S_SUB).expect("sub present");
    assert_eq!(sub["ef"], 0);
    assert_eq!(sub["ep"], 1);
    assert_eq!(sub["suspicion_ochiai"].as_f64().unwrap(), 0.0);

    // Direct db read: add populated, unused NULL everywhere.
    let (ef, ep, oc, ta) = read_scores(&repo.path(), S_ADD);
    assert_eq!((ef, ep), (Some(1), Some(0)));
    assert_eq!(oc, Some(1.0));
    assert_eq!(ta, Some(1.0));
    let (ef, ep, oc, ta) = read_scores(&repo.path(), S_UNUSED);
    assert_eq!((ef, ep, oc, ta), (None, None, None, None), "untouched symbol stays NULL");
}

// ── 3. Passing coverage lowers a shared symbol below 1.0 ───────────────────────

#[test]
fn scenario_03_shared_symbol_lower_suspicion() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());
    // Both a failing and a passing test touch `add`.
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5]);
    write_info(&cov.path(), &repo.path(), "tests::test_sub", &[5]);

    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    let add = suspect(&res, S_ADD).expect("add present");
    // ef=1, ep=1, F=1, P=1 → ochiai = 1/sqrt(1*2) = 0.7071..., tarantula = 1/(1+1) = 0.5.
    assert_eq!(add["ef"], 1);
    assert_eq!(add["ep"], 1);
    let oc = add["suspicion_ochiai"].as_f64().unwrap();
    assert!((oc - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9, "ochiai was {oc}");
    assert_eq!(add["suspicion_tarantula"].as_f64().unwrap(), 0.5);
}

// ── 5. Reset between runs ──────────────────────────────────────────────────────

#[test]
fn scenario_05_reset_between_runs() {
    let repo = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());

    // Run 1: test_add covers `add`.
    let cov1 = TestDir::new();
    write_info(&cov1.path(), &repo.path(), "tests::test_add", &[5]);
    let r1 = run_tool(localize_op(&repo.path(), &cov1.path(), &["tests::test_add"]));
    assert_eq!(r1["status"], "passed");
    assert_eq!(read_scores(&repo.path(), S_ADD).0, Some(1), "add scored after run 1");

    // Run 2: a different failing test covers only `sub`.
    let cov2 = TestDir::new();
    write_info(&cov2.path(), &repo.path(), "tests::test_sub", &[10]);
    let r2 = run_tool(localize_op(&repo.path(), &cov2.path(), &["tests::test_sub"]));
    assert_eq!(r2["status"], "passed");
    // `add` was reset to NULL (run 2's coverage never touched it).
    assert_eq!(read_scores(&repo.path(), S_ADD), (None, None, None, None), "add reset after run 2");
    assert_eq!(read_scores(&repo.path(), S_SUB).0, Some(1), "sub scored in run 2");
}

// ── 6. Failing test with no coverage file ──────────────────────────────────────

#[test]
fn scenario_06_failing_without_coverage() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5]);
    // test_missing has no .info file, but test_add does → still passes overall.
    let res =
        run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add", "tests::test_missing"]));
    assert_eq!(res["status"], "passed");
    let without = res["data"]["failing_tests_without_coverage"].as_array().unwrap();
    assert_eq!(without.len(), 1);
    assert_eq!(without[0], "tests::test_missing");
    assert_eq!(res["data"]["failing_test_count"], 1);
}

// ── 7. Unparseable .info → skipped_files ───────────────────────────────────────

#[test]
fn scenario_07_unparseable_skipped() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5]);
    // A passing test's file is garbage → skipped, but does not sink the run.
    fs::write(cov.path().join("tests::test_junk.info"), "not lcov at all\n").unwrap();
    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "passed");
    let skipped = res["data"]["skipped_files"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0], "tests::test_junk.info");
    assert_eq!(res["data"]["passing_test_count"], 0, "junk file is not a valid passing test");
}

// ── 8. F==0 → failed ───────────────────────────────────────────────────────────

#[test]
fn scenario_08_zero_failing_coverage_fails() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());
    // Only a passing test has coverage; the failing test's file is missing.
    write_info(&cov.path(), &repo.path(), "tests::test_sub", &[10]);
    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("F == 0"), "msg: {}", res["message"]);
}

// ── 9. Missing db → failed naming index_repository ─────────────────────────────

#[test]
fn scenario_09_missing_db_fails() {
    let repo = TestDir::new(); // exists, but no .murmur/code-graph.db
    let cov = TestDir::new();
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5]);
    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "failed");
    let msg = res["message"].as_str().unwrap();
    assert!(msg.contains("index_repository"), "msg: {msg}");
    // Must NOT have created the db.
    assert!(!repo.path().join(".murmur").join("code-graph.db").exists(), "db must not be created");
}

// ── 10. coverage_dir missing / not a dir → failed ──────────────────────────────

#[test]
fn scenario_10_coverage_dir_invalid() {
    let repo = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());

    // Missing directory.
    let res = run_tool(json!({
        "operation": "localize",
        "repo_path": repo.path().display().to_string(),
        "coverage_dir": "/nonexistent/coverage/dir",
        "failing_tests": ["tests::test_add"],
    }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("coverage_dir"));

    // A file, not a directory.
    let file = repo.path().join("not_a_dir.txt");
    fs::write(&file, "x").unwrap();
    let res2 = run_tool(json!({
        "operation": "localize",
        "repo_path": repo.path().display().to_string(),
        "coverage_dir": file.display().to_string(),
        "failing_tests": ["tests::test_add"],
    }));
    assert_eq!(res2["status"], "failed");
    assert!(res2["message"].as_str().unwrap().contains("not a directory"));
}

// ── 11. Malformed input ────────────────────────────────────────────────────────

#[test]
fn scenario_11_malformed_input() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());

    // Missing operation.
    let res = run_tool(json!({ "coverage_dir": cov.path().display().to_string(), "failing_tests": ["t"] }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("operation"));

    // Unknown operation.
    let res = run_tool(json!({ "operation": "explode" }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("unknown operation"));

    // Missing coverage_dir.
    let res = run_tool(json!({
        "operation": "localize",
        "repo_path": repo.path().display().to_string(),
        "failing_tests": ["tests::test_add"],
    }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("coverage_dir"));

    // Empty failing_tests.
    let res = run_tool(json!({
        "operation": "localize",
        "repo_path": repo.path().display().to_string(),
        "coverage_dir": cov.path().display().to_string(),
        "failing_tests": [],
    }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("failing_tests"));
}

// ── 12. stable_id resolution (unique + ambiguous) ──────────────────────────────

#[test]
fn scenario_12_stable_id_resolution() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    // test_add is unique; add a second row colliding on (module='tests', qn='dupe')
    // so `tests::dupe` resolves ambiguously to null.
    let mut syms = standard_symbols();
    syms.push(("rust://fixture/tests#dupe(->())", "tests", "dupe", "function", 50, 55));
    syms.push(("rust://fixture/tests#dupe(i64->())", "tests", "dupe", "function", 56, 60));
    build_graph_db(&repo.path(), &syms);
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5, 20]);
    write_info(&cov.path(), &repo.path(), "tests::dupe", &[50]);

    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add", "tests::dupe"]));
    let ids = res["data"]["failing_test_stable_ids"].as_array().unwrap();
    let by_name = |n: &str| ids.iter().find(|e| e["test_name"] == n).unwrap();
    assert_eq!(by_name("tests::test_add")["stable_id"], S_TEST_ADD);
    assert!(by_name("tests::dupe")["stable_id"].is_null(), "ambiguous → null");
}

// ── 13. Migration adds the four columns ────────────────────────────────────────

#[test]
fn scenario_13_migration_adds_columns() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());

    // Pre-condition: fixture db has none of the four columns.
    let db = repo.path().join(".murmur").join("code-graph.db");
    let cols_before = column_set(&db);
    for c in ["suspicion_ef", "suspicion_ep", "suspicion_ochiai", "suspicion_tarantula"] {
        assert!(!cols_before.contains(c), "{c} should be absent before migration");
    }

    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5]);
    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "passed");

    let cols_after = column_set(&db);
    for c in ["suspicion_ef", "suspicion_ep", "suspicion_ochiai", "suspicion_tarantula"] {
        assert!(cols_after.contains(c), "{c} should exist after migration");
    }
    // Existing columns/rows untouched: the files/symbols schema still present.
    assert!(cols_after.contains("visibility") && cols_after.contains("attributes"));
}

fn column_set(db: &Path) -> std::collections::HashSet<String> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(symbols)").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

// ── 14. top_suspects truncation ────────────────────────────────────────────────

#[test]
fn scenario_14_truncation_to_disk() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    // Five symbols spanning distinct line ranges, all overlapped by coverage.
    let ids: Vec<String> = (0..5).map(|i| format!("rust://fixture/#f{i}(->i64)")).collect();
    let names: Vec<String> = (0..5).map(|i| format!("f{i}")).collect();
    let mut real: Vec<Sym> = Vec::new();
    for i in 0..5usize {
        real.push((
            ids[i].as_str(),
            "",
            names[i].as_str(),
            "function",
            (i as i64) * 10 + 1,
            (i as i64) * 10 + 5,
        ));
    }
    build_graph_db(&repo.path(), &real);
    // Hit one line in every symbol's range.
    write_info(&cov.path(), &repo.path(), "tests::t", &[1, 11, 21, 31, 41]);

    let res = run_tool(json!({
        "operation": "localize",
        "repo_path": repo.path().display().to_string(),
        "coverage_dir": cov.path().display().to_string(),
        "failing_tests": ["tests::t"],
        "limit": 2,
    }));
    assert_eq!(res["status"], "passed");
    assert_eq!(res["data"]["scored_symbols"], 5);
    assert_eq!(res["data"]["truncated"], true);
    assert_eq!(res["data"]["top_suspects"].as_array().unwrap().len(), 2, "inline capped at limit");

    let data_path = res["data"]["data_path"].as_str().expect("data_path set when truncated");
    let disk: Value = serde_json::from_str(&fs::read_to_string(data_path).unwrap()).unwrap();
    assert_eq!(disk["top_suspects"].as_array().unwrap().len(), 5, "full list on disk");
    assert_eq!(disk["truncated"], false);
    // Written under repo/.murmur/coverage/.
    assert_eq!(
        Path::new(data_path).parent().unwrap(),
        repo.path().join(".murmur").join("coverage"),
        "full payload lives under .murmur/coverage"
    );
}

// ── 15. P==0: no passing tests, Tarantula still defined ────────────────────────

#[test]
fn scenario_15_no_passing_tests() {
    let repo = TestDir::new();
    let cov = TestDir::new();
    build_graph_db(&repo.path(), &standard_symbols());
    // Only the failing test has coverage — P == 0.
    write_info(&cov.path(), &repo.path(), "tests::test_add", &[5, 20]);
    let res = run_tool(localize_op(&repo.path(), &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "passed");
    assert_eq!(res["data"]["passing_test_count"], 0);
    let add = suspect(&res, S_ADD).expect("add present");
    // ef=1, ep=0, F=1, P=0 → ochiai = 1/sqrt(1*1) = 1.0; tarantula pass term = 0 → 1.0.
    assert_eq!(add["suspicion_ochiai"].as_f64().unwrap(), 1.0);
    assert_eq!(add["suspicion_tarantula"].as_f64().unwrap(), 1.0);
}

// ── 16. Symlinked repo_path: SF: paths must still join against the graph ──────

#[cfg(unix)]
#[test]
fn scenario_16_symlinked_repo_path_still_matches() {
    // `resolve_repo` canonicalizes `repo_path` (resolving any symlink components)
    // before it's used to strip the LCOV `SF:` prefix. A caller naturally invokes
    // `cargo llvm-cov` and this tool with the *same* literal repo_path; on a
    // system where that path resolves through a symlink (e.g. macOS's /tmp ->
    // /private/tmp), `cargo llvm-cov` embeds the literal (non-canonical) absolute
    // path in `SF:`. If that path isn't canonicalized the same way before prefix
    // stripping, the join silently matches zero symbols despite a "passed" status.
    let real = TestDir::new();
    build_graph_db(&real.path(), &standard_symbols());
    // The fix canonicalizes the `SF:` path before stripping, which requires the
    // referenced source file to actually exist on disk (as it would for any real
    // `cargo llvm-cov` run) — the fixture db's `files` row is otherwise a bare
    // logical path with nothing backing it on the filesystem.
    fs::create_dir_all(real.path().join("src")).unwrap();
    fs::write(real.path().join("src").join("lib.rs"), "// fixture\n").unwrap();
    let cov = TestDir::new();

    let link_parent = TestDir::new();
    let link = link_parent.path().join("repo_link");
    std::os::unix::fs::symlink(real.path(), &link).expect("symlink should be created");

    // SF: uses the symlinked (non-canonical) path, exactly as a caller who passed
    // `link` to both `cargo llvm-cov` and this tool would produce.
    write_info(&cov.path(), &link, "tests::test_add", &[5]);

    let res = run_tool(localize_op(&link, &cov.path(), &["tests::test_add"]));
    assert_eq!(res["status"], "passed", "res: {res}");
    assert_eq!(
        res["data"]["scored_symbols"], 1,
        "`add` must be matched even though repo_path was given via a symlink: {res}"
    );
}
