//! Host-target scenario tests proving `push` works from a shallow clone (`git clone --depth 1`),
//! the workspace this tool is meant to run in: a commit made on top of one lands on the
//! identical SHA, exactly as the same commit does from a full clone.
//!
//! Unlike the checked-in fixtures under `tests/fixtures/`, these repositories are built at test
//! time by the system `git` CLI, because only `git clone` itself produces a shallow clone's
//! real shape — a `.git/shallow` file, one pack holding the tip's tree and nothing behind it.
//! This crate has no `git` dependency and cannot build a pack itself. A missing `git` is a
//! failure, not a skip. Every `git` call runs against a scratch `HOME` with system and global
//! config disabled, signing off, and a fixed identity and date per commit, so every SHA is
//! deterministic.
//!
//! `GithubEcho` here is a trimmed copy of `tests/ops_scenarios.rs`'s: each test file is a
//! separate compilation unit against the compiled rlib.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use tempfile::TempDir;

use tool_github::ops::{self, HttpReply, Response, Transport};

// ── the system git, isolated ─────────────────────────────────────────────────

/// The system `git`, run with a scratch `HOME` and no system or global config.
struct Git {
    home: TempDir,
}

/// One commit's identity and date, fixed so its SHA is the same on every run and in every
/// clone.
const IDENTITY_NAME: &str = "Test";
const IDENTITY_EMAIL: &str = "test@example.com";

impl Git {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ]);
        command
    }

    /// Run `git <args>` in `dir`; panic with its output unless it succeeds. Returns stdout,
    /// trimmed.
    fn run(&self, dir: &Path, args: &[&str]) -> String {
        self.run_with_date(dir, args, None)
    }

    fn run_with_date(&self, dir: &Path, args: &[&str], date: Option<&str>) -> String {
        let mut command = self.command(dir);
        command.args(args);
        if let Some(date) = date {
            command
                .env("GIT_AUTHOR_NAME", IDENTITY_NAME)
                .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
                .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL)
                .env("GIT_COMMITTER_DATE", date);
        }
        let output = command.output().unwrap_or_else(|err| {
            panic!(
                "could not run the system `git` ({err}). These tests build their shallow and full \
                 clones with the git CLI at test time, so a missing git is a failure, not a skip."
            )
        });
        assert!(
            output.status.success(),
            "git {args:?} in {} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            dir.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Whether `git <args>` in `dir` exits zero.
    fn succeeds(&self, dir: &Path, args: &[&str]) -> bool {
        self.command(dir)
            .args(args)
            .output()
            .expect("spawn git")
            .status
            .success()
    }

    /// Commit everything staged in `dir` as `message` at `date`; return the new commit's SHA.
    fn commit(&self, dir: &Path, message: &str, date: &str) -> String {
        self.run_with_date(dir, &["commit", "-q", "-m", message], Some(date));
        self.run(dir, &["rev-parse", "HEAD"])
    }
}

// ── the fixture repositories ─────────────────────────────────────────────────

/// The source repository: three commits on `main`, each changing `a.txt`, the second also
/// adding `dir/b.txt` so the history carries a subtree.
struct Source {
    dir: TempDir,
    c1: String,
    c2: String,
    c3: String,
}

fn build_source(git: &Git) -> Source {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git.run(root, &["init", "-q"]);

    fs::write(root.join("a.txt"), "one\n").unwrap();
    git.run(root, &["add", "a.txt"]);
    let c1 = git.commit(root, "c1", "1600000000 +0000");

    fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    fs::create_dir(root.join("dir")).unwrap();
    fs::write(root.join("dir/b.txt"), "nested\n").unwrap();
    git.run(root, &["add", "a.txt", "dir/b.txt"]);
    let c2 = git.commit(root, "c2", "1600000100 +0000");

    fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    git.run(root, &["add", "a.txt"]);
    let c3 = git.commit(root, "c3", "1600000200 +0000");

    Source { dir, c1, c2, c3 }
}

/// `git clone [--depth 1] file://<source> <tempdir>/clone`. The tempdir is returned so the
/// clone outlives this call.
fn clone(git: &Git, source: &Source, depth: Option<u32>) -> (TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let url = format!("file://{}", source.dir.path().display());
    let depth = depth.map(|d| d.to_string());
    let mut args = vec!["clone", "-q"];
    if let Some(depth) = depth.as_deref() {
        args.extend(["--depth", depth]);
    }
    args.extend([url.as_str(), "clone"]);
    git.run(parent.path(), &args);
    let clone = parent.path().join("clone");
    (parent, clone)
}

/// Commit `c4` on top of a clone's `main`: `a.txt` changed again, same content, identity and
/// date in every clone, so its SHA is the same wherever it is made.
fn commit_c4(git: &Git, clone: &Path) -> String {
    fs::write(clone.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    git.run(clone, &["add", "a.txt"]);
    git.commit(clone, "c4", "1600000300 +0000")
}

fn assert_is_a_shallow_clone_with_a_pack(git: &Git, clone: &Path, source: &Source) {
    let shallow = fs::read_to_string(clone.join(".git/shallow"))
        .unwrap_or_else(|err| panic!("{}/.git/shallow must exist: {err}", clone.display()));
    assert!(
        shallow.contains(&source.c3),
        "the shallow boundary is c3: {shallow}"
    );
    assert!(
        !git.succeeds(clone, &["cat-file", "-e", &source.c2]),
        "c2 must be absent from the shallow clone"
    );
    let pack_dir = clone.join(".git/objects/pack");
    let paired = fs::read_dir(&pack_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .filter(|pack| pack.with_extension("idx").is_file())
        .count();
    assert!(
        paired >= 1,
        "{} must hold a .pack with a matching .idx",
        pack_dir.display()
    );
}

// ── a fake GitHub that recomputes real git ids ───────────────────────────────

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: Option<Value>,
}

/// A fake GitHub that recomputes real git object hashes for every blob, tree and commit it is
/// asked to create, answers `GET …/git/commits/<sha>` from the commits it holds, and keeps
/// branch refs — so a passing push proves the request bodies byte-reproduce the clone's real,
/// `git`-computed history.
struct GithubEcho {
    refs: RefCell<HashMap<String, String>>,
    /// Commits the fake repository holds: those created through it, plus any seeded.
    commits: RefCell<HashSet<String>>,
    requests: RefCell<Vec<Recorded>>,
}

impl GithubEcho {
    fn new(commits: &[&str], refs: &[(&str, &str)]) -> Self {
        Self {
            refs: RefCell::new(
                refs.iter()
                    .map(|(b, s)| (b.to_string(), s.to_string()))
                    .collect(),
            ),
            commits: RefCell::new(commits.iter().map(|s| s.to_string()).collect()),
            requests: RefCell::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.borrow().clone()
    }

    fn posts_to(&self, suffix: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.ends_with(suffix))
            .collect()
    }
}

fn hex_sha1(store: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(store);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn object_sha(kind: &str, content: &[u8]) -> String {
    let header = format!("{kind} {}\0", content.len());
    let mut store = header.into_bytes();
    store.extend_from_slice(content);
    hex_sha1(&store)
}

fn base64_decode(text: &str) -> Vec<u8> {
    const TABLE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let clean: Vec<u8> = text
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let values: Vec<u32> = chunk
            .iter()
            .map(|&b| TABLE.find(b as char).unwrap() as u32)
            .collect();
        let n = values
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, v)| acc | (v << (18 - 6 * i)));
        out.push((n >> 16) as u8);
        if values.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if values.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn hex_decode(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn identity_tail(name: &str, email: &str, date: &str) -> String {
    let year: i64 = date[0..4].parse().unwrap();
    let month: i64 = date[5..7].parse().unwrap();
    let day: i64 = date[8..10].parse().unwrap();
    let hour: i64 = date[11..13].parse().unwrap();
    let minute: i64 = date[14..16].parse().unwrap();
    let second: i64 = date[17..19].parse().unwrap();
    let sign = &date[19..20];
    let oh: i64 = date[20..22].parse().unwrap();
    let om: i64 = date[23..25].parse().unwrap();
    let offset_minutes = if sign == "-" {
        -(oh * 60 + om)
    } else {
        oh * 60 + om
    };
    let days = days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    let seconds = local_seconds - offset_minutes * 60;
    format!("{name} <{email}> {seconds} {sign}{oh:02}{om:02}")
}

impl Transport for GithubEcho {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<HttpReply, String> {
        self.requests.borrow_mut().push(Recorded {
            method: method.to_string(),
            path: path.to_string(),
            body: body.cloned(),
        });
        let reply = |status: u16, body: Value| HttpReply {
            status,
            location: None,
            body: body.to_string().into_bytes(),
        };

        if path.ends_with("/git/blobs") {
            let content = base64_decode(body.unwrap()["content"].as_str().unwrap());
            return Ok(reply(201, json!({"sha": object_sha("blob", &content)})));
        }
        if path.ends_with("/git/trees") {
            let mut content = Vec::new();
            for entry in body.unwrap()["tree"].as_array().unwrap() {
                // Like GitHub: only six-digit modes are accepted, and a directory is stored as `40000`.
                let mode = match entry["mode"].as_str().unwrap() {
                    "040000" => "40000",
                    mode @ ("100644" | "100755" | "120000" | "160000") => mode,
                    _ => {
                        return Ok(reply(
                            422,
                            json!({"message": "Must supply a valid tree.mode"}),
                        ))
                    }
                };
                let name = entry["path"].as_str().unwrap();
                let sha = entry["sha"].as_str().unwrap();
                content.extend_from_slice(format!("{mode} {name}\0").as_bytes());
                content.extend_from_slice(&hex_decode(sha));
            }
            return Ok(reply(201, json!({"sha": object_sha("tree", &content)})));
        }
        if path.ends_with("/git/commits") && method == "POST" {
            let body = body.unwrap();
            let mut header = format!("tree {}\n", body["tree"].as_str().unwrap());
            for parent in body["parents"].as_array().unwrap() {
                header.push_str(&format!("parent {}\n", parent.as_str().unwrap()));
            }
            let author = &body["author"];
            let committer = &body["committer"];
            header.push_str(&format!(
                "author {}\n",
                identity_tail(
                    author["name"].as_str().unwrap(),
                    author["email"].as_str().unwrap(),
                    author["date"].as_str().unwrap()
                )
            ));
            header.push_str(&format!(
                "committer {}\n",
                identity_tail(
                    committer["name"].as_str().unwrap(),
                    committer["email"].as_str().unwrap(),
                    committer["date"].as_str().unwrap()
                )
            ));
            let content = format!("{header}\n{}", body["message"].as_str().unwrap());
            let sha = object_sha("commit", content.as_bytes());
            self.commits.borrow_mut().insert(sha.clone());
            return Ok(reply(201, json!({"sha": sha})));
        }
        if let Some(sha) = path.strip_prefix("/repos/acme/widgets/git/commits/") {
            return Ok(if self.commits.borrow().contains(sha) {
                reply(200, json!({"sha": sha}))
            } else {
                reply(404, json!({"message": "Not Found"}))
            });
        }
        if path.contains("/git/refs/heads/") && method == "GET" {
            let branch = path.rsplit('/').next().unwrap();
            return Ok(match self.refs.borrow().get(branch) {
                Some(sha) => reply(200, json!({"object": {"sha": sha}})),
                None => reply(404, json!({"message": "not found"})),
            });
        }
        if path.ends_with("/git/refs") && method == "POST" {
            let body = body.unwrap();
            let ref_name = body["ref"].as_str().unwrap();
            let branch = ref_name.strip_prefix("refs/heads/").unwrap();
            let sha = body["sha"].as_str().unwrap().to_string();
            self.refs
                .borrow_mut()
                .insert(branch.to_string(), sha.clone());
            return Ok(reply(201, json!({"ref": ref_name, "object": {"sha": sha}})));
        }
        if path == "/repos/acme/widgets" {
            return Ok(reply(200, json!({"default_branch": "main"})));
        }
        Ok(reply(
            404,
            json!({"message": format!("unhandled path {path}")}),
        ))
    }
}

const CONFIG: &str = r#"{"config_version":1,"repo":"acme/widgets"}"#;

fn push(fake: &mut GithubEcho, clone: &Path, head_sha: &str, remote_branch: &str) -> Value {
    let input = json!({"op": "push", "head_sha": head_sha, "remote_branch": remote_branch});
    let response: Response = ops::run(
        Some("http://gateway.invalid"),
        Some(CONFIG),
        &input.to_string(),
        fake,
        clone,
    );
    serde_json::from_str(&response.data).unwrap()
}

/// A fake that already holds the source's whole history, as GitHub would once the source was
/// pushed: `{c1, c2, c3}` with `main` at `c3`.
fn github_holding(source: &Source) -> GithubEcho {
    GithubEcho::new(
        &[&source.c1, &source.c2, &source.c3],
        &[("main", &source.c3)],
    )
}

// ── scenarios ────────────────────────────────────────────────────────────────

#[test]
fn a_commit_on_a_shallow_clone_pushes_and_lands_the_identical_sha() {
    let git = Git::new();
    let source = build_source(&git);
    let (_keep, clone) = clone(&git, &source, Some(1));
    assert_is_a_shallow_clone_with_a_pack(&git, &clone, &source);
    let c4 = commit_c4(&git, &clone);

    let mut fake = github_holding(&source);
    let value = push(&mut fake, &clone, &c4, "feature-shallow");

    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["created"], json!(true), "{value}");
    assert_eq!(value["head_sha"], json!(c4), "{value}");
    assert_eq!(value["objects_uploaded"], json!(3), "{value}");
    assert_eq!(fake.refs.borrow().get("feature-shallow"), Some(&c4));

    let commits = fake.posts_to("/git/commits");
    assert_eq!(commits.len(), 1, "exactly one commit created: {commits:?}");
    let trees = fake.posts_to("/git/trees");
    let blobs = fake.posts_to("/git/blobs");
    assert_eq!(
        trees.len(),
        1,
        "only the root tree; dir/ is unchanged since c3: {trees:?}"
    );
    assert_eq!(blobs.len(), 1, "only the new a.txt: {blobs:?}");
    let blob = base64_decode(blobs[0].body.as_ref().unwrap()["content"].as_str().unwrap());
    assert_eq!(blob, b"one\ntwo\nthree\nfour\n");
    assert_eq!(
        commits[0].body.as_ref().unwrap()["tree"],
        json!(git.run(&clone, &["rev-parse", "HEAD^{tree}"]))
    );
    assert_eq!(
        commits[0].body.as_ref().unwrap()["parents"],
        json!([source.c3])
    );
}

#[test]
fn the_same_push_from_a_full_clone_lands_the_same_sha() {
    let git = Git::new();
    let source = build_source(&git);

    let (_keep_shallow, shallow) = clone(&git, &source, Some(1));
    let shallow_c4 = commit_c4(&git, &shallow);

    let (_keep_full, full) = clone(&git, &source, None);
    assert!(
        !full.join(".git/shallow").exists(),
        "a full clone has no .git/shallow"
    );
    assert!(
        git.succeeds(&full, &["cat-file", "-e", &source.c1]),
        "a full clone holds c1"
    );
    let c4 = commit_c4(&git, &full);
    assert_eq!(
        c4, shallow_c4,
        "c4 is the same commit whichever clone it is made in"
    );

    let mut fake = github_holding(&source);
    let value = push(&mut fake, &full, &c4, "feature-full");

    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["objects_uploaded"], json!(3), "{value}");
    assert_eq!(fake.refs.borrow().get("feature-full"), Some(&c4));
}

#[test]
fn a_shallow_tip_whose_parent_github_lacks_is_repo_unreadable() {
    let git = Git::new();
    let source = build_source(&git);
    let (_keep, clone) = clone(&git, &source, Some(1));
    assert_is_a_shallow_clone_with_a_pack(&git, &clone, &source);

    let mut fake = GithubEcho::new(&[], &[]);
    let value = push(&mut fake, &clone, &source.c3, "fresh");

    assert_eq!(value["error_kind"], json!("repo_unreadable"), "{value}");
    let message = value["message"].as_str().unwrap();
    assert!(
        message.contains(&source.c2),
        "the message names the missing parent c2: {message}"
    );
    let posts: Vec<Recorded> = fake
        .requests()
        .into_iter()
        .filter(|r| r.method != "GET")
        .collect();
    assert!(posts.is_empty(), "nothing may be uploaded: {posts:?}");
}
