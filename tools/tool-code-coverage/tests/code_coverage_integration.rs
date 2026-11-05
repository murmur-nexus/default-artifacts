//! Real-filesystem, real-SQLite behaviour tests for tool-code-coverage, run against the compiled
//! `wasm32-wasip2` component in Wasmtime (tests/common).
//!
//! Each call gets a fresh `Store` with a scratch work folder at `.`, holding `repo/` (with
//! `repo/.murmur/code-graph.db`) and `cov/`, and names them relatively, as a model would. The
//! code-graph fixture db is hand-built on the host with `rusqlite` against the exact schema
//! `tool-code-graph` documents, and the four `suspicion_*` columns are deliberately omitted from
//! the fixture so the migration path is exercised. LCOV `SF:` paths are absolute host paths, as
//! `cargo llvm-cov` writes them.
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
//!   16 SF: paths spelled through a symlink to the repository still map onto indexed files

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use common::{
    build_graph_db, call, db_path, localize, read_scores, standard_symbols, write_info, Outcome,
    Sym, TestDir, S_ADD, S_SUB, S_TEST_ADD, S_UNUSED,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

/// A work folder holding `repo/` with the given symbols indexed, and the repository's host path
/// (what `SF:` lines name).
fn work_with_repo(symbols: &[Sym]) -> (TestDir, std::path::PathBuf) {
    let work = TestDir::new("it");
    let repo = work.path().join("repo");
    build_graph_db(&repo, symbols);
    (work, repo)
}

fn run(work: &TestDir, failing: &[&str]) -> Outcome {
    call(work.path(), &[], &localize("repo", "cov", failing))
}

// ── 1 & 2 & 4. Happy path F=1/P=1, exact scores, db populated ──────────────────

#[test]
fn scenario_01_happy_path_exact_scores() {
    let (work, repo) = work_with_repo(&standard_symbols());
    let cov = work.path().join("cov");
    // Failing test_add covers add(5,6) + itself(20,21). Passing test_sub covers
    // sub(10,11) + itself(30,31).
    write_info(&cov, &repo, "tests::test_add", &[5, 6, 20, 21]);
    write_info(&cov, &repo, "tests::test_sub", &[10, 11, 30, 31]);

    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "passed", "res: {res:?}");
    assert_eq!(res.data["failing_test_count"], 1);
    assert_eq!(res.data["passing_test_count"], 1);
    assert_eq!(res.data["repo_path"], "repo");
    assert_eq!(res.data["coverage_dir"], "cov");

    // `add`: touched only by the failing test → ochiai=1.0, tarantula=1.0.
    let add = res.suspect(S_ADD).expect("add present");
    assert_eq!(add["ef"], 1);
    assert_eq!(add["ep"], 0);
    assert_eq!(add["suspicion_ochiai"].as_f64().unwrap(), 1.0);
    assert_eq!(add["suspicion_tarantula"].as_f64().unwrap(), 1.0);

    // The failing test's own symbol self-covers (ef>=1) — expected, not filtered.
    let t = res.suspect(S_TEST_ADD).expect("test_add present");
    assert_eq!(t["ef"], 1);

    // `sub`: only the passing test → ef=0, both scores 0.0 (scored, not NULL).
    let sub = res.suspect(S_SUB).expect("sub present");
    assert_eq!(sub["ef"], 0);
    assert_eq!(sub["ep"], 1);
    assert_eq!(sub["suspicion_ochiai"].as_f64().unwrap(), 0.0);

    // Direct db read: add populated, unused NULL everywhere.
    let (ef, ep, oc, ta) = read_scores(&repo, S_ADD);
    assert_eq!((ef, ep), (Some(1), Some(0)));
    assert_eq!(oc, Some(1.0));
    assert_eq!(ta, Some(1.0));
    assert_eq!(
        read_scores(&repo, S_UNUSED),
        (None, None, None, None),
        "untouched symbol stays NULL"
    );
}

// ── 3. Passing coverage lowers a shared symbol below 1.0 ───────────────────────

#[test]
fn scenario_03_shared_symbol_lower_suspicion() {
    let (work, repo) = work_with_repo(&standard_symbols());
    let cov = work.path().join("cov");
    // Both a failing and a passing test touch `add`.
    write_info(&cov, &repo, "tests::test_add", &[5]);
    write_info(&cov, &repo, "tests::test_sub", &[5]);

    let res = run(&work, &["tests::test_add"]);
    let add = res.suspect(S_ADD).expect("add present");
    // ef=1, ep=1, F=1, P=1 → ochiai = 1/sqrt(1*2) = 0.7071..., tarantula = 1/(1+1) = 0.5.
    assert_eq!(add["ef"], 1);
    assert_eq!(add["ep"], 1);
    let oc = add["suspicion_ochiai"].as_f64().unwrap();
    assert!(
        (oc - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9,
        "ochiai was {oc}"
    );
    assert_eq!(add["suspicion_tarantula"].as_f64().unwrap(), 0.5);
}

// ── 5. Reset between runs ──────────────────────────────────────────────────────

#[test]
fn scenario_05_reset_between_runs() {
    let (work, repo) = work_with_repo(&standard_symbols());

    // Run 1: test_add covers `add`.
    write_info(&work.path().join("cov1"), &repo, "tests::test_add", &[5]);
    let r1 = call(
        work.path(),
        &[],
        &localize("repo", "cov1", &["tests::test_add"]),
    );
    assert_eq!(r1.status, "passed", "{r1:?}");
    assert_eq!(
        read_scores(&repo, S_ADD).0,
        Some(1),
        "add scored after run 1"
    );

    // Run 2: a different failing test covers only `sub`.
    write_info(&work.path().join("cov2"), &repo, "tests::test_sub", &[10]);
    let r2 = call(
        work.path(),
        &[],
        &localize("repo", "cov2", &["tests::test_sub"]),
    );
    assert_eq!(r2.status, "passed", "{r2:?}");
    // `add` was reset to NULL (run 2's coverage never touched it).
    assert_eq!(
        read_scores(&repo, S_ADD),
        (None, None, None, None),
        "add reset after run 2"
    );
    assert_eq!(read_scores(&repo, S_SUB).0, Some(1), "sub scored in run 2");
}

// ── 6. Failing test with no coverage file ──────────────────────────────────────

#[test]
fn scenario_06_failing_without_coverage() {
    let (work, repo) = work_with_repo(&standard_symbols());
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);
    // test_missing has no .info file, but test_add does → still passes overall.
    let res = run(&work, &["tests::test_add", "tests::test_missing"]);
    assert_eq!(res.status, "passed");
    let without = res.data["failing_tests_without_coverage"]
        .as_array()
        .unwrap();
    assert_eq!(without.len(), 1);
    assert_eq!(without[0], "tests::test_missing");
    assert_eq!(res.data["failing_test_count"], 1);
}

// ── 7. Unparseable .info → skipped_files ───────────────────────────────────────

#[test]
fn scenario_07_unparseable_skipped() {
    let (work, repo) = work_with_repo(&standard_symbols());
    let cov = work.path().join("cov");
    write_info(&cov, &repo, "tests::test_add", &[5]);
    // A passing test's file is garbage → skipped, but does not sink the run.
    fs::write(cov.join("tests::test_junk.info"), "not lcov at all\n").unwrap();
    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "passed");
    let skipped = res.data["skipped_files"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0], "tests::test_junk.info");
    assert_eq!(
        res.data["passing_test_count"], 0,
        "junk file is not a valid passing test"
    );
}

// ── 8. F==0 → failed ───────────────────────────────────────────────────────────

#[test]
fn scenario_08_zero_failing_coverage_fails() {
    let (work, repo) = work_with_repo(&standard_symbols());
    // Only a passing test has coverage; the failing test's file is missing.
    write_info(&work.path().join("cov"), &repo, "tests::test_sub", &[10]);
    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "failed");
    assert!(
        res.summary().contains("F == 0"),
        "summary: {}",
        res.summary()
    );
    assert_eq!(res.data, Value::Null);
}

// ── 9. Missing db → failed naming index_repository ─────────────────────────────

#[test]
fn scenario_09_missing_db_fails() {
    let work = TestDir::new("it");
    let repo = work.path().join("repo"); // exists, but no .murmur/code-graph.db
    fs::create_dir_all(&repo).unwrap();
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);
    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "failed");
    assert_eq!(
        res.summary(),
        "localize: code graph db not found at repo/.murmur/code-graph.db — run tool-code-graph index_repository first"
    );
    // Must NOT have created the db, nor its folder.
    assert!(!repo.join(".murmur").exists(), "db must not be created");
}

// ── 10. coverage_dir missing / not a dir → failed ──────────────────────────────

#[test]
fn scenario_10_coverage_dir_invalid() {
    let (work, repo) = work_with_repo(&standard_symbols());

    // Missing directory.
    let res = call(
        work.path(),
        &[],
        &localize("repo", "missing-cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "failed");
    assert_eq!(
        res.summary(),
        "localize: coverage_dir does not exist: missing-cov"
    );

    // A file, not a directory.
    fs::write(repo.join("not_a_dir.txt"), "x").unwrap();
    let res2 = call(
        work.path(),
        &[],
        &localize("repo", "repo/not_a_dir.txt", &["tests::test_add"]),
    );
    assert_eq!(res2.status, "failed");
    assert_eq!(
        res2.summary(),
        "localize: coverage_dir is not a directory: repo/not_a_dir.txt"
    );
}

// ── 11. Malformed input ────────────────────────────────────────────────────────

#[test]
fn scenario_11_malformed_input() {
    let (work, _repo) = work_with_repo(&standard_symbols());
    fs::create_dir_all(work.path().join("cov")).unwrap();

    // Missing operation.
    let res = call(
        work.path(),
        &[],
        &json!({ "coverage_dir": "cov", "failing_tests": ["t"] }),
    );
    assert_eq!(res.status, "failed");
    assert!(res.summary().contains("operation"));

    // Unknown operation.
    let res = call(work.path(), &[], &json!({ "operation": "explode" }));
    assert_eq!(res.status, "failed");
    assert!(res.summary().contains("unknown operation"));

    // Missing coverage_dir.
    let res = call(
        work.path(),
        &[],
        &json!({
            "operation": "localize",
            "repo_path": "repo",
            "failing_tests": ["tests::test_add"],
        }),
    );
    assert_eq!(res.status, "failed");
    assert!(res.summary().contains("coverage_dir"));

    // Empty failing_tests.
    let res = call(work.path(), &[], &localize("repo", "cov", &[]));
    assert_eq!(res.status, "failed");
    assert!(res.summary().contains("failing_tests"));
}

// ── 12. stable_id resolution (unique + ambiguous) ──────────────────────────────

#[test]
fn scenario_12_stable_id_resolution() {
    // test_add is unique; add a second row colliding on (module='tests', qn='dupe')
    // so `tests::dupe` resolves ambiguously to null.
    let mut syms = standard_symbols();
    syms.push((
        "rust://fixture/tests#dupe(->())",
        "tests",
        "dupe",
        "function",
        50,
        55,
    ));
    syms.push((
        "rust://fixture/tests#dupe(i64->())",
        "tests",
        "dupe",
        "function",
        56,
        60,
    ));
    let (work, repo) = work_with_repo(&syms);
    let cov = work.path().join("cov");
    write_info(&cov, &repo, "tests::test_add", &[5, 20]);
    write_info(&cov, &repo, "tests::dupe", &[50]);

    let res = run(&work, &["tests::test_add", "tests::dupe"]);
    let ids = res.data["failing_test_stable_ids"].as_array().unwrap();
    let by_name = |n: &str| ids.iter().find(|e| e["test_name"] == n).unwrap();
    assert_eq!(by_name("tests::test_add")["stable_id"], S_TEST_ADD);
    assert!(
        by_name("tests::dupe")["stable_id"].is_null(),
        "ambiguous → null"
    );
}

// ── 13. Migration adds the four columns ────────────────────────────────────────

#[test]
fn scenario_13_migration_adds_columns() {
    let (work, repo) = work_with_repo(&standard_symbols());

    // Pre-condition: fixture db has none of the four columns.
    let db = db_path(&repo);
    let cols_before = column_set(&db);
    for c in SUSPICION {
        assert!(
            !cols_before.contains(c),
            "{c} should be absent before migration"
        );
    }

    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);
    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "passed");

    let cols_after = column_set(&db);
    for c in SUSPICION {
        assert!(cols_after.contains(c), "{c} should exist after migration");
    }
    // Existing columns/rows untouched: the files/symbols schema still present.
    assert!(cols_after.contains("visibility") && cols_after.contains("attributes"));
}

const SUSPICION: [&str; 4] = [
    "suspicion_ef",
    "suspicion_ep",
    "suspicion_ochiai",
    "suspicion_tarantula",
];

fn column_set(db: &Path) -> HashSet<String> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(symbols)").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

// ── 14. top_suspects truncation ────────────────────────────────────────────────

#[test]
fn scenario_14_truncation_to_disk() {
    // Five symbols spanning distinct line ranges, all overlapped by coverage.
    let ids: Vec<String> = (0..5)
        .map(|i| format!("rust://fixture/#f{i}(->i64)"))
        .collect();
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
    let (work, repo) = work_with_repo(&real);
    // Hit one line in every symbol's range.
    write_info(
        &work.path().join("cov"),
        &repo,
        "tests::t",
        &[1, 11, 21, 31, 41],
    );

    let mut args = localize("repo", "cov", &["tests::t"]);
    args["limit"] = json!(2);
    let res = call(work.path(), &[], &args);
    assert_eq!(res.status, "passed");
    assert_eq!(res.data["scored_symbols"], 5);
    assert_eq!(res.data["truncated"], true);
    assert_eq!(
        res.data["top_suspects"].as_array().unwrap().len(),
        2,
        "inline capped at limit"
    );

    // Written under repo/.murmur/coverage/, named as the call named the repository.
    let data_path = res.data["data_path"]
        .as_str()
        .expect("data_path set when truncated");
    let name = data_path
        .strip_prefix("repo/.murmur/coverage/localize-")
        .and_then(|rest| rest.strip_suffix(".json"))
        .unwrap_or_else(|| panic!("data_path: {data_path}"));
    assert!(
        name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()),
        "data_path: {data_path}"
    );
    let disk: Value =
        serde_json::from_str(&fs::read_to_string(work.path().join(data_path)).unwrap()).unwrap();
    assert_eq!(
        disk["top_suspects"].as_array().unwrap().len(),
        5,
        "full list on disk"
    );
    assert_eq!(disk["truncated"], false);
    assert_eq!(disk["repo_path"], "repo");
}

// ── 15. P==0: no passing tests, Tarantula still defined ────────────────────────

#[test]
fn scenario_15_no_passing_tests() {
    let (work, repo) = work_with_repo(&standard_symbols());
    // Only the failing test has coverage — P == 0.
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5, 20]);
    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "passed");
    assert_eq!(res.data["passing_test_count"], 0);
    let add = res.suspect(S_ADD).expect("add present");
    // ef=1, ep=0, F=1, P=0 → ochiai = 1/sqrt(1*1) = 1.0; tarantula pass term = 0 → 1.0.
    assert_eq!(add["suspicion_ochiai"].as_f64().unwrap(), 1.0);
    assert_eq!(add["suspicion_tarantula"].as_f64().unwrap(), 1.0);
}

// ── 16. SF: paths spelled through a symlink still match ────────────────────────

#[cfg(unix)]
#[test]
fn scenario_16_symlinked_repo_path_still_matches() {
    // A caller may run `cargo llvm-cov` from a path that resolves through a symlink (e.g.
    // macOS's /tmp -> /private/tmp), so `SF:` names the repository by a spelling that is not
    // its canonical host path. The tool never opens or canonicalizes an `SF:` path: it infers
    // the repository prefix from the paths themselves, so any spelling maps onto the indexed
    // `src/lib.rs`.
    let (work, repo) = work_with_repo(&standard_symbols());
    let link_parent = TestDir::new("link");
    let link = link_parent.path().join("repo_link");
    std::os::unix::fs::symlink(&repo, &link).expect("symlink should be created");

    write_info(&work.path().join("cov"), &link, "tests::test_add", &[5]);

    let res = run(&work, &["tests::test_add"]);
    assert_eq!(res.status, "passed", "res: {res:?}");
    assert_eq!(
        res.data["scored_symbols"], 1,
        "`add` must be matched even though SF: names the repository through a symlink: {res:?}"
    );
    assert!(res.suspect(S_ADD).is_some());
    assert_eq!(read_scores(&repo, S_ADD).0, Some(1));
}
