//! The component against the native 0.6.0 tool's recorded results, and its SQLite, host-folder
//! and path rules on WASI. Each call runs in a fresh Wasmtime `Store` (`tests/common`).

mod common;

use std::fs;
use std::path::Path;

use common::{
    assert_no_lock_or_journal, call, call_unchecked, fixtures_dir, snapshot, strip_os_error, tree,
    write_repo, HostFolder, Outcome, TestDir,
};
use rusqlite::Connection;
use serde_json::{json, Value};

fn read_json(name: &str) -> Value {
    let text = fs::read_to_string(fixtures_dir().join(name)).expect("fixture file");
    serde_json::from_str(&text).expect("fixture JSON")
}

/// Replace `@REPO@` in every string of `value`: with `repo_path` in the `repo_path` input, and
/// with `absolute` everywhere else.
fn substitute_args(value: &Value, repo_path: &str, absolute: &str) -> Value {
    let mut out = value.clone();
    if let Value::Object(map) = &mut out {
        for (key, v) in map.iter_mut() {
            if let Value::String(s) = v {
                let with = if key == "repo_path" {
                    repo_path
                } else {
                    absolute
                };
                *s = s.replace("@REPO@", with);
            }
        }
    }
    out
}

/// `value` with `@REPO@` replaced by `repo` in every string, compared as JSON.
fn substitute(value: &Value, repo: &str) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    serde_json::from_str(&value.to_string().replace("@REPO@", repo)).unwrap()
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// The case folder is the work folder at `.`, and `repo_path` is `fixture_repo`.
    Relative,
    /// The case folder is a read-write host folder, and `repo_path` is the repository's
    /// absolute path; the work folder is a separate empty folder.
    HostRoot,
}

/// Every case of `native-0.6.0-cases.json`, replayed in both modes, against the native 0.6.0
/// tool's `native-0.6.0-results.json`: `status`, `summary` (with ` (os error N)` stripped) and
/// `data` as a JSON value, the `data_path` file parsed, and the database rows after every call.
/// Where native named the repository by its absolute path, mode `Relative` names it
/// `fixture_repo`; that rewrite is the only difference allowed.
#[test]
fn native_0_6_0_results() {
    let cases = read_json("native-0.6.0-cases.json");
    let results = read_json("native-0.6.0-results.json");
    let cases = cases["cases"].as_array().unwrap();
    let results = results["cases"].as_array().unwrap();
    assert_eq!(cases.len(), results.len());

    for mode in [Mode::Relative, Mode::HostRoot] {
        let mut compared = 0;
        for (case, recorded) in cases.iter().zip(results) {
            let name = case["name"].as_str().unwrap();
            assert_eq!(recorded["name"], name);
            let td = TestDir::new("parity");
            let case_dir = td.path().join(name);
            let repo = case_dir.join("fixture_repo");
            fs::create_dir_all(&repo).unwrap();
            write_repo(case["fixture"].as_str().unwrap(), &repo);
            let absolute = repo.to_str().unwrap().to_string();
            let empty_work = td.path().join("work");
            fs::create_dir_all(&empty_work).unwrap();
            let (work, repo_path, hosts) = match mode {
                Mode::Relative => (case_dir.as_path(), "fixture_repo".to_string(), vec![]),
                Mode::HostRoot => (
                    empty_work.as_path(),
                    absolute.clone(),
                    vec![HostFolder {
                        path: &case_dir,
                        writable: true,
                    }],
                ),
            };

            let calls = recorded["calls"].as_array().unwrap();
            let mut next = calls.iter();
            for step in case["steps"].as_array().unwrap() {
                if let Some(write) = step.get("write") {
                    fs::write(
                        repo.join(write["path"].as_str().unwrap()),
                        write["content"].as_str().unwrap(),
                    )
                    .unwrap();
                    continue;
                }
                if let Some(path) = step.get("remove") {
                    fs::remove_file(repo.join(path.as_str().unwrap())).unwrap();
                    continue;
                }
                let expected = next.next().expect("a recorded result per call");
                assert_eq!(
                    expected["args"], step["call"],
                    "{name}: cases and results differ"
                );
                let args = substitute_args(&step["call"], &repo_path, &absolute);
                let got = call(work, &hosts, &args);
                let ctx = format!("{mode:?} {name} {args}");

                assert_eq!(got.status, expected["status"], "{ctx}: {got}");
                let summary = expected["summary"]
                    .as_str()
                    .map(|s| strip_os_error(&s.replace("@REPO@", &repo_path)));
                assert_eq!(
                    got.summary.as_deref().map(strip_os_error),
                    summary,
                    "{ctx}: summary"
                );
                assert_eq!(
                    got.data,
                    substitute(&expected["data"], &repo_path),
                    "{ctx}: data"
                );

                let payload = got.data.get("data_path").and_then(Value::as_str).map(|dp| {
                    let on_host = work.join(dp);
                    serde_json::from_str::<Value>(&fs::read_to_string(&on_host).unwrap()).unwrap()
                });
                assert_eq!(
                    payload.unwrap_or(Value::Null),
                    substitute(&expected["payload"], &repo_path),
                    "{ctx}: data_path file"
                );

                let rows = snapshot(&repo.join(".murmur/code-graph.db"));
                let recorded_rows = match expected["db"].as_u64() {
                    Some(i) => recorded["snapshots"][i as usize].clone(),
                    None => Value::Null,
                };
                assert!(rows == recorded_rows, "{ctx}: database rows differ");
                compared += 1;
            }
            assert!(next.next().is_none(), "{name}: unreplayed recorded calls");
            if let Mode::HostRoot = mode {
                assert!(
                    tree(&empty_work).is_empty(),
                    "{name}: an absolute repo_path wrote into the work folder"
                );
            }
        }
        eprintln!(
            "native_0_6_0_results {mode:?}: {} cases, {compared} calls equal",
            cases.len()
        );
    }
}

// ── S6: SQLite on WASI ─────────────────────────────────────────────────────────

fn op(operation: &str, repo_path: &str, extra: Value) -> Value {
    let mut args = json!({ "operation": operation, "repo_path": repo_path });
    if let Value::Object(map) = extra {
        for (k, v) in map {
            args[k] = v;
        }
    }
    args
}

fn symbol_count(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn s6a_a_database_written_in_one_store_is_read_in_the_next() {
    let td = TestDir::new("s6a");
    write_repo("rust", td.path());
    let indexed = call(td.path(), &[], &op("index_repository", ".", json!({})));
    assert_eq!(indexed.status, "passed", "{indexed}");
    // Read back on the host between the calls.
    let db = td.path().join(".murmur/code-graph.db");
    let n = symbol_count(&db);
    assert_eq!(json!(n), indexed.data["total_symbols"]);
    assert!(n >= 6);
    let found = call(
        td.path(),
        &[],
        &op("find_symbol", ".", json!({"query": "widget"})),
    );
    assert_eq!(found.status, "passed", "{found}");
    assert!(
        found.data["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["symbol_id"] == "rust://fixture_crate/#Widget::new(i64->Widget)"),
        "{found}"
    );
    assert_eq!(
        fs::read_dir(td.path().join(".murmur"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>(),
        vec!["code-graph.db"]
    );
}

#[test]
fn s6c_a_wal_mode_database_is_an_error_and_is_left_unchanged() {
    let td = TestDir::new("s6c");
    write_repo("rust", td.path());
    assert_eq!(
        call(td.path(), &[], &op("index_repository", ".", json!({}))).status,
        "passed"
    );
    let db = td.path().join(".murmur/code-graph.db");
    {
        let conn = Connection::open(&db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    // Closing the last connection checkpoints and removes -wal/-shm: the tool sees only the
    // main file, whose header says WAL.
    assert_eq!(tree(&td.path().join(".murmur")), vec!["code-graph.db"]);
    let before = fs::read(&db).unwrap();
    for operation in ["find_symbol", "index_repository"] {
        let got = call(td.path(), &[], &op(operation, ".", json!({"query": "add"})));
        eprintln!("s6c: {got}");
        assert_eq!(got.status, "error", "{got}");
        assert!(
            got.summary()
                .starts_with(&format!("{operation}: failed to open code graph db: ")),
            "{got}"
        );
        assert_eq!(fs::read(&db).unwrap(), before, "the WAL database changed");
    }
}

#[test]
fn s6d_a_left_over_lock_directory_names_itself_and_clears_when_removed() {
    let td = TestDir::new("s6d");
    let repo = td.path().join("proj");
    fs::create_dir_all(&repo).unwrap();
    write_repo("rust", &repo);
    assert_eq!(
        call(td.path(), &[], &op("index_repository", "proj", json!({}))).status,
        "passed"
    );
    let lock = repo.join(".murmur/code-graph.db.lock");
    fs::create_dir(&lock).unwrap();
    let got = call_unchecked(
        td.path(),
        &[],
        &op("find_symbol", "proj", json!({"query": "add"})),
    );
    assert_eq!(got.status, "error", "{got}");
    assert_eq!(
        got.summary(),
        "find_symbol: failed to open code graph db: database is locked; a previous call may \
         have been cut off mid-write: if no other call is using the graph, the directory \
         proj/.murmur/code-graph.db.lock can be removed"
    );
    fs::remove_dir(&lock).unwrap();
    let again = call(
        td.path(),
        &[],
        &op("find_symbol", "proj", json!({"query": "add"})),
    );
    assert_eq!(again.status, "passed", "{again}");
}

#[test]
fn s6e_a_garbage_database_is_an_error() {
    let td = TestDir::new("s6e");
    write_repo("rust", td.path());
    fs::create_dir_all(td.path().join(".murmur")).unwrap();
    fs::write(
        td.path().join(".murmur/code-graph.db"),
        b"this is not a sqlite database, only some garbage bytes padded out to a page".repeat(64),
    )
    .unwrap();
    for operation in ["index_repository", "get_symbol"] {
        let got = call(
            td.path(),
            &[],
            &op(operation, ".", json!({"symbol_id": "rust://x/#y()"})),
        );
        eprintln!("s6e: {got}");
        assert_eq!(got.status, "error", "{got}");
        assert!(
            got.summary()
                .starts_with(&format!("{operation}: failed to open code graph db: ")),
            "{got}"
        );
    }
}

/// The pre-migration schema (impact_10's): no visibility/attributes/confidence/call_style.
const OLD_SCHEMA: &str = "
CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
    content_hash TEXT NOT NULL, language TEXT NOT NULL DEFAULT 'rust');
CREATE TABLE symbols (id INTEGER PRIMARY KEY, symbol_id TEXT NOT NULL UNIQUE,
    language TEXT NOT NULL, package TEXT NOT NULL, module TEXT NOT NULL,
    qualified_name TEXT NOT NULL, simple_name TEXT NOT NULL, signature TEXT NOT NULL,
    kind TEXT NOT NULL, file_id INTEGER NOT NULL, start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL, doc_comment TEXT NOT NULL DEFAULT '');
CREATE TABLE edges (id INTEGER PRIMARY KEY, src_symbol_id TEXT NOT NULL, dst_symbol_id TEXT,
    dst_name TEXT NOT NULL, edge_kind TEXT NOT NULL, UNIQUE(src_symbol_id, dst_name, edge_kind));
CREATE VIRTUAL TABLE symbols_fts USING fts5(symbol_id UNINDEXED, qualified_name, doc_comment);
";

#[test]
fn s6f_a_read_only_host_folder_answers_queries_immutably_and_refuses_to_index() {
    let td = TestDir::new("s6f");
    let work = td.path().join("work");
    let host = td.path().join("host");
    let repo = host.join("repo");
    let bare = host.join("bare");
    let old = host.join("old");
    for dir in [&work, &repo, &bare, &old] {
        fs::create_dir_all(dir).unwrap();
    }
    write_repo("rust", &repo);
    write_repo("rust", &bare);
    write_repo("rust", &old);
    // Indexed beforehand, through a read-write opening of the same folder.
    let repo_abs = repo.to_str().unwrap();
    let rw = [HostFolder {
        path: &host,
        writable: true,
    }];
    assert_eq!(
        call(&work, &rw, &op("index_repository", repo_abs, json!({}))).status,
        "passed"
    );
    fs::create_dir_all(old.join(".murmur")).unwrap();
    Connection::open(old.join(".murmur/code-graph.db"))
        .unwrap()
        .execute_batch(OLD_SCHEMA)
        .unwrap();

    let ro = [HostFolder {
        path: &host,
        writable: false,
    }];
    let host_before = tree(&host);
    let canonical_host = host.canonicalize().unwrap();

    let index = call(&work, &ro, &op("index_repository", repo_abs, json!({})));
    assert_eq!(index.status, "failed", "{index}");
    assert_eq!(
        index.summary(),
        format!(
            "index_repository: repo_path '{repo_abs}' is inside a host folder opened read-only \
             ({}); index_repository writes {repo_abs}/.murmur/code-graph.db, so it needs that \
             folder read-write",
            canonical_host.display()
        )
    );
    let bare_index = call(
        &work,
        &ro,
        &op("index_repository", bare.to_str().unwrap(), json!({})),
    );
    assert_eq!(bare_index.status, "failed", "{bare_index}");

    let top = "rust://fixture_crate/#top(->i64)";
    let found = call(
        &work,
        &ro,
        &op("find_symbol", repo_abs, json!({"query": "widget"})),
    );
    assert_eq!(found.status, "passed", "{found}");
    assert!(found.data["count"].as_i64().unwrap() >= 1, "{found}");
    let got = call(
        &work,
        &ro,
        &op("get_symbol", repo_abs, json!({"symbol_id": top})),
    );
    assert_eq!(got.status, "passed", "{got}");
    assert_eq!(got.data["qualified_name"], "top");
    let slice = call(
        &work,
        &ro,
        &op("slice_symbol", repo_abs, json!({"symbol_id": top})),
    );
    assert_eq!(slice.status, "passed", "{slice}");
    assert_eq!(slice.data["data_path"], Value::Null, "{slice}");
    assert!(slice.data["node_count"].as_i64().unwrap() >= 3, "{slice}");

    for (dir, reason) in [
        (
            &bare,
            "/.murmur/code-graph.db does not exist, and a folder opened read-only",
        ),
        (&old, "needs a migration"),
    ] {
        let got = call(
            &work,
            &ro,
            &op(
                "find_symbol",
                dir.to_str().unwrap(),
                json!({"query": "top"}),
            ),
        );
        eprintln!("s6f: {got}");
        assert_eq!(got.status, "error", "{got}");
        assert!(
            got.summary()
                .starts_with("find_symbol: failed to open code graph db: "),
            "{got}"
        );
        assert!(got.summary().contains(reason), "{got}");
        assert!(!got.summary().contains("murmur-host"), "{got}");
    }
    assert_eq!(tree(&host), host_before, "a read-only host folder changed");
    assert!(tree(&work).is_empty());
}

// ── S7: absolute paths open only through host folders ──────────────────────────

#[test]
fn s7_absolute_repo_paths_open_only_inside_host_folders() {
    let td = TestDir::new("s7");
    let work = td.path().join("work");
    let host = td.path().join("host");
    let other = td.path().join("other");
    let repo = host.join("repo");
    for dir in [&work, &repo, &other] {
        fs::create_dir_all(dir).unwrap();
    }
    write_repo("rust", &repo);
    fs::write(work.join("keep.txt"), "the work folder").unwrap();
    let work_before = tree(&work);
    let repo_abs = repo.to_str().unwrap();
    let rw = [HostFolder {
        path: &host,
        writable: true,
    }];

    let index = call(&work, &rw, &op("index_repository", repo_abs, json!({})));
    assert_eq!(index.status, "passed", "{index}");
    assert_eq!(index.data["repo_path"], repo_abs);
    assert_eq!(index.data["changed_files"], 2);
    assert!(repo.join(".murmur/code-graph.db").exists());
    let slice = call(
        &work,
        &rw,
        &op(
            "slice_symbol",
            &format!("{repo_abs}/"),
            json!({"symbol_id": "rust://fixture_crate/#top(->i64)"}),
        ),
    );
    assert_eq!(slice.status, "passed", "{slice}");
    let dp = slice.data["data_path"].as_str().unwrap();
    assert!(
        dp.starts_with(&format!("{repo_abs}/.murmur/slices/slice-")),
        "{dp}"
    );
    assert!(Path::new(dp).exists(), "{dp}");

    let missing = call(
        &work,
        &rw,
        &op("get_symbol", &format!("{repo_abs}/nope"), json!({})),
    );
    assert_eq!(
        missing.summary(),
        format!("get_symbol: repo_path does not exist: {repo_abs}/nope")
    );

    let no_roots = call(&work, &[], &op("index_repository", repo_abs, json!({})));
    assert_eq!(no_roots.status, "failed");
    assert_eq!(
        no_roots.summary(),
        format!(
            "index_repository: could not open repo_path '{repo_abs}': this session opens no host \
             folder to tool-code-graph; a relative path opens in the work folder ('.' is the work \
             folder itself)"
        )
    );

    let roots = [
        HostFolder {
            path: &host,
            writable: true,
        },
        HostFolder {
            path: &other,
            writable: false,
        },
    ];
    let outside = td.path().join("elsewhere");
    let uncovered = call(
        &work,
        &roots,
        &op(
            "find_symbol",
            outside.to_str().unwrap(),
            json!({"query": "x"}),
        ),
    );
    assert_eq!(uncovered.status, "failed");
    assert_eq!(
        uncovered.summary(),
        format!(
            "find_symbol: could not open repo_path '{}': it is outside every host folder \
             tool-code-graph may open: {} (read-write), {} (read). A relative path opens in the \
             work folder ('.' is the work folder itself)",
            outside.display(),
            host.canonicalize().unwrap().display(),
            other.canonicalize().unwrap().display()
        )
    );

    let reserved = call(
        &work,
        &rw,
        &op("index_repository", "./murmur-host/srv/repo", json!({})),
    );
    assert_eq!(reserved.status, "failed");
    assert_eq!(
        reserved.summary(),
        "index_repository: 'repo_path' starts with murmur-host, where this tool's host folders \
         are opened; name the host path absolutely: '/srv/repo'"
    );

    assert_eq!(
        tree(&work),
        work_before,
        "an absolute case touched the work folder"
    );
}

// ── S8: package fallback and reported paths ────────────────────────────────────

fn write_unmanifested(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/thing.rs"),
        "pub fn thing() -> i64 {\n    1\n}\n",
    )
    .unwrap();
    fs::write(dir.join("tool.py"), "def run():\n    return 1\n").unwrap();
}

fn ids(out: &Outcome) -> Vec<String> {
    out.data["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["symbol_id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn s8_the_package_fallback_is_the_repo_path_as_written() {
    for repo_path in [None, Some(".")] {
        let td = TestDir::new("s8");
        write_unmanifested(td.path());
        let mut index = json!({"operation": "index_repository"});
        let mut thing = json!({"operation": "find_symbol", "query": "thing"});
        let mut run = json!({"operation": "find_symbol", "query": "run"});
        if let Some(p) = repo_path {
            index["repo_path"] = json!(p);
            thing["repo_path"] = json!(p);
            run["repo_path"] = json!(p);
        }
        let indexed = call(td.path(), &[], &index);
        assert_eq!(indexed.status, "passed", "{indexed}");
        assert_eq!(indexed.data["repo_path"], ".");
        assert_eq!(
            ids(&call(td.path(), &[], &thing)),
            vec!["rust://crate/thing#thing(->i64)"]
        );
        assert_eq!(
            ids(&call(td.path(), &[], &run)),
            vec!["python://package/tool#run()"]
        );
    }

    let td = TestDir::new("s8");
    write_unmanifested(&td.path().join("proj"));
    let indexed = call(td.path(), &[], &op("index_repository", "proj/", json!({})));
    assert_eq!(indexed.status, "passed", "{indexed}");
    assert_eq!(indexed.data["repo_path"], "proj");
    let thing = call(
        td.path(),
        &[],
        &op("find_symbol", "proj/", json!({"query": "thing"})),
    );
    assert_eq!(ids(&thing), vec!["rust://proj/thing#thing(->i64)"]);
    let run = call(
        td.path(),
        &[],
        &op("find_symbol", "proj", json!({"query": "run"})),
    );
    assert_eq!(ids(&run), vec!["python://proj/tool#run()"]);
    let slice = call(
        td.path(),
        &[],
        &op(
            "slice_symbol",
            "proj/",
            json!({"symbol_id": "rust://proj/thing#thing(->i64)"}),
        ),
    );
    let dp = slice.data["data_path"].as_str().unwrap();
    assert!(dp.starts_with("proj/.murmur/slices/slice-"), "{dp}");
    assert!(td.path().join(dp).exists());
    assert_no_lock_or_journal(&td.path().join("proj"));
}

// ── S5: a large repository ─────────────────────────────────────────────────────

/// A repository large enough that `resolve_edges`' `UPDATE` writes a statement journal past
/// SQLite's 64 KiB in-memory threshold. Spilled to a temp file, that journal trapped the
/// component (`fchmod` is absent from SQLite's WASI build); the default-artifacts tree itself
/// reached it.
#[test]
fn s5_a_large_index_keeps_its_temporary_storage_in_memory() {
    let td = TestDir::new("s5");
    fs::write(
        td.path().join("Cargo.toml"),
        "[package]\nname = \"big\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(td.path().join("src")).unwrap();
    for file in 0..40 {
        let mut text = String::new();
        for f in 0..100 {
            text.push_str(&format!(
                "/// Function {f} of file {file}, with a doc comment long enough to fill pages.\n\
                 pub fn function_{file}_{f}(x: i64) -> i64 {{\n    \
                 function_{file}_{}(x) + helper_{}(x) + shared(x)\n}}\n\n",
                (f + 1) % 100,
                (file + 1) % 40
            ));
        }
        text.push_str(&format!(
            "pub fn helper_{file}(x: i64) -> i64 {{\n    x\n}}\n\npub fn shared(x: i64) -> i64 {{\n    x\n}}\n"
        ));
        fs::write(td.path().join(format!("src/m{file}.rs")), text).unwrap();
    }
    let index = call(td.path(), &[], &op("index_repository", ".", json!({})));
    assert_eq!(index.status, "passed", "{index}");
    assert_eq!(index.data["changed_files"], 40);
    let edges = index.data["total_edges"].as_i64().unwrap();
    assert!(edges > 10_000, "{index}");
    // A re-index after touching every file rewrites every row in one transaction.
    for file in 0..40 {
        let path = td.path().join(format!("src/m{file}.rs"));
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("// touched\n{text}")).unwrap();
    }
    let again = call(td.path(), &[], &op("index_repository", ".", json!({})));
    assert_eq!(again.status, "passed", "{again}");
    assert_eq!(again.data["changed_files"], 40);
    assert_eq!(again.data["total_edges"], edges);
}
