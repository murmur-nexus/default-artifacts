//! Integration tests for the `symbol_history` operation (slice
//! deb59b9b-stable-id-aware-git-history).
//!
//! Every test builds a real git repo in a tempdir and a hand-built
//! `.murmur/code-graph.db` matching `murmur-tool-code-graph`'s documented schema
//! (this tool cannot link the binary-only sibling crate as a library). No mocks.
//!
//! Scenario coverage (from the slice spec):
//!   1  happy_path_single_commit
//!   2  multi_commit_history_with_n
//!   3  range_shifted_by_unrelated_edits   (the core value proposition)
//!   4  unknown_symbol_id
//!   5  repo_never_indexed
//!   6  missing_symbol_id / empty_symbol_id
//!   7  symbol_added_but_never_committed
//!   8  stale_index_range_beyond_eof

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

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Tempdir / repo helpers ──────────────────────────────────────────────────────

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        path.push(format!("murmur-git-symhist-{}-{nanos}-{seq}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir created");
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

fn git_ok<const N: usize>(args: [&str; N]) {
    let out = Command::new("git").args(args).output().expect("git should run");
    assert!(
        out.status.success(),
        "git {} failed\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    let r = repo.to_str().unwrap();
    git_ok(["init", r]);
    git_ok(["-C", r, "config", "user.email", "test@example.com"]);
    git_ok(["-C", r, "config", "user.name", "Test"]);
    // A committed README so HEAD always exists even before the target file lands.
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    git_ok(["-C", r, "add", "README.md"]);
    git_ok(["-C", r, "commit", "-m", "initial"]);
}

/// Write `path` (repo-relative) with `body`, stage it, and commit with `msg`.
fn write_commit(repo: &Path, path: &str, body: &str, msg: &str) {
    let full = repo.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, body).unwrap();
    let r = repo.to_str().unwrap();
    git_ok(["-C", r, "add", path]);
    git_ok(["-C", r, "commit", "-m", msg]);
}

// ── code-graph fixture db ───────────────────────────────────────────────────────

/// A symbol row: (symbol_id, file_path, start_line, end_line).
type Sym<'a> = (&'a str, &'a str, i64, i64);

/// Build `<repo>/.murmur/code-graph.db` with the documented code-graph schema and
/// the given symbols. Each distinct file path gets one `files` row.
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
        "#,
    )
    .unwrap();

    // Distinct file paths → files rows.
    let mut file_ids: Vec<(&str, i64)> = Vec::new();
    for (_, path, _, _) in symbols {
        if !file_ids.iter().any(|(p, _)| p == path) {
            let id = file_ids.len() as i64 + 1;
            conn.execute(
                "INSERT INTO files (id, path, content_hash) VALUES (?1, ?2, 'deadbeef')",
                rusqlite::params![id, path],
            )
            .unwrap();
            file_ids.push((path, id));
        }
    }
    for (sid, path, start, end) in symbols {
        let file_id = file_ids.iter().find(|(p, _)| p == path).unwrap().1;
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name,
                 signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture', '', 'sym', 'sym', '(->())', 'function', ?2, ?3, ?4)",
            rusqlite::params![sid, file_id, start, end],
        )
        .unwrap();
    }
}

// ── Tool harness ────────────────────────────────────────────────────────────────

fn run_tool(op: Value) -> Value {
    let envelope = json!({ "data": op.to_string(), "log_path": null }).to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_murmur-tool-git"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary should start");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(envelope.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("tool should complete");
    assert!(
        out.status.success(),
        "binary exited non-zero\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be valid JSON ({e}): {stdout:?}"))
}

fn symbol_history(repo: &Path, symbol_id: &str, n: Option<u64>) -> Value {
    let mut op = json!({
        "operation": "symbol_history",
        "repo": repo.to_str().unwrap(),
        "symbol_id": symbol_id,
    });
    if let Some(n) = n {
        op["n"] = json!(n);
    }
    run_tool(op)
}

const SID: &str = "rust://fixture/#sym(->())";

// ── Scenario 1: happy path, single commit ───────────────────────────────────────

#[test]
fn scenario_01_happy_path_single_commit() {
    let repo = TestDir::new();
    init_repo(repo.path());
    // A 4-line function, its entire content added in one commit.
    write_commit(
        repo.path(),
        "src/lib.rs",
        "fn sym() {\n    let x = 1;\n    x\n}\n",
        "add sym",
    );
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = symbol_history(repo.path(), SID, None);

    assert_eq!(res["ok"], true, "res={res}");
    assert_eq!(res["status"], "passed");
    assert_eq!(res["symbol_id"], SID);
    assert_eq!(res["file"], "src/lib.rs");
    assert_eq!(res["start_line"], 1);
    assert_eq!(res["end_line"], 4);
    let commits = res["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "exactly one commit; res={res}");
    let c = &commits[0];
    assert_eq!(c["subject"], "add sym");
    assert_eq!(c["author"], "Test");
    assert!(c["hash"].as_str().unwrap().len() >= 40);
    assert!(!c["short_hash"].as_str().unwrap().is_empty());
    assert!(!c["date_iso"].as_str().unwrap().is_empty());
}

// ── Scenario 2: multi-commit history with n, newest first ────────────────────────

#[test]
fn scenario_02_multi_commit_history_with_n() {
    let repo = TestDir::new();
    init_repo(repo.path());
    // Create the function, then edit a line inside its range across 3 commits.
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    let x = 1;\n    x\n}\n", "c1 create");
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    let x = 2;\n    x\n}\n", "c2 edit");
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    let x = 3;\n    x\n}\n", "c3 edit");
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = symbol_history(repo.path(), SID, Some(3));
    assert_eq!(res["ok"], true, "res={res}");
    let commits = res["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 3, "up to 3 commits; res={res}");
    // Newest first.
    assert_eq!(commits[0]["subject"], "c3 edit");
    assert_eq!(commits[1]["subject"], "c2 edit");
    assert_eq!(commits[2]["subject"], "c1 create");
    // Distinct commits.
    let hashes: Vec<&str> = commits.iter().map(|c| c["hash"].as_str().unwrap()).collect();
    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);

    // Default n=1 returns only the newest.
    let one = symbol_history(repo.path(), SID, None);
    assert_eq!(one["commits"].as_array().unwrap().len(), 1);
    assert_eq!(one["commits"][0]["subject"], "c3 edit");
}

// ── Scenario 3: range shifted by unrelated edits (core value proposition) ────────

#[test]
fn scenario_03_range_shifted_by_unrelated_edits() {
    let repo = TestDir::new();
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

    let res = symbol_history(repo.path(), SID, None);
    assert_eq!(res["ok"], true, "res={res}");
    assert_eq!(res["start_line"], 6);
    assert_eq!(res["end_line"], 9);
    let commits = res["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "res={res}");
    // The last commit that touched the symbol's own content is A, NOT the later
    // insertion B. A stale line number / raw git blame would wrongly report B.
    assert_eq!(
        commits[0]["subject"], "A: create sym",
        "must attribute to the symbol's own edit, not the unrelated insertion; res={res}"
    );
}

// ── Scenario 4: unknown symbol_id → not_found ────────────────────────────────────

#[test]
fn scenario_04_unknown_symbol_id() {
    let repo = TestDir::new();
    init_repo(repo.path());
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    1\n}\n", "add");
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 3)]);

    let res = symbol_history(repo.path(), "rust://fixture/#does_not_exist(->())", None);
    assert_eq!(res["ok"], false, "res={res}");
    assert_eq!(res["status"], "error");
    assert_eq!(res["error_kind"], "not_found");
    assert!(res["commits"].is_null(), "no partial commit list; res={res}");
}

// ── Scenario 5: repo never indexed → not_indexed, db not created ─────────────────

#[test]
fn scenario_05_repo_never_indexed() {
    let repo = TestDir::new();
    init_repo(repo.path());
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    1\n}\n", "add");
    // No build_graph_db — .murmur/code-graph.db is absent.
    assert!(!repo.path().join(".murmur").join("code-graph.db").exists());

    let res = symbol_history(repo.path(), SID, None);
    assert_eq!(res["ok"], false, "res={res}");
    assert_eq!(res["status"], "error");
    assert_eq!(res["error_kind"], "not_indexed");
    let msg = res["message"].as_str().unwrap();
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

#[test]
fn scenario_06_missing_symbol_id() {
    let repo = TestDir::new();
    init_repo(repo.path());
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = run_tool(json!({
        "operation": "symbol_history",
        "repo": repo.path().to_str().unwrap(),
    }));
    assert_eq!(res["ok"], false);
    assert_eq!(res["status"], "error");
    assert_eq!(res["message"], "missing required field: symbol_id");
}

#[test]
fn scenario_06_empty_symbol_id() {
    let repo = TestDir::new();
    init_repo(repo.path());
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 1, 4)]);

    let res = symbol_history(repo.path(), "", None);
    assert_eq!(res["ok"], false);
    assert_eq!(res["message"], "missing required field: symbol_id");
}

// ── Scenario 7: symbol added but never committed → passed, empty commits ─────────

#[test]
fn scenario_07_symbol_added_but_never_committed() {
    let repo = TestDir::new();
    init_repo(repo.path());
    // Write the symbol file into the working tree but never commit it.
    fs::write(
        repo.path().join("uncommitted.rs"),
        "fn sym() {\n    1\n}\n",
    )
    .unwrap();
    build_graph_db(repo.path(), &[(SID, "uncommitted.rs", 1, 3)]);

    let res = symbol_history(repo.path(), SID, None);
    assert_eq!(res["ok"], true, "empty is not an error; res={res}");
    assert_eq!(res["status"], "passed");
    assert_eq!(res["file"], "uncommitted.rs");
    assert_eq!(
        res["commits"].as_array().unwrap().len(),
        0,
        "no commit touches an uncommitted symbol; res={res}"
    );
}

// ── Scenario 8: stale index (range beyond committed EOF) → error, git stderr ─────

#[test]
fn scenario_08_stale_index_range_beyond_eof() {
    let repo = TestDir::new();
    init_repo(repo.path());
    // Committed file has only 3 lines.
    write_commit(repo.path(), "src/lib.rs", "fn sym() {\n    1\n}\n", "add");
    // Index is stale: it claims the symbol starts at line 5 (beyond the 3 committed
    // lines) — as if the file grew but was not re-indexed.
    build_graph_db(repo.path(), &[(SID, "src/lib.rs", 5, 8)]);

    let res = symbol_history(repo.path(), SID, None);
    assert_eq!(res["ok"], false, "res={res}");
    assert_eq!(res["status"], "error");
    // No not_found / not_indexed error_kind — this is a git-level failure, surfaced
    // via fail_msg (which carries no error_kind).
    assert!(res["error_kind"].is_null(), "res={res}");
    let msg = res["message"].as_str().unwrap();
    assert!(
        msg.contains("only") && msg.contains("lines"),
        "must surface git's stderr; msg={msg}"
    );
}
