// The `symbol_history` suite (slice deb59b9b-stable-id-aware-git-history). Compiled once per
// backend (see tests/common/mod.rs).
//
// Every test builds a real git repo at `<work>/repo` and a hand-built
// `.murmur/code-graph.db` matching `tool-code-graph`'s documented schema. No mocks. Each
// scenario runs twice: with `repo` the absolute path, and with `repo` the relative `repo`.
//
// Scenario coverage (from the slice spec):
//   1  happy_path_single_commit
//   2  multi_commit_history_with_n
//   3  range_shifted_by_unrelated_edits   (the core value proposition)
//   4  unknown_symbol_id
//   5  repo_never_indexed
//   6  missing_symbol_id / empty_symbol_id
//   7  symbol_added_but_never_committed
//   8  stale_index_range_beyond_eof

use std::{
    fs,
    path::{Path, PathBuf},
};

use common::{build_graph_db, git_ok};
use serde_json::{json, Value};

/// How a scenario names the repository: by its absolute path or relative to the work folder.
#[derive(Clone, Copy)]
enum Mode {
    Absolute,
    Relative,
}

/// A work folder holding the repository `repo/`.
struct Fixture {
    work: common::TestDir,
    repo: PathBuf,
    mode: Mode,
}

impl Fixture {
    fn new(mode: Mode) -> Self {
        let work = common::TestDir::new("murmur-git-symhist");
        let repo = work.path().join("repo");
        Fixture { work, repo, mode }
    }

    fn path(&self) -> &Path {
        &self.repo
    }

    /// The `repo` value the call passes.
    fn arg(&self) -> String {
        match self.mode {
            Mode::Absolute => self.repo.to_str().unwrap().to_string(),
            Mode::Relative => "repo".to_string(),
        }
    }

    fn call(&self, op: Value) -> Value {
        common::Env::new(BACKEND, self.work.path()).call(op)
    }
}

fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    let r = repo.to_str().unwrap();
    git_ok(&["init", r]);
    git_ok(&["-C", r, "config", "user.email", "test@example.com"]);
    git_ok(&["-C", r, "config", "user.name", "Test"]);
    // A committed README so HEAD always exists even before the target file lands.
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    git_ok(&["-C", r, "add", "README.md"]);
    git_ok(&["-C", r, "commit", "-m", "initial"]);
}

/// Write `path` (repo-relative) with `body`, stage it, and commit with `msg`.
fn write_commit(repo: &Path, path: &str, body: &str, msg: &str) {
    let full = repo.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, body).unwrap();
    let r = repo.to_str().unwrap();
    git_ok(&["-C", r, "add", path]);
    git_ok(&["-C", r, "commit", "-m", msg]);
}

/// The call's `error_kind`, which only the core's envelope carries.
fn assert_kind(res: &Value, kind: Option<&str>) {
    if BACKEND == common::Backend::Core {
        assert_eq!(res["error_kind"].as_str(), kind, "error_kind; got {res}");
    }
}

fn symbol_history(repo: &Fixture, symbol_id: &str, n: Option<u64>) -> Value {
    let mut op = json!({
        "operation": "symbol_history",
        "repo": repo.arg(),
        "symbol_id": symbol_id,
    });
    if let Some(n) = n {
        op["n"] = json!(n);
    }
    repo.call(op)
}

const SID: &str = "rust://fixture/#sym(->())";

// ── Scenario 1: happy path, single commit ───────────────────────────────────────

fn scenario_01_happy_path_single_commit(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    // A 4-line function, its entire content added in one commit.
    write_commit(
        repo.path(),
        "src/lib.rs",
        "fn sym() {\n    let x = 1;\n    x\n}\n",
        "add sym",
    );
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = symbol_history(&repo, SID, None);

    assert_eq!(res["status"], "passed", "res={res}");
    assert_eq!(res["status"], "passed");
    assert_eq!(res["data"]["symbol_id"], SID);
    assert_eq!(res["data"]["file"], "src/lib.rs");
    assert_eq!(res["data"]["start_line"], 1);
    assert_eq!(res["data"]["end_line"], 4);
    let commits = res["data"]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "exactly one commit; res={res}");
    let c = &commits[0];
    assert_eq!(c["subject"], "add sym");
    assert_eq!(c["author"], "Test");
    assert!(c["hash"].as_str().unwrap().len() >= 40);
    assert!(!c["short_hash"].as_str().unwrap().is_empty());
    assert!(!c["date_iso"].as_str().unwrap().is_empty());
}

// ── Scenario 2: multi-commit history with n, newest first ────────────────────────

fn scenario_02_multi_commit_history_with_n(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    // Create the function, then edit a line inside its range across 3 commits.
    write_commit(
        repo.path(),
        "src/lib.rs",
        "fn sym() {\n    let x = 1;\n    x\n}\n",
        "c1 create",
    );
    write_commit(
        repo.path(),
        "src/lib.rs",
        "fn sym() {\n    let x = 2;\n    x\n}\n",
        "c2 edit",
    );
    write_commit(
        repo.path(),
        "src/lib.rs",
        "fn sym() {\n    let x = 3;\n    x\n}\n",
        "c3 edit",
    );
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = symbol_history(&repo, SID, Some(3));
    assert_eq!(res["status"], "passed", "res={res}");
    let commits = res["data"]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 3, "up to 3 commits; res={res}");
    // Newest first.
    assert_eq!(commits[0]["subject"], "c3 edit");
    assert_eq!(commits[1]["subject"], "c2 edit");
    assert_eq!(commits[2]["subject"], "c1 create");
    // Distinct commits.
    let hashes: Vec<&str> = commits
        .iter()
        .map(|c| c["hash"].as_str().unwrap())
        .collect();
    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);

    // Default n=1 returns only the newest.
    let one = symbol_history(&repo, SID, None);
    assert_eq!(one["data"]["commits"].as_array().unwrap().len(), 1);
    assert_eq!(one["data"]["commits"][0]["subject"], "c3 edit");
}

// ── Scenario 3: range shifted by unrelated edits (core value proposition) ────────

fn scenario_03_range_shifted_by_unrelated_edits(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    // Commit A: the symbol's real content at lines 1-4.
    write_commit(
        repo.path(),
        "src/lib.rs",
        "fn sym() {\n    let x = 1;\n    x\n}\n",
        "A: create sym",
    );
    // Commit B: insert 5 unrelated lines ABOVE the symbol. The symbol's own
    // content is untouched; it now lives at lines 6-9.
    write_commit(
        repo.path(),
        "src/lib.rs",
        "// pad 1\n// pad 2\n// pad 3\n// pad 4\n// pad 5\nfn sym() {\n    let x = 1;\n    x\n}\n",
        "B: insert unrelated lines above",
    );
    // Re-index: the symbol's current range is now 6-9.
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 6, 9)]);

    let res = symbol_history(&repo, SID, None);
    assert_eq!(res["status"], "passed", "res={res}");
    assert_eq!(res["data"]["start_line"], 6);
    assert_eq!(res["data"]["end_line"], 9);
    let commits = res["data"]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "res={res}");
    // The last commit that touched the symbol's own content is A, NOT the later
    // insertion B. A stale line number / raw git blame would wrongly report B.
    assert_eq!(
        commits[0]["subject"], "A: create sym",
        "must attribute to the symbol's own edit, not the unrelated insertion; res={res}"
    );
}

// ── Scenario 4: unknown symbol_id → not_found ────────────────────────────────────

fn scenario_04_unknown_symbol_id(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    1\n}\n", "add");
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 3)]);

    let res = symbol_history(&repo, "rust://fixture/#does_not_exist(->())", None);
    assert_eq!(res["status"], "error", "res={res}");
    assert_eq!(res["status"], "error");
    assert_kind(&res, Some("not_found"));
    assert!(
        res["data"]["commits"].is_null(),
        "no partial commit list; res={res}"
    );
}

// ── Scenario 5: repo never indexed → not_indexed, db not created ─────────────────

fn scenario_05_repo_never_indexed(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    1\n}\n", "add");
    // No build_graph_db — .murmur/code-graph.db is absent.
    assert!(!repo.path().join(".murmur").join("code-graph.db").exists());

    let res = symbol_history(&repo, SID, None);
    assert_eq!(res["status"], "error", "res={res}");
    assert_eq!(res["status"], "error");
    assert_kind(&res, Some("not_indexed"));
    let msg = res["summary"].as_str().unwrap();
    assert!(
        msg.contains("index_repository"),
        "message names the remedy; msg={msg}"
    );
    // Must never create the db as a side effect.
    assert!(
        !repo.path().join(".murmur").join("code-graph.db").exists(),
        "operation must not create a db file"
    );
}

// ── Scenario 6: missing / empty symbol_id ────────────────────────────────────────

fn scenario_06_missing_symbol_id(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = repo.call(json!({
        "operation": "symbol_history",
        "repo": repo.arg(),
    }));
    assert_eq!(res["status"], "error");
    assert_eq!(res["status"], "error");
    assert_eq!(res["summary"], "missing required field: symbol_id");
}

fn scenario_06_empty_symbol_id(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = symbol_history(&repo, "", None);
    assert_eq!(res["status"], "error");
    assert_eq!(res["summary"], "missing required field: symbol_id");
}

// ── Scenario 7: symbol added but never committed → passed, empty commits ─────────

fn scenario_07_symbol_added_but_never_committed(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    // Write the symbol file into the working tree but never commit it.
    fs::write(repo.path().join("uncommitted.rs"), "fn sym() {\n    1\n}\n").unwrap();
    build_graph_db(repo.path(), &[(SID, "uncommitted.rs", 1, 3)]);

    let res = symbol_history(&repo, SID, None);
    assert_eq!(res["status"], "passed", "empty is not an error; res={res}");
    assert_eq!(res["status"], "passed");
    assert_eq!(res["data"]["file"], "uncommitted.rs");
    assert_eq!(
        res["data"]["commits"].as_array().unwrap().len(),
        0,
        "no commit touches an uncommitted symbol; res={res}"
    );
}

// ── Scenario 8: stale index (range beyond committed EOF) → error, git stderr ─────

fn scenario_08_stale_index_range_beyond_eof(mode: Mode) {
    let repo = Fixture::new(mode);
    init_repo(repo.path());
    // Committed file has only 3 lines.
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    1\n}\n", "add");
    // Index is stale: it claims the symbol starts at line 5 (beyond the 3 committed
    // lines) — as if the file grew but was not re-indexed.
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 5, 8)]);

    let res = symbol_history(&repo, SID, None);
    assert_eq!(res["status"], "error", "res={res}");
    assert_eq!(res["status"], "error");
    // No not_found / not_indexed error_kind — this is a git-level failure, surfaced
    // via fail_msg (which carries no error_kind).
    assert_kind(&res, None);
    let msg = res["summary"].as_str().unwrap();
    assert!(
        msg.contains("only") && msg.contains("lines"),
        "must surface git's stderr; msg={msg}"
    );
}

// ── Both ways of naming the repository ──────────────────────────────────────────

#[test]
fn scenario_01_happy_path_single_commit_absolute() {
    scenario_01_happy_path_single_commit(Mode::Absolute);
}

#[test]
fn scenario_01_happy_path_single_commit_relative() {
    scenario_01_happy_path_single_commit(Mode::Relative);
}

#[test]
fn scenario_02_multi_commit_history_with_n_absolute() {
    scenario_02_multi_commit_history_with_n(Mode::Absolute);
}

#[test]
fn scenario_02_multi_commit_history_with_n_relative() {
    scenario_02_multi_commit_history_with_n(Mode::Relative);
}

#[test]
fn scenario_03_range_shifted_by_unrelated_edits_absolute() {
    scenario_03_range_shifted_by_unrelated_edits(Mode::Absolute);
}

#[test]
fn scenario_03_range_shifted_by_unrelated_edits_relative() {
    scenario_03_range_shifted_by_unrelated_edits(Mode::Relative);
}

#[test]
fn scenario_04_unknown_symbol_id_absolute() {
    scenario_04_unknown_symbol_id(Mode::Absolute);
}

#[test]
fn scenario_04_unknown_symbol_id_relative() {
    scenario_04_unknown_symbol_id(Mode::Relative);
}

#[test]
fn scenario_05_repo_never_indexed_absolute() {
    scenario_05_repo_never_indexed(Mode::Absolute);
}

#[test]
fn scenario_05_repo_never_indexed_relative() {
    scenario_05_repo_never_indexed(Mode::Relative);
}

#[test]
fn scenario_06_missing_symbol_id_absolute() {
    scenario_06_missing_symbol_id(Mode::Absolute);
}

#[test]
fn scenario_06_missing_symbol_id_relative() {
    scenario_06_missing_symbol_id(Mode::Relative);
}

#[test]
fn scenario_06_empty_symbol_id_absolute() {
    scenario_06_empty_symbol_id(Mode::Absolute);
}

#[test]
fn scenario_06_empty_symbol_id_relative() {
    scenario_06_empty_symbol_id(Mode::Relative);
}

#[test]
fn scenario_07_symbol_added_but_never_committed_absolute() {
    scenario_07_symbol_added_but_never_committed(Mode::Absolute);
}

#[test]
fn scenario_07_symbol_added_but_never_committed_relative() {
    scenario_07_symbol_added_but_never_committed(Mode::Relative);
}

#[test]
fn scenario_08_stale_index_range_beyond_eof_absolute() {
    scenario_08_stale_index_range_beyond_eof(Mode::Absolute);
}

#[test]
fn scenario_08_stale_index_range_beyond_eof_relative() {
    scenario_08_stale_index_range_beyond_eof(Mode::Relative);
}
