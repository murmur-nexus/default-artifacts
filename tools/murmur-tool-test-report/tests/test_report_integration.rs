//! Real-filesystem integration tests for murmur-tool-test-report.
//!
//! Each test spawns the actual compiled binary via `std::process::Command`,
//! feeds a real JSON envelope on stdin, and inspects the real JSON on stdout —
//! no mocks. Format fixtures live under `tests/fixtures/`; the code-graph
//! fixture db (scenarios 7–9) is built at test time with `rusqlite` against the
//! schema `murmur-tool-code-graph` documents, because that tool's branch was not
//! merged to `main` when this slice was written.
//!
//! Scenario map (14 total):
//!   1  cargo_test explicit parse
//!   2  pytest explicit parse
//!   3  go_test explicit parse (test-failure block + panic dump)
//!   4  jest explicit parse
//!   5  format "auto" detection matches explicit output
//!   6  "auto" on unrecognized text → failed, asks for explicit format
//!   7  stable_id resolved for a unique cargo_test symbol match
//!   8  stable_id null when repo_path has no code-graph.db
//!   9  stable_id null when the symbol match is ambiguous (>1 row)
//!   10 large list (>50) is capped, truncated=true, data_path written
//!   11 below the cap → full inline list, data_path null
//!   12 missing/unreadable input_path → failed
//!   13 missing required field (input_path / operation) → failed
//!   14 malformed operation / unparseable input → failed
//!   15 all tests passing → status "passed", zero failures, no crash

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::Connection;
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
        path.push(format!("murmur-tool-test-report-it-{}-{nanos}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("temp test directory should be created");
        Self { path }
    }
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Spawn the compiled binary, feed one envelope, return the parsed stdout JSON.
fn run_tool(op: Value) -> Value {
    let envelope = json!({ "data": op.to_string(), "log_path": null }).to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_murmur-tool-test-report"))
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

fn parse_op(input_path: &Path, format: &str) -> Value {
    json!({
        "operation": "parse",
        "input_path": input_path.display().to_string(),
        "format": format,
    })
}

/// Build a code-graph.db under `repo/.murmur/` with the documented schema and the
/// given `(module, qualified_name, symbol_id)` function rows.
fn build_graph_db(repo: &Path, rows: &[(&str, &str, &str)]) {
    let dir = repo.join(".murmur");
    fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join("code-graph.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY, path TEXT UNIQUE, content_hash TEXT,
            language TEXT DEFAULT 'rust'
        );
        CREATE TABLE symbols (
            id INTEGER PRIMARY KEY, symbol_id TEXT UNIQUE, language TEXT, package TEXT,
            module TEXT, qualified_name TEXT, simple_name TEXT, signature TEXT, kind TEXT,
            file_id INTEGER, start_line INTEGER, end_line INTEGER, doc_comment TEXT DEFAULT ''
        );
        "#,
    )
    .unwrap();
    for (module, qname, symbol_id) in rows {
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name, signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture_crate', ?2, ?3, ?3, '(->())', 'function', 1, 1, 3)",
            rusqlite::params![symbol_id, module, qname],
        )
        .unwrap();
    }
}

fn failures(res: &Value) -> &Vec<Value> {
    res["data"]["failures"].as_array().expect("failures array")
}

// ── 1. cargo_test ─────────────────────────────────────────────────────────────

#[test]
fn scenario_01_cargo_test_parse() {
    let res = run_tool(parse_op(&fixtures_dir().join("cargo_test.txt"), "cargo_test"));
    assert_eq!(res["status"], "failed"); // tests failed → status mirrors outcome
    assert_eq!(res["data"]["format_used"], "cargo_test");
    assert_eq!(res["data"]["passed"], 3);
    assert_eq!(res["data"]["failed"], 2);
    assert_eq!(res["data"]["total"], 5);
    let f = failures(&res);
    assert_eq!(f.len(), 2);
    assert_eq!(f[0]["test_name"], "tests::test_add_wrong");
    assert_eq!(f[0]["file"], "src/lib.rs");
    assert_eq!(f[0]["line"], 42);
    assert_eq!(f[0]["exception"], "panic");
    assert!(f[0]["message"].as_str().unwrap().contains("left: 4"));
    assert!(f[0]["stable_id"].is_null());
    assert_eq!(f[1]["test_name"], "math::tests::test_divide");
    assert_eq!(f[1]["message"], "attempt to divide by zero");
}

// ── 2. pytest ─────────────────────────────────────────────────────────────────

#[test]
fn scenario_02_pytest_parse() {
    let res = run_tool(parse_op(&fixtures_dir().join("pytest.txt"), "pytest"));
    assert_eq!(res["data"]["format_used"], "pytest");
    assert_eq!(res["data"]["passed"], 3);
    let f = failures(&res);
    assert_eq!(f.len(), 2);
    assert_eq!(f[0]["test_name"], "test_math.py::test_add");
    assert_eq!(f[0]["file"], "test_math.py");
    assert_eq!(f[0]["line"], 10);
    assert_eq!(f[0]["exception"], "AssertionError");
    assert!(f[0]["message"].as_str().unwrap().contains("assert 3 == 4"));
    assert_eq!(f[1]["exception"], "ValueError");
    assert!(f[1]["stable_id"].is_null());
}

// ── 3. go_test ────────────────────────────────────────────────────────────────

#[test]
fn scenario_03_go_test_parse() {
    let res = run_tool(parse_op(&fixtures_dir().join("go_test.txt"), "go_test"));
    assert_eq!(res["data"]["format_used"], "go_test");
    assert_eq!(res["data"]["passed"], 1);
    let f = failures(&res);
    assert_eq!(f.len(), 2);
    assert_eq!(f[0]["test_name"], "TestSubtractWrong");
    assert_eq!(f[0]["file"], "sub_test.go");
    assert_eq!(f[0]["line"], 12);
    assert_eq!(f[0]["exception"], "test_failure");
    assert_eq!(f[0]["message"], "expected 4, got 3");
    assert_eq!(f[1]["test_name"], "TestPanics");
    assert_eq!(f[1]["exception"], "panic");
    assert_eq!(f[1]["file"], "/home/user/proj/pkg/panic_test.go");
    assert_eq!(f[1]["line"], 20);
    assert!(f[1]["message"].as_str().unwrap().contains("boom"));
}

// ── 4. jest ───────────────────────────────────────────────────────────────────

#[test]
fn scenario_04_jest_parse() {
    let res = run_tool(parse_op(&fixtures_dir().join("jest.txt"), "jest"));
    assert_eq!(res["data"]["format_used"], "jest");
    assert_eq!(res["data"]["passed"], 2);
    let f = failures(&res);
    assert_eq!(f.len(), 2);
    assert_eq!(f[0]["test_name"], "math \u{203a} subtracts numbers");
    assert_eq!(f[0]["file"], "src/math.test.js");
    assert_eq!(f[0]["line"], 10);
    assert_eq!(f[0]["exception"], "AssertionError");
    assert_eq!(f[1]["exception"], "TypeError");
    assert!(f[1]["message"].as_str().unwrap().contains("Cannot read properties"));
}

// ── 5. auto detection ─────────────────────────────────────────────────────────

#[test]
fn scenario_05_auto_matches_explicit() {
    for fmt in ["cargo_test", "pytest", "go_test", "jest"] {
        let path = fixtures_dir().join(format!("{fmt}.txt"));
        let explicit = run_tool(parse_op(&path, fmt));
        let auto = run_tool(parse_op(&path, "auto"));
        assert_eq!(auto["data"]["format_used"], fmt, "auto should detect {fmt}");
        assert_eq!(auto["data"], explicit["data"], "auto output must equal explicit for {fmt}");
    }
}

// ── 6. auto detection failure ─────────────────────────────────────────────────

#[test]
fn scenario_06_auto_unrecognized_fails() {
    let dir = TestDir::new();
    let path = dir.path().join("garbage.txt");
    fs::write(&path, "this is not any known test runner output\njust prose\n").unwrap();
    let res = run_tool(parse_op(&path, "auto"));
    assert_eq!(res["status"], "failed");
    assert_eq!(res["ok"], false);
    assert!(res["data"].is_null());
    let msg = res["message"].as_str().unwrap();
    assert!(msg.contains("auto-detect") && msg.contains("format"), "msg was: {msg}");
}

// ── 7. stable_id unique match ─────────────────────────────────────────────────

#[test]
fn scenario_07_stable_id_unique_match() {
    let repo = TestDir::new();
    let sid = "rust://fixture_crate/tests#test_add_wrong(->())";
    build_graph_db(repo.path(), &[("tests", "test_add_wrong", sid)]);
    let res = run_tool(json!({
        "operation": "parse",
        "input_path": fixtures_dir().join("cargo_test.txt").display().to_string(),
        "format": "cargo_test",
        "repo_path": repo.path().display().to_string(),
    }));
    let f = failures(&res);
    // tests::test_add_wrong resolves uniquely; math::tests::test_divide has no row.
    assert_eq!(f[0]["stable_id"], sid);
    assert!(f[1]["stable_id"].is_null());
}

// ── 8. stable_id null when no db ──────────────────────────────────────────────

#[test]
fn scenario_08_stable_id_null_without_db() {
    let repo = TestDir::new(); // exists, but has no .murmur/code-graph.db
    let res = run_tool(json!({
        "operation": "parse",
        "input_path": fixtures_dir().join("cargo_test.txt").display().to_string(),
        "format": "cargo_test",
        "repo_path": repo.path().display().to_string(),
    }));
    assert_eq!(res["status"], "failed"); // still a normal parse, tests failed
    for f in failures(&res) {
        assert!(f["stable_id"].is_null(), "no db → stable_id null");
    }
}

// ── 9. stable_id null when ambiguous ──────────────────────────────────────────

#[test]
fn scenario_09_stable_id_null_when_ambiguous() {
    let repo = TestDir::new();
    // Two rows with the same (module, qualified_name) → ambiguous → null.
    build_graph_db(
        repo.path(),
        &[
            ("tests", "test_add_wrong", "rust://fixture_crate/tests#test_add_wrong(->())"),
            ("tests", "test_add_wrong", "rust://fixture_crate/tests#test_add_wrong(i64->())"),
        ],
    );
    let res = run_tool(json!({
        "operation": "parse",
        "input_path": fixtures_dir().join("cargo_test.txt").display().to_string(),
        "format": "cargo_test",
        "repo_path": repo.path().display().to_string(),
    }));
    for f in failures(&res) {
        assert!(f["stable_id"].is_null(), "ambiguous match → stable_id null");
    }
}

// ── 10. large list truncation ─────────────────────────────────────────────────

#[test]
fn scenario_10_large_list_truncated_to_disk() {
    let dir = TestDir::new();
    let path = dir.path().join("big_cargo.txt");
    let mut raw = String::from("\nrunning 60 tests\n");
    for i in 0..60 {
        raw.push_str(&format!("test tests::t{i} ... FAILED\n"));
    }
    raw.push_str("\nfailures:\n\n");
    for i in 0..60 {
        raw.push_str(&format!(
            "---- tests::t{i} stdout ----\nthread 'tests::t{i}' panicked at src/lib.rs:{}:5:\nboom {i}\n\n",
            i + 1
        ));
    }
    raw.push_str("test result: FAILED. 0 passed; 60 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n");
    fs::write(&path, raw).unwrap();

    let res = run_tool(parse_op(&path, "cargo_test"));
    assert_eq!(res["data"]["failed"], 60);
    assert_eq!(res["data"]["truncated"], true);
    assert_eq!(failures(&res).len(), 50, "inline list capped at 50");

    let data_path = res["data"]["data_path"].as_str().expect("data_path set when truncated");
    let disk: Value = serde_json::from_str(&fs::read_to_string(data_path).unwrap()).unwrap();
    assert_eq!(disk["failures"].as_array().unwrap().len(), 60, "full list on disk");
    assert_eq!(disk["truncated"], false);
    // written next to the input file
    assert_eq!(
        Path::new(data_path).parent().unwrap(),
        dir.path(),
        "full payload lives beside input_path"
    );
}

// ── 11. below cap → inline, no data_path ──────────────────────────────────────

#[test]
fn scenario_11_below_cap_inline() {
    let res = run_tool(parse_op(&fixtures_dir().join("cargo_test.txt"), "cargo_test"));
    assert_eq!(res["data"]["truncated"], false);
    assert!(res["data"]["data_path"].is_null());
    assert_eq!(failures(&res).len(), 2);
}

// ── 12. unreadable input_path ─────────────────────────────────────────────────

#[test]
fn scenario_12_missing_input_file_fails() {
    let res = run_tool(parse_op(Path::new("/nonexistent/definitely/missing.txt"), "cargo_test"));
    assert_eq!(res["status"], "failed");
    assert_eq!(res["ok"], false);
    assert!(res["data"].is_null());
    assert!(res["message"].as_str().unwrap().contains("could not read input_path"));
}

// ── 13. missing required field ────────────────────────────────────────────────

#[test]
fn scenario_13_missing_required_field_fails() {
    // Missing input_path.
    let res = run_tool(json!({ "operation": "parse", "format": "cargo_test" }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("input_path"));

    // Missing operation.
    let res2 = run_tool(json!({ "input_path": "/tmp/x.txt" }));
    assert_eq!(res2["status"], "failed");
    assert!(res2["message"].as_str().unwrap().contains("operation"));
}

// ── 14. malformed operation / unparseable input ───────────────────────────────

#[test]
fn scenario_14_malformed_operation_and_format_fails() {
    let res = run_tool(json!({ "operation": "explode", "input_path": "/tmp/x.txt" }));
    assert_eq!(res["status"], "failed");
    assert!(res["message"].as_str().unwrap().contains("unknown operation"));

    // Unknown explicit format.
    let dir = TestDir::new();
    let path = dir.path().join("x.txt");
    fs::write(&path, "whatever").unwrap();
    let res2 = run_tool(parse_op(&path, "junit"));
    assert_eq!(res2["status"], "failed");
    assert!(res2["message"].as_str().unwrap().contains("unknown format"));
}

// ── 15. all tests passing — zero failures ─────────────────────────────────────

#[test]
fn scenario_15_all_passing_zero_failures() {
    let dir = TestDir::new();
    let path = dir.path().join("all_green.txt");
    fs::write(
        &path,
        "\nrunning 5 tests\n\
         test tests::a ... ok\n\
         test tests::b ... ok\n\
         test tests::c ... ok\n\
         test tests::d ... ok\n\
         test tests::e ... ok\n\n\
         test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
    )
    .unwrap();
    let res = run_tool(parse_op(&path, "cargo_test"));
    assert_eq!(res["status"], "passed");
    assert_eq!(res["ok"], true);
    assert_eq!(res["data"]["passed"], 5);
    assert_eq!(res["data"]["failed"], 0);
    assert_eq!(failures(&res).len(), 0);
    assert_eq!(res["summary"], "5 passed, 0 failed");
}
