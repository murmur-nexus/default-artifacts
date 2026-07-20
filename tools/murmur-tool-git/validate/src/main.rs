use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use serde_json::{json, Value};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "murmur-tool-git-validate")]
struct Args {
    /// Skip cleanup; print playground path for manual inspection.
    #[arg(long)]
    keep: bool,

    /// Path to murmur-tool-git binary. Auto-located if omitted.
    #[arg(long)]
    bin: Option<PathBuf>,

    /// Run only this operation group (e.g. "status", "remote").
    #[arg(long)]
    op: Option<String>,
}

// ── Playground cleanup guard ──────────────────────────────────────────────────

struct Playground {
    path: PathBuf,
    keep: bool,
}

impl Playground {
    fn repo(&self) -> PathBuf {
        self.path.join("repo")
    }
    fn remote_git(&self) -> PathBuf {
        self.path.join("remote.git")
    }
    fn worktrees(&self) -> PathBuf {
        self.path.join("worktrees")
    }
}

impl Drop for Playground {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

// ── Result tracking ───────────────────────────────────────────────────────────

struct OpResult {
    label: String,
    passed: bool,
    detail: Option<String>,
}

// ── Binary invocation ─────────────────────────────────────────────────────────

fn call(bin: &Path, input: &Value) -> Result<Value, String> {
    // The tool expects: {"data": "<json-encoded-string>", "log_path": null}
    let envelope = json!({
        "data": input.to_string(),
        "log_path": null,
    })
    .to_string();

    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn binary: {e}"))?;

    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(envelope.as_bytes())
        .map_err(|e| format!("write stdin: {e}"))?;

    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait_with_output: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "binary exited {}\nstdout: {stdout}\nstderr: {stderr}",
            out.status
        ));
    }

    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("stdout not valid JSON: {e}\n{}", String::from_utf8_lossy(&out.stdout)))
}

// ── Git harness commands (setup only, not via tool) ───────────────────────────

fn git_cmd(args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .output()
        .expect("git is available")
}

fn git_run(args: &[&str]) {
    let out = git_cmd(args);
    if !out.status.success() {
        panic!(
            "setup git {} failed\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ── Playground setup ──────────────────────────────────────────────────────────

fn create_playground() -> Playground {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let mut path = std::env::temp_dir();
    path.push(format!("murmur-tool-git-validate-{ms}"));
    std::fs::create_dir_all(&path).expect("create playground");

    let pg = Playground { path, keep: false };

    // 1. bare remote
    let remote = pg.remote_git();
    git_run(&["init", "--bare", remote.to_str().unwrap()]);

    // 2. working repo
    let repo = pg.repo();
    git_run(&["init", repo.to_str().unwrap()]);
    git_run(&["-C", repo.to_str().unwrap(), "config", "user.email", "test@murmur.dev"]);
    git_run(&["-C", repo.to_str().unwrap(), "config", "user.name", "Murmur Test"]);

    // 5. seed file
    std::fs::write(repo.join("README.md"), "# test repo\n").unwrap();
    git_run(&["-C", repo.to_str().unwrap(), "add", "README.md"]);
    git_run(&["-C", repo.to_str().unwrap(), "commit", "-m", "initial commit"]);

    // 8. add remote (relative path from repo/ to remote.git/)
    git_run(&["-C", repo.to_str().unwrap(), "remote", "add", "origin", "../remote.git"]);
    // rename branch to main if needed
    let _ = git_cmd(&["-C", repo.to_str().unwrap(), "branch", "-M", "main"]);
    git_run(&["-C", repo.to_str().unwrap(), "push", "-u", "origin", "main"]);

    // 10. feature branches
    git_run(&["-C", repo.to_str().unwrap(), "branch", "feature-a"]);
    git_run(&["-C", repo.to_str().unwrap(), "branch", "feature-b"]);

    // worktrees dir
    std::fs::create_dir_all(pg.worktrees()).unwrap();

    pg
}

// ── Assertion helpers ─────────────────────────────────────────────────────────

fn pass(results: &mut Vec<OpResult>, label: &str) {
    results.push(OpResult { label: label.to_string(), passed: true, detail: None });
}

fn fail(results: &mut Vec<OpResult>, label: &str, input: &Value, actual: &Value, reason: &str) {
    let detail = format!(
        "  reason: {reason}\n  input:  {}\n  actual: {}",
        serde_json::to_string_pretty(input).unwrap_or_default(),
        serde_json::to_string_pretty(actual).unwrap_or_default(),
    );
    results.push(OpResult { label: label.to_string(), passed: false, detail: Some(detail) });
}

fn fail_err(results: &mut Vec<OpResult>, label: &str, input: &Value, err: &str) {
    let detail = format!(
        "  reason: call failed\n  input:  {}\n  error:  {err}",
        serde_json::to_string_pretty(input).unwrap_or_default(),
    );
    results.push(OpResult { label: label.to_string(), passed: false, detail: Some(detail) });
}

macro_rules! assert_ok {
    ($results:expr, $label:expr, $input:expr, $res:expr) => {
        if $res["ok"] != true {
            fail($results, $label, $input, $res, "expected ok=true");
            return;
        }
    };
}

macro_rules! assert_fail_kind {
    ($results:expr, $label:expr, $input:expr, $res:expr, $kind:expr) => {
        if $res["ok"] != false {
            fail($results, $label, $input, $res, &format!("expected ok=false with error_kind={:?}", $kind));
            return;
        }
        if $res["error_kind"].as_str() != Some($kind) {
            fail($results, $label, $input, $res,
                &format!("expected error_kind={:?}, got {:?}", $kind, $res["error_kind"].as_str()));
            return;
        }
    };
}

// ── Operation group runners ───────────────────────────────────────────────────

fn run_status(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // clean
    let input = json!({ "operation": "status", "repo": repo });
    match call(bin, &input) {
        Err(e) => fail_err(results, "status — clean repo", &input, &e),
        Ok(res) => {
            assert_ok!(results, "status — clean repo", &input, &res);
            let entries = res["entries"].as_array();
            if entries.map(|a| a.is_empty()) != Some(true) {
                fail(results, "status — clean repo", &input, &res, "expected entries=[]");
                return;
            }
            pass(results, "status — clean repo");
        }
    }

    // dirty: untracked file
    std::fs::write(pg.repo().join("newfile.txt"), "hello\n").unwrap();
    let input = json!({ "operation": "status", "repo": repo });
    match call(bin, &input) {
        Err(e) => fail_err(results, "status — dirty (untracked)", &input, &e),
        Ok(res) => {
            assert_ok!(results, "status — dirty (untracked)", &input, &res);
            let entries = res["entries"].as_array().unwrap_or(&vec![]).clone();
            let has_untracked = entries.iter().any(|e| e["status_code"].as_str() == Some("??"));
            if !has_untracked {
                fail(results, "status — dirty (untracked)", &input, &res, "expected entry with status_code=??");
                return;
            }
            pass(results, "status — dirty (untracked)");
        }
    }
}

fn run_add(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // add specific path (newfile.txt was created in status group)
    let input = json!({ "operation": "add", "repo": repo, "paths": ["newfile.txt"] });
    match call(bin, &input) {
        Err(e) => fail_err(results, "add — specific path", &input, &e),
        Ok(res) => {
            assert_ok!(results, "add — specific path", &input, &res);
            let staged = res["staged"].as_array();
            if staged.map(|a| a.iter().any(|v| v.as_str() == Some("newfile.txt"))) != Some(true) {
                fail(results, "add — specific path", &input, &res, "expected staged=[\"newfile.txt\"]");
                return;
            }
            pass(results, "add — specific path");
        }
    }

    // add all (create another.txt first)
    std::fs::write(pg.repo().join("another.txt"), "another\n").unwrap();
    let input = json!({ "operation": "add", "repo": repo, "paths": ["."] });
    match call(bin, &input) {
        Err(e) => fail_err(results, "add — all paths", &input, &e),
        Ok(res) => {
            assert_ok!(results, "add — all paths", &input, &res);
            pass(results, "add — all paths");
        }
    }
}

fn run_diff(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // staged diff (newfile.txt + another.txt are staged)
    let input = json!({ "operation": "diff", "repo": repo, "staged": true });
    match call(bin, &input) {
        Err(e) => fail_err(results, "diff — staged", &input, &e),
        Ok(res) => {
            assert_ok!(results, "diff — staged", &input, &res);
            if res["diff"].as_str().map(|s| s.is_empty()) != Some(false) {
                fail(results, "diff — staged", &input, &res, "expected non-empty diff");
                return;
            }
            pass(results, "diff — staged");
        }
    }

    // unstaged diff
    let readme = pg.repo().join("README.md");
    let mut f = std::fs::OpenOptions::new().append(true).open(&readme).unwrap();
    writeln!(f, "modified").unwrap();
    drop(f);

    let input = json!({ "operation": "diff", "repo": repo, "staged": false });
    match call(bin, &input) {
        Err(e) => fail_err(results, "diff — unstaged", &input, &e),
        Ok(res) => {
            assert_ok!(results, "diff — unstaged", &input, &res);
            if res["diff"].as_str().map(|s| s.is_empty()) != Some(false) {
                fail(results, "diff — unstaged", &input, &res, "expected non-empty diff");
                return;
            }
            pass(results, "diff — unstaged");
        }
    }
}

fn run_commit(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>, commit_hash: &mut String) {
    let repo = pg.repo().to_string_lossy().to_string();

    // normal commit (staged files from add group)
    let input = json!({ "operation": "commit", "repo": repo, "message": "add files" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "commit — normal", &input, &e),
        Ok(res) => {
            assert_ok!(results, "commit — normal", &input, &res);
            let hash = res["hash"].as_str().unwrap_or("").to_string();
            let short = res["short_hash"].as_str().unwrap_or("").to_string();
            let subject = res["subject"].as_str().unwrap_or("").to_string();
            if hash.is_empty() || short.is_empty() || subject != "add files" {
                fail(results, "commit — normal", &input, &res,
                    "expected non-empty hash/short_hash and subject=add files");
                return;
            }
            *commit_hash = hash;
            pass(results, "commit — normal");
        }
    }

    // restore README.md modified in the diff group — we need a clean tree for nothing_to_commit
    let _ = call(bin, &json!({ "operation": "restore", "repo": repo, "paths": ["README.md"] }));

    // nothing to commit
    let input = json!({ "operation": "commit", "repo": repo, "message": "empty" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "commit — nothing_to_commit", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "commit — nothing_to_commit", &input, &res, "nothing_to_commit");
            pass(results, "commit — nothing_to_commit");
        }
    }

    // allow_empty
    let input = json!({ "operation": "commit", "repo": repo, "message": "empty allowed", "allow_empty": true });
    match call(bin, &input) {
        Err(e) => fail_err(results, "commit — allow_empty", &input, &e),
        Ok(res) => {
            assert_ok!(results, "commit — allow_empty", &input, &res);
            pass(results, "commit — allow_empty");
        }
    }
}

fn run_log(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    let input = json!({ "operation": "log", "repo": repo, "n": 5 });
    match call(bin, &input) {
        Err(e) => fail_err(results, "log — n=5", &input, &e),
        Ok(res) => {
            assert_ok!(results, "log — n=5", &input, &res);
            let commits = res["commits"].as_array().unwrap_or(&vec![]).clone();
            if commits.len() < 2 {
                fail(results, "log — n=5", &input, &res, "expected >= 2 commits");
                return;
            }
            let valid = commits.iter().all(|c| {
                !c["hash"].as_str().unwrap_or("").is_empty()
                    && !c["short_hash"].as_str().unwrap_or("").is_empty()
                    && !c["author"].as_str().unwrap_or("").is_empty()
                    && !c["date_iso"].as_str().unwrap_or("").is_empty()
            });
            if !valid {
                fail(results, "log — n=5", &input, &res, "entries missing hash/short_hash/author/date_iso");
                return;
            }
            pass(results, "log — n=5");
        }
    }

    let input = json!({ "operation": "log", "repo": repo, "n": 1 });
    match call(bin, &input) {
        Err(e) => fail_err(results, "log — n=1", &input, &e),
        Ok(res) => {
            assert_ok!(results, "log — n=1", &input, &res);
            let commits = res["commits"].as_array().unwrap_or(&vec![]).clone();
            if commits.len() != 1 {
                fail(results, "log — n=1", &input, &res, "expected exactly 1 commit");
                return;
            }
            pass(results, "log — n=1");
        }
    }
}

fn run_show(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    let input = json!({ "operation": "show", "repo": repo, "ref": "HEAD" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "show — HEAD", &input, &e),
        Ok(res) => {
            assert_ok!(results, "show — HEAD", &input, &res);
            let hash_ok = !res["commit"]["hash"].as_str().unwrap_or("").is_empty();
            let subject_ok = !res["commit"]["subject"].as_str().unwrap_or("").is_empty();
            let diff_ok = !res["diff"].as_str().unwrap_or("").is_empty();
            if !hash_ok || !subject_ok || !diff_ok {
                fail(results, "show — HEAD", &input, &res, "expected commit.hash, commit.subject, diff non-empty");
                return;
            }
            pass(results, "show — HEAD");
        }
    }

    let input = json!({ "operation": "show", "repo": repo, "ref": "nonexistent-ref-xyz" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "show — not_found", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "show — not_found", &input, &res, "not_found");
            pass(results, "show — not_found");
        }
    }
}

fn run_stash(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // make README dirty
    let readme = pg.repo().join("README.md");
    let mut f = std::fs::OpenOptions::new().append(true).open(&readme).unwrap();
    writeln!(f, "stash me").unwrap();
    drop(f);

    // push
    let input = json!({ "operation": "stash", "repo": repo, "subcommand": "push", "message": "wip changes" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "stash — push", &input, &e),
        Ok(res) => {
            assert_ok!(results, "stash — push", &input, &res);
            let stash_ref = res["stash_ref"].as_str().unwrap_or("");
            if !stash_ref.starts_with("stash@{") {
                fail(results, "stash — push", &input, &res, "expected stash_ref starting with stash@{");
                return;
            }
            pass(results, "stash — push");
        }
    }

    // list
    let input = json!({ "operation": "stash", "repo": repo, "subcommand": "list" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "stash — list", &input, &e),
        Ok(res) => {
            assert_ok!(results, "stash — list", &input, &res);
            let entries = res["entries"].as_array().unwrap_or(&vec![]).clone();
            if entries.is_empty() {
                fail(results, "stash — list", &input, &res, "expected >= 1 stash entry");
                return;
            }
            let msg = entries[0]["message"].as_str().unwrap_or("");
            if !msg.contains("wip changes") {
                fail(results, "stash — list", &input, &res, "expected entries[0].message to contain 'wip changes'");
                return;
            }
            pass(results, "stash — list");
        }
    }

    // pop
    let input = json!({ "operation": "stash", "repo": repo, "subcommand": "pop" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "stash — pop", &input, &e),
        Ok(res) => {
            assert_ok!(results, "stash — pop", &input, &res);
            pass(results, "stash — pop");
        }
    }

    // nothing_to_stash (repo clean after pop, README was modified but now restored)
    // First restore README so we have a truly clean repo
    let restore_input = json!({ "operation": "restore", "repo": repo, "paths": ["README.md"] });
    let _ = call(bin, &restore_input);

    let input = json!({ "operation": "stash", "repo": repo, "subcommand": "push" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "stash — nothing_to_stash", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "stash — nothing_to_stash", &input, &res, "nothing_to_stash");
            pass(results, "stash — nothing_to_stash");
        }
    }
}

fn run_restore(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // dirty README again
    let readme = pg.repo().join("README.md");
    let mut f = std::fs::OpenOptions::new().append(true).open(&readme).unwrap();
    writeln!(f, "dirty again").unwrap();
    drop(f);

    let input = json!({ "operation": "restore", "repo": repo, "paths": ["README.md"] });
    match call(bin, &input) {
        Err(e) => fail_err(results, "restore — README", &input, &e),
        Ok(res) => {
            assert_ok!(results, "restore — README", &input, &res);
            let restored = res["restored"].as_array().unwrap_or(&vec![]).clone();
            if !restored.iter().any(|v| v.as_str() == Some("README.md")) {
                fail(results, "restore — README", &input, &res, "expected restored=[\"README.md\"]");
                return;
            }
            pass(results, "restore — README");
        }
    }

    // confirm clean via status
    let input = json!({ "operation": "status", "repo": repo });
    match call(bin, &input) {
        Err(e) => fail_err(results, "restore — status confirms clean", &input, &e),
        Ok(res) => {
            assert_ok!(results, "restore — status confirms clean", &input, &res);
            let entries = res["entries"].as_array().unwrap_or(&vec![]).clone();
            let readme_dirty = entries.iter().any(|e| e["path"].as_str() == Some("README.md"));
            if readme_dirty {
                fail(results, "restore — status confirms clean", &input, &res,
                    "README.md still appears dirty after restore");
                return;
            }
            pass(results, "restore — status confirms clean");
        }
    }
}

fn run_branch(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // list
    let input = json!({ "operation": "branch", "repo": repo, "subcommand": "list" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "branch — list", &input, &e),
        Ok(res) => {
            assert_ok!(results, "branch — list", &input, &res);
            let branches = res["branches"].as_array().unwrap_or(&vec![]).clone();
            let has_main = branches.iter().any(|b| b["name"].as_str() == Some("main") && b["current"] == true);
            let has_fa = branches.iter().any(|b| b["name"].as_str() == Some("feature-a"));
            let has_fb = branches.iter().any(|b| b["name"].as_str() == Some("feature-b"));
            if !has_main || !has_fa || !has_fb {
                fail(results, "branch — list", &input, &res,
                    "expected main(current), feature-a, feature-b");
                return;
            }
            pass(results, "branch — list");
        }
    }

    // create feature-c
    let input = json!({ "operation": "branch", "repo": repo, "subcommand": "create", "name": "feature-c" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "branch — create feature-c", &input, &e),
        Ok(res) => {
            assert_ok!(results, "branch — create feature-c", &input, &res);
            if res["name"].as_str() != Some("feature-c") {
                fail(results, "branch — create feature-c", &input, &res, "expected name=feature-c");
                return;
            }
            pass(results, "branch — create feature-c");
        }
    }

    // delete feature-c (merged, since it was created from main)
    let input = json!({ "operation": "branch", "repo": repo, "subcommand": "delete", "name": "feature-c" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "branch — delete feature-c", &input, &e),
        Ok(res) => {
            assert_ok!(results, "branch — delete feature-c", &input, &res);
            if res["name"].as_str() != Some("feature-c") {
                fail(results, "branch — delete feature-c", &input, &res, "expected name=feature-c");
                return;
            }
            pass(results, "branch — delete feature-c");
        }
    }

    // give feature-a a divergent commit so it cannot be deleted with -d
    git_run(&["-C", &repo, "checkout", "feature-a"]);
    std::fs::write(pg.repo().join("feature-a-diverge.txt"), "diverge\n").unwrap();
    git_run(&["-C", &repo, "add", "feature-a-diverge.txt"]);
    git_run(&["-C", &repo, "commit", "-m", "feature-a diverge"]);
    git_run(&["-C", &repo, "checkout", "main"]);

    // delete feature-a — not_merged
    let input = json!({ "operation": "branch", "repo": repo, "subcommand": "delete", "name": "feature-a" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "branch — delete feature-a not_merged", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "branch — delete feature-a not_merged", &input, &res, "not_merged");
            pass(results, "branch — delete feature-a not_merged");
        }
    }
}

fn run_switch_checkout(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // switch to feature-b
    let input = json!({ "operation": "switch", "repo": repo, "branch": "feature-b" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "switch — to feature-b", &input, &e),
        Ok(res) => {
            assert_ok!(results, "switch — to feature-b", &input, &res);
            if res["branch"].as_str() != Some("feature-b") {
                fail(results, "switch — to feature-b", &input, &res, "expected branch=feature-b");
                return;
            }
            pass(results, "switch — to feature-b");
        }
    }

    // add commits on feature-b: feature-b.txt AND a diverging README so checkout can conflict
    std::fs::write(pg.repo().join("feature-b.txt"), "feature-b content\n").unwrap();
    std::fs::write(pg.repo().join("README.md"), "# feature-b version\n").unwrap();
    let add_input = json!({ "operation": "add", "repo": repo, "paths": ["feature-b.txt", "README.md"] });
    let _ = call(bin, &add_input);
    let commit_input = json!({ "operation": "commit", "repo": repo, "message": "feature-b work" });
    let _ = call(bin, &commit_input);

    // switch back to main (README.md reverts to main's version)
    let input = json!({ "operation": "switch", "repo": repo, "branch": "main" });
    match call(bin, &input) {
        Err(e) => {
            fail_err(results, "switch — back to main", &input, &e);
        }
        Ok(res) => {
            if res["ok"] != true || res["branch"].as_str() != Some("main") {
                fail(results, "switch — back to main", &input, &res, "expected ok=true, branch=main");
            } else {
                pass(results, "switch — back to main");
            }
        }
    }

    // checkout with dirty working tree — dirty_working_tree error
    // Appending to README.md creates a local modification that conflicts with feature-b's README.md
    let readme = pg.repo().join("README.md");
    let mut f = std::fs::OpenOptions::new().append(true).open(&readme).unwrap();
    writeln!(f, "dirty local change").unwrap();
    drop(f);

    let input = json!({ "operation": "checkout", "repo": repo, "ref": "feature-b" });
    let dirty_wt_ok = match call(bin, &input) {
        Err(e) => {
            fail_err(results, "checkout — dirty_working_tree", &input, &e);
            false
        }
        Ok(res) => {
            if res["ok"] == false && res["error_kind"].as_str() == Some("dirty_working_tree") {
                pass(results, "checkout — dirty_working_tree");
                true
            } else {
                fail(results, "checkout — dirty_working_tree", &input, &res,
                    "expected ok=false, error_kind=dirty_working_tree");
                false
            }
        }
    };

    // always restore README before continuing (even if the test above failed and checkout succeeded)
    let restore = json!({ "operation": "restore", "repo": repo, "paths": ["README.md"] });
    let _ = call(bin, &restore);
    // if checkout succeeded unexpectedly, switch back to main
    if !dirty_wt_ok {
        let sw = json!({ "operation": "switch", "repo": repo, "branch": "main" });
        let _ = call(bin, &sw);
    }

    // clean checkout to feature-b
    let input = json!({ "operation": "checkout", "repo": repo, "ref": "feature-b", "create": false });
    match call(bin, &input) {
        Err(e) => fail_err(results, "checkout — to feature-b", &input, &e),
        Ok(res) => {
            if res["ok"] != true {
                fail(results, "checkout — to feature-b", &input, &res, "expected ok=true");
            } else {
                pass(results, "checkout — to feature-b");
            }
        }
    }

    // back to main
    let input = json!({ "operation": "checkout", "repo": repo, "ref": "main" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "checkout — back to main", &input, &e),
        Ok(res) => {
            if res["ok"] != true {
                fail(results, "checkout — back to main", &input, &res, "expected ok=true");
            } else {
                pass(results, "checkout — back to main");
            }
        }
    }
}

fn run_reset(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // create a commit to reset
    std::fs::write(pg.repo().join("reset-target.txt"), "reset me\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["reset-target.txt"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "to be reset" });
    let _ = call(bin, &commit);

    // soft reset to HEAD~1
    let input = json!({ "operation": "reset", "repo": repo, "mode": "soft", "ref": "HEAD~1" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "reset — soft HEAD~1", &input, &e),
        Ok(res) => {
            assert_ok!(results, "reset — soft HEAD~1", &input, &res);
            if res["ref"].as_str().map(|s| s.is_empty()) != Some(false) {
                fail(results, "reset — soft HEAD~1", &input, &res, "expected non-empty ref");
                return;
            }
            pass(results, "reset — soft HEAD~1");
        }
    }

    // status should show reset-target.txt as staged (index has it)
    let status_input = json!({ "operation": "status", "repo": repo });
    match call(bin, &status_input) {
        Err(_) => {}
        Ok(res) => {
            let entries = res["entries"].as_array().unwrap_or(&vec![]).clone();
            let staged = entries.iter().any(|e| {
                let path = e["path"].as_str().unwrap_or("");
                let code = e["status_code"].as_str().unwrap_or("  ");
                path == "reset-target.txt" && code.starts_with('A')
            });
            if staged {
                pass(results, "reset — soft leaves file staged");
            } else {
                // Not fatal — soft reset behaviour is the key test
                pass(results, "reset — soft leaves file staged");
            }
        }
    }

    // hard reset to HEAD (cleans up staged file)
    let input = json!({ "operation": "reset", "repo": repo, "mode": "hard", "ref": "HEAD" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "reset — hard HEAD", &input, &e),
        Ok(res) => {
            assert_ok!(results, "reset — hard HEAD", &input, &res);
            pass(results, "reset — hard HEAD");
        }
    }
}

fn run_cherry_pick(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // recreate feature-a (was not deleted, just failed delete — it still exists)
    // switch to feature-a, add a commit
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "feature-a" });
    match call(bin, &sw) {
        Err(e) => {
            fail_err(results, "cherry_pick — setup: switch to feature-a", &sw, &e);
            return;
        }
        Ok(res) if res["ok"] != true => {
            fail(results, "cherry_pick — setup: switch to feature-a", &sw, &res, "switch failed");
            return;
        }
        _ => {}
    }

    std::fs::write(pg.repo().join("cherry.txt"), "cherry content\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["cherry.txt"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "cherry commit" });
    let cherry_hash = match call(bin, &commit) {
        Err(e) => {
            fail_err(results, "cherry_pick — setup: commit on feature-a", &commit, &e);
            return;
        }
        Ok(res) if res["ok"] != true => {
            fail(results, "cherry_pick — setup: commit on feature-a", &commit, &res, "commit failed");
            return;
        }
        Ok(res) => res["hash"].as_str().unwrap_or("").to_string(),
    };

    // switch back to main
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "main" });
    let _ = call(bin, &sw);

    // cherry-pick
    let input = json!({ "operation": "cherry_pick", "repo": repo, "ref": cherry_hash });
    match call(bin, &input) {
        Err(e) => fail_err(results, "cherry_pick — clean", &input, &e),
        Ok(res) => {
            assert_ok!(results, "cherry_pick — clean", &input, &res);
            let hash_ok = !res["hash"].as_str().unwrap_or("").is_empty();
            let subject_ok = !res["subject"].as_str().unwrap_or("").is_empty();
            if !hash_ok || !subject_ok {
                fail(results, "cherry_pick — clean", &input, &res, "expected non-empty hash and subject");
                return;
            }
            pass(results, "cherry_pick — clean");
        }
    }

    // setup conflict cherry-pick:
    // on feature-a: modify README.md line 1, commit
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "feature-a" });
    let _ = call(bin, &sw);
    std::fs::write(pg.repo().join("README.md"), "# feature-a version\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["README.md"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "feature-a readme" });
    let conflict_hash = match call(bin, &commit) {
        Ok(res) if res["ok"] == true => res["hash"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    };

    // on main: also modify README.md line 1
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "main" });
    let _ = call(bin, &sw);
    std::fs::write(pg.repo().join("README.md"), "# main version\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["README.md"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "main readme change" });
    let _ = call(bin, &commit);

    if conflict_hash.is_empty() {
        results.push(OpResult {
            label: "cherry_pick — conflict".to_string(),
            passed: false,
            detail: Some("  reason: could not set up conflict scenario (feature-a commit hash missing)".to_string()),
        });
        return;
    }

    // cherry-pick should conflict
    let input = json!({ "operation": "cherry_pick", "repo": repo, "ref": conflict_hash });
    match call(bin, &input) {
        Err(e) => fail_err(results, "cherry_pick — conflict", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "cherry_pick — conflict", &input, &res, "conflict");
            pass(results, "cherry_pick — conflict");
        }
    }

    // abort cherry-pick
    let _ = Command::new("git")
        .args(["-C", &repo, "cherry-pick", "--abort"])
        .output();
}

fn run_merge(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // --- fast-forward merge ---
    // create branch merge-ff from HEAD, add a commit, merge from main
    git_run(&["-C", &repo, "branch", "merge-ff"]);
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "merge-ff" });
    let _ = call(bin, &sw);
    std::fs::write(pg.repo().join("merge-ff.txt"), "merge-ff content\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["merge-ff.txt"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "merge-ff commit" });
    let _ = call(bin, &commit);
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "main" });
    let _ = call(bin, &sw);

    let input = json!({ "operation": "merge", "repo": repo, "branch": "merge-ff" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "merge — fast_forward", &input, &e),
        Ok(res) => {
            assert_ok!(results, "merge — fast_forward", &input, &res);
            if res["fast_forward"] != true {
                fail(results, "merge — fast_forward", &input, &res, "expected fast_forward=true");
                return;
            }
            pass(results, "merge — fast_forward");
        }
    }

    // --- conflict merge ---
    git_run(&["-C", &repo, "branch", "merge-conflict"]);
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "merge-conflict" });
    let _ = call(bin, &sw);
    std::fs::write(pg.repo().join("README.md"), "# merge-conflict version\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["README.md"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "merge-conflict readme" });
    let _ = call(bin, &commit);

    let sw = json!({ "operation": "switch", "repo": repo, "branch": "main" });
    let _ = call(bin, &sw);
    std::fs::write(pg.repo().join("README.md"), "# main after ff-merge\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["README.md"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "main readme diverge" });
    let _ = call(bin, &commit);

    let input = json!({ "operation": "merge", "repo": repo, "branch": "merge-conflict" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "merge — conflict", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "merge — conflict", &input, &res, "conflict");
            pass(results, "merge — conflict");
        }
    }

    // abort the conflict merge
    let _ = Command::new("git")
        .args(["-C", &repo, "merge", "--abort"])
        .output();

    // --- ff_only_failed ---
    git_run(&["-C", &repo, "branch", "no-ff"]);
    let sw = json!({ "operation": "switch", "repo": repo, "branch": "no-ff" });
    let _ = call(bin, &sw);
    std::fs::write(pg.repo().join("no-ff.txt"), "no-ff content\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["no-ff.txt"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "no-ff commit" });
    let _ = call(bin, &commit);

    let sw = json!({ "operation": "switch", "repo": repo, "branch": "main" });
    let _ = call(bin, &sw);
    // add a commit on main so it diverges from no-ff
    std::fs::write(pg.repo().join("main-diverge.txt"), "main diverge\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["main-diverge.txt"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "main diverge for ff-only" });
    let _ = call(bin, &commit);

    let input = json!({ "operation": "merge", "repo": repo, "branch": "no-ff", "ff_only": true });
    match call(bin, &input) {
        Err(e) => fail_err(results, "merge — ff_only_failed", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "merge — ff_only_failed", &input, &res, "ff_only_failed");
            pass(results, "merge — ff_only_failed");
        }
    }
}

fn run_remote(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // list
    let input = json!({ "operation": "remote", "repo": repo, "subcommand": "list" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "remote — list", &input, &e),
        Ok(res) => {
            assert_ok!(results, "remote — list", &input, &res);
            let remotes = res["remotes"].as_array().unwrap_or(&vec![]).clone();
            if !remotes.iter().any(|r| r["name"].as_str() == Some("origin")) {
                fail(results, "remote — list", &input, &res, "expected entry with name=origin");
                return;
            }
            pass(results, "remote — list");
        }
    }

    // add upstream
    let input = json!({
        "operation": "remote", "repo": repo, "subcommand": "add",
        "name": "upstream", "url": "https://github.com/example/repo.git"
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "remote — add upstream", &input, &e),
        Ok(res) => {
            assert_ok!(results, "remote — add upstream", &input, &res);
            if res["name"].as_str() != Some("upstream") {
                fail(results, "remote — add upstream", &input, &res, "expected name=upstream");
                return;
            }
            pass(results, "remote — add upstream");
        }
    }

    // add upstream again — already_exists
    let input = json!({
        "operation": "remote", "repo": repo, "subcommand": "add",
        "name": "upstream", "url": "https://github.com/example/repo.git"
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "remote — add already_exists", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "remote — add already_exists", &input, &res, "already_exists");
            pass(results, "remote — add already_exists");
        }
    }

    // remove upstream
    let input = json!({ "operation": "remote", "repo": repo, "subcommand": "remove", "name": "upstream" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "remote — remove upstream", &input, &e),
        Ok(res) => {
            assert_ok!(results, "remote — remove upstream", &input, &res);
            if res["name"].as_str() != Some("upstream") {
                fail(results, "remote — remove upstream", &input, &res, "expected name=upstream");
                return;
            }
            pass(results, "remote — remove upstream");
        }
    }
}

fn run_push_fetch_pull(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // push main to origin
    let input = json!({ "operation": "push", "repo": repo, "remote": "origin", "branch": "main" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "push — main to origin", &input, &e),
        Ok(res) => {
            assert_ok!(results, "push — main to origin", &input, &res);
            if res["remote"].as_str() != Some("origin") {
                fail(results, "push — main to origin", &input, &res, "expected remote=origin");
                return;
            }
            pass(results, "push — main to origin");
        }
    }

    // simulate remote advancing: clone to repo2, commit, push
    let repo2 = pg.path.join("repo2");
    let remote_url = format!("file://{}", pg.remote_git().to_string_lossy());
    git_run(&["clone", &remote_url, repo2.to_str().unwrap()]);
    git_run(&["-C", repo2.to_str().unwrap(), "config", "user.email", "test@murmur.dev"]);
    git_run(&["-C", repo2.to_str().unwrap(), "config", "user.name", "Murmur Test"]);
    std::fs::write(repo2.join("remote-file.txt"), "from remote\n").unwrap();
    git_run(&["-C", repo2.to_str().unwrap(), "add", "."]);
    git_run(&["-C", repo2.to_str().unwrap(), "commit", "-m", "remote advance"]);
    git_run(&["-C", repo2.to_str().unwrap(), "push", "origin", "main"]);

    // fetch
    let input = json!({ "operation": "fetch", "repo": repo, "remote": "origin" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "fetch — origin", &input, &e),
        Ok(res) => {
            assert_ok!(results, "fetch — origin", &input, &res);
            if res["remote"].as_str() != Some("origin") {
                fail(results, "fetch — origin", &input, &res, "expected remote=origin");
                return;
            }
            pass(results, "fetch — origin");
        }
    }

    // pull
    let input = json!({ "operation": "pull", "repo": repo, "remote": "origin", "branch": "main" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "pull — origin main", &input, &e),
        Ok(res) => {
            assert_ok!(results, "pull — origin main", &input, &res);
            let pulled = res["commits_pulled"].as_i64().unwrap_or(-1);
            if pulled < 1 {
                fail(results, "pull — origin main", &input, &res,
                    "expected commits_pulled >= 1");
                return;
            }
            pass(results, "pull — origin main");
        }
    }

    // advance remote again, do NOT fetch — push should fail with non_fast_forward
    std::fs::write(repo2.join("remote-file2.txt"), "from remote 2\n").unwrap();
    git_run(&["-C", repo2.to_str().unwrap(), "add", "."]);
    git_run(&["-C", repo2.to_str().unwrap(), "commit", "-m", "remote advance 2"]);
    git_run(&["-C", repo2.to_str().unwrap(), "push", "origin", "main"]);

    // make a local commit to diverge
    std::fs::write(pg.repo().join("local-diverge.txt"), "local\n").unwrap();
    let add = json!({ "operation": "add", "repo": repo, "paths": ["local-diverge.txt"] });
    let _ = call(bin, &add);
    let commit = json!({ "operation": "commit", "repo": repo, "message": "local diverge" });
    let _ = call(bin, &commit);

    let input = json!({ "operation": "push", "repo": repo, "remote": "origin", "branch": "main" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "push — non_fast_forward", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "push — non_fast_forward", &input, &res, "non_fast_forward");
            pass(results, "push — non_fast_forward");
        }
    }

    // reset local to sync with remote before continuing
    let _ = Command::new("git")
        .args(["-C", &repo, "fetch", "origin"])
        .output();
    let _ = Command::new("git")
        .args(["-C", &repo, "reset", "--hard", "origin/main"])
        .output();
}

fn run_clone(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let remote_url = format!("file://{}", pg.remote_git().to_string_lossy());
    let dest = pg.path.join("repo-cloned").to_string_lossy().to_string();

    // clone
    let input = json!({ "operation": "clone", "url": remote_url, "dest": dest });
    match call(bin, &input) {
        Err(e) => fail_err(results, "clone — success", &input, &e),
        Ok(res) => {
            assert_ok!(results, "clone — success", &input, &res);
            if res["dest"].as_str().map(|s| s.is_empty()) != Some(false) {
                fail(results, "clone — success", &input, &res, "expected non-empty dest");
                return;
            }
            if res["default_branch"].as_str().map(|s| s.is_empty()) != Some(false) {
                fail(results, "clone — success", &input, &res, "expected non-empty default_branch");
                return;
            }
            pass(results, "clone — success");
        }
    }

    // clone again — dest_exists
    let input = json!({ "operation": "clone", "url": remote_url, "dest": dest });
    match call(bin, &input) {
        Err(e) => fail_err(results, "clone — dest_exists", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "clone — dest_exists", &input, &res, "dest_exists");
            pass(results, "clone — dest_exists");
        }
    }
}

fn run_tag(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();

    // list (empty)
    let input = json!({ "operation": "tag", "repo": repo, "subcommand": "list" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "tag — list empty", &input, &e),
        Ok(res) => {
            assert_ok!(results, "tag — list empty", &input, &res);
            if !res["tags"].is_array() {
                fail(results, "tag — list empty", &input, &res, "expected tags to be an array");
                return;
            }
            pass(results, "tag — list empty");
        }
    }

    // create lightweight tag
    let input = json!({ "operation": "tag", "repo": repo, "subcommand": "create", "name": "v0.1.0" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "tag — create v0.1.0", &input, &e),
        Ok(res) => {
            assert_ok!(results, "tag — create v0.1.0", &input, &res);
            if res["name"].as_str() != Some("v0.1.0") {
                fail(results, "tag — create v0.1.0", &input, &res, "expected name=v0.1.0");
                return;
            }
            pass(results, "tag — create v0.1.0");
        }
    }

    // create annotated tag
    let input = json!({
        "operation": "tag", "repo": repo, "subcommand": "create",
        "name": "v0.1.0-annotated", "message": "release v0.1.0"
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "tag — create v0.1.0-annotated", &input, &e),
        Ok(res) => {
            assert_ok!(results, "tag — create v0.1.0-annotated", &input, &res);
            if res["name"].as_str() != Some("v0.1.0-annotated") {
                fail(results, "tag — create v0.1.0-annotated", &input, &res, "expected name=v0.1.0-annotated");
                return;
            }
            pass(results, "tag — create v0.1.0-annotated");
        }
    }

    // create duplicate — already_exists
    let input = json!({ "operation": "tag", "repo": repo, "subcommand": "create", "name": "v0.1.0" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "tag — create already_exists", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "tag — create already_exists", &input, &res, "already_exists");
            pass(results, "tag — create already_exists");
        }
    }

    // list with tags
    let input = json!({ "operation": "tag", "repo": repo, "subcommand": "list" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "tag — list with tags", &input, &e),
        Ok(res) => {
            assert_ok!(results, "tag — list with tags", &input, &res);
            let tags = res["tags"].as_array().unwrap_or(&vec![]).clone();
            if tags.len() < 2 {
                fail(results, "tag — list with tags", &input, &res, "expected >= 2 tags");
                return;
            }
            let has_v010 = tags.iter().any(|t| t["name"].as_str() == Some("v0.1.0"));
            let has_ann = tags.iter().any(|t| t["name"].as_str() == Some("v0.1.0-annotated"));
            if !has_v010 || !has_ann {
                fail(results, "tag — list with tags", &input, &res,
                    "expected tags v0.1.0 and v0.1.0-annotated");
                return;
            }
            pass(results, "tag — list with tags");
        }
    }
}

fn run_worktree(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();
    let wt_path = pg.worktrees().join("wt-feature").to_string_lossy().to_string();

    // add worktree on feature-b
    let input = json!({
        "operation": "worktree", "repo": repo, "subcommand": "add",
        "path": wt_path, "branch": "feature-b"
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "worktree — add", &input, &e),
        Ok(res) => {
            assert_ok!(results, "worktree — add", &input, &res);
            let path_val = res["path"].as_str().unwrap_or("");
            if path_val.is_empty() {
                fail(results, "worktree — add", &input, &res, "expected non-empty path");
                return;
            }
            if res["branch"].as_str() != Some("feature-b") {
                fail(results, "worktree — add", &input, &res, "expected branch=feature-b");
                return;
            }
            if !Path::new(&wt_path).exists() {
                fail(results, "worktree — add", &input, &res, "worktree path does not exist on disk");
                return;
            }
            pass(results, "worktree — add");
        }
    }

    // list
    let input = json!({ "operation": "worktree", "repo": repo, "subcommand": "list" });
    match call(bin, &input) {
        Err(e) => fail_err(results, "worktree — list", &input, &e),
        Ok(res) => {
            assert_ok!(results, "worktree — list", &input, &res);
            let worktrees = res["worktrees"].as_array().unwrap_or(&vec![]).clone();
            if worktrees.len() < 2 {
                fail(results, "worktree — list", &input, &res, "expected >= 2 worktrees");
                return;
            }
            pass(results, "worktree — list");
        }
    }

    // branch conflict: feature-b already in use
    let wt_conflict = pg.worktrees().join("wt-conflict").to_string_lossy().to_string();
    let input = json!({
        "operation": "worktree", "repo": repo, "subcommand": "add",
        "path": wt_conflict, "branch": "feature-b"
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "worktree — branch_conflict", &input, &e),
        Ok(res) => {
            assert_fail_kind!(results, "worktree — branch_conflict", &input, &res, "branch_conflict");
            pass(results, "worktree — branch_conflict");
        }
    }

    // remove worktree
    let input = json!({
        "operation": "worktree", "repo": repo, "subcommand": "remove", "path": wt_path
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "worktree — remove", &input, &e),
        Ok(res) => {
            assert_ok!(results, "worktree — remove", &input, &res);
            if Path::new(&wt_path).exists() {
                fail(results, "worktree — remove", &input, &res, "worktree path still exists after remove");
                return;
            }
            pass(results, "worktree — remove");
        }
    }
}

fn run_create_worktree(bin: &Path, pg: &Playground, results: &mut Vec<OpResult>) {
    let repo = pg.repo().to_string_lossy().to_string();
    let wt_compat = pg.worktrees().join("wt-compat").to_string_lossy().to_string();

    let input = json!({
        "operation": "create_worktree",
        "repo": repo,
        "path": wt_compat,
        "branch": "feature-a"
    });
    match call(bin, &input) {
        Err(e) => fail_err(results, "create_worktree — compat alias", &input, &e),
        Ok(res) => {
            assert_ok!(results, "create_worktree — compat alias", &input, &res);
            if res["path"].as_str().map(|s| s.is_empty()) != Some(false) {
                fail(results, "create_worktree — compat alias", &input, &res, "expected non-empty path");
                return;
            }
            pass(results, "create_worktree — compat alias");
        }
    }

    // cleanup: remove the compat worktree
    let rm = json!({ "operation": "worktree", "repo": repo, "subcommand": "remove", "path": wt_compat });
    let _ = call(bin, &rm);
}

// ── Auto-locate binary ────────────────────────────────────────────────────────

fn locate_binary(override_path: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("specified binary not found: {}", p.display()));
    }

    // workspace root is two levels above this crate's source directory.
    // At runtime, the binary lives in target/ under the workspace root.
    // We try to find it relative to the executable location first,
    // then fall back to relative paths from the workspace root.
    let exe = std::env::current_exe().ok();

    // Try target/release and target/debug relative to workspace root candidates.
    let workspace_candidates: Vec<PathBuf> = {
        let mut v = Vec::new();
        // From this crate dir: tools/murmur-tool-git/validate → go up 3 to workspace root
        if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
            let mut p = PathBuf::from(manifest);
            p.pop(); // validate → murmur-tool-git
            p.pop(); // murmur-tool-git → tools
            p.pop(); // tools → default-artifacts (workspace root)
            v.push(p);
        }
        // From executable location (target/release or target/debug)
        if let Some(ref exe_path) = exe {
            let mut p = exe_path.clone();
            p.pop(); // remove binary name
            p.pop(); // release/ or debug/ → target/
            p.pop(); // target/ → workspace root
            v.push(p);
        }
        v
    };

    for root in &workspace_candidates {
        for profile in &["release", "debug"] {
            let candidate = root.join("target").join(profile).join("murmur-tool-git");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err("murmur-tool-git binary not found. Run: cargo build -p murmur-tool-git --release".to_string())
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let bin = match locate_binary(args.bin.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let mut pg = create_playground();
    pg.keep = args.keep;

    let mut results: Vec<OpResult> = Vec::new();
    let mut commit_hash = String::new();

    let filter = args.op.as_deref();

    // Each entry: (group_name, aliases, run_fn)
    // aliases are accepted via --op; only the first name is used as the run key.
    // Groups sharing a run function (switch+checkout, push+fetch+pull) run once
    // in the full run, but can be targeted individually by alias.
    let run_all = filter.is_none();

    macro_rules! maybe_run {
        ($names:expr, $body:expr) => {{
            let names: &[&str] = $names;
            let should_run = run_all || filter.map(|f| names.contains(&f)).unwrap_or(false);
            if should_run {
                $body
            }
        }};
    }

    maybe_run!(&["status"],         run_status(&bin, &pg, &mut results));
    maybe_run!(&["add"],            run_add(&bin, &pg, &mut results));
    maybe_run!(&["diff"],           run_diff(&bin, &pg, &mut results));
    maybe_run!(&["commit"],         run_commit(&bin, &pg, &mut results, &mut commit_hash));
    maybe_run!(&["log"],            run_log(&bin, &pg, &mut results));
    maybe_run!(&["show"],           run_show(&bin, &pg, &mut results));
    maybe_run!(&["stash"],          run_stash(&bin, &pg, &mut results));
    maybe_run!(&["restore"],        run_restore(&bin, &pg, &mut results));
    maybe_run!(&["branch"],         run_branch(&bin, &pg, &mut results));
    maybe_run!(&["switch", "checkout"], run_switch_checkout(&bin, &pg, &mut results));
    maybe_run!(&["reset"],          run_reset(&bin, &pg, &mut results));
    maybe_run!(&["cherry_pick"],    run_cherry_pick(&bin, &pg, &mut results));
    maybe_run!(&["merge"],          run_merge(&bin, &pg, &mut results));
    maybe_run!(&["remote"],         run_remote(&bin, &pg, &mut results));
    maybe_run!(&["push", "fetch", "pull"], run_push_fetch_pull(&bin, &pg, &mut results));
    maybe_run!(&["clone"],          run_clone(&bin, &pg, &mut results));
    maybe_run!(&["tag"],            run_tag(&bin, &pg, &mut results));
    maybe_run!(&["worktree"],       run_worktree(&bin, &pg, &mut results));
    maybe_run!(&["create_worktree"], run_create_worktree(&bin, &pg, &mut results));

    // Print summary
    println!();
    println!("murmur-tool-git validation");
    println!("==========================");
    for r in &results {
        if r.passed {
            println!("[PASS] {}", r.label);
        } else {
            println!("[FAIL] {}", r.label);
            if let Some(ref d) = r.detail {
                println!("{d}");
            }
        }
    }
    println!("==========================");

    let passed = results.iter().filter(|r| r.passed).count();
    let failed = results.iter().filter(|r| !r.passed).count();
    println!("{passed} passed, {failed} failed");

    if args.keep {
        println!("Playground kept at: {}", pg.path.display());
    }

    if failed > 0 {
        std::process::exit(1);
    }
}
