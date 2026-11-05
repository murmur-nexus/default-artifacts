//! What only the compiled `wasm32-wasip2` component can prove, run through Wasmtime with the
//! preopens murmur gives a WASM tool: the work folder at `.` and each host folder at
//! `murmur-host<path>`.
//!
//! * Parity with the native 0.5.0 tool: every call recorded from it
//!   (`tests/fixtures/native-0.5.0-results.json`, from `native-0.5.0-cases.json` with the recipe
//!   in `tests/fixtures/README.md`) gives the model the same `status`, `summary` and `data`, and
//!   leaves the same suspicion columns in the database.
//! * Writing SQLite from WASI: scores written and re-written across calls, no lock or journal
//!   file left behind, and a WAL-mode, locked or garbage database failing with `status: error`.
//! * Absolute paths open only inside a host folder, `repo_path` only inside a read-write one,
//!   refusals name the value as written, and nothing lands in the work folder.

mod common;

use std::fs;
use std::path::Path;

use common::{
    build_graph_db, call, db_path, fixtures_dir, localize, read_scores, snapshot, standard_symbols,
    store_entries, tree, write_info, HostFolder, Sym, TestDir, S_ADD, S_SUB,
};
use rusqlite::Connection;
use serde_json::{json, Value};

// ── Native 0.5.0 parity ─────────────────────────────────────────────────────────

/// `text` with every capped-list file name's hash replaced by `<hash>`: the name is the hash of
/// the repository as named, which native named by its canonical host path.
fn mask_hash(text: &str) -> String {
    const PREFIX: &str = "localize-";
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let (head, tail) = rest.split_at(at + PREFIX.len());
        out.push_str(head);
        let hex = tail.bytes().take_while(u8::is_ascii_hexdigit).count();
        if hex == 64 && tail[64..].starts_with(".json") {
            out.push_str("<hash>");
            rest = &tail[64..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// A recorded value with the native scratch directory rewritten as the component names it: a
/// path under it relative, the directory itself `.`.
fn relative(recorded: &Value) -> Value {
    let text = recorded
        .to_string()
        .replace("@SCRATCH@/", "")
        .replace("@SCRATCH@", ".");
    serde_json::from_str(&text).unwrap()
}

/// `value` through JSON text and back, as the recorded side went: floats compare exactly.
fn through_text(value: &Value) -> Value {
    serde_json::from_str(&value.to_string()).unwrap()
}

/// Lay out `case` from the cases file in `scratch`: the database (when it has symbols), the extra
/// files, and the coverage with `@SCRATCH@` replaced by the scratch folder's host path.
fn set_up(case: &Value, scratch: &Path) {
    let db_dir = scratch.join(case["db_at"].as_str().unwrap());
    if let Some(symbols) = case["symbols"].as_array() {
        let rows: Vec<(String, String, String, String, i64, i64)> = symbols
            .iter()
            .map(|r| {
                (
                    r[0].as_str().unwrap().to_string(),
                    r[1].as_str().unwrap().to_string(),
                    r[2].as_str().unwrap().to_string(),
                    r[3].as_str().unwrap().to_string(),
                    r[4].as_i64().unwrap(),
                    r[5].as_i64().unwrap(),
                )
            })
            .collect();
        let syms: Vec<Sym> = rows
            .iter()
            .map(|(a, b, c, d, e, f)| (a.as_str(), b.as_str(), c.as_str(), d.as_str(), *e, *f))
            .collect();
        build_graph_db(&db_dir, &syms);
    }
    let host = scratch.display().to_string();
    let files = case["extra_files"]
        .as_object()
        .unwrap()
        .iter()
        .chain(case["coverage"].as_object().unwrap());
    for (rel, body) in files {
        let path = scratch.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body.as_str().unwrap().replace("@SCRATCH@", &host)).unwrap();
    }
}

#[test]
fn native_0_5_0_results() {
    let read = |name: &str| -> Vec<Value> {
        serde_json::from_str(&fs::read_to_string(fixtures_dir().join(name)).unwrap()).unwrap()
    };
    let cases = read("native-0.5.0-cases.json");
    let recorded = read("native-0.5.0-results.json");
    assert_eq!(cases.len(), 15, "the cases file holds 15 cases");
    assert_eq!(recorded.len(), cases.len(), "one recording per case");

    for (case, rec) in cases.iter().zip(&recorded) {
        let name = case["name"].as_str().unwrap();
        assert_eq!(rec["name"], case["name"]);
        let scratch = TestDir::new("native");
        set_up(case, scratch.path());
        let db = scratch
            .path()
            .join(case["db_at"].as_str().unwrap())
            .join(".murmur/code-graph.db");

        let calls = rec["calls"].as_array().unwrap();
        assert_eq!(calls.len(), case["calls"].as_array().unwrap().len());
        for (i, want) in calls.iter().enumerate() {
            let want = relative(want);
            let at = format!("{name} call {i}");
            assert_eq!(want["args"], case["calls"][i], "{at}: args");
            let got = call(scratch.path(), &[], &want["args"]);

            assert_eq!(got.status, want["status"].as_str().unwrap(), "{at}: status");
            assert_eq!(
                mask_hash(got.summary()),
                mask_hash(want["summary"].as_str().unwrap()),
                "{at}: summary"
            );
            assert_eq!(
                mask_hash(&got.data.to_string()),
                mask_hash(&want["data"].to_string()),
                "{at}: data"
            );

            let data_path_file = match got.data["data_path"].as_str() {
                Some(path) => serde_json::from_str(
                    &fs::read_to_string(scratch.path().join(path)).expect("data_path exists"),
                )
                .unwrap(),
                None => Value::Null,
            };
            assert_eq!(
                data_path_file, want["data_path_file"],
                "{at}: data_path file"
            );
            assert_eq!(
                through_text(&snapshot(&db)),
                want["db_after"],
                "{at}: suspicion columns"
            );
            if db.exists() {
                assert_eq!(
                    lock_and_journal(&db),
                    Vec::<String>::new(),
                    "{at}: no lock or journal"
                );
            }
        }
    }
}

// ── S5: writing SQLite from WASI ────────────────────────────────────────────────

/// Which of `<db>.lock` and `<db>-journal` exist beside `db`.
fn lock_and_journal(db: &Path) -> Vec<String> {
    let dir = db.parent().unwrap();
    ["code-graph.db.lock", "code-graph.db-journal"]
        .into_iter()
        .filter(|name| dir.join(name).exists())
        .map(str::to_string)
        .collect()
}

/// A work folder holding `repo/` with the standard symbols indexed.
fn standard_work() -> (TestDir, std::path::PathBuf) {
    let work = TestDir::new("s5");
    let repo = work.path().join("repo");
    build_graph_db(&repo, &standard_symbols());
    (work, repo)
}

#[test]
fn s5a_s5b_scores_are_written_kept_and_reset_across_calls_leaving_no_lock_or_journal() {
    let (work, repo) = standard_work();
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);
    let args = localize("repo", "cov", &["tests::test_add"]);

    // 1: the first write adds the columns and scores `add`.
    let first = call(work.path(), &[], &args);
    assert_eq!(first.status, "passed", "{first:?}");
    assert_eq!(
        read_scores(&repo, S_ADD),
        (Some(1), Some(0), Some(1.0), Some(1.0))
    );
    assert_eq!(store_entries(&repo), ["code-graph.db"]);
    let after_first = snapshot(&db_path(&repo));

    // 2: the identical call leaves the same scores and the same answer.
    let second = call(work.path(), &[], &args);
    assert_eq!(second.status, "passed");
    assert_eq!(second.data_raw, first.data_raw);
    assert_eq!(snapshot(&db_path(&repo)), after_first);
    assert_eq!(store_entries(&repo), ["code-graph.db"]);

    // 3: different coverage resets `add` and scores `sub`.
    fs::remove_dir_all(work.path().join("cov")).unwrap();
    write_info(&work.path().join("cov"), &repo, "tests::test_sub", &[10]);
    let third = call(
        work.path(),
        &[],
        &localize("repo", "cov", &["tests::test_sub"]),
    );
    assert_eq!(third.status, "passed", "{third:?}");
    assert_eq!(read_scores(&repo, S_ADD), (None, None, None, None));
    assert_eq!(read_scores(&repo, S_SUB).0, Some(1));
    assert_eq!(store_entries(&repo), ["code-graph.db"]);
}

#[test]
fn s5c_a_wal_mode_database_fails_with_error_and_is_left_unchanged() {
    let (work, repo) = standard_work();
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);
    {
        let conn = Connection::open(db_path(&repo)).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    // Closing the last connection checkpoints and removes `-wal` and `-shm`: the tool sees only
    // the main file, whose header says WAL.
    assert_eq!(store_entries(&repo), ["code-graph.db"]);
    let before = fs::read(db_path(&repo)).unwrap();

    let res = call(
        work.path(),
        &[],
        &localize("repo", "cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "error", "{res:?}");
    assert!(
        res.summary()
            .starts_with("localize: failed to open/migrate code graph db: "),
        "{}",
        res.summary()
    );
    assert!(
        res.summary().ends_with(
            " (if the database is in WAL mode, which this tool cannot open, switch it back with PRAGMA journal_mode=DELETE)"
        ),
        "{}",
        res.summary()
    );
    assert_eq!(res.data, Value::Null);
    assert_eq!(
        fs::read(db_path(&repo)).unwrap(),
        before,
        "db bytes unchanged"
    );
    assert_eq!(store_entries(&repo), ["code-graph.db"]);
}

#[test]
fn s5d_a_lock_left_behind_fails_with_error_naming_it_until_it_is_removed() {
    let (work, repo) = standard_work();
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);
    let lock = repo.join(".murmur/code-graph.db.lock");
    fs::create_dir(&lock).unwrap();
    let before = fs::read(db_path(&repo)).unwrap();
    let args = localize("repo", "cov", &["tests::test_add"]);

    let res = call(work.path(), &[], &args);
    assert_eq!(res.status, "error", "{res:?}");
    assert_eq!(
        res.summary(),
        "localize: code graph db is locked: repo/.murmur/code-graph.db.lock exists; a localize call that was cut off leaves it behind — if no other call is using the database, delete it and retry"
    );
    assert_eq!(
        fs::read(db_path(&repo)).unwrap(),
        before,
        "db bytes unchanged"
    );
    assert!(lock.is_dir(), "another holder's lock is left alone");

    fs::remove_dir(&lock).unwrap();
    let res = call(work.path(), &[], &args);
    assert_eq!(res.status, "passed", "{res:?}");
    assert_eq!(read_scores(&repo, S_ADD).0, Some(1));
    assert_eq!(store_entries(&repo), ["code-graph.db"]);
}

#[test]
fn s5e_a_garbage_database_fails_with_error() {
    let work = TestDir::new("s5");
    let repo = work.path().join("repo");
    fs::create_dir_all(repo.join(".murmur")).unwrap();
    let garbage = "this is not a SQLite database, just some bytes in its place\n".repeat(64);
    fs::write(db_path(&repo), &garbage).unwrap();
    write_info(&work.path().join("cov"), &repo, "tests::test_add", &[5]);

    let res = call(
        work.path(),
        &[],
        &localize("repo", "cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "error", "{res:?}");
    assert!(
        res.summary()
            .starts_with("localize: failed to open/migrate code graph db: "),
        "{}",
        res.summary()
    );
    assert_eq!(fs::read_to_string(db_path(&repo)).unwrap(), garbage);
    assert_eq!(store_entries(&repo), ["code-graph.db"]);
}

// ── S6: absolute paths and host folders ─────────────────────────────────────────

#[test]
fn s6_absolute_paths_open_only_inside_host_folders() {
    let work = TestDir::new("s6-work");
    let host_rw = TestDir::new("s6-rw");
    let host_ro = TestDir::new("s6-ro");
    let rw = host_rw.path();
    let ro = host_ro.path();

    // The work folder: its own repository and coverage, for the coverage_dir cases.
    let local_repo = work.path().join("repo");
    build_graph_db(&local_repo, &standard_symbols());
    fs::create_dir_all(work.path().join("cov")).unwrap();
    // A repository in the read-write host folder, its coverage beside it.
    let rw_repo = rw.join("repo");
    build_graph_db(&rw_repo, &standard_symbols());
    write_info(&rw.join("cov"), &rw_repo, "tests::test_add", &[5]);
    write_info(&rw.join("cov"), &rw_repo, "tests::test_sub", &[10]);
    // A repository and coverage in the read-only host folder.
    let ro_repo = ro.join("repo");
    build_graph_db(&ro_repo, &standard_symbols());
    write_info(&ro.join("cov"), &local_repo, "tests::test_add", &[5]);

    let before = tree(work.path());
    let both = [
        HostFolder {
            path: rw,
            writable: true,
        },
        HostFolder {
            path: ro,
            writable: false,
        },
    ];
    let abs = |p: &Path| p.display().to_string();

    // An absolute repo_path in a read-write host folder: scores land in the host database, and
    // the capped list is named by its absolute host path.
    let mut args = localize(&abs(&rw_repo), &abs(&rw.join("cov")), &["tests::test_add"]);
    args["limit"] = json!(1);
    let res = call(work.path(), &both, &args);
    assert_eq!(res.status, "passed", "{res:?}");
    assert_eq!(res.data["repo_path"], abs(&rw_repo));
    assert_eq!(res.data["truncated"], true);
    assert_eq!(read_scores(&rw_repo, S_ADD).0, Some(1));
    let data_path = res.data["data_path"].as_str().unwrap();
    let prefix = format!("{}/.murmur/coverage/localize-", abs(&rw_repo));
    assert!(data_path.starts_with(&prefix), "{data_path}");
    assert!(Path::new(data_path).is_file(), "{data_path} on the host");
    assert_eq!(lock_and_journal(&db_path(&rw_repo)), Vec::<String>::new());

    // A trailing `/` names the same repository.
    let res = call(
        work.path(),
        &both,
        &localize(
            &format!("{}/", abs(&rw_repo)),
            &abs(&rw.join("cov")),
            &["tests::test_add"],
        ),
    );
    assert_eq!(res.status, "passed", "{res:?}");
    assert_eq!(res.data["repo_path"], abs(&rw_repo));

    // An absolute coverage_dir in a read-only host folder is read.
    let res = call(
        work.path(),
        &both,
        &localize("repo", &abs(&ro.join("cov")), &["tests::test_add"]),
    );
    assert_eq!(res.status, "passed", "{res:?}");
    assert_eq!(res.data["coverage_dir"], abs(&ro.join("cov")));
    assert_eq!(read_scores(&local_repo, S_ADD).0, Some(1));

    // A repo_path in a read-only host folder is refused before the database is touched.
    let ro_db_before = fs::read(db_path(&ro_repo)).unwrap();
    let res = call(
        work.path(),
        &both,
        &localize(&abs(&ro_repo), "cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "failed");
    assert_eq!(
        res.summary(),
        format!(
            "localize: repo_path '{}' is in {}, which this session opens read-only; localize writes its scores into .murmur/code-graph.db there",
            abs(&ro_repo),
            abs(ro)
        )
    );
    assert_eq!(fs::read(db_path(&ro_repo)).unwrap(), ro_db_before);
    assert_eq!(store_entries(&ro_repo), ["code-graph.db"]);

    // No host folder at all.
    let res = call(
        work.path(),
        &[],
        &localize("/nowhere/repo", "cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "failed");
    assert_eq!(
        res.summary(),
        "localize: could not open repo_path '/nowhere/repo': this session opens no host folder to tool-code-coverage; a relative path opens in the work folder ('.' is the work folder itself)"
    );
    let res = call(
        work.path(),
        &[],
        &localize("repo", "/nowhere/cov", &["tests::test_add"]),
    );
    assert_eq!(
        res.summary(),
        "localize: could not open coverage_dir '/nowhere/cov': this session opens no host folder to tool-code-coverage; a relative path opens in the work folder"
    );

    // Outside every host folder.
    let roots = format!("{} (read-write), {} (read)", abs(rw), abs(ro));
    let res = call(
        work.path(),
        &both,
        &localize("/nowhere/repo", "cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "failed");
    assert_eq!(
        res.summary(),
        format!(
            "localize: could not open repo_path '/nowhere/repo': it is outside every host folder tool-code-coverage may open: {roots}. A relative path opens in the work folder"
        )
    );
    let res = call(
        work.path(),
        &both,
        &localize("repo", "/nowhere/cov", &["tests::test_add"]),
    );
    assert_eq!(
        res.summary(),
        format!(
            "localize: could not open coverage_dir '/nowhere/cov': it is outside every host folder tool-code-coverage may open: {roots}. A relative path opens in the work folder"
        )
    );

    // A relative value starting with murmur-host, while a host folder is held.
    let res = call(
        work.path(),
        &both,
        &localize("murmur-host/srv/repo", "cov", &["tests::test_add"]),
    );
    assert_eq!(res.status, "failed");
    assert_eq!(
        res.summary(),
        "localize: 'repo_path' starts with murmur-host, where this tool's host folders are opened; name the host path absolutely: '/srv/repo'"
    );
    let res = call(
        work.path(),
        &both,
        &localize("repo", "./murmur-host/srv/cov", &["tests::test_add"]),
    );
    assert_eq!(
        res.summary(),
        "localize: 'coverage_dir' starts with murmur-host, where this tool's host folders are opened; name the host path absolutely: '/srv/cov'"
    );

    // Only the work-folder repository's own database changed: no file appeared from the
    // absolute-value calls.
    assert_eq!(tree(work.path()), before, "nothing new in the work folder");
}
