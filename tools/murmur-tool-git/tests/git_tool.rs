// slice1 + slice3 integration tests.
// Each test initialises a real git repo in a tempdir; no mocks.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Shared helpers ────────────────────────────────────────────────────────────

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
        // Include an atomic counter so concurrent tests within the same process
        // never collide even when two calls land in the same nanosecond.
        let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        path.push(format!(
            "murmur-git-slice1-{}-{nanos}-{seq}",
            std::process::id()
        ));
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

fn init_git_repo(repo: &Path) {
    // Pin the initial branch to `main`: a bare `git init` inherits the host's
    // `init.defaultBranch`, which several tests below check out by name.
    run_git([
        "init",
        "-b",
        "main",
        repo.to_str().expect("repo path utf-8"),
    ]);
    run_git(["-C", repo.to_str().unwrap(), "config", "user.email", "test@example.com"]);
    run_git(["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    run_git(["-C", repo.to_str().unwrap(), "add", "README.md"]);
    run_git(["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
}

/// Invoke the tool binary with a JSON payload and return the parsed JSON result.
fn run_tool_in(cwd: &Path, payload: Value) -> Value {
    let envelope = json!({
        "data": payload.to_string(),
        "log_path": null,
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_murmur-tool-git"))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary should start");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(envelope.as_bytes())
        .expect("write envelope");

    let out = child.wait_with_output().expect("output");
    assert!(
        out.status.success(),
        "binary exited non-zero\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not valid JSON: {e}\nstdout: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn run_git<const N: usize>(args: [&str; N]) {
    let out = Command::new("git")
        .args(args)
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git {} failed\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── STATUS ────────────────────────────────────────────────────────────────────

#[test]
fn slice1_status_clean_repo() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );

    assert_eq!(res["ok"], true);
    assert_eq!(res["message"], "working tree clean");
    let entries = res["entries"].as_array().expect("entries array");
    assert!(entries.is_empty(), "clean repo should have no entries");
}

#[test]
fn slice1_status_dirty_repo() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    fs::write(repo.join("new_file.txt"), "untracked\n").unwrap();

    let res = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );

    assert_eq!(res["ok"], true);
    let entries = res["entries"].as_array().expect("entries array");
    assert!(!entries.is_empty(), "dirty repo should have entries");

    let found = entries.iter().any(|e| {
        e["path"].as_str() == Some("new_file.txt")
            && e["status_code"].as_str() == Some("??")
    });
    assert!(found, "untracked file should have status_code '??'; entries: {entries:?}");
}

// ── ADD ───────────────────────────────────────────────────────────────────────

#[test]
fn slice1_add_specific_path() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    fs::write(repo.join("only_this.txt"), "content\n").unwrap();
    fs::write(repo.join("not_this.txt"), "other\n").unwrap();

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "add",
            "repo": repo.to_str().unwrap(),
            "paths": ["only_this.txt"],
        }),
    );

    assert_eq!(res["ok"], true, "add failed: {}", res["message"]);
    let staged = res["staged"].as_array().expect("staged array");
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0], "only_this.txt");

    // only_this.txt should be staged (A in index), not_this.txt still untracked
    let status = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );
    let entries = status["entries"].as_array().unwrap();
    let staged_entry = entries
        .iter()
        .find(|e| e["path"].as_str() == Some("only_this.txt"))
        .expect("only_this.txt should be in status");
    // Staged new file: "A " in porcelain v1
    assert_eq!(
        staged_entry["status_code"].as_str(),
        Some("A "),
        "should be staged-added; got {:?}",
        staged_entry["status_code"]
    );
}

#[test]
fn slice1_add_all() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    fs::write(repo.join("alpha.txt"), "a\n").unwrap();
    fs::write(repo.join("beta.txt"), "b\n").unwrap();

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "add",
            "repo": repo.to_str().unwrap(),
            "paths": ["."],
        }),
    );

    assert_eq!(res["ok"], true, "add . failed: {}", res["message"]);

    // After staging all, status should show two A  entries
    let status = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );
    let entries = status["entries"].as_array().unwrap();
    let staged_count = entries
        .iter()
        .filter(|e| e["status_code"].as_str() == Some("A "))
        .count();
    assert_eq!(staged_count, 2, "both files should be staged-added; entries: {entries:?}");
}

// ── DIFF ──────────────────────────────────────────────────────────────────────

#[test]
fn slice1_diff_unstaged() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Modify a tracked file without staging
    fs::write(repo.join("README.md"), "modified content\n").unwrap();

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "diff",
            "repo": repo.to_str().unwrap(),
            "staged": false,
        }),
    );

    assert_eq!(res["ok"], true, "diff failed: {}", res["message"]);
    let diff = res["diff"].as_str().expect("diff string");
    assert!(!diff.is_empty(), "unstaged diff should not be empty");
    assert!(diff.contains("README.md"), "diff should mention README.md");
}

#[test]
fn slice1_diff_staged() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create and stage a new file
    fs::write(repo.join("staged_file.txt"), "staged content\n").unwrap();
    run_git([
        "-C",
        repo.to_str().unwrap(),
        "add",
        "staged_file.txt",
    ]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "diff",
            "repo": repo.to_str().unwrap(),
            "staged": true,
        }),
    );

    assert_eq!(res["ok"], true, "diff --staged failed: {}", res["message"]);
    let diff = res["diff"].as_str().expect("diff string");
    assert!(!diff.is_empty(), "staged diff should not be empty");
    assert!(diff.contains("staged_file.txt"), "diff should mention the staged file");
}

// ── RESTORE ───────────────────────────────────────────────────────────────────

#[test]
fn slice1_restore_unstaged() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Modify tracked file (unstaged)
    fs::write(repo.join("README.md"), "dirty\n").unwrap();

    // Confirm it shows dirty
    let before = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );
    assert!(
        !before["entries"].as_array().unwrap().is_empty(),
        "should be dirty before restore"
    );

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "restore",
            "repo": repo.to_str().unwrap(),
            "paths": ["README.md"],
            "staged": false,
        }),
    );

    assert_eq!(res["ok"], true, "restore failed: {}", res["message"]);
    let restored = res["restored"].as_array().expect("restored array");
    assert_eq!(restored[0], "README.md");

    // Confirm clean
    let after = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );
    assert_eq!(after["message"], "working tree clean");
}

// ── STASH ─────────────────────────────────────────────────────────────────────

#[test]
fn slice1_stash_push_pop() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Make an unstaged change
    fs::write(repo.join("README.md"), "stashable change\n").unwrap();

    // Push to stash
    let push_res = run_tool_in(
        &repo,
        json!({
            "operation": "stash",
            "repo": repo.to_str().unwrap(),
            "subcommand": "push",
            "message": "test stash",
        }),
    );
    assert_eq!(push_res["ok"], true, "stash push failed: {}", push_res["message"]);
    assert_eq!(push_res["stash_ref"], "stash@{0}");

    // Working tree should be clean
    let status = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );
    assert_eq!(status["message"], "working tree clean");

    // Pop the stash
    let pop_res = run_tool_in(
        &repo,
        json!({
            "operation": "stash",
            "repo": repo.to_str().unwrap(),
            "subcommand": "pop",
        }),
    );
    assert_eq!(pop_res["ok"], true, "stash pop failed: {}", pop_res["message"]);

    // File should be back
    let content = fs::read_to_string(repo.join("README.md")).unwrap();
    assert_eq!(content, "stashable change\n");
}

#[test]
fn slice1_stash_list() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // First stash
    fs::write(repo.join("README.md"), "change one\n").unwrap();
    run_tool_in(
        &repo,
        json!({
            "operation": "stash",
            "repo": repo.to_str().unwrap(),
            "subcommand": "push",
            "message": "first stash",
        }),
    );

    // Second stash
    fs::write(repo.join("README.md"), "change two\n").unwrap();
    run_tool_in(
        &repo,
        json!({
            "operation": "stash",
            "repo": repo.to_str().unwrap(),
            "subcommand": "push",
            "message": "second stash",
        }),
    );

    let list_res = run_tool_in(
        &repo,
        json!({
            "operation": "stash",
            "repo": repo.to_str().unwrap(),
            "subcommand": "list",
        }),
    );

    assert_eq!(list_res["ok"], true, "stash list failed: {}", list_res["message"]);
    let entries = list_res["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2, "should have 2 stash entries; got: {entries:?}");

    // Each entry should have index, message, branch
    for e in entries {
        assert!(e["index"].is_number(), "entry missing index: {e}");
        assert!(e["message"].is_string(), "entry missing message: {e}");
        assert!(e["branch"].is_string(), "entry missing branch: {e}");
    }
}

#[test]
fn slice1_stash_push_empty() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Clean tree — nothing to stash
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "stash",
            "repo": repo.to_str().unwrap(),
            "subcommand": "push",
        }),
    );

    assert_eq!(res["ok"], false);
    assert_eq!(res["error_kind"], "nothing_to_stash");
}

// ── LOG ───────────────────────────────────────────────────────────────────────

#[test]
fn slice1_log_returns_commits() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo); // commit 1

    // Commit 2
    fs::write(repo.join("file1.txt"), "one\n").unwrap();
    run_git(["-C", repo.to_str().unwrap(), "add", "file1.txt"]);
    run_git(["-C", repo.to_str().unwrap(), "commit", "-m", "second commit"]);

    // Commit 3
    fs::write(repo.join("file2.txt"), "two\n").unwrap();
    run_git(["-C", repo.to_str().unwrap(), "add", "file2.txt"]);
    run_git(["-C", repo.to_str().unwrap(), "commit", "-m", "third commit"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "log",
            "repo": repo.to_str().unwrap(),
            "n": 3,
        }),
    );

    assert_eq!(res["ok"], true, "log failed: {}", res["message"]);
    let commits = res["commits"].as_array().expect("commits array");
    assert_eq!(commits.len(), 3, "should return 3 commits");

    // Most-recent first
    assert_eq!(commits[0]["subject"].as_str(), Some("third commit"));
    assert_eq!(commits[1]["subject"].as_str(), Some("second commit"));
    assert_eq!(commits[2]["subject"].as_str(), Some("initial"));

    // Verify field shapes
    let c = &commits[0];
    assert_eq!(
        c["hash"].as_str().map(|s| s.len()),
        Some(40),
        "hash should be 40 hex chars"
    );
    assert!(
        c["short_hash"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "short_hash should be non-empty"
    );
    assert_eq!(c["author"].as_str(), Some("Test"));
    assert!(
        c["date_iso"].as_str().is_some(),
        "date_iso should be present"
    );
}

#[test]
fn slice1_log_with_since() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // A since date in the distant past should return the commit
    let res_past = run_tool_in(
        &repo,
        json!({
            "operation": "log",
            "repo": repo.to_str().unwrap(),
            "n": 10,
            "since": "1970-01-01",
        }),
    );
    assert_eq!(res_past["ok"], true);
    let commits_past = res_past["commits"].as_array().unwrap();
    assert!(!commits_past.is_empty(), "should have commits since epoch");

    // A since date in the future should return zero commits
    let res_future = run_tool_in(
        &repo,
        json!({
            "operation": "log",
            "repo": repo.to_str().unwrap(),
            "n": 10,
            "since": "2099-01-01",
        }),
    );
    assert_eq!(res_future["ok"], true);
    let commits_future = res_future["commits"].as_array().unwrap();
    assert!(
        commits_future.is_empty(),
        "should have no commits since 2099; got: {commits_future:?}"
    );
}

// ── SHOW ──────────────────────────────────────────────────────────────────────

#[test]
fn slice1_show_head() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "show",
            "repo": repo.to_str().unwrap(),
            "ref": "HEAD",
        }),
    );

    assert_eq!(res["ok"], true, "show failed: {}", res["message"]);

    let commit = res["commit"].as_object().expect("commit object");
    assert_eq!(
        commit["hash"].as_str().map(|s| s.len()),
        Some(40),
        "hash should be 40 chars"
    );
    assert_eq!(commit["author"].as_str(), Some("Test"));
    assert!(
        commit["date_iso"].as_str().is_some(),
        "date_iso should be present"
    );
    assert_eq!(commit["subject"].as_str(), Some("initial"));

    let diff = res["diff"].as_str().expect("diff string");
    assert!(!diff.is_empty(), "diff for the initial commit should not be empty");
    assert!(
        diff.contains("README.md"),
        "diff should mention README.md; got: {diff}"
    );
}

#[test]
fn slice1_show_not_found() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "show",
            "repo": repo.to_str().unwrap(),
            "ref": "does-not-exist-abc123",
        }),
    );

    assert_eq!(res["ok"], false);
    assert_eq!(res["error_kind"], "not_found");
}

// ── Slice 3 helpers ───────────────────────────────────────────────────────────

/// Dynamic-args variant of run_git for test setup with variable-length arg lists.
fn run_git_dyn(args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git {} failed\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Create a bare repo + a source repo with one initial commit already pushed.
/// Returns (bare_path, source_path).
fn setup_remote_pair(td: &TestDir) -> (PathBuf, PathBuf) {
    let bare = td.path().join("bare.git");
    let source = td.path().join("source");

    // Pin the bare repo's HEAD to `main` too, so it matches the `main` branch
    // that `init_git_repo` pins the source repo to.
    run_git_dyn(&["init", "--bare", "-b", "main", bare.to_str().unwrap()]);
    init_git_repo(&source);
    run_git_dyn(&["-C", source.to_str().unwrap(), "remote", "add", "origin", bare.to_str().unwrap()]);
    run_git_dyn(&["-C", source.to_str().unwrap(), "push", "-u", "origin", "HEAD"]);

    (bare, source)
}

/// Clone `bare` to `dest`, then configure user identity.
fn clone_from(bare: &Path, dest: &Path) {
    run_git_dyn(&["clone", bare.to_str().unwrap(), dest.to_str().unwrap()]);
    run_git_dyn(&["-C", dest.to_str().unwrap(), "config", "user.email", "test@example.com"]);
    run_git_dyn(&["-C", dest.to_str().unwrap(), "config", "user.name", "Test"]);
}

/// Push one new commit from `repo` to its `origin`.
fn push_new_commit(repo: &Path, filename: &str) {
    let file = repo.join(filename);
    fs::write(&file, "content\n").expect("write file");
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", filename]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", &format!("add {filename}")]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "push"]);
}

// ── REMOTE subcommand ─────────────────────────────────────────────────────────

#[test]
fn slice3_remote_list() {
    let td = TestDir::new();
    let (bare, source) = setup_remote_pair(&td);
    // source already has origin configured; list it
    let res = run_tool_in(
        &source,
        json!({ "operation": "remote", "repo": source.to_str().unwrap(), "subcommand": "list" }),
    );
    assert_eq!(res["ok"], true, "remote list failed: {}", res["message"]);
    let remotes = res["remotes"].as_array().expect("remotes array");
    assert_eq!(remotes.len(), 1, "expected 1 remote; got {remotes:?}");
    assert_eq!(remotes[0]["name"], "origin");
    assert!(
        remotes[0]["fetch_url"].as_str().map(|s| s.contains(bare.file_name().unwrap().to_str().unwrap())).unwrap_or(false),
        "fetch_url should reference bare path; got: {:?}",
        remotes[0]["fetch_url"]
    );
}

#[test]
fn slice3_remote_add() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "remote",
            "repo": repo.to_str().unwrap(),
            "subcommand": "add",
            "name": "upstream",
            "url": "https://example.com/repo.git",
        }),
    );
    assert_eq!(res["ok"], true, "remote add failed: {}", res["message"]);
    assert_eq!(res["name"], "upstream");
    assert_eq!(res["url"], "https://example.com/repo.git");

    // Verify it appears in list
    let list = run_tool_in(
        &repo,
        json!({ "operation": "remote", "repo": repo.to_str().unwrap(), "subcommand": "list" }),
    );
    let remotes = list["remotes"].as_array().unwrap();
    assert!(
        remotes.iter().any(|r| r["name"] == "upstream"),
        "upstream not in list; got {remotes:?}"
    );
}

#[test]
fn slice3_remote_remove() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Add then remove
    run_git_dyn(&["-C", repo.to_str().unwrap(), "remote", "add", "to_remove", "https://example.com/r.git"]);
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "remote",
            "repo": repo.to_str().unwrap(),
            "subcommand": "remove",
            "name": "to_remove",
        }),
    );
    assert_eq!(res["ok"], true, "remote remove failed: {}", res["message"]);
    assert_eq!(res["name"], "to_remove");

    // Must be gone from list
    let list = run_tool_in(
        &repo,
        json!({ "operation": "remote", "repo": repo.to_str().unwrap(), "subcommand": "list" }),
    );
    let remotes = list["remotes"].as_array().unwrap();
    assert!(
        !remotes.iter().any(|r| r["name"] == "to_remove"),
        "to_remove still in list after removal; got {remotes:?}"
    );
}

#[test]
fn slice3_remote_add_duplicate() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    run_git_dyn(&["-C", repo.to_str().unwrap(), "remote", "add", "dup", "https://example.com/r.git"]);
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "remote",
            "repo": repo.to_str().unwrap(),
            "subcommand": "add",
            "name": "dup",
            "url": "https://example.com/other.git",
        }),
    );
    assert_eq!(res["ok"], false);
    assert_eq!(res["error_kind"], "already_exists", "expected already_exists; got {res:?}");
}

// ── FETCH ─────────────────────────────────────────────────────────────────────

#[test]
fn slice3_fetch_from_local() {
    let td = TestDir::new();
    let (bare, source) = setup_remote_pair(&td);
    let clone1 = td.path().join("clone1");
    clone_from(&bare, &clone1);

    // Push a new commit from source so bare is ahead of clone1
    push_new_commit(&source, "extra.txt");

    let res = run_tool_in(
        &clone1,
        json!({ "operation": "fetch", "repo": clone1.to_str().unwrap(), "remote": "origin" }),
    );
    assert_eq!(res["ok"], true, "fetch failed: {}", res["message"]);
    assert_eq!(res["remote"], "origin");
    let updated = res["updated_refs"].as_array().expect("updated_refs array");
    assert!(
        !updated.is_empty(),
        "expected non-empty updated_refs after fetching a new commit; got {res:?}"
    );
}

#[test]
fn slice3_fetch_up_to_date() {
    let td = TestDir::new();
    let (bare, _source) = setup_remote_pair(&td);
    let clone1 = td.path().join("clone1");
    clone_from(&bare, &clone1);
    // No new commits pushed — clone1 is already current

    let res = run_tool_in(
        &clone1,
        json!({ "operation": "fetch", "repo": clone1.to_str().unwrap(), "remote": "origin" }),
    );
    assert_eq!(res["ok"], true, "fetch failed: {}", res["message"]);
    let updated = res["updated_refs"].as_array().expect("updated_refs array");
    assert!(
        updated.is_empty(),
        "expected empty updated_refs when already up to date; got {res:?}"
    );
}

// ── PULL ──────────────────────────────────────────────────────────────────────

#[test]
fn slice3_pull_fast_forward() {
    let td = TestDir::new();
    let (bare, source) = setup_remote_pair(&td);
    let clone1 = td.path().join("clone1");
    clone_from(&bare, &clone1);

    // Push a new commit from source so bare is one ahead of clone1
    push_new_commit(&source, "pulled_file.txt");

    let res = run_tool_in(
        &clone1,
        json!({ "operation": "pull", "repo": clone1.to_str().unwrap(), "remote": "origin" }),
    );
    assert_eq!(res["ok"], true, "pull failed: {}", res["message"]);
    assert_eq!(res["remote"], "origin");
    assert_eq!(res["fast_forward"], true, "expected fast_forward=true; got {res:?}");
    assert_eq!(res["commits_pulled"], 1, "expected 1 commit pulled; got {res:?}");
    assert!(
        clone1.join("pulled_file.txt").exists(),
        "pulled_file.txt should exist in clone after pull"
    );
}

// ── PUSH ──────────────────────────────────────────────────────────────────────

#[test]
fn slice3_push_success() {
    let td = TestDir::new();
    let (bare, source) = setup_remote_pair(&td);
    let _ = bare; // used via origin remote in source

    // Make another local commit and push it via the tool
    fs::write(source.join("new.txt"), "data\n").unwrap();
    run_git_dyn(&["-C", source.to_str().unwrap(), "add", "new.txt"]);
    run_git_dyn(&["-C", source.to_str().unwrap(), "commit", "-m", "add new.txt"]);

    let res = run_tool_in(
        &source,
        json!({ "operation": "push", "repo": source.to_str().unwrap(), "remote": "origin" }),
    );
    assert_eq!(res["ok"], true, "push failed: {}", res["message"]);
    assert_eq!(res["remote"], "origin");
    let remote_ref = res["remote_ref"].as_str().expect("remote_ref string");
    assert!(
        remote_ref.starts_with("refs/heads/"),
        "remote_ref should start with refs/heads/; got {remote_ref}"
    );
}

#[test]
fn slice3_push_non_fast_forward() {
    let td = TestDir::new();
    let (bare, _source) = setup_remote_pair(&td);

    let clone_a = td.path().join("clone_a");
    let clone_b = td.path().join("clone_b");
    clone_from(&bare, &clone_a);
    clone_from(&bare, &clone_b);

    // Advance remote via clone_a
    push_new_commit(&clone_a, "from_a.txt");

    // Make a diverging commit in clone_b (not pulling first)
    fs::write(clone_b.join("from_b.txt"), "b\n").unwrap();
    run_git_dyn(&["-C", clone_b.to_str().unwrap(), "add", "from_b.txt"]);
    run_git_dyn(&["-C", clone_b.to_str().unwrap(), "commit", "-m", "diverging commit"]);

    let res = run_tool_in(
        &clone_b,
        json!({ "operation": "push", "repo": clone_b.to_str().unwrap(), "remote": "origin" }),
    );
    assert_eq!(res["ok"], false);
    assert_eq!(
        res["error_kind"], "non_fast_forward",
        "expected non_fast_forward; got {res:?}"
    );
}

#[test]
fn slice3_push_force() {
    let td = TestDir::new();
    let (bare, _source) = setup_remote_pair(&td);

    let clone_a = td.path().join("clone_a");
    let clone_b = td.path().join("clone_b");
    clone_from(&bare, &clone_a);
    clone_from(&bare, &clone_b);

    // Advance remote via clone_a
    push_new_commit(&clone_a, "from_a.txt");

    // Make a diverging commit in clone_b and force-push
    fs::write(clone_b.join("from_b.txt"), "b\n").unwrap();
    run_git_dyn(&["-C", clone_b.to_str().unwrap(), "add", "from_b.txt"]);
    run_git_dyn(&["-C", clone_b.to_str().unwrap(), "commit", "-m", "force push commit"]);

    let res = run_tool_in(
        &clone_b,
        json!({
            "operation": "push",
            "repo": clone_b.to_str().unwrap(),
            "remote": "origin",
            "force": true,
        }),
    );
    assert_eq!(res["ok"], true, "force push failed: {}", res["message"]);
    assert_eq!(res["remote"], "origin");
}

// ── CLONE ─────────────────────────────────────────────────────────────────────

#[test]
fn slice3_clone_success() {
    let td = TestDir::new();
    let (bare, _source) = setup_remote_pair(&td);
    let dest = td.path().join("cloned_repo");

    let res = run_tool_in(
        td.path(),
        json!({
            "operation": "clone",
            "url": bare.to_str().unwrap(),
            "dest": dest.to_str().unwrap(),
        }),
    );
    assert_eq!(res["ok"], true, "clone failed: {}", res["message"]);
    assert_eq!(res["dest"], dest.to_str().unwrap());
    let default_branch = res["default_branch"].as_str().expect("default_branch string");
    assert!(!default_branch.is_empty(), "default_branch should not be empty");
    assert!(dest.join("README.md").exists(), "cloned repo should contain README.md");
}

#[test]
fn slice3_clone_dest_exists() {
    let td = TestDir::new();
    let (bare, _source) = setup_remote_pair(&td);
    let dest = td.path().join("existing_dest");
    fs::create_dir_all(&dest).unwrap();

    let res = run_tool_in(
        td.path(),
        json!({
            "operation": "clone",
            "url": bare.to_str().unwrap(),
            "dest": dest.to_str().unwrap(),
        }),
    );
    assert_eq!(res["ok"], false);
    assert_eq!(res["error_kind"], "dest_exists", "expected dest_exists; got {res:?}");
}

// ── TAG ───────────────────────────────────────────────────────────────────────

#[test]
fn slice4_tag_list_empty() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "tag",
            "repo": repo.to_str().unwrap(),
            "subcommand": "list",
        }),
    );

    assert_eq!(res["ok"], true, "tag list failed: {}", res["message"]);
    let tags = res["tags"].as_array().expect("tags array");
    assert!(tags.is_empty(), "fresh repo should have no tags; got: {tags:?}");
}

#[test]
fn slice4_tag_create_lightweight() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "tag",
            "repo": repo.to_str().unwrap(),
            "subcommand": "create",
            "name": "v1.0",
        }),
    );

    assert_eq!(res["ok"], true, "tag create failed: {}", res["message"]);
    assert_eq!(res["name"], "v1.0");
    assert!(
        res["ref"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "ref should be non-empty; got: {res:?}"
    );

    // Must appear in list
    let list = run_tool_in(
        &repo,
        json!({ "operation": "tag", "repo": repo.to_str().unwrap(), "subcommand": "list" }),
    );
    let tags = list["tags"].as_array().unwrap();
    assert!(
        tags.iter().any(|t| t["name"] == "v1.0"),
        "v1.0 not found in tag list; got: {tags:?}"
    );
}

#[test]
fn slice4_tag_create_annotated() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "tag",
            "repo": repo.to_str().unwrap(),
            "subcommand": "create",
            "name": "v2.0",
            "message": "release v2.0",
        }),
    );

    assert_eq!(res["ok"], true, "annotated tag create failed: {}", res["message"]);
    assert_eq!(res["name"], "v2.0");

    // Must appear in list
    let list = run_tool_in(
        &repo,
        json!({ "operation": "tag", "repo": repo.to_str().unwrap(), "subcommand": "list" }),
    );
    let tags = list["tags"].as_array().unwrap();
    assert!(
        tags.iter().any(|t| t["name"] == "v2.0"),
        "v2.0 not found in tag list; got: {tags:?}"
    );
}

#[test]
fn slice4_tag_create_duplicate() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create once
    run_tool_in(
        &repo,
        json!({
            "operation": "tag",
            "repo": repo.to_str().unwrap(),
            "subcommand": "create",
            "name": "dup-tag",
        }),
    );

    // Create again — must fail with already_exists
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "tag",
            "repo": repo.to_str().unwrap(),
            "subcommand": "create",
            "name": "dup-tag",
        }),
    );

    assert_eq!(res["ok"], false);
    assert_eq!(res["error_kind"], "already_exists", "expected already_exists; got {res:?}");
}

// ── MERGE ─────────────────────────────────────────────────────────────────────

#[test]
fn slice4_merge_fast_forward() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create feature branch with one commit ahead of main
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-b", "feature"]);
    fs::write(repo.join("feat.txt"), "feature\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "feat.txt"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "feature commit"]);

    // Switch back to main (previous branch)
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "merge",
            "repo": repo.to_str().unwrap(),
            "branch": "feature",
        }),
    );

    assert_eq!(res["ok"], true, "merge failed: {}", res["message"]);
    assert_eq!(res["fast_forward"], true, "expected fast_forward=true; got {res:?}");
    assert!(
        res["merged_commit"].is_null(),
        "merged_commit should be null for fast-forward; got {res:?}"
    );
}

#[test]
fn slice4_merge_no_ff() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create feature branch with one commit
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-b", "feature"]);
    fs::write(repo.join("feat.txt"), "feature content\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "feat.txt"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "feature commit"]);

    // Switch back to main and add a diverging commit
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-"]);
    fs::write(repo.join("main_extra.txt"), "main extra\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "main_extra.txt"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "main diverges"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "merge",
            "repo": repo.to_str().unwrap(),
            "branch": "feature",
            "message": "merge feature into main",
        }),
    );

    assert_eq!(res["ok"], true, "merge failed: {}", res["message"]);
    assert_eq!(res["fast_forward"], false, "expected fast_forward=false; got {res:?}");
    let merged_commit = res["merged_commit"].as_str();
    assert!(
        merged_commit.map(|s| !s.is_empty()).unwrap_or(false),
        "merged_commit should be non-null for 3-way merge; got {res:?}"
    );
}

#[test]
fn slice4_merge_conflict() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create feature branch, modify README differently
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-b", "feature"]);
    fs::write(repo.join("README.md"), "feature content\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "README.md"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "feature changes README"]);

    // Switch back to main, make a conflicting change to README
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-"]);
    fs::write(repo.join("README.md"), "main content\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "README.md"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "main changes README"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "merge",
            "repo": repo.to_str().unwrap(),
            "branch": "feature",
        }),
    );

    assert_eq!(res["ok"], false, "conflicting merge should fail; got {res:?}");
    assert_eq!(res["error_kind"], "conflict", "expected conflict error_kind; got {res:?}");

    // Repo must be left in conflict state (not aborted)
    let status = run_tool_in(
        &repo,
        json!({ "operation": "status", "repo": repo.to_str().unwrap() }),
    );
    let entries = status["entries"].as_array().expect("entries array");
    let has_unmerged = entries.iter().any(|e| {
        e["status"].as_str() == Some("unmerged")
            || e["status_code"].as_str().map(|c| c.contains('U')).unwrap_or(false)
    });
    assert!(has_unmerged, "repo should be in conflict state; entries: {entries:?}");
}

#[test]
fn slice4_merge_ff_only_fails() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create feature branch with one commit
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-b", "feature"]);
    fs::write(repo.join("feat.txt"), "feature\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "feat.txt"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "feature commit"]);

    // Switch to main and add a diverging commit (making fast-forward impossible)
    run_git_dyn(&["-C", repo.to_str().unwrap(), "checkout", "-"]);
    fs::write(repo.join("extra.txt"), "extra\n").unwrap();
    run_git_dyn(&["-C", repo.to_str().unwrap(), "add", "extra.txt"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "commit", "-m", "diverging main commit"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "merge",
            "repo": repo.to_str().unwrap(),
            "branch": "feature",
            "ff_only": true,
        }),
    );

    assert_eq!(res["ok"], false, "ff_only merge should fail on diverged branches; got {res:?}");
    assert_eq!(
        res["error_kind"],
        "ff_only_failed",
        "expected ff_only_failed; got {res:?}"
    );
}

// ── WORKTREE ──────────────────────────────────────────────────────────────────

#[test]
fn slice4_worktree_add_success() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Create a branch to add as a worktree
    run_git_dyn(&["-C", repo.to_str().unwrap(), "branch", "wt-branch"]);

    let wt_path = td.path().join("my-worktree");
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "add",
            "repo": repo.to_str().unwrap(),
            "path": wt_path.to_str().unwrap(),
            "branch": "wt-branch",
        }),
    );

    assert_eq!(res["ok"], true, "worktree add failed: {}", res["message"]);
    assert_eq!(res["branch"], "wt-branch");
    assert!(wt_path.exists(), "worktree path should exist on disk after add");
}

#[test]
fn slice4_worktree_list() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    run_git_dyn(&["-C", repo.to_str().unwrap(), "branch", "branch-a"]);
    run_git_dyn(&["-C", repo.to_str().unwrap(), "branch", "branch-b"]);

    let wt_a = td.path().join("wt-a");
    let wt_b = td.path().join("wt-b");

    run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "add",
            "repo": repo.to_str().unwrap(),
            "path": wt_a.to_str().unwrap(),
            "branch": "branch-a",
        }),
    );
    run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "add",
            "repo": repo.to_str().unwrap(),
            "path": wt_b.to_str().unwrap(),
            "branch": "branch-b",
        }),
    );

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "list",
            "repo": repo.to_str().unwrap(),
        }),
    );

    assert_eq!(res["ok"], true, "worktree list failed: {}", res["message"]);
    let worktrees = res["worktrees"].as_array().expect("worktrees array");

    let has_wt_a = worktrees.iter().any(|w| {
        w["path"].as_str().map(|p| p.contains("wt-a")).unwrap_or(false)
    });
    let has_wt_b = worktrees.iter().any(|w| {
        w["path"].as_str().map(|p| p.contains("wt-b")).unwrap_or(false)
    });
    assert!(has_wt_a, "wt-a should appear in list; got: {worktrees:?}");
    assert!(has_wt_b, "wt-b should appear in list; got: {worktrees:?}");

    // Each entry must have path, branch, head, bare fields
    for wt in worktrees {
        assert!(wt["path"].is_string(), "path missing in entry: {wt}");
        assert!(wt["head"].is_string(), "head missing in entry: {wt}");
        assert!(wt["bare"].is_boolean(), "bare missing in entry: {wt}");
    }
}

#[test]
fn slice4_worktree_remove() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    run_git_dyn(&["-C", repo.to_str().unwrap(), "branch", "removable"]);

    let wt_path = td.path().join("wt-remove");
    run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "add",
            "repo": repo.to_str().unwrap(),
            "path": wt_path.to_str().unwrap(),
            "branch": "removable",
        }),
    );
    assert!(wt_path.exists(), "worktree path should exist before remove");

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "remove",
            "repo": repo.to_str().unwrap(),
            "path": wt_path.to_str().unwrap(),
        }),
    );

    assert_eq!(res["ok"], true, "worktree remove failed: {}", res["message"]);
    assert!(!wt_path.exists(), "worktree path should be gone after remove");
}

#[test]
fn slice4_worktree_branch_conflict() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // The current branch is already checked out — get its name dynamically
    let branch_out = std::process::Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "branch", "--show-current"])
        .output()
        .expect("git branch --show-current");
    let current_branch = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();

    let wt_path = td.path().join("wt-conflict");
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "worktree",
            "subcommand": "add",
            "repo": repo.to_str().unwrap(),
            "path": wt_path.to_str().unwrap(),
            "branch": current_branch,
        }),
    );

    assert_eq!(res["ok"], false, "should fail for branch already checked out; got {res:?}");
    assert_eq!(
        res["error_kind"],
        "branch_conflict",
        "expected branch_conflict; got {res:?}"
    );
    assert!(!wt_path.exists(), "worktree path should not be created on conflict");
}

#[test]
fn slice4_create_worktree_compat() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    run_git_dyn(&["-C", repo.to_str().unwrap(), "branch", "compat-branch"]);

    let wt_path = td.path().join("compat-wt");
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "create_worktree",
            "repo": repo.to_str().unwrap(),
            "path": wt_path.to_str().unwrap(),
            "branch": "compat-branch",
        }),
    );

    assert_eq!(res["ok"], true, "create_worktree (compat) failed: {}", res["message"]);
    assert_eq!(res["branch"], "compat-branch");
    assert!(wt_path.exists(), "compat worktree should exist on disk");
}

// ── Slice 2 helpers ───────────────────────────────────────────────────────────

/// Stage a file, commit it, and return the commit hash.
fn make_commit(repo: &Path, filename: &str, content: &str, message: &str) -> String {
    let repo_s = repo.to_str().expect("repo path utf-8");
    fs::write(repo.join(filename), content).expect("write file");
    run_git_dyn(&["-C", repo_s, "add", filename]);
    run_git_dyn(&["-C", repo_s, "commit", "-m", message]);

    let out = Command::new("git")
        .args(["-C", repo_s, "rev-parse", "HEAD"])
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git rev-parse HEAD failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Return `git status --porcelain=v1` output for `repo`.
fn git_status_porcelain(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["-C", repo.to_str().expect("repo path utf-8"), "status", "--porcelain=v1"])
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git status failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Return the current HEAD hash for `repo`.
fn head_hash(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["-C", repo.to_str().expect("repo path utf-8"), "rev-parse", "HEAD"])
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git rev-parse HEAD failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ── Slice 2 tests: COMMIT ─────────────────────────────────────────────────────

#[test]
fn slice2_commit_success() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    fs::write(repo.join("new.txt"), "content\n").unwrap();
    run_git_dyn(&["-C", repo_s, "add", "new.txt"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "commit",
            "repo": repo_s,
            "message": "add new.txt",
        }),
    );

    assert_eq!(res["ok"], true, "commit should succeed; got: {res:?}");
    assert!(
        !res["hash"].as_str().unwrap_or("").is_empty(),
        "hash must be populated"
    );
    assert!(
        !res["short_hash"].as_str().unwrap_or("").is_empty(),
        "short_hash must be populated"
    );
    assert_eq!(res["subject"], "add new.txt", "subject must match commit message");
}

#[test]
fn slice2_commit_nothing() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    // Nothing staged — should fail.
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "commit",
            "repo": repo.to_str().unwrap(),
            "message": "should fail",
        }),
    );

    assert_eq!(res["ok"], false, "commit with nothing staged should fail");
    assert_eq!(res["error_kind"], "nothing_to_commit");
}

#[test]
fn slice2_commit_allow_empty() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "commit",
            "repo": repo.to_str().unwrap(),
            "message": "empty commit",
            "allow_empty": true,
        }),
    );

    assert_eq!(
        res["ok"], true,
        "allow_empty commit on clean tree should succeed; got: {res:?}"
    );
    assert!(!res["hash"].as_str().unwrap_or("").is_empty());
    assert_eq!(res["subject"], "empty commit");
}

// ── Slice 2 tests: CHERRY-PICK ────────────────────────────────────────────────

#[test]
fn slice2_cherry_pick_success() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    // Create feature branch and make a unique commit on it.
    run_git_dyn(&["-C", repo_s, "checkout", "-b", "feature"]);
    let pick_hash = make_commit(&repo, "cherry.txt", "cherry content\n", "add cherry.txt");

    // Switch back to main.
    run_git_dyn(&["-C", repo_s, "checkout", "main"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "cherry_pick",
            "repo": repo_s,
            "ref": pick_hash,
        }),
    );

    assert_eq!(res["ok"], true, "cherry-pick should succeed; got: {res:?}");
    assert!(!res["hash"].as_str().unwrap_or("").is_empty());
    assert_eq!(res["subject"], "add cherry.txt");
    assert!(
        repo.join("cherry.txt").exists(),
        "cherry.txt should exist in main after pick"
    );
}

#[test]
fn slice2_cherry_pick_conflict() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    // feature: modify README.md one way and commit.
    run_git_dyn(&["-C", repo_s, "checkout", "-b", "feature"]);
    let pick_hash = make_commit(&repo, "README.md", "feature version\n", "feature change");

    // main: modify README.md a different way and commit (creates divergence).
    run_git_dyn(&["-C", repo_s, "checkout", "main"]);
    make_commit(&repo, "README.md", "main version\n", "main change");

    // cherry-pick feature commit onto main → conflict.
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "cherry_pick",
            "repo": repo_s,
            "ref": pick_hash,
        }),
    );

    assert_eq!(res["ok"], false, "conflicting cherry-pick should fail; got: {res:?}");
    assert_eq!(res["error_kind"], "conflict");

    // Repo must still be in conflict state (not auto-aborted).
    let status_text = git_status_porcelain(&repo);
    assert!(
        status_text.contains("UU") || status_text.contains("AA"),
        "repo should be in conflict state after failed cherry-pick; status:\n{status_text}"
    );
}

// ── Slice 2 tests: BRANCH ─────────────────────────────────────────────────────

#[test]
fn slice2_branch_list() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo.to_str().unwrap(),
            "subcommand": "list",
        }),
    );

    assert_eq!(res["ok"], true, "branch list should succeed; got: {res:?}");
    let branches = res["branches"].as_array().expect("branches must be an array");
    let current = branches.iter().find(|b| b["current"] == true);
    assert!(current.is_some(), "one branch should be flagged as current");
    let current_name = current.unwrap()["name"].as_str().unwrap_or("");
    assert!(!current_name.is_empty(), "current branch name must not be empty");
}

#[test]
fn slice2_branch_create() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "create",
            "name": "new-feature",
        }),
    );
    assert_eq!(res["ok"], true, "branch create should succeed; got: {res:?}");
    assert_eq!(res["name"], "new-feature");

    // Verify branch appears in list.
    let list = run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "list",
        }),
    );
    let branches = list["branches"].as_array().unwrap();
    assert!(
        branches.iter().any(|b| b["name"] == "new-feature"),
        "new-feature should appear in branch list"
    );
}

#[test]
fn slice2_branch_delete() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    // Create a branch (same content as main → will be "merged").
    run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "create",
            "name": "to-delete",
        }),
    );

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "delete",
            "name": "to-delete",
        }),
    );
    assert_eq!(res["ok"], true, "branch delete should succeed; got: {res:?}");
    assert_eq!(res["name"], "to-delete");

    // Verify branch is gone.
    let list = run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "list",
        }),
    );
    let branches = list["branches"].as_array().unwrap();
    assert!(
        !branches.iter().any(|b| b["name"] == "to-delete"),
        "to-delete should be gone from branch list"
    );
}

#[test]
fn slice2_branch_delete_not_merged() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    // Create branch and add a commit not in main → not merged.
    run_git_dyn(&["-C", repo_s, "checkout", "-b", "unmerged"]);
    make_commit(&repo, "extra.txt", "data\n", "unmerged commit");
    run_git_dyn(&["-C", repo_s, "checkout", "main"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "delete",
            "name": "unmerged",
        }),
    );

    assert_eq!(res["ok"], false, "deleting unmerged branch should fail");
    assert_eq!(res["error_kind"], "not_merged");
}

// ── Slice 2 tests: CHECKOUT / SWITCH ──────────────────────────────────────────

#[test]
fn slice2_checkout_switch_branch() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    run_git_dyn(&["-C", repo_s, "branch", "other"]);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "checkout",
            "repo": repo_s,
            "ref": "other",
        }),
    );

    assert_eq!(res["ok"], true, "checkout should succeed; got: {res:?}");
    assert_eq!(res["branch"], "other");
    assert_eq!(res["detached"], false);
}

#[test]
fn slice2_checkout_create() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "checkout",
            "repo": repo.to_str().unwrap(),
            "ref": "fresh-branch",
            "create": true,
        }),
    );

    assert_eq!(res["ok"], true, "checkout -b should succeed; got: {res:?}");
    assert_eq!(res["branch"], "fresh-branch");
    assert_eq!(res["detached"], false);
}

#[test]
fn slice2_checkout_dirty() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    // Create `other` branch where README.md differs from main.
    run_git_dyn(&["-C", repo_s, "checkout", "-b", "other"]);
    make_commit(&repo, "README.md", "other branch version\n", "other change");
    run_git_dyn(&["-C", repo_s, "checkout", "main"]);

    // Dirty the working tree: modify README.md locally (unstaged) so it conflicts with `other`.
    fs::write(repo.join("README.md"), "local dirty change\n").unwrap();

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "checkout",
            "repo": repo_s,
            "ref": "other",
        }),
    );

    assert_eq!(res["ok"], false, "checkout with conflicting dirty tree should fail");
    assert_eq!(res["error_kind"], "dirty_working_tree");
}

#[test]
fn slice2_switch_create() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "switch",
            "repo": repo.to_str().unwrap(),
            "branch": "switched-branch",
            "create": true,
        }),
    );

    assert_eq!(res["ok"], true, "switch -c should succeed; got: {res:?}");
    assert_eq!(res["branch"], "switched-branch");
}

// ── Slice 2 tests: RESET ──────────────────────────────────────────────────────

#[test]
fn slice2_reset_soft() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    // Initial commit is the parent we'll reset back to.
    let parent_hash = head_hash(&repo);

    // Add a second commit.
    make_commit(&repo, "extra.txt", "data\n", "second commit");

    // Soft reset back to the initial commit.
    let res = run_tool_in(
        &repo,
        json!({
            "operation": "reset",
            "repo": repo_s,
            "mode": "soft",
            "ref": &parent_hash,
        }),
    );

    assert_eq!(res["ok"], true, "reset soft should succeed; got: {res:?}");
    assert_eq!(
        res["ref"].as_str().unwrap_or(""),
        parent_hash,
        "HEAD should point to the parent commit after soft reset"
    );

    // With soft reset, changes should be staged (index should still have extra.txt).
    let status_text = git_status_porcelain(&repo);
    assert!(
        status_text.contains("extra.txt"),
        "after soft reset, extra.txt should be staged; status:\n{status_text}"
    );
}

#[test]
fn slice2_reset_hard() {
    let td = TestDir::new();
    let repo = td.path().join("repo");
    init_git_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    let parent_hash = head_hash(&repo);

    make_commit(&repo, "extra.txt", "data\n", "second commit");

    let res = run_tool_in(
        &repo,
        json!({
            "operation": "reset",
            "repo": repo_s,
            "mode": "hard",
            "ref": &parent_hash,
        }),
    );

    assert_eq!(res["ok"], true, "reset hard should succeed; got: {res:?}");
    assert_eq!(res["ref"].as_str().unwrap_or(""), parent_hash);

    // Hard reset: extra.txt must not exist and working dir must be clean.
    assert!(
        !repo.join("extra.txt").exists(),
        "extra.txt should be gone after hard reset"
    );
    let status_text = git_status_porcelain(&repo);
    assert!(
        status_text.trim().is_empty(),
        "working directory should be clean after hard reset; status:\n{status_text}"
    );
}
