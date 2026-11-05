//! Behaviour tests for tool-test-report, run against the compiled `wasm32-wasip2` component.
//!
//! Each call instantiates the component in a fresh Wasmtime `Store` with the test's scratch
//! directory preopened at `.`, the work folder murmur gives a WASM tool, and sends a real call
//! as `tool-input.data` — no mocks. Paths are relative to the scratch directory, into which
//! the format fixtures under `tests/fixtures/` are copied. Assertions read what the model
//! receives: `status`, `summary` and the parsed `data`. The code-graph fixture db (scenarios
//! 7–9) is built on the host with `rusqlite` against tool-code-graph's schema.
//!
//! Scenario map (15 total):
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

mod common;

use std::fs;
use std::path::Path;

use common::{build_graph_db, call, copy_fixture, many_cargo_failures, Outcome, TestDir};
use serde_json::{json, Value};

/// One call with `dir` as the work folder and no host folder.
fn run_tool(dir: &Path, op: Value) -> Outcome {
    call(dir, &[], &op)
}

fn parse_op(input_path: &str, format: &str) -> Value {
    json!({
        "operation": "parse",
        "input_path": input_path,
        "format": format,
    })
}

/// A scratch work folder holding the fixture `name`.
fn with_fixture(name: &str) -> TestDir {
    let dir = TestDir::new("it");
    copy_fixture(name, dir.path());
    dir
}

// ── 1. cargo_test ─────────────────────────────────────────────────────────────

#[test]
fn scenario_01_cargo_test_parse() {
    let dir = with_fixture("cargo_test.txt");
    let res = run_tool(dir.path(), parse_op("cargo_test.txt", "cargo_test"));
    assert_eq!(res.status, "failed"); // tests failed → status mirrors outcome
    assert_eq!(res.summary(), "3 passed, 2 failed");
    assert_eq!(res.data["format_used"], "cargo_test");
    assert_eq!(res.data["passed"], 3);
    assert_eq!(res.data["failed"], 2);
    assert_eq!(res.data["total"], 5);
    let f = res.failures();
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
    let dir = with_fixture("pytest.txt");
    let res = run_tool(dir.path(), parse_op("pytest.txt", "pytest"));
    assert_eq!(res.data["format_used"], "pytest");
    assert_eq!(res.data["passed"], 3);
    let f = res.failures();
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
    let dir = with_fixture("go_test.txt");
    let res = run_tool(dir.path(), parse_op("go_test.txt", "go_test"));
    assert_eq!(res.data["format_used"], "go_test");
    assert_eq!(res.data["passed"], 1);
    let f = res.failures();
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
    let dir = with_fixture("jest.txt");
    let res = run_tool(dir.path(), parse_op("jest.txt", "jest"));
    assert_eq!(res.data["format_used"], "jest");
    assert_eq!(res.data["passed"], 2);
    let f = res.failures();
    assert_eq!(f.len(), 2);
    assert_eq!(f[0]["test_name"], "math \u{203a} subtracts numbers");
    assert_eq!(f[0]["file"], "src/math.test.js");
    assert_eq!(f[0]["line"], 10);
    assert_eq!(f[0]["exception"], "AssertionError");
    assert_eq!(f[1]["exception"], "TypeError");
    assert!(f[1]["message"]
        .as_str()
        .unwrap()
        .contains("Cannot read properties"));
}

// ── 5. auto detection ─────────────────────────────────────────────────────────

#[test]
fn scenario_05_auto_matches_explicit() {
    for fmt in ["cargo_test", "pytest", "go_test", "jest"] {
        let name = format!("{fmt}.txt");
        let dir = with_fixture(&name);
        let explicit = run_tool(dir.path(), parse_op(&name, fmt));
        let auto = run_tool(dir.path(), parse_op(&name, "auto"));
        assert_eq!(auto.data["format_used"], fmt, "auto should detect {fmt}");
        assert_eq!(
            auto.data, explicit.data,
            "auto output must equal explicit for {fmt}"
        );
        assert_eq!(auto.summary, explicit.summary);
    }
}

// ── 6. auto detection failure ─────────────────────────────────────────────────

#[test]
fn scenario_06_auto_unrecognized_fails() {
    let dir = TestDir::new("it");
    fs::write(
        dir.path().join("garbage.txt"),
        "this is not any known test runner output\njust prose\n",
    )
    .unwrap();
    let res = run_tool(dir.path(), parse_op("garbage.txt", "auto"));
    assert_eq!(res.status, "failed");
    assert!(res.data.is_null());
    let msg = res.summary();
    assert!(
        msg.contains("auto-detect") && msg.contains("format"),
        "msg was: {msg}"
    );
}

// ── 7. stable_id unique match ─────────────────────────────────────────────────

#[test]
fn scenario_07_stable_id_unique_match() {
    let dir = with_fixture("cargo_test.txt");
    let sid = "rust://fixture_crate/tests#test_add_wrong(->())";
    build_graph_db(
        &dir.path().join("repo"),
        &[("tests", "test_add_wrong", sid)],
    );
    let res = run_tool(
        dir.path(),
        json!({
            "operation": "parse",
            "input_path": "cargo_test.txt",
            "format": "cargo_test",
            "repo_path": "repo",
        }),
    );
    let f = res.failures();
    // tests::test_add_wrong resolves uniquely; math::tests::test_divide has no row.
    assert_eq!(f[0]["stable_id"], sid);
    assert!(f[1]["stable_id"].is_null());
}

// ── 8. stable_id null when no db ──────────────────────────────────────────────

#[test]
fn scenario_08_stable_id_null_without_db() {
    let dir = with_fixture("cargo_test.txt");
    fs::create_dir(dir.path().join("repo")).unwrap(); // exists, but has no .murmur/code-graph.db
    let res = run_tool(
        dir.path(),
        json!({
            "operation": "parse",
            "input_path": "cargo_test.txt",
            "format": "cargo_test",
            "repo_path": "repo",
        }),
    );
    assert_eq!(res.status, "failed"); // still a normal parse, tests failed
    for f in res.failures() {
        assert!(f["stable_id"].is_null(), "no db → stable_id null");
    }
}

// ── 9. stable_id null when ambiguous ──────────────────────────────────────────

#[test]
fn scenario_09_stable_id_null_when_ambiguous() {
    let dir = with_fixture("cargo_test.txt");
    // Two rows with the same (module, qualified_name) → ambiguous → null.
    build_graph_db(
        &dir.path().join("repo"),
        &[
            (
                "tests",
                "test_add_wrong",
                "rust://fixture_crate/tests#test_add_wrong(->())",
            ),
            (
                "tests",
                "test_add_wrong",
                "rust://fixture_crate/tests#test_add_wrong(i64->())",
            ),
        ],
    );
    let res = run_tool(
        dir.path(),
        json!({
            "operation": "parse",
            "input_path": "cargo_test.txt",
            "format": "cargo_test",
            "repo_path": "repo",
        }),
    );
    for f in res.failures() {
        assert!(f["stable_id"].is_null(), "ambiguous match → stable_id null");
    }
}

// ── 10. large list truncation ─────────────────────────────────────────────────

#[test]
fn scenario_10_large_list_truncated_to_disk() {
    let dir = TestDir::new("it");
    fs::create_dir(dir.path().join("out")).unwrap();
    fs::write(
        dir.path().join("out/big_cargo.txt"),
        many_cargo_failures(60),
    )
    .unwrap();

    let res = run_tool(dir.path(), parse_op("out/big_cargo.txt", "cargo_test"));
    assert_eq!(res.data["failed"], 60);
    assert_eq!(res.data["truncated"], true);
    assert_eq!(res.failures().len(), 50, "inline list capped at 50");

    // Named as the input was, beside it.
    let data_path = res.data["data_path"]
        .as_str()
        .expect("data_path set when truncated");
    assert_eq!(data_path, "out/big_cargo.failures.json");
    let disk: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(data_path)).unwrap()).unwrap();
    assert_eq!(
        disk["failures"].as_array().unwrap().len(),
        60,
        "full list on disk"
    );
    assert_eq!(disk["truncated"], false);
}

// ── 11. below cap → inline, no data_path ──────────────────────────────────────

#[test]
fn scenario_11_below_cap_inline() {
    let dir = with_fixture("cargo_test.txt");
    let res = run_tool(dir.path(), parse_op("cargo_test.txt", "cargo_test"));
    assert_eq!(res.data["truncated"], false);
    assert!(res.data["data_path"].is_null());
    assert_eq!(res.failures().len(), 2);
}

// ── 12. unreadable input_path ─────────────────────────────────────────────────

#[test]
fn scenario_12_missing_input_file_fails() {
    let dir = TestDir::new("it");
    for input in [
        "nonexistent/definitely/missing.txt",
        "/nonexistent/definitely/missing.txt",
    ] {
        let res = run_tool(dir.path(), parse_op(input, "cargo_test"));
        assert_eq!(res.status, "failed");
        assert!(res.data.is_null());
        assert!(
            res.summary()
                .starts_with(&format!("could not read input_path '{input}': ")),
            "{}",
            res.summary()
        );
    }
}

// ── 13. missing required field ────────────────────────────────────────────────

#[test]
fn scenario_13_missing_required_field_fails() {
    let dir = TestDir::new("it");
    // Missing input_path.
    let res = run_tool(
        dir.path(),
        json!({ "operation": "parse", "format": "cargo_test" }),
    );
    assert_eq!(res.status, "failed");
    assert!(res.summary().contains("input_path"));

    // Missing operation.
    let res2 = run_tool(dir.path(), json!({ "input_path": "x.txt" }));
    assert_eq!(res2.status, "failed");
    assert!(res2.summary().contains("operation"));
}

// ── 14. malformed operation / unparseable input ───────────────────────────────

#[test]
fn scenario_14_malformed_operation_and_format_fails() {
    let dir = TestDir::new("it");
    let res = run_tool(
        dir.path(),
        json!({ "operation": "explode", "input_path": "x.txt" }),
    );
    assert_eq!(res.status, "failed");
    assert!(res.summary().contains("unknown operation"));

    // Unknown explicit format.
    fs::write(dir.path().join("x.txt"), "whatever").unwrap();
    let res2 = run_tool(dir.path(), parse_op("x.txt", "junit"));
    assert_eq!(res2.status, "failed");
    assert!(res2.summary().contains("unknown format"));
}

// ── 15. all tests passing — zero failures ─────────────────────────────────────

#[test]
fn scenario_15_all_passing_zero_failures() {
    let dir = TestDir::new("it");
    fs::write(
        dir.path().join("all_green.txt"),
        "\nrunning 5 tests\n\
         test tests::a ... ok\n\
         test tests::b ... ok\n\
         test tests::c ... ok\n\
         test tests::d ... ok\n\
         test tests::e ... ok\n\n\
         test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
    )
    .unwrap();
    let res = run_tool(dir.path(), parse_op("all_green.txt", "cargo_test"));
    assert_eq!(res.status, "passed");
    assert_eq!(res.data["passed"], 5);
    assert_eq!(res.data["failed"], 0);
    assert_eq!(res.failures().len(), 0);
    assert_eq!(res.summary(), "5 passed, 0 failed");
}
