//! The parity fixture: one deterministic work folder that [`super::sequence`] runs every
//! operation against.
//!
//! - `repo/`: branch `main` with a local identity, holding `README.md` and `src/lib.rs` (`fn
//!   sym()`), and the branches `feature`, `topic`, `conflict` and `side`. Its `origin` is
//!   `../remote.git`, which is one commit ahead on `main` (`UPSTREAM.md`) and holds a `side` that
//!   has diverged from the local one. `README.md` is modified and `NEW.md` untracked.
//! - `remote.git/`: bare.
//! - `repo/.murmur/code-graph.db`: code-graph's schema, holding `sym` at `src/lib.rs` 1–4,
//!   excluded from git through `.git/info/exclude`.
//!
//! Every fixture commit has its own fixed author and committer date, and every fixture git
//! command runs with no global or system config, so the fixture's hashes are the same on every
//! run.

use std::{fs, path::Path, process::Command};

use super::{build_graph_db, scratch, sequence::SID};

/// Run `git <args>` in `dir` for the fixture, with no global or system config; `date` fixes the
/// author and committer date of any commit it makes.
fn g(dir: &Path, date: u32, args: &[&str]) {
    let home = scratch("fixture-home");
    let stamp = format!("2026-01-01T00:00:{date:02}+00:00");
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", &stamp)
        .env("GIT_COMMITTER_DATE", &stamp)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "fixture: git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(home);
}

fn commit(dir: &Path, date: u32, file: &str, body: &str, subject: &str) {
    let path = dir.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    g(dir, date, &["add", "--all"]);
    g(dir, date, &["commit", "-q", "-m", subject]);
}

/// Build the fixture in the empty folder `work`.
pub fn build(work: &Path) {
    let repo = work.join("repo");
    let remote = work.join("remote.git");
    fs::create_dir_all(&repo).unwrap();
    g(
        work,
        0,
        &["init", "-q", "--bare", "-b", "main", "remote.git"],
    );
    g(&repo, 0, &["init", "-q", "-b", "main"]);
    g(&repo, 0, &["config", "user.name", "Test"]);
    g(&repo, 0, &["config", "user.email", "test@example.com"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    commit(
        &repo,
        1,
        "src/lib.rs",
        "fn sym() {\n    let x = 1;\n    x\n}\n",
        "initial",
    );
    g(&repo, 1, &["remote", "add", "origin", "../remote.git"]);
    g(&repo, 1, &["push", "-q", "-u", "origin", "main"]);

    for (date, branch, file, body, subject) in [
        (2, "feature", "FEATURE.md", "feature\n", "feature change"),
        (3, "topic", "TOPIC.md", "topic\n", "topic change"),
        (
            4,
            "conflict",
            "UPSTREAM.md",
            "conflict\n",
            "conflicting change",
        ),
        (5, "side", "SIDE.md", "local\n", "local side"),
    ] {
        g(&repo, date, &["checkout", "-q", "-b", branch, "main"]);
        commit(&repo, date, file, body, subject);
    }
    g(&repo, 5, &["checkout", "-q", "main"]);

    // Another clone, outside the work folder, moves the remote ahead.
    let seed_parent = scratch("fixture-seed");
    g(
        &seed_parent,
        6,
        &["clone", "-q", remote.to_str().unwrap(), "seed"],
    );
    let seed = seed_parent.join("seed");
    commit(&seed, 6, "UPSTREAM.md", "upstream\n", "upstream change");
    g(&seed, 6, &["push", "-q", "origin", "main"]);
    g(&seed, 7, &["checkout", "-q", "-b", "side", "main~1"]);
    commit(&seed, 7, "SIDE.md", "remote\n", "remote side");
    g(&seed, 7, &["push", "-q", "origin", "side"]);
    let _ = fs::remove_dir_all(seed_parent);

    let exclude = repo.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str(".murmur/\n");
    fs::write(exclude, text).unwrap();
    fs::write(repo.join("README.md"), "hello\nchanged\n").unwrap();
    fs::write(repo.join("NEW.md"), "new\n").unwrap();
    build_graph_db(&repo, &[(SID, "src/lib.rs", 1, 4)]);
}
