//! What only the compiled `wasm32-wasip2` component can prove, run through Wasmtime with the
//! preopens murmur gives a WASM tool: the work folder at `.` and each host folder at
//! `murmur-host<path>`.
//!
//! * Parity with the native 0.5.0 tool: every call recorded from it
//!   (`tests/fixtures/native-0.5.0-results.json`, recipe in `tests/fixtures/README.md`) gives
//!   the model the same `status`, `summary` and `data`.
//! * `stable_id` from code-graph's database across calls and folders: a database written
//!   between two calls is read by the next, one in a read-only host folder is read, no lock or
//!   journal file is ever created, and a WAL-mode or garbage database degrades to null.
//! * Absolute paths open only inside a host folder, refusals name the value as written, and
//!   nothing lands in the work folder.

mod common;

use std::fs;

use common::{
    build_graph_db, call, copy_fixture, fixtures_dir, many_cargo_failures, tree, HostFolder,
    Outcome, TestDir,
};
use rusqlite::Connection;
use serde_json::{json, Value};

/// `summary` with a trailing ` (os error N)` removed: the errno differs between the native
/// build (Linux `2`) and WASI (`44`) for the same error.
fn without_os_error(summary: &str) -> &str {
    match summary.rfind(" (os error ") {
        Some(at) if summary.ends_with(')') => &summary[..at],
        _ => summary,
    }
}

#[test]
fn native_0_5_0_results() {
    let recorded: Vec<Value> = serde_json::from_str(
        &fs::read_to_string(fixtures_dir().join("native-0.5.0-results.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(recorded.len(), 13, "the recipe records 13 cases");

    let work = TestDir::new("native");
    for entry in fs::read_dir(fixtures_dir()).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".txt") {
            copy_fixture(&name, work.path());
        }
    }
    let before = tree(work.path());

    for case in &recorded {
        let name = case["name"].as_str().unwrap();
        let got = call(work.path(), &[], &case["args"]);
        assert_eq!(
            got.status,
            case["status"].as_str().unwrap(),
            "{name}: status"
        );
        let want_summary = case["summary"].as_str().unwrap();
        assert_eq!(
            without_os_error(got.summary()),
            without_os_error(want_summary),
            "{name}: summary"
        );
        // Byte for byte: the compact JSON murmur made of the native `data`.
        let want_data = match &case["data"] {
            Value::Null => None,
            data => Some(serde_json::to_string(data).unwrap()),
        };
        assert_eq!(got.data_raw, want_data, "{name}: data");
    }
    assert_eq!(tree(work.path()), before, "parsing wrote nothing");
}

fn cargo_call(repo_path: &str) -> Value {
    json!({
        "operation": "parse",
        "input_path": "cargo.txt",
        "format": "cargo_test",
        "repo_path": repo_path,
    })
}

const SID: &str = "rust://fixture_crate/tests#test_add_wrong(->())";

/// The `stable_id` of `tests::test_add_wrong`, the first failure of the cargo fixture, after
/// checking the call was a normal parse of failing tests.
fn first_stable_id(outcome: &Outcome) -> Value {
    assert_eq!(outcome.status, "failed", "{outcome:?}");
    assert_eq!(outcome.summary(), "3 passed, 2 failed");
    let failures = outcome.failures();
    assert_eq!(failures[0]["test_name"], "tests::test_add_wrong");
    assert!(failures[1]["stable_id"].is_null());
    failures[0]["stable_id"].clone()
}

/// The names in `<repo>/.murmur`: nothing but `code-graph.db` may be there.
fn murmur_dir_entries(repo: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(repo.join(".murmur"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn s6a_a_database_written_between_calls_is_read_by_the_next() {
    let work = TestDir::new("s6a");
    fs::copy(
        fixtures_dir().join("cargo_test.txt"),
        work.path().join("cargo.txt"),
    )
    .unwrap();

    let first = call(work.path(), &[], &cargo_call("."));
    assert!(first_stable_id(&first).is_null(), "no database yet");
    assert!(
        !work.path().join(".murmur").exists(),
        "the lookup created nothing"
    );

    build_graph_db(work.path(), &[("tests", "test_add_wrong", SID)]);
    let second = call(work.path(), &[], &cargo_call("."));
    assert_eq!(first_stable_id(&second), SID);
    assert_eq!(murmur_dir_entries(work.path()), ["code-graph.db"]);
}

#[test]
fn s6b_a_database_in_a_read_only_host_folder_is_read_without_a_lock_file() {
    let work = TestDir::new("s6b-work");
    fs::copy(
        fixtures_dir().join("cargo_test.txt"),
        work.path().join("cargo.txt"),
    )
    .unwrap();
    let host = TestDir::new("s6b-host");
    let repo = host.path().join("repo");
    build_graph_db(&repo, &[("tests", "test_add_wrong", SID)]);
    let work_before = tree(work.path());

    let outcome = call(
        work.path(),
        &[HostFolder {
            path: host.path(),
            writable: false,
        }],
        &cargo_call(repo.to_str().unwrap()),
    );
    assert_eq!(first_stable_id(&outcome), SID);
    assert_eq!(murmur_dir_entries(&repo), ["code-graph.db"]);
    assert_eq!(
        tree(work.path()),
        work_before,
        "nothing landed in the work folder"
    );

    // The same database through a read-write folder leaves no lock or journal either.
    let outcome = call(
        work.path(),
        &[HostFolder {
            path: host.path(),
            writable: true,
        }],
        &cargo_call(repo.to_str().unwrap()),
    );
    assert_eq!(first_stable_id(&outcome), SID);
    assert_eq!(murmur_dir_entries(&repo), ["code-graph.db"]);
}

#[test]
fn s6d_a_wal_mode_database_degrades_to_null() {
    let work = TestDir::new("s6d");
    fs::copy(
        fixtures_dir().join("cargo_test.txt"),
        work.path().join("cargo.txt"),
    )
    .unwrap();
    build_graph_db(work.path(), &[("tests", "test_add_wrong", SID)]);
    let db = work.path().join(".murmur/code-graph.db");
    {
        let conn = Connection::open(&db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    // Closing the last connection checkpoints the WAL and removes -wal and -shm, so the file
    // alone is what the tool sees: a database whose header says WAL.
    assert_eq!(murmur_dir_entries(work.path()), ["code-graph.db"]);

    let outcome = call(work.path(), &[], &cargo_call("."));
    assert!(
        first_stable_id(&outcome).is_null(),
        "WAL is unreadable on WASI"
    );
    assert_eq!(murmur_dir_entries(work.path()), ["code-graph.db"]);
}

#[test]
fn s6e_a_garbage_database_degrades_to_null() {
    let work = TestDir::new("s6e");
    fs::copy(
        fixtures_dir().join("cargo_test.txt"),
        work.path().join("cargo.txt"),
    )
    .unwrap();
    fs::create_dir(work.path().join(".murmur")).unwrap();
    fs::write(
        work.path().join(".murmur/code-graph.db"),
        b"this is not an sqlite database, just bytes \x00\x01\x02",
    )
    .unwrap();
    let outcome = call(work.path(), &[], &cargo_call("."));
    assert!(first_stable_id(&outcome).is_null());
    assert_eq!(murmur_dir_entries(work.path()), ["code-graph.db"]);
}

fn parse_abs(input: &str) -> Value {
    json!({ "operation": "parse", "input_path": input, "format": "cargo_test" })
}

#[test]
fn s7_absolute_paths_open_only_inside_host_folders() {
    let work = TestDir::new("s7-work");
    let rw = TestDir::new("s7-rw");
    let ro = TestDir::new("s7-ro");
    for dir in [rw.path(), ro.path()] {
        fs::create_dir(dir.join("out")).unwrap();
        fs::write(dir.join("out/big.txt"), many_cargo_failures(60)).unwrap();
    }
    copy_fixture("pytest.txt", work.path());
    copy_fixture("cargo_test.txt", work.path());
    let work_before = tree(work.path());
    let rw_root = HostFolder {
        path: rw.path(),
        writable: true,
    };
    let ro_root = HostFolder {
        path: ro.path(),
        writable: false,
    };

    // Under a read-write root: the full list is written beside the input, and data_path is the
    // absolute host path.
    let input = format!("{}/out/big.txt", rw.path().display());
    let outcome = call(work.path(), &[rw_root], &parse_abs(&input));
    assert_eq!(outcome.status, "failed");
    assert_eq!(outcome.data["failed"], 60);
    assert_eq!(outcome.data["truncated"], true);
    let data_path = format!("{}/out/big.failures.json", rw.path().display());
    assert_eq!(outcome.data["data_path"], data_path.as_str());
    let disk: Value = serde_json::from_str(&fs::read_to_string(&data_path).unwrap()).unwrap();
    assert_eq!(disk["failures"].as_array().unwrap().len(), 60);

    // Under a read-only root: parses, but nothing is written and data_path is null.
    let ro_before = tree(ro.path());
    let input = format!("{}/out/big.txt", ro.path().display());
    let outcome = call(work.path(), &[ro_root], &parse_abs(&input));
    assert_eq!(outcome.status, "failed");
    assert_eq!(outcome.data["failed"], 60);
    assert_eq!(outcome.data["truncated"], true);
    assert!(outcome.data["data_path"].is_null());
    assert_eq!(tree(ro.path()), ro_before);

    let rw_root = || HostFolder {
        path: rw.path(),
        writable: true,
    };
    let ro_root = || HostFolder {
        path: ro.path(),
        writable: false,
    };

    // No root at all.
    let input = format!("{}/out/big.txt", rw.path().display());
    let outcome = call(work.path(), &[], &parse_abs(&input));
    assert_eq!(outcome.status, "failed");
    assert!(outcome.data.is_null());
    assert_eq!(
        outcome.summary(),
        format!(
            "could not read input_path '{input}': this session opens no host folder to \
             tool-test-report; a relative path opens in the work folder"
        )
    );

    // Roots held, none covering.
    let outside = "/definitely/not/a/root/cargo.txt";
    let outcome = call(work.path(), &[rw_root(), ro_root()], &parse_abs(outside));
    assert_eq!(outcome.status, "failed");
    assert_eq!(
        outcome.summary(),
        format!(
            "could not read input_path '{outside}': it is outside every host folder \
             tool-test-report may open: {} (read-write), {} (read). A relative path opens in \
             the work folder",
            rw.path().display(),
            ro.path().display()
        )
    );

    // An uncovered repo_path fails a cargo_test call, and is ignored for pytest.
    let repo = "/definitely/not/a/root/repo";
    let outcome = call(
        work.path(),
        &[rw_root()],
        &json!({ "operation": "parse", "input_path": "cargo_test.txt",
                 "format": "cargo_test", "repo_path": repo }),
    );
    assert_eq!(outcome.status, "failed");
    assert!(outcome.data.is_null());
    assert_eq!(
        outcome.summary(),
        format!(
            "could not open repo_path '{repo}': it is outside every host folder \
             tool-test-report may open: {} (read-write). A relative path opens in the work \
             folder ('.' is the work folder itself)",
            rw.path().display()
        )
    );
    let outcome = call(
        work.path(),
        &[],
        &json!({ "operation": "parse", "input_path": "cargo_test.txt",
                 "format": "cargo_test", "repo_path": repo }),
    );
    assert_eq!(
        outcome.summary(),
        format!(
            "could not open repo_path '{repo}': this session opens no host folder to \
             tool-test-report; a relative path opens in the work folder ('.' is the work \
             folder itself)"
        )
    );
    for hosts in [vec![rw_root()], vec![]] {
        let outcome = call(
            work.path(),
            &hosts,
            &json!({ "operation": "parse", "input_path": "pytest.txt",
                     "format": "pytest", "repo_path": repo }),
        );
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.summary(), "3 passed, 2 failed");
        assert_eq!(outcome.data["format_used"], "pytest");
    }

    // A relative value starting with murmur-host, while a root is held.
    let outcome = call(work.path(), &[rw_root()], &parse_abs("murmur-host/x"));
    assert_eq!(
        outcome.summary(),
        "'input_path' starts with murmur-host, where this tool's host folders are opened; name \
         the host path absolutely: '/x'"
    );
    let outcome = call(
        work.path(),
        &[rw_root()],
        &json!({ "operation": "parse", "input_path": "cargo_test.txt",
                 "format": "cargo_test", "repo_path": "./murmur-host/srv/repo" }),
    );
    assert_eq!(
        outcome.summary(),
        "'repo_path' starts with murmur-host, where this tool's host folders are opened; name \
         the host path absolutely: '/srv/repo'"
    );

    assert_eq!(
        tree(work.path()),
        work_before,
        "no absolute value landed in the work folder"
    );
}
