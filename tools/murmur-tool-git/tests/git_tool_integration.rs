// Original integration tests — migrated to the ok/message/error_kind envelope.
// Backward-compat operations (create_worktree, status) are covered here.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::{json, Value};

// ── Shared helpers ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ToolResult {
    ok: bool,
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    error_kind: Option<String>,
    /// All operation-specific fields (entries, files, path, branch, …) land here.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        path.push(format!(
            "murmur-tool-git-integration-{}-{nanos}",
            std::process::id()
        ));
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

fn init_git_repo(repo: &Path) {
    run_git(["init", repo.to_str().expect("repo path utf-8")]);
    run_git(["-C", repo.to_str().unwrap(), "config", "user.email", "test@example.com"]);
    run_git(["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    run_git(["-C", repo.to_str().unwrap(), "add", "README.md"]);
    run_git(["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
}

fn run_tool_in(cwd: &Path, payload: Value) -> ToolResult {
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
        .expect("murmur-tool-git binary should start");

    let mut stdin = child.stdin.take().expect("stdin should be available");
    stdin
        .write_all(envelope.as_bytes())
        .expect("envelope should be written");
    drop(stdin);

    let output = child
        .wait_with_output()
        .expect("tool output should be captured");
    assert!(
        output.status.success(),
        "murmur-tool-git exited non-zero:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice::<ToolResult>(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout was not valid ToolResult JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn run_git<const N: usize>(args: [&str; N]) -> Output {
    let output = Command::new("git")
        .args(args)
        .output()
        .expect("git command should start");
    assert!(
        output.status.success(),
        "git command failed: git {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn binary_should_manage_worktree_lifecycle() {
    let temp = TestDir::new();
    let repo_path = temp.path().join("repo");

    init_git_repo(&repo_path);

    let branch = "feature/worktree-test";
    run_git(["-C", repo_path.to_str().unwrap(), "branch", branch]);

    let worktree_path = repo_path.join("wt-feature");
    let worktree_rel = "wt-feature";

    let create_result = run_tool_in(
        &repo_path,
        json!({ "operation": "create_worktree", "path": worktree_rel, "branch": branch }),
    );
    assert!(
        create_result.ok,
        "create_worktree failed: {}",
        create_result.message
    );

    assert_eq!(create_result.extra["path"], worktree_rel);
    assert_eq!(create_result.extra["branch"], branch);

    assert!(worktree_path.exists(), "worktree should exist on disk");

    let branch_output = run_git([
        "-C",
        worktree_path.to_str().unwrap(),
        "branch",
        "--show-current",
    ]);
    let checked_out = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();
    assert_eq!(checked_out, branch);

    // status on the worktree — clean
    let status_result = run_tool_in(
        &repo_path,
        json!({ "operation": "status", "repo": worktree_path.to_str().unwrap() }),
    );
    assert!(status_result.ok, "status failed: {}", status_result.message);
    assert_eq!(status_result.message, "working tree clean");

    let entries = status_result.extra["entries"]
        .as_array()
        .expect("entries should be array");
    assert!(entries.is_empty(), "clean worktree should have no entries");

    // list_files was removed from v1 scope; use `status` to verify README.md is tracked.
    // Migration: instead of checking tracked files via ls-files, we verify the worktree
    // is clean (README.md is tracked, nothing unstaged), which confirms the file is present.
    let files_result = run_tool_in(
        &repo_path,
        json!({ "operation": "status", "repo": worktree_path.to_str().unwrap() }),
    );
    assert!(
        files_result.ok,
        "status (migrated from list_files) failed: {}",
        files_result.message
    );
    assert_eq!(
        files_result.message, "working tree clean",
        "worktree should be clean (README.md is tracked)"
    );

    // Attempt to create another worktree on the same branch — should fail
    let duplicate_result = run_tool_in(
        &repo_path,
        json!({ "operation": "create_worktree", "path": "wt-duplicate", "branch": branch }),
    );
    assert!(
        !duplicate_result.ok,
        "duplicate branch checkout should return ok=false; got: {:?}",
        duplicate_result
    );
    assert!(
        duplicate_result.message.contains("already checked out"),
        "unexpected message: {}",
        duplicate_result.message
    );
    assert!(
        !repo_path.join("wt-duplicate").exists(),
        "duplicate worktree path should not be created"
    );
}

#[test]
fn status_shows_modified_file() {
    let temp = TestDir::new();
    let repo_path = temp.path().join("repo");

    init_git_repo(&repo_path);

    let branch = "feature/status-test";
    run_git(["-C", repo_path.to_str().unwrap(), "branch", branch]);

    run_tool_in(
        &repo_path,
        json!({ "operation": "create_worktree", "path": "wt-status", "branch": branch }),
    );

    // Modify a file in the worktree (unstaged)
    fs::write(repo_path.join("wt-status").join("README.md"), "modified\n").unwrap();

    let wt_path = repo_path.join("wt-status");
    let status_result = run_tool_in(
        &repo_path,
        json!({ "operation": "status", "repo": wt_path.to_str().unwrap() }),
    );
    assert!(status_result.ok, "status failed: {}", status_result.message);

    let entries = status_result.extra["entries"]
        .as_array()
        .expect("entries should be array");
    assert!(!entries.is_empty(), "should have at least one modified entry");

    // Unstaged modification shows as " M" in porcelain v1
    let found = entries.iter().any(|e| {
        e["path"].as_str() == Some("README.md")
            && e["status_code"]
                .as_str()
                .map(|c| c.contains('M'))
                .unwrap_or(false)
    });
    assert!(
        found,
        "README.md should appear with status_code containing 'M'; entries: {entries:?}"
    );
}

#[test]
fn unknown_operation_returns_error_not_panic() {
    let temp = TestDir::new();
    let repo_path = temp.path().join("repo");
    init_git_repo(&repo_path);

    let result = run_tool_in(
        &repo_path,
        json!({ "operation": "invalid_op", "path": "." }),
    );
    assert!(!result.ok);
    assert!(
        result.message.contains("unknown operation"),
        "got: {}",
        result.message
    );
}
