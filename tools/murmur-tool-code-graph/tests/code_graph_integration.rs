//! Real-filesystem integration tests for murmur-tool-code-graph.
//!
//! Each test spawns the actual compiled binary via `std::process::Command`,
//! feeds a real JSON envelope on stdin, and inspects the real JSON on stdout —
//! no mocks. The fixture is a small real Rust crate written to a fresh temp
//! directory per test (the `TestDir` helper below), mirroring the pattern in
//! `murmur-tool-git`'s integration suite.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

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
        path.push(format!("murmur-tool-code-graph-it-{}-{nanos}-{n}", std::process::id()));
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

// ── Fixture: a small real crate named `fixture_crate` ─────────────────────────

const PKG: &str = "fixture_crate";
const SYM_TOP: &str = "rust://fixture_crate/#top(->i64)";
const SYM_ADD: &str = "rust://fixture_crate/#add(i64,i64->i64)";
const SYM_DOUBLED: &str = "rust://fixture_crate/#Widget::doubled(&self->i64)";
const SYM_NEW: &str = "rust://fixture_crate/#Widget::new(i64->Widget)";
const SYM_PING: &str = "rust://fixture_crate/#ping(->i64)";

fn write_fixture(repo: &Path) {
    fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{PKG}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        r#"//! Fixture crate root.

/// Adds two numbers.
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

/// A widget holding a number.
pub struct Widget {
    pub n: i64,
}

impl Widget {
    /// Construct a widget.
    pub fn new(n: i64) -> Widget {
        Widget { n }
    }
    /// Double the widget via add.
    pub fn doubled(&self) -> i64 {
        add(self.n, self.n)
    }
}

/// Entry point that builds a widget and doubles it.
pub fn top() -> i64 {
    let w = Widget::new(2);
    w.doubled()
}

/// Mutually recursive with pong — exercises cycle termination.
pub fn ping() -> i64 {
    pong()
}

/// Mutually recursive with ping.
pub fn pong() -> i64 {
    ping()
}
"#,
    )
    .unwrap();
    fs::write(
        repo.join("src/util.rs"),
        "/// An unrelated helper in another file.\npub fn helper() -> i64 {\n    42\n}\n",
    )
    .unwrap();
}

// ── Harness ───────────────────────────────────────────────────────────────────

fn run_tool(repo: &Path, op: Value) -> Value {
    let envelope = json!({ "data": op.to_string(), "log_path": null }).to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_murmur-tool-code-graph"))
        .current_dir(repo)
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

fn index(repo: &Path) -> Value {
    run_tool(repo, json!({ "operation": "index_repository", "repo_path": repo.to_str().unwrap() }))
}

// ── Scenario 1: index populates and reports a summary ─────────────────────────

#[test]
fn scenario_01_index_populates_and_reports() {
    let td = TestDir::new();
    write_fixture(td.path());
    let r = index(td.path());
    assert_eq!(r["status"], "passed");
    assert!(r["total_symbols"].as_i64().unwrap() >= 6, "symbols: {r}");
    assert!(r["total_edges"].as_i64().unwrap() >= 1, "edges: {r}");
    assert_eq!(r["changed_files"], 2); // lib.rs + util.rs
    // The db file is really on disk.
    assert!(td.path().join(".murmur/code-graph.db").exists());
}

// ── Scenario 2: identical reindex is a no-op read ─────────────────────────────

#[test]
fn scenario_02_reindex_is_noop_read() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = index(td.path());
    assert_eq!(r["status"], "passed");
    assert_eq!(r["changed_files"], 0);
    assert_eq!(r["unchanged_files"], 2);
    // Declares read so the redundant-call detector flags the repeat.
    assert_eq!(r["metadata"]["state_effect"], "read");
}

// ── Scenario 3: find_symbol ranked FTS search ─────────────────────────────────

#[test]
fn scenario_03_find_symbol_matches() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "find_symbol", "repo_path": td.path().to_str().unwrap(), "query": "widget" }),
    );
    assert_eq!(r["status"], "passed");
    let ids: Vec<&str> =
        r["matches"].as_array().unwrap().iter().map(|m| m["symbol_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&SYM_NEW) || ids.contains(&SYM_DOUBLED), "matches: {ids:?}");
}

// ── Scenario 4: find_symbol empty result is not an error ──────────────────────

#[test]
fn scenario_04_find_symbol_empty_is_passed() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "find_symbol", "repo_path": td.path().to_str().unwrap(), "query": "nonexistentzzz" }),
    );
    assert_eq!(r["status"], "passed");
    assert_eq!(r["count"], 0);
    assert!(r["matches"].as_array().unwrap().is_empty());
}

// ── Scenario 5: get_symbol full detail ────────────────────────────────────────

#[test]
fn scenario_05_get_symbol_detail() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": SYM_TOP }),
    );
    assert_eq!(r["status"], "passed");
    assert_eq!(r["qualified_name"], "top");
    assert_eq!(r["kind"], "function");
    assert_eq!(r["file"], "src/lib.rs");
    // top calls Widget::new and Widget::doubled.
    let callees: Vec<&str> = r["callees"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(callees.contains(&SYM_NEW) && callees.contains(&SYM_DOUBLED), "callees: {callees:?}");
    // Declared resource_id is the symbol_id itself (no path field to sniff).
    assert_eq!(r["metadata"]["resource_id"], SYM_TOP);
    assert_eq!(r["metadata"]["state_effect"], "read");
}

// ── Scenario 6: identity survives an unrelated edit + reindex ─────────────────

#[test]
fn scenario_06_identity_survives_unrelated_edit() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    // Edit an unrelated file (util.rs), leaving top() untouched.
    fs::write(
        td.path().join("src/util.rs"),
        "/// Edited helper.\npub fn helper() -> i64 {\n    99\n}\n",
    )
    .unwrap();
    let re = index(td.path());
    assert_eq!(re["changed_files"], 1, "only util.rs changed: {re}");
    // Same symbol_id still resolves.
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": SYM_TOP }),
    );
    assert_eq!(r["status"], "passed", "top must still resolve: {r}");
}

// ── Scenario 7: editing the symbol's own signature mints a new id ─────────────

#[test]
fn scenario_07_signature_change_mints_new_id() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    // Change top's own signature: top() -> top(x: i64).
    let lib = fs::read_to_string(td.path().join("src/lib.rs")).unwrap();
    let lib = lib.replace("pub fn top() -> i64 {", "pub fn top(x: i64) -> i64 {");
    fs::write(td.path().join("src/lib.rs"), lib).unwrap();
    index(td.path());

    // Old id no longer resolves — by design, not a defect.
    let old = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": SYM_TOP }),
    );
    assert_eq!(old["status"], "failed", "old id should be not-found: {old}");

    // New id resolves.
    let new_id = "rust://fixture_crate/#top(i64->i64)";
    let new = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": new_id }),
    );
    assert_eq!(new["status"], "passed", "new id should resolve: {new}");
}

// ── Scenario 8: slice_symbol bounded by max_depth ─────────────────────────────

#[test]
fn scenario_08_slice_bounded_by_depth() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    // Depth 1 from top reaches only its direct callees, not add (depth 2).
    let r = run_tool(
        td.path(),
        json!({ "operation": "slice_symbol", "repo_path": td.path().to_str().unwrap(),
                "symbol_id": SYM_TOP, "max_depth": 1, "max_nodes": 50 }),
    );
    assert_eq!(r["status"], "passed");
    let ids: Vec<&str> =
        r["nodes"].as_array().unwrap().iter().map(|n| n["symbol_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&SYM_TOP) && ids.contains(&SYM_DOUBLED), "ids: {ids:?}");
    assert!(!ids.contains(&SYM_ADD), "add is depth 2, excluded at max_depth 1: {ids:?}");
}

// ── Scenario 8b: slice terminates on a cyclic call graph ──────────────────────

#[test]
fn scenario_08b_slice_terminates_on_cycle() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    // ping <-> pong is a cycle; the depth bound must terminate it.
    let r = run_tool(
        td.path(),
        json!({ "operation": "slice_symbol", "repo_path": td.path().to_str().unwrap(),
                "symbol_id": SYM_PING, "max_depth": 10, "max_nodes": 50 }),
    );
    assert_eq!(r["status"], "passed");
    // Only ping and pong exist in the cycle; no runaway.
    assert_eq!(r["node_count"], 2, "cycle should yield exactly ping+pong: {r}");
}

// ── Scenario 9: slice bounded by max_nodes + summary/data_path ────────────────

#[test]
fn scenario_09_slice_bounded_by_nodes_and_writes_path() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "slice_symbol", "repo_path": td.path().to_str().unwrap(),
                "symbol_id": SYM_TOP, "max_depth": 5, "max_nodes": 2 }),
    );
    assert_eq!(r["status"], "passed");
    assert_eq!(r["node_count"], 2);
    assert_eq!(r["truncated"], true);
    // data_path points at the full slice on disk, not an inline dump.
    let dp = r["data_path"].as_str().expect("data_path present");
    assert!(Path::new(dp).exists(), "slice file should exist: {dp}");
    let full: Value = serde_json::from_str(&fs::read_to_string(dp).unwrap()).unwrap();
    assert!(full["nodes"].is_array() && full["edges"].is_array());
}

// ── Scenario 10: explain_path finds a path ────────────────────────────────────

#[test]
fn scenario_10_explain_path_found() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "explain_path", "repo_path": td.path().to_str().unwrap(),
                "from": SYM_TOP, "to": SYM_ADD, "max_depth": 6 }),
    );
    assert_eq!(r["status"], "passed");
    assert_eq!(r["found"], true);
    let path: Vec<&str> = r["path"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(path.first(), Some(&SYM_TOP));
    assert_eq!(path.last(), Some(&SYM_ADD));
}

// ── Scenario 11: explain_path no-path is a passed result ──────────────────────

#[test]
fn scenario_11_explain_path_no_path_is_passed() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    // add never calls top, so there is no add -> top path.
    let r = run_tool(
        td.path(),
        json!({ "operation": "explain_path", "repo_path": td.path().to_str().unwrap(),
                "from": SYM_ADD, "to": SYM_TOP, "max_depth": 6 }),
    );
    assert_eq!(r["status"], "passed", "no-path is not an error: {r}");
    assert_eq!(r["found"], false);
    assert!(r["message"].as_str().unwrap().contains("no path"));
}

// ── Scenario 12: missing/invalid operation → failed ───────────────────────────

#[test]
fn scenario_12_missing_operation_failed() {
    let td = TestDir::new();
    let missing = run_tool(td.path(), json!({ "repo_path": td.path().to_str().unwrap() }));
    assert_eq!(missing["status"], "failed");
    let bogus = run_tool(td.path(), json!({ "operation": "frobnicate" }));
    assert_eq!(bogus["status"], "failed");
    assert!(bogus["message"].as_str().unwrap().contains("unknown operation"));
}

// ── Scenario 13: missing required per-op field → failed ───────────────────────

#[test]
fn scenario_13_missing_required_field_failed() {
    let td = TestDir::new();
    write_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap() }),
    );
    assert_eq!(r["status"], "failed");
    assert!(r["message"].as_str().unwrap().contains("symbol_id"));
}

// ── Scenario 14: non-existent repo_path → failed ──────────────────────────────

#[test]
fn scenario_14_bad_repo_path_failed() {
    let td = TestDir::new();
    let r = run_tool(
        td.path(),
        json!({ "operation": "index_repository", "repo_path": "/no/such/repo/anywhere" }),
    );
    assert_eq!(r["status"], "failed");
    assert!(r["message"].as_str().unwrap().contains("does not exist"));
}

// ── Scenario 15: SWE-bench go/no-go is an external manual follow-up ────────────
//
// Scenario 15 (the SWE-bench go/no-go comparison) is NOT covered here: it is an
// external, manual evaluation activity outside default-artifacts' build+install
// scope. Documented as such in the build summary. No automated assertion exists
// for it by design.

// ── Extra invariant: get_symbol on a missing repo db is failed, not a panic ───

#[test]
fn extra_get_symbol_before_index_is_failed() {
    let td = TestDir::new();
    write_fixture(td.path());
    // No index yet — the db is empty; symbol lookup must be a clean failure.
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": SYM_TOP }),
    );
    assert_eq!(r["status"], "failed");
}

// ══════════════════════════════════════════════════════════════════════════════
// impact_analysis + confidence scoring (card 421cefc1)
// ══════════════════════════════════════════════════════════════════════════════

// A richer, multi-file fixture exercising every confidence case and every
// classification bucket. Call graph (forward edges), leaf `core` at the bottom:
//
//   handler ──free──▶ service ──path──▶ repo::save ──path──▶ core
//                                        │
//                                        └──free──▶ execute        (persistence marker)
//   Engine::run ──method──▶ Engine::helper ──free──▶ core
//   ambiguous_caller ──free──▶ shared        (ambiguous: ambig_a + ambig_b)
//   unit_tests::test_core ──path──▶ core     (#[test])
//   handler carries #[get("/core")]          (route)

const IPKG: &str = "impact_fixture";
const I_CORE: &str = "rust://impact_fixture/#core(->i64)";
const I_RUN: &str = "rust://impact_fixture/#Engine::run(&self->i64)";
const I_HELPER: &str = "rust://impact_fixture/#Engine::helper(&self->i64)";
const I_SAVE: &str = "rust://impact_fixture/repo#save(->i64)";
// Named `svc`, not `service`, to avoid colliding with the `mod service` symbol
// that `pub mod service;` mints (which would make the callee name ambiguous).
const I_SERVICE: &str = "rust://impact_fixture/service#svc(->i64)";
const I_HANDLER: &str = "rust://impact_fixture/handlers#handler(->i64)";
const I_TEST_CORE: &str = "rust://impact_fixture/unit_tests#test_core()";
const I_AMBIG_CALLER: &str = "rust://impact_fixture/#ambiguous_caller(->i64)";
const I_SHARED_A: &str = "rust://impact_fixture/ambig_a#shared(->i64)";

fn write_impact_fixture(repo: &Path) {
    fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{IPKG}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        r#"//! Impact analysis fixture crate.

pub mod repo;
pub mod service;
pub mod handlers;
pub mod ambig_a;
pub mod ambig_b;

/// The core leaf everyone depends on.
pub fn core() -> i64 {
    1
}

/// An engine that reaches core through a method call.
pub struct Engine;

impl Engine {
    pub fn run(&self) -> i64 {
        self.helper()
    }
    fn helper(&self) -> i64 {
        core()
    }
}

/// Free call to an ambiguous name `shared`.
pub fn ambiguous_caller() -> i64 {
    shared()
}

#[cfg(test)]
mod unit_tests {
    #[test]
    fn test_core() {
        super::core();
    }
}
"#,
    )
    .unwrap();
    fs::write(
        repo.join("src/repo.rs"),
        "//! Repository / persistence layer.\n\n/// Saves via a raw execute call.\npub fn save() -> i64 {\n    execute();\n    crate::core()\n}\n\nfn execute() -> i64 {\n    0\n}\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/service.rs"),
        "//! Service layer.\n\npub fn svc() -> i64 {\n    crate::repo::save()\n}\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/handlers.rs"),
        "//! HTTP handlers.\n\n#[get(\"/core\")]\npub fn handler() -> i64 {\n    svc()\n}\n",
    )
    .unwrap();
    fs::write(repo.join("src/ambig_a.rs"), "pub fn shared() -> i64 {\n    0\n}\n").unwrap();
    fs::write(repo.join("src/ambig_b.rs"), "pub fn shared() -> i64 {\n    1\n}\n").unwrap();
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|e| e["symbol_id"].as_str().unwrap().to_string())
        .collect()
}

fn conf_of(list: &Value, sid: &str) -> Option<String> {
    list.as_array()?
        .iter()
        .find(|e| e["symbol_id"] == sid)
        .map(|e| e["confidence"].as_str().unwrap().to_string())
}

/// Run impact_analysis over the single line a symbol occupies (looked up via
/// get_symbol), so tests don't hard-code brittle line numbers.
fn impact_over_symbol(repo: &Path, file: &str, symbol_id: &str, max_nodes: i64) -> Value {
    let g = run_tool(
        repo,
        json!({ "operation": "get_symbol", "repo_path": repo.to_str().unwrap(), "symbol_id": symbol_id }),
    );
    let start = g["start_line"].as_i64().unwrap();
    let end = g["end_line"].as_i64().unwrap();
    run_tool(
        repo,
        json!({ "operation": "impact_analysis", "repo_path": repo.to_str().unwrap(),
                "file": file, "start_line": start, "end_line": end, "max_nodes": max_nodes }),
    )
}

// ── impact 01: happy path — roots, callers, all four buckets, confidences ─────

#[test]
fn impact_01_full_shape_and_confidence() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());

    let r = impact_over_symbol(td.path(), "src/lib.rs", I_CORE, 50);
    assert_eq!(r["status"], "passed", "{r}");

    // roots = exactly core, depth 0, definite.
    assert_eq!(ids(&r["roots"]), vec![I_CORE.to_string()]);
    assert_eq!(r["roots"][0]["depth"], 0);
    assert_eq!(r["roots"][0]["confidence"], "definite");

    // callers = the reverse call-graph closure (roots excluded).
    let callers = ids(&r["callers"]);
    for expect in [I_HELPER, I_SAVE, I_TEST_CORE, I_RUN, I_SERVICE, I_HANDLER] {
        assert!(callers.contains(&expect.to_string()), "missing caller {expect}: {callers:?}");
    }
    assert!(!callers.contains(&I_CORE.to_string()), "root must not appear as its own caller");

    // Confidence: unique free/path calls are definite; the method hop to helper
    // makes run heuristic (weakest along the path).
    assert_eq!(conf_of(&r["callers"], I_HELPER).as_deref(), Some("definite"));
    assert_eq!(conf_of(&r["callers"], I_SAVE).as_deref(), Some("definite"));
    assert_eq!(conf_of(&r["callers"], I_SERVICE).as_deref(), Some("definite"));
    assert_eq!(conf_of(&r["callers"], I_HANDLER).as_deref(), Some("definite"));
    assert_eq!(conf_of(&r["callers"], I_RUN).as_deref(), Some("heuristic"),
        "run reaches core via a method hop → weakest confidence heuristic: {r}");

    // Classification buckets (drawn from roots ∪ callers).
    let tests = ids(&r["tests"]);
    assert!(tests.contains(&I_TEST_CORE.to_string()), "tests: {tests:?}");
    let routes = ids(&r["routes"]);
    assert_eq!(routes, vec![I_HANDLER.to_string()], "routes: {routes:?}");
    let persistence = ids(&r["persistence_operations"]);
    assert!(persistence.contains(&I_SAVE.to_string()), "persistence: {persistence:?}");
    let public = ids(&r["public_apis"]);
    for expect in [I_CORE, I_SAVE, I_SERVICE, I_HANDLER, I_RUN] {
        assert!(public.contains(&expect.to_string()), "public should contain {expect}: {public:?}");
    }
    // private helper / non-pub test fn are not public APIs.
    assert!(!public.contains(&I_HELPER.to_string()), "helper is private: {public:?}");
    assert!(!public.contains(&I_TEST_CORE.to_string()), "test_core is not pub: {public:?}");

    assert_eq!(r["truncated"], false);
    // Full payload is on disk.
    let dp = r["data_path"].as_str().expect("data_path present");
    assert!(Path::new(dp).exists(), "impact file should exist: {dp}");
}

// ── impact 02: root is itself a test/route/persistence/public (buckets ∋ root) ─

#[test]
fn impact_02_root_classification() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());

    // Editing the test fn itself: it is a root AND classified as a test.
    let rt = impact_over_symbol(td.path(), "src/lib.rs", I_TEST_CORE, 50);
    assert_eq!(rt["status"], "passed");
    assert!(ids(&rt["tests"]).contains(&I_TEST_CORE.to_string()), "root test bucket: {rt}");

    // Editing the handler: root, and classified as a route + public api.
    let rh = impact_over_symbol(td.path(), "src/handlers.rs", I_HANDLER, 50);
    assert!(ids(&rh["routes"]).contains(&I_HANDLER.to_string()), "root route bucket: {rh}");
    assert!(ids(&rh["public_apis"]).contains(&I_HANDLER.to_string()), "root public bucket: {rh}");

    // Editing save: root, classified as a persistence op + public api.
    let rs = impact_over_symbol(td.path(), "src/repo.rs", I_SAVE, 50);
    assert!(ids(&rs["persistence_operations"]).contains(&I_SAVE.to_string()), "root persist: {rs}");
    assert!(ids(&rs["public_apis"]).contains(&I_SAVE.to_string()), "root public: {rs}");
}

// ── impact 03: ambiguous free call surfaces as `possible` on the caller ───────

#[test]
fn impact_03_ambiguous_name_is_possible() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());

    // Editing ambig_a::shared (the lexicographically-first, hence resolved,
    // candidate for the bare `shared()` call): its caller was reached via an
    // ambiguous name → possible.
    let r = impact_over_symbol(td.path(), "src/ambig_a.rs", I_SHARED_A, 50);
    assert_eq!(r["status"], "passed", "{r}");
    assert_eq!(conf_of(&r["callers"], I_AMBIG_CALLER).as_deref(), Some("possible"),
        "ambiguous free call must be possible: {r}");
}

// ── impact 04: edge-level confidence for all four cases (direct DB inspection) ─

#[test]
fn impact_04_edge_confidence_in_db() {
    use rusqlite::Connection;
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());

    let db = td.path().join(".murmur/code-graph.db");
    let conn = Connection::open(db).unwrap();

    let conf = |src: &str, name: &str| -> String {
        conn.query_row(
            "SELECT confidence FROM edges WHERE src_symbol_id = ?1 AND dst_name = ?2 AND edge_kind = 'calls'",
            rusqlite::params![src, name],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };

    // unique free call → definite
    assert_eq!(conf(I_HELPER, "core"), "definite");
    // ambiguous free call → possible
    assert_eq!(conf(I_AMBIG_CALLER, "shared"), "possible");
    // method call → heuristic (even though `helper` is a unique name)
    assert_eq!(conf(I_RUN, "helper"), "heuristic");
    // path call to a unique name → definite
    assert_eq!(conf(I_SAVE, "core"), "definite");

    // contains edges are always definite.
    let contains_conf: String = conn
        .query_row("SELECT confidence FROM edges WHERE edge_kind = 'contains' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(contains_conf, "definite");
}

// ── impact 05: range overlapping no symbol is a passed, empty result ──────────

#[test]
fn impact_05_range_overlaps_nothing_is_empty() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "impact_analysis", "repo_path": td.path().to_str().unwrap(),
                "file": "src/lib.rs", "start_line": 9000, "end_line": 9001 }),
    );
    assert_eq!(r["status"], "passed", "empty overlap is not an error: {r}");
    assert!(ids(&r["roots"]).is_empty());
    assert!(ids(&r["callers"]).is_empty());
    assert_eq!(r["truncated"], false);
}

// ── impact 06: a leaf with no callers returns an empty callers list ───────────

#[test]
fn impact_06_leaf_has_no_callers() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());
    // Nobody calls handler; it is a root with zero callers.
    let r = impact_over_symbol(td.path(), "src/handlers.rs", I_HANDLER, 50);
    assert_eq!(r["status"], "passed");
    assert_eq!(ids(&r["roots"]), vec![I_HANDLER.to_string()]);
    assert!(ids(&r["callers"]).is_empty(), "handler has no callers: {r}");
}

// ── impact 07: max_nodes truncation ───────────────────────────────────────────

#[test]
fn impact_07_max_nodes_truncates() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());
    let r = impact_over_symbol(td.path(), "src/lib.rs", I_CORE, 2);
    assert_eq!(r["status"], "passed");
    assert_eq!(r["truncated"], true, "{r}");
    let total = ids(&r["roots"]).len() + ids(&r["callers"]).len();
    assert_eq!(total, 2, "roots+callers capped at max_nodes: {r}");
}

// ── impact 08: malformed input → failed ───────────────────────────────────────

#[test]
fn impact_08_malformed_inputs_failed() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());
    let base = td.path().to_str().unwrap();

    let missing_file = run_tool(
        td.path(),
        json!({ "operation": "impact_analysis", "repo_path": base, "start_line": 1, "end_line": 2 }),
    );
    assert_eq!(missing_file["status"], "failed");
    assert!(missing_file["message"].as_str().unwrap().contains("file"));

    let missing_start = run_tool(
        td.path(),
        json!({ "operation": "impact_analysis", "repo_path": base, "file": "src/lib.rs", "end_line": 2 }),
    );
    assert_eq!(missing_start["status"], "failed");
    assert!(missing_start["message"].as_str().unwrap().contains("start_line"));

    let missing_end = run_tool(
        td.path(),
        json!({ "operation": "impact_analysis", "repo_path": base, "file": "src/lib.rs", "start_line": 2 }),
    );
    assert_eq!(missing_end["status"], "failed");
    assert!(missing_end["message"].as_str().unwrap().contains("end_line"));

    let inverted = run_tool(
        td.path(),
        json!({ "operation": "impact_analysis", "repo_path": base, "file": "src/lib.rs",
                "start_line": 10, "end_line": 5 }),
    );
    assert_eq!(inverted["status"], "failed");
    assert!(inverted["message"].as_str().unwrap().contains("start_line"));
}

// ── impact 09: a file that was never indexed → failed (distinct from empty) ────

#[test]
fn impact_09_file_not_indexed_failed() {
    let td = TestDir::new();
    write_impact_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "impact_analysis", "repo_path": td.path().to_str().unwrap(),
                "file": "src/does_not_exist.rs", "start_line": 1, "end_line": 2 }),
    );
    assert_eq!(r["status"], "failed");
    assert!(r["message"].as_str().unwrap().contains("not indexed"), "{r}");
}

// ── impact 10: pre-slice database migrates + clears + repopulates ─────────────

/// The exact pre-migration schema: no visibility/attributes/confidence/
/// call_style columns. Used to build a database the new binary must migrate.
const OLD_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL DEFAULT 'rust'
);
CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    symbol_id TEXT NOT NULL UNIQUE,
    language TEXT NOT NULL,
    package TEXT NOT NULL,
    module TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    simple_name TEXT NOT NULL,
    signature TEXT NOT NULL,
    kind TEXT NOT NULL,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    doc_comment TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY,
    src_symbol_id TEXT NOT NULL,
    dst_symbol_id TEXT,
    dst_name TEXT NOT NULL,
    edge_kind TEXT NOT NULL,
    UNIQUE(src_symbol_id, dst_name, edge_kind)
);
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    symbol_id UNINDEXED, qualified_name, doc_comment, tokenize = 'unicode61'
);
"#;

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[test]
fn impact_10_pre_slice_db_migrates_and_repopulates() {
    use rusqlite::Connection;
    let td = TestDir::new();
    write_impact_fixture(td.path());

    // Build a pre-slice-shape database by hand. Crucially, store src/lib.rs with
    // its *real* current content hash and a stale symbol row: without the
    // migration clearing tables, the content-hash no-op fast path would skip
    // re-parsing lib.rs and the new columns would stay blank forever.
    let lib_hash = sha256_hex(&fs::read(td.path().join("src/lib.rs")).unwrap());
    fs::create_dir_all(td.path().join(".murmur")).unwrap();
    let db_path = td.path().join(".murmur/code-graph.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(OLD_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES ('src/lib.rs', ?1, 'rust')",
            rusqlite::params![lib_hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (symbol_id, language, package, module, qualified_name, simple_name,
                                  signature, kind, file_id, start_line, end_line, doc_comment)
             VALUES ('rust://impact_fixture/#stale()', 'rust', 'impact_fixture', '', 'stale', 'stale',
                     '', 'function', 1, 1, 1, '')",
            [],
        )
        .unwrap();
    }

    // Open with the new binary via a normal index pass (triggers db::open →
    // migration). Migration must add the columns and clear all rows.
    let idx = index(td.path());
    assert_eq!(idx["status"], "passed", "{idx}");

    // The stale pre-slice symbol is gone (tables were cleared on migration).
    let stale = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(),
                "symbol_id": "rust://impact_fixture/#stale()" }),
    );
    assert_eq!(stale["status"], "failed", "stale symbol must be cleared: {stale}");

    // core resolves with a *populated* visibility column — only possible if
    // lib.rs was re-parsed despite the matching content hash (i.e. the clear
    // defeated the no-op fast path).
    let g = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": I_CORE }),
    );
    assert_eq!(g["status"], "passed", "core must resolve post-migration: {g}");
    assert_eq!(g["visibility"], "pub", "visibility must be populated: {g}");

    // The new columns really exist on disk.
    let conn = Connection::open(&db_path).unwrap();
    for (table, col) in
        [("symbols", "visibility"), ("symbols", "attributes"), ("edges", "confidence"), ("edges", "call_style")]
    {
        let found: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                rusqlite::params![col],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "column {table}.{col} must exist after migration");
    }

    // And impact_analysis now sees populated confidence data.
    let r = impact_over_symbol(td.path(), "src/lib.rs", I_CORE, 50);
    assert_eq!(r["status"], "passed", "{r}");
    assert_eq!(conf_of(&r["callers"], I_RUN).as_deref(), Some("heuristic"), "{r}");
}

// ══════════════════════════════════════════════════════════════════════════════
// Python indexer (card 5a10f12b)
// ══════════════════════════════════════════════════════════════════════════════
//
// A small real Python package `pyfixture` written to disk. Layout:
//
//   pyproject.toml                 ([project] name = "pyfixture")
//   pyfixture/__init__.py          (empty — package root, collapses to "pyfixture")
//   pyfixture/core.py              add(), Widget(+methods), top(), _private()
//   pyfixture/handlers.py          handler() [@app.get route], save() [persistence], ...
//   tests/test_core.py             test_add()  (pytest naming)
//
// Forward call graph (Python):
//   top ──free──▶ Widget            (bare `Widget(2)`)
//   top ──method─▶ doubled          (`w.doubled()` — heuristic)
//   Widget::doubled ──free──▶ add
//   handler ──free──▶ save ──free──▶ execute   (execute is a persistence marker)
//   test_add ──free──▶ add
//   use_client ──method─▶ fetch     (unresolved: no `fetch` symbol)

const PYPKG: &str = "pyfixture";
const PY_ADD: &str = "python://pyfixture/pyfixture.core#add(a,b)";
const PY_WIDGET: &str = "python://pyfixture/pyfixture.core#Widget()";
const PY_DOUBLED: &str = "python://pyfixture/pyfixture.core#Widget::doubled(self)";
const PY_HIDDEN: &str = "python://pyfixture/pyfixture.core#Widget::_hidden(self)";
const PY_TOP: &str = "python://pyfixture/pyfixture.core#top()";
const PY_PRIVATE: &str = "python://pyfixture/pyfixture.core#_private()";
const PY_HANDLER: &str = "python://pyfixture/pyfixture.handlers#handler()";
const PY_SAVE: &str = "python://pyfixture/pyfixture.handlers#save()";
const PY_TEST_ADD: &str = "python://pyfixture/tests.test_core#test_add()";

fn write_python_fixture(repo: &Path) {
    fs::write(
        repo.join("pyproject.toml"),
        format!("[project]\nname = \"{PYPKG}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    fs::create_dir_all(repo.join("pyfixture")).unwrap();
    fs::write(repo.join("pyfixture/__init__.py"), "").unwrap();
    fs::write(
        repo.join("pyfixture/core.py"),
        r#""""Core module."""


def add(a, b):
    return a + b


class Widget:
    """A widget holding a number."""

    def __init__(self, n):
        self.n = n

    def doubled(self):
        return add(self.n, self.n)

    def _hidden(self):
        return add(1, 2)


def top():
    w = Widget(2)
    return w.doubled()


def _private():
    return 0
"#,
    )
    .unwrap();
    fs::write(
        repo.join("pyfixture/handlers.py"),
        r#"@app.get("/core")
def handler():
    return save()


def save():
    execute()
    return 1


def execute():
    return 0


def use_client(c):
    return c.fetch()
"#,
    )
    .unwrap();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(
        repo.join("tests/test_core.py"),
        "from pyfixture.core import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n",
    )
    .unwrap();
}

// ── py 01: index populates python rows, python:// ids, files.language ─────────

#[test]
fn py_01_index_populates_python_rows() {
    use rusqlite::Connection;
    let td = TestDir::new();
    write_python_fixture(td.path());
    let r = index(td.path());
    assert_eq!(r["status"], "passed", "{r}");
    assert_eq!(r["changed_files"], 4, "4 .py files: {r}");
    assert!(r["total_symbols"].as_i64().unwrap() >= 8, "symbols: {r}");

    let conn = Connection::open(td.path().join(".murmur/code-graph.db")).unwrap();
    // Every files row is language = 'python' (the upsert_file '\''rust'\'' literal fix).
    let non_python: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE language != 'python'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(non_python, 0, "no file row may read language != python for a pure-Python repo");
    // Symbols carry language = 'python' and python:// ids.
    let py_syms: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols WHERE language = 'python'", [], |r| r.get(0))
        .unwrap();
    assert!(py_syms >= 8, "python symbols: {py_syms}");
    let bad_ids: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols WHERE symbol_id NOT LIKE 'python://%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bad_ids, 0, "every symbol id must use the python:// scheme");
}

// ── py 02: identical reindex is a no-op read ──────────────────────────────────

#[test]
fn py_02_reindex_is_noop_read() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    let r = index(td.path());
    assert_eq!(r["status"], "passed");
    assert_eq!(r["changed_files"], 0);
    assert_eq!(r["unchanged_files"], 4);
    assert_eq!(r["metadata"]["state_effect"], "read");
}

// ── py 03: find_symbol ranked FTS search ──────────────────────────────────────

#[test]
fn py_03_find_symbol_matches() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "find_symbol", "repo_path": td.path().to_str().unwrap(), "query": "widget" }),
    );
    assert_eq!(r["status"], "passed");
    let ids: Vec<&str> =
        r["matches"].as_array().unwrap().iter().map(|m| m["symbol_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&PY_WIDGET) || ids.contains(&PY_DOUBLED), "matches: {ids:?}");
}

// ── py 04: get_symbol full detail (language, visibility, callees/callers) ──────

#[test]
fn py_04_get_symbol_detail() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": PY_TOP }),
    );
    assert_eq!(r["status"], "passed", "{r}");
    assert_eq!(r["qualified_name"], "top");
    assert_eq!(r["kind"], "function");
    assert_eq!(r["language"], "python");
    assert_eq!(r["file"], "pyfixture/core.py");
    // top calls Widget (free) and doubled (method); both resolve.
    let callees: Vec<&str> = r["callees"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(callees.contains(&PY_WIDGET) && callees.contains(&PY_DOUBLED), "callees: {callees:?}");
    assert_eq!(r["metadata"]["resource_id"], PY_TOP);
}

// ── py 05: visibility from underscore convention ──────────────────────────────

#[test]
fn py_05_visibility_underscore_convention() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    let vis = |sid: &str| -> String {
        run_tool(
            td.path(),
            json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": sid }),
        )["visibility"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(vis(PY_ADD), "pub", "add is public");
    assert_eq!(vis(PY_WIDGET), "pub", "Widget is public");
    assert_eq!(vis(PY_PRIVATE), "", "_private is private");
    assert_eq!(vis(PY_HIDDEN), "", "_hidden is private");
}

// ── py 06: identity survives an unrelated edit + reindex ───────────────────────

#[test]
fn py_06_identity_survives_unrelated_edit() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    // Edit an unrelated file (handlers.py), leaving core.py untouched.
    fs::write(
        td.path().join("pyfixture/handlers.py"),
        "def save():\n    execute()\n    return 2\n\n\ndef execute():\n    return 0\n",
    )
    .unwrap();
    let re = index(td.path());
    assert_eq!(re["changed_files"], 1, "only handlers.py changed: {re}");
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": PY_ADD }),
    );
    assert_eq!(r["status"], "passed", "add must still resolve: {r}");
}

// ── py 07: editing the symbol's own parameter list mints a new id ─────────────

#[test]
fn py_07_signature_change_mints_new_id() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    // Change add's own parameter list: add(a, b) -> add(a, b, c).
    let core = fs::read_to_string(td.path().join("pyfixture/core.py")).unwrap();
    let core = core.replace("def add(a, b):", "def add(a, b, c):");
    fs::write(td.path().join("pyfixture/core.py"), core).unwrap();
    index(td.path());

    // Old id no longer resolves.
    let old = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": PY_ADD }),
    );
    assert_eq!(old["status"], "failed", "old id should be not-found: {old}");

    // New id resolves.
    let new_id = "python://pyfixture/pyfixture.core#add(a,b,c)";
    let new = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(), "symbol_id": new_id }),
    );
    assert_eq!(new["status"], "passed", "new id should resolve: {new}");
}

// ── py 08: slice_symbol bounded by max_depth ──────────────────────────────────

#[test]
fn py_08_slice_bounded_by_depth() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    // Depth 1 from top reaches its direct callees (Widget, doubled) not add (depth 2).
    let r = run_tool(
        td.path(),
        json!({ "operation": "slice_symbol", "repo_path": td.path().to_str().unwrap(),
                "symbol_id": PY_TOP, "max_depth": 1, "max_nodes": 50 }),
    );
    assert_eq!(r["status"], "passed");
    let ids: Vec<&str> =
        r["nodes"].as_array().unwrap().iter().map(|n| n["symbol_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&PY_TOP) && ids.contains(&PY_DOUBLED), "ids: {ids:?}");
    assert!(!ids.contains(&PY_ADD), "add is depth 2, excluded at max_depth 1: {ids:?}");
}

// ── py 09: explain_path finds a path (through a method hop) ────────────────────

#[test]
fn py_09_explain_path_found() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "explain_path", "repo_path": td.path().to_str().unwrap(),
                "from": PY_TOP, "to": PY_ADD, "max_depth": 6 }),
    );
    assert_eq!(r["status"], "passed");
    assert_eq!(r["found"], true, "{r}");
    let path: Vec<&str> = r["path"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(path.first(), Some(&PY_TOP));
    assert_eq!(path.last(), Some(&PY_ADD));
}

// ── py 10: impact_analysis — full shape, buckets, python classification ────────

#[test]
fn py_10_impact_full_shape() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());

    let r = impact_over_symbol(td.path(), "pyfixture/core.py", PY_ADD, 50);
    assert_eq!(r["status"], "passed", "{r}");

    // roots = exactly add, depth 0, definite.
    assert_eq!(ids(&r["roots"]), vec![PY_ADD.to_string()]);
    assert_eq!(r["roots"][0]["confidence"], "definite");

    // callers include doubled, _hidden, test_add, and (transitively) top.
    let callers = ids(&r["callers"]);
    for expect in [PY_DOUBLED, PY_HIDDEN, PY_TEST_ADD, PY_TOP] {
        assert!(callers.contains(&expect.to_string()), "missing caller {expect}: {callers:?}");
    }
    assert!(!callers.contains(&PY_ADD.to_string()), "root must not be its own caller");

    // top reaches add via a method hop (w.doubled()) → heuristic (weakest).
    assert_eq!(conf_of(&r["callers"], PY_TOP).as_deref(), Some("heuristic"),
        "top reaches add through a method call → heuristic: {r}");
    // doubled -> add is a bare free call to a unique name → definite.
    assert_eq!(conf_of(&r["callers"], PY_DOUBLED).as_deref(), Some("definite"));

    // tests bucket: test_add (test_ prefix + test_*.py file + tests/ dir).
    assert!(ids(&r["tests"]).contains(&PY_TEST_ADD.to_string()), "tests: {r}");

    // public_apis: names not starting with _  (add, top, doubled); not _hidden.
    let public = ids(&r["public_apis"]);
    assert!(public.contains(&PY_ADD.to_string()), "add public: {public:?}");
    assert!(public.contains(&PY_DOUBLED.to_string()), "doubled public: {public:?}");
    assert!(!public.contains(&PY_HIDDEN.to_string()), "_hidden is private: {public:?}");
}

// ── py 11: impact_analysis — route + persistence classification ───────────────

#[test]
fn py_11_impact_route_and_persistence() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());

    // handler carries @app.get("/core") → route; it is a root with no callers.
    let rh = impact_over_symbol(td.path(), "pyfixture/handlers.py", PY_HANDLER, 50);
    assert_eq!(rh["status"], "passed");
    assert!(ids(&rh["routes"]).contains(&PY_HANDLER.to_string()), "route bucket: {rh}");

    // save calls execute() (a persistence marker) → persistence bucket.
    let rs = impact_over_symbol(td.path(), "pyfixture/handlers.py", PY_SAVE, 50);
    assert!(ids(&rs["persistence_operations"]).contains(&PY_SAVE.to_string()), "persist: {rs}");
}

// ── py 12: method call is always heuristic (edge-level DB inspection) ──────────

#[test]
fn py_12_method_call_is_heuristic() {
    use rusqlite::Connection;
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());

    let conn = Connection::open(td.path().join(".murmur/code-graph.db")).unwrap();
    let conf = |src: &str, name: &str| -> String {
        conn.query_row(
            "SELECT confidence FROM edges WHERE src_symbol_id = ?1 AND dst_name = ?2 AND edge_kind = 'calls'",
            rusqlite::params![src, name],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    // top -> doubled is a method call (w.doubled()); even though `doubled` is a
    // unique name it must be heuristic, never definite.
    assert_eq!(conf(PY_TOP, "doubled"), "heuristic");
    // doubled -> add is a free call to a unique name → definite.
    assert_eq!(conf(PY_DOUBLED, "add"), "definite");
    // Its call_style column is recorded as 'method'.
    let style: String = conn
        .query_row(
            "SELECT call_style FROM edges WHERE src_symbol_id = ?1 AND dst_name = 'doubled'",
            rusqlite::params![PY_TOP],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(style, "method");
}

// ── py 13: missing symbol_id → failed (input validation unchanged) ────────────

#[test]
fn py_13_missing_symbol_id_failed() {
    let td = TestDir::new();
    write_python_fixture(td.path());
    index(td.path());
    let r = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap() }),
    );
    assert_eq!(r["status"], "failed");
    assert!(r["message"].as_str().unwrap().contains("symbol_id"));
}

// ── py 14: a single index pass covers a mixed Rust + Python repo ───────────────

fn write_mixed_fixture(repo: &Path) {
    // A Rust crate at the root...
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"mixcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn rust_fn() -> i64 {\n    1\n}\n").unwrap();
    // ...and a Python tree beside it.
    fs::create_dir_all(repo.join("py")).unwrap();
    fs::write(
        repo.join("py/app.py"),
        "def py_fn():\n    return helper()\n\n\ndef helper():\n    return 2\n",
    )
    .unwrap();
}

#[test]
fn py_14_mixed_repo_indexes_both_languages() {
    use rusqlite::Connection;
    let td = TestDir::new();
    write_mixed_fixture(td.path());
    let r = index(td.path());
    assert_eq!(r["status"], "passed", "{r}");
    assert_eq!(r["changed_files"], 2, "lib.rs + app.py: {r}");

    let conn = Connection::open(td.path().join(".murmur/code-graph.db")).unwrap();
    // Rust file/symbol...
    let rust_files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE language = 'rust'", [], |r| r.get(0))
        .unwrap();
    let py_files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE language = 'python'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rust_files, 1, "one rust file");
    assert_eq!(py_files, 1, "one python file");

    // Both symbol schemes present in one graph.
    let rust_fn = run_tool(
        td.path(),
        json!({ "operation": "get_symbol", "repo_path": td.path().to_str().unwrap(),
                "symbol_id": "rust://mixcrate/#rust_fn(->i64)" }),
    );
    assert_eq!(rust_fn["status"], "passed", "rust symbol resolves: {rust_fn}");
    // The Python package name falls back to the repo directory name (an opaque
    // tempdir), so assert the Python symbol via find_symbol rather than a
    // hard-coded id, keeping the test independent of the tempdir name.
    let found = run_tool(
        td.path(),
        json!({ "operation": "find_symbol", "repo_path": td.path().to_str().unwrap(), "query": "py_fn" }),
    );
    let ids: Vec<String> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["symbol_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.iter().any(|s| s.starts_with("python://") && s.contains("py.app#py_fn")),
        "python symbol indexed in mixed repo: {ids:?}");
}
