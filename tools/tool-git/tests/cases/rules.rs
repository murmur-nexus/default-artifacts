// What murmur's `run` adds around each git command, and where the tool looks at files itself.
// Compiled once per backend (see tests/common/mod.rs).
//
// * Refusals: a program outside `capabilities.shell.allow`, an `on-shell` deny hook, and a
//   refusal on a later command of the same call, each ending the call with
//   `tool-git: murmur refused to run git: <rule>: <reason>` and running nothing more.
// * `failed(text)` reads as the native runner's spawn error; a time-out, as either the error or
//   a killed run, names the subcommand.
// * Output caps: a stream cut at 1 MiB keeps complete lines only, and the result says so.
// * `symbol_history`'s database: found relative, discovered, absolute in the work folder or
//   under a host folder, refused outside them; missing, WAL and corrupt databases; never
//   written.
// * `clone`'s existing destination, and `log_path`.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use common::{build_graph_db, git_ok, process::Inject, Env, Sym, TestDir};
use serde_json::{json, Value};

const SID: &str = "rust://fixture/#sym(->())";

fn env(work: &Path) -> Env {
    Env::new(BACKEND, work)
}

/// A repository at `dir` with a local identity and one commit of `README.md` and a 4-line
/// `src/lib.rs` holding `sym`.
fn init_repo(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    let d = dir.to_str().unwrap();
    git_ok(&["init", "-q", "-b", "main", d]);
    git_ok(&["-C", d, "config", "user.email", "test@example.com"]);
    git_ok(&["-C", d, "config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    fs::write(dir.join("src/lib.rs"), "fn sym() {\n    let x = 1;\n    x\n}\n").unwrap();
    git_ok(&["-C", d, "add", "--all"]);
    git_ok(&["-C", d, "commit", "-q", "-m", "initial"]);
}

fn indexed_repo(dir: &Path) {
    init_repo(dir);
    let symbols: [Sym; 1] = [(SID, "src/lib.rs", 1, 4)];
    build_graph_db(dir, &symbols);
}

fn history(env: &Env, repo: Option<&str>) -> Value {
    let mut op = json!({"operation": "symbol_history", "symbol_id": SID});
    if let Some(repo) = repo {
        op["repo"] = json!(repo);
    }
    env.call(op)
}

fn assert_resolved(res: &Value) {
    assert_eq!(res["status"], "passed", "{res}");
    assert_eq!(res["summary"], format!("1 commit(s) for {SID}"));
    assert_eq!(res["data"]["file"], "src/lib.rs");
    assert_eq!(res["data"]["commits"][0]["subject"], "initial");
}

/// Every file under `dir` with its bytes and mtime.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(snapshot(&path));
        } else {
            let meta = fs::metadata(&path).unwrap();
            out.insert(path.clone(), (fs::read(&path).unwrap(), meta.modified().unwrap()));
        }
    }
    out
}

fn refusal(rule: &str, reason: &str) -> String {
    format!("tool-git: murmur refused to run git: {rule}: {reason}")
}

// ── refusals ────────────────────────────────────────────────────────────────

#[test]
fn git_outside_shell_allow_is_refused_and_nothing_runs() {
    let td = TestDir::new("rules");
    init_repo(&td.path().join("repo"));
    let env = env(td.path()).shell_allow(&[]);
    let res = env.call(json!({"operation": "status", "repo": "repo"}));
    assert_eq!(res["status"], "error");
    assert_eq!(
        res["summary"],
        refusal(
            "capabilities.shell.allow",
            "binary 'git' is not in capabilities.shell.allow"
        )
    );
    assert_eq!(res["data"], Value::Null);
    env.assert_kind(&res, Some("refused"));
    let records = env.records();
    assert_eq!(records.len(), 1);
    assert!(!records[0].ran, "no git ran");
}

#[test]
fn an_on_shell_refusal_of_a_force_push_ends_the_call() {
    let td = TestDir::new("rules");
    let repo = td.path().join("repo");
    init_repo(&repo);
    git_ok(&["init", "-q", "--bare", td.path().join("remote.git").to_str().unwrap()]);
    let r = repo.to_str().unwrap();
    git_ok(&["-C", r, "remote", "add", "origin", "../remote.git"]);
    let env = env(td.path());
    env.inject(
        |args| args.iter().any(|a| a == "push") && args.iter().any(|a| a == "--force"),
        Inject::on_shell("no force push"),
    );
    let res = env.call(json!({"operation": "push", "repo": "repo", "branch": "main", "force": true}));
    assert_eq!(res["status"], "error");
    assert_eq!(res["summary"], refusal("on-shell", "no force push"));
    env.assert_kind(&res, Some("refused"));
    assert!(env.records().iter().all(|r| !r.ran));
    let refs = git_ok(&["-C", td.path().join("remote.git").to_str().unwrap(), "for-each-ref"]);
    assert!(refs.is_empty(), "the remote is unchanged: {refs}");

    // The same push without force is not refused.
    let res = env.call(json!({"operation": "push", "repo": "repo", "branch": "main"}));
    assert_eq!(res["status"], "passed", "{res}");
}

#[test]
fn a_refusal_on_a_later_command_ends_the_call_there() {
    let td = TestDir::new("rules");
    let repo = td.path().join("repo");
    init_repo(&repo);
    fs::write(repo.join("NEW.md"), "new\n").unwrap();
    git_ok(&["-C", repo.to_str().unwrap(), "add", "NEW.md"]);
    let env = env(td.path());
    env.inject(
        |args| args.iter().any(|a| a == "rev-parse"),
        Inject::Refuse {
            rule: "capabilities.filesystem.read_only".into(),
            reason: "repo is read-only".into(),
        },
    );
    let res = env.call(json!({"operation": "commit", "repo": "repo", "message": "add new"}));
    assert_eq!(res["status"], "error");
    assert_eq!(
        res["summary"],
        refusal("capabilities.filesystem.read_only", "repo is read-only")
    );
    env.assert_kind(&res, Some("refused"));
    let records = env.records();
    let argv: Vec<_> = records.iter().map(|r| (r.request.args.clone(), r.ran)).collect();
    assert_eq!(argv.len(), 2, "commit ran, rev-parse was refused, nothing after: {argv:?}");
    assert_eq!(argv[0].0[2], "commit");
    assert!(argv[0].1);
    assert_eq!(argv[1].0[2], "rev-parse");
    assert!(!argv[1].1);
}

// ── failed and timed out ────────────────────────────────────────────────────

#[test]
fn a_failed_start_reads_as_the_native_spawn_error() {
    let td = TestDir::new("rules");
    init_repo(&td.path().join("repo"));
    let env = env(td.path());
    env.inject(|_| true, Inject::Fail("No such file or directory (os error 2)".into()));
    let res = env.call(json!({"operation": "status", "repo": "repo"}));
    assert_eq!(res["status"], "error");
    assert_eq!(
        res["summary"],
        "git status failed: failed to execute git: No such file or directory (os error 2)"
    );
    env.assert_kind(&res, None);
}

#[test]
fn a_time_out_names_the_subcommand() {
    let td = TestDir::new("rules");
    init_repo(&td.path().join("repo"));
    let env = env(td.path());
    env.inject(|args| args.iter().any(|a| a == "fetch"), Inject::TimeOut);
    env.inject(|args| args.iter().any(|a| a == "log"), Inject::KilledAtDeadline);
    let res = env.call(json!({"operation": "fetch", "repo": "repo"}));
    assert_eq!(res["status"], "error");
    assert_eq!(res["summary"], "git fetch timed out");
    env.assert_kind(&res, Some("timed_out"));
    let res = env.call(json!({"operation": "log", "repo": "repo"}));
    assert_eq!(res["summary"], "git log timed out");
    env.assert_kind(&res, Some("timed_out"));
}

// ── output caps ─────────────────────────────────────────────────────────────

#[test]
fn a_diff_over_1_mib_passes_cut_at_a_line() {
    let td = TestDir::new("rules");
    let repo = td.path().join("repo");
    init_repo(&repo);
    let line = "0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz\n";
    fs::write(repo.join("README.md"), line.repeat(20_000)).unwrap();
    let env = env(td.path());
    let res = env.call(json!({"operation": "diff", "repo": "repo"}));
    assert_eq!(res["status"], "passed", "{}", res["summary"]);
    assert_eq!(res["summary"], "diff complete; git output cut at 1 MiB");
    assert_eq!(res["truncated"], true);
    let diff = res["data"]["diff"].as_str().unwrap();
    assert!(diff.len() <= 1 << 20 && diff.len() > (1 << 20) - 200, "{}", diff.len());
    assert!(diff.starts_with("diff --git a/README.md b/README.md"));
    assert_eq!(diff.lines().last().unwrap(), format!("+{}", line.trim_end()));
}

#[test]
fn cut_listings_parse_only_complete_lines() {
    let td = TestDir::new("rules");
    let repo = td.path().join("repo");
    init_repo(&repo);
    let r = repo.to_str().unwrap();
    for i in 0..6 {
        git_ok(&["-C", r, "commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
        git_ok(&["-C", r, "branch", &format!("b{i}")]);
        fs::write(repo.join(format!("untracked-{i}.txt")), "x").unwrap();
    }
    let env = env(td.path());
    env.inject(|args| args.iter().any(|a| a == "log"), Inject::CapAt(300));
    env.inject(|args| args.iter().any(|a| a == "status"), Inject::CapAt(70));
    env.inject(|args| args.iter().any(|a| a == "branch"), Inject::CapAt(40));

    let log = env.call(json!({"operation": "log", "repo": "repo"}));
    assert_eq!(log["truncated"], true);
    let commits = log["data"]["commits"].as_array().unwrap();
    assert!(!commits.is_empty() && commits.len() < 7, "{log}");
    for c in commits {
        assert_eq!(c["hash"].as_str().unwrap().len(), 40);
        assert!(c["subject"].as_str().unwrap().starts_with('c'), "{c}");
    }
    assert_eq!(
        log["summary"],
        format!("{} commit(s); git output cut at 1 MiB", commits.len())
    );

    let status = env.call(json!({"operation": "status", "repo": "repo"}));
    let entries = status["data"]["entries"].as_array().unwrap();
    assert!(!entries.is_empty() && entries.len() < 6, "{status}");
    for e in entries {
        let path = e["path"].as_str().unwrap();
        assert!(repo.join(path).exists(), "a complete path: {path}");
    }

    let branches = env.call(json!({"operation": "branch", "repo": "repo", "subcommand": "list"}));
    let names: Vec<&str> = branches["data"]["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert!(!names.is_empty() && names.len() < 7, "{branches}");
    for name in names {
        assert!(name == "main" || name.len() == 2, "a complete name: {name}");
    }
    assert_eq!(branches["truncated"], true);
}

// ── symbol_history's database ───────────────────────────────────────────────

#[test]
fn an_omitted_repo_resolves_when_the_work_folder_is_the_repository() {
    let td = TestDir::new("rules");
    indexed_repo(td.path());
    let before = snapshot(&td.path().join(".murmur"));
    assert_resolved(&history(&env(td.path()), None));
    assert_eq!(snapshot(&td.path().join(".murmur")), before);
}

#[test]
fn absolute_repos_in_the_work_folder_resolve_with_no_root_held() {
    let td = TestDir::new("rules");
    indexed_repo(&td.path().join("repo"));
    indexed_repo(&td.path().join("sub/deeper/repo"));
    let env = env(td.path());
    for repo in ["repo", "sub/deeper/repo"] {
        let abs = td.path().join(repo);
        assert_resolved(&history(&env, Some(abs.to_str().unwrap())));
        // A subfolder of the repository names the database below it, as the native tool did.
        let sub = history(&env, Some(abs.join("src").to_str().unwrap()));
        assert_eq!(sub["status"], "error");
        env.assert_kind(&sub, Some("not_indexed"));
    }
    // Relative forms of the same.
    assert_resolved(&history(&env, Some("repo")));
    assert_resolved(&history(&env, Some("./sub/deeper/repo/")));
}

#[test]
fn a_repo_under_a_read_only_host_folder_resolves_and_leaves_no_file() {
    let host = TestDir::new("rules-host");
    let repo = host.path().join("repo");
    indexed_repo(&repo);
    let work = TestDir::new("rules-work");
    let before = snapshot(&repo.join(".murmur"));
    let env = env(work.path()).root(host.path(), false);
    assert_resolved(&history(&env, Some(repo.to_str().unwrap())));
    assert_eq!(
        snapshot(&repo.join(".murmur")),
        before,
        "no lock or journal file, and the database unchanged"
    );
    assert_eq!(fs::read_dir(work.path()).unwrap().count(), 0);

    // Under the whole host too.
    let env = Env::new(BACKEND, work.path()).root(Path::new("/"), false);
    assert_resolved(&history(&env, Some(repo.to_str().unwrap())));
}

#[test]
fn a_repo_outside_every_folder_is_refused_by_name() {
    let host = TestDir::new("rules-host");
    let repo = host.path().join("repo");
    indexed_repo(&repo);
    let other = TestDir::new("rules-other");
    let work = TestDir::new("rules-work");
    let value = repo.to_str().unwrap();
    for env in [env(work.path()), env(work.path()).root(other.path(), true)] {
        let res = history(&env, Some(value));
        assert_eq!(res["status"], "error");
        assert_eq!(
            res["summary"],
            format!(
                "symbol_history: cannot read {value}/.murmur/code-graph.db: it is outside the \
                 work folder and every host folder this session opens to tool-git"
            )
        );
        env.assert_kind(&res, Some("outside_roots"));
    }
}

#[test]
fn a_reserved_relative_repo_is_refused_while_a_root_is_held() {
    let host = TestDir::new("rules-host");
    let work = TestDir::new("rules-work");
    let env = env(work.path()).root(host.path(), true);
    let res = history(&env, Some("murmur-host/srv/repo"));
    assert_eq!(res["status"], "error");
    assert_eq!(
        res["summary"],
        "symbol_history: 'repo' starts with murmur-host, where this tool's host folders are \
         opened; name the host path absolutely: '/srv/repo'"
    );
    env.assert_kind(&res, Some("reserved_name"));
}

#[test]
fn a_missing_database_is_not_indexed() {
    let host = TestDir::new("rules-host");
    init_repo(&host.path().join("repo"));
    let work = TestDir::new("rules-work");
    let env = env(work.path()).root(host.path(), false);
    let res = history(&env, Some(host.path().join("repo").to_str().unwrap()));
    assert_eq!(res["status"], "error");
    assert_eq!(
        res["summary"],
        "repo has not been indexed: no .murmur/code-graph.db found. Run index_repository \
         (tool-code-graph) first to build the symbol graph this operation resolves against."
    );
    env.assert_kind(&res, Some("not_indexed"));
    assert!(!host.path().join("repo/.murmur").exists());
}

#[test]
fn a_wal_database_cannot_be_read_by_the_component() {
    let td = TestDir::new("rules");
    let repo = td.path().join("repo");
    indexed_repo(&repo);
    let db = repo.join(".murmur/code-graph.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(conn);
    let before = snapshot(&repo.join(".murmur"));
    let env = env(td.path());
    let res = history(&env, Some("repo"));
    match BACKEND {
        // SQLite built for WASI omits WAL.
        common::Backend::Component => {
            assert_eq!(res["status"], "error");
            assert_eq!(
                res["summary"],
                "symbol_history: failed to read code-graph db: file is not a database"
            );
        }
        // The host's SQLite reads WAL.
        common::Backend::Core => assert_resolved(&res),
    }
    assert_eq!(snapshot(&repo.join(".murmur")), before);
}

#[test]
fn a_corrupt_database_fails_to_read() {
    let td = TestDir::new("rules");
    let repo = td.path().join("repo");
    init_repo(&repo);
    fs::create_dir_all(repo.join(".murmur")).unwrap();
    fs::write(repo.join(".murmur/code-graph.db"), vec![0x5a; 8192]).unwrap();
    let before = snapshot(&repo.join(".murmur"));
    let env = env(td.path());
    let res = history(&env, Some("repo"));
    assert_eq!(res["status"], "error");
    assert_eq!(
        res["summary"],
        "symbol_history: failed to read code-graph db: file is not a database"
    );
    env.assert_kind(&res, None);
    assert_eq!(snapshot(&repo.join(".murmur")), before);
}

// ── clone's destination ─────────────────────────────────────────────────────

fn bare_remote(td: &Path) -> String {
    let source = td.join("source");
    init_repo(&source);
    let bare = td.join("remote.git");
    git_ok(&["clone", "-q", "--bare", source.to_str().unwrap(), bare.to_str().unwrap()]);
    bare.to_str().unwrap().to_string()
}

fn clone(env: &Env, url: &str, dest: &str) -> Value {
    env.call(json!({"operation": "clone", "url": url, "dest": dest}))
}

fn assert_dest_exists(env: &Env, res: &Value, dest: &str) {
    assert_eq!(res["status"], "error", "{res}");
    assert_eq!(res["summary"], format!("destination '{dest}' already exists"));
    env.assert_kind(res, Some("dest_exists"));
}

#[test]
fn an_existing_relative_dest_is_refused_before_git_runs() {
    let td = TestDir::new("rules");
    let url = bare_remote(td.path());
    fs::write(td.path().join("a-file"), "x").unwrap();
    fs::create_dir_all(td.path().join("empty")).unwrap();
    fs::create_dir_all(td.path().join("full")).unwrap();
    fs::write(td.path().join("full/x"), "x").unwrap();
    let env = env(td.path());
    for dest in ["a-file", "empty", "full", "./empty/"] {
        let res = clone(&env, &url, dest);
        assert_dest_exists(&env, &res, dest);
    }
    assert!(env.records().is_empty(), "no git ran");
    assert!(!td.path().join("empty/.git").exists());
}

#[test]
fn an_absolute_dest_under_a_held_root_gets_the_same_check() {
    let td = TestDir::new("rules");
    let url = bare_remote(td.path());
    let host = TestDir::new("rules-host");
    fs::create_dir_all(host.path().join("empty")).unwrap();
    let dest = host.path().join("empty");
    let env = env(td.path()).root(host.path(), true);
    let res = clone(&env, &url, dest.to_str().unwrap());
    assert_dest_exists(&env, &res, dest.to_str().unwrap());
    assert!(env.records().is_empty(), "no git ran");
}

#[test]
fn an_uncovered_absolute_dest_is_left_to_git() {
    let td = TestDir::new("rules");
    let url = bare_remote(td.path());
    let outside = TestDir::new("rules-outside");
    let full = outside.path().join("full");
    fs::create_dir_all(&full).unwrap();
    fs::write(full.join("x"), "x").unwrap();
    let env = env(td.path());
    // git refuses a non-empty folder, and its answer reads as the guest's.
    let res = clone(&env, &url, full.to_str().unwrap());
    assert_dest_exists(&env, &res, full.to_str().unwrap());
    assert_eq!(env.argvs().len(), 1, "git ran and refused");

    // git clones into an existing empty folder: the one difference from the native tool.
    let empty = outside.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let res = clone(&env, &url, empty.to_str().unwrap());
    assert_eq!(res["status"], "passed", "{res}");
    assert_eq!(res["data"]["dest"], empty.to_str().unwrap());
    assert_eq!(res["data"]["default_branch"], "main");
    assert!(empty.join("README.md").exists());
}

// ── log_path ────────────────────────────────────────────────────────────────

#[test]
fn log_path_appends_where_it_opens_and_never_fails_the_call() {
    let td = TestDir::new("rules");
    init_repo(&td.path().join("repo"));
    let host = TestDir::new("rules-host");
    let status = json!({"operation": "status", "repo": "repo"});

    let env = env(td.path()).root(host.path(), true);
    let res = env.call_with_log(status.clone(), "tool.log");
    assert_eq!(res["status"], "passed");
    let res = env.call_with_log(status.clone(), host.path().join("abs.log").to_str().unwrap());
    assert_eq!(res["status"], "passed");
    let text = fs::read_to_string(td.path().join("tool.log")).unwrap();
    assert!(text.starts_with("status: repo=\"repo\"\n"), "{text}");
    let text = fs::read_to_string(host.path().join("abs.log")).unwrap();
    assert!(text.starts_with("status: repo=\"repo\"\n"), "{text}");

    // An absolute path no root covers, and one in a missing folder, are skipped.
    let outside = TestDir::new("rules-outside");
    let env = Env::new(BACKEND, td.path());
    for log in [
        outside.path().join("x.log").display().to_string(),
        "missing/dir/x.log".to_string(),
    ] {
        let res = env.call_with_log(status.clone(), &log);
        assert_eq!(res["status"], "passed");
    }
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
