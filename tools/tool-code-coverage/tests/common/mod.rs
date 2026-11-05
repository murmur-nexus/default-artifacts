//! Shared harness for the suites that drive the compiled `tool-code-coverage` **wasm32-wasip2
//! component** in Wasmtime: building and loading it once, calling it in a fresh `Store` per call
//! with a scratch work folder at `.` and host folders at `murmur-host<path>` (the preopens murmur
//! gives a WASM tool), scratch directories, code-graph fixture databases and LCOV files.
#![allow(dead_code)] // each suite uses its own subset

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// Host-side bindings for `world tool` (murmur:tool/run). The component imports no murmur:*
// interface, so the linker only needs WASI.
mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput};
use bindings::Tool;

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Whether the wasm32-wasip2 C compiler is there: `<workspace>/.wasi-sdk/bin/clang`, or the one an
/// exported `CC_wasm32_wasip2` names. Cargo hands its `[env]` table (`.cargo/config.toml`) to the
/// test process too, so the variable is always set, to the `.wasi-sdk` path unless the developer
/// exported another; what is checked is that the compiler it names exists. A bare command name is
/// left to `PATH`.
fn wasm_c_compiler_present(root: &Path) -> bool {
    match std::env::var_os("CC_wasm32_wasip2") {
        Some(cc) if !Path::new(&cc).as_os_str().is_empty() => {
            let cc = Path::new(&cc);
            cc.components().count() == 1 || cc.exists()
        }
        _ => root.join(".wasi-sdk/bin/clang").exists(),
    }
}

/// Path to the compiled component, building it once per test process. `cargo test --workspace`
/// (CI) runs before the separate wasm build step, so the component may not exist yet; it is
/// built alone, as a release builds it. The build compiles bundled SQLite with the pinned
/// wasi-sdk, which a test never downloads: without it the suite stops here, naming the script
/// that installs it.
pub fn component_path() -> PathBuf {
    static BUILD: Once = Once::new();
    let root = workspace_root();
    assert!(
        wasm_c_compiler_present(&root),
        "the pinned wasi-sdk is not installed: run ./scripts/wasi-sdk.sh (see BUILD.md, Prerequisites)"
    );
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let wasm = target.join("wasm32-wasip2/release/tool_code_coverage.wasm");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-code-coverage",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(&root)
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(status.success(), "cargo build of the wasm component failed");
    });
    assert!(
        wasm.exists(),
        "compiled component not found at {} (build did not produce it)",
        wasm.display()
    );
    wasm
}

/// The one engine every test in the process shares, and the component compiled into it once.
fn loaded() -> &'static (Engine, Component, Linker<HostState>) {
    static LOADED: OnceLock<(Engine, Component, Linker<HostState>)> = OnceLock::new();
    LOADED.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("failed to create wasmtime engine");
        let component =
            Component::from_file(&engine, component_path()).expect("load the component");
        let mut linker: Linker<HostState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
        (engine, component, linker)
    })
}

/// A host folder opened to the component besides the work folder, as a `host_paths` entry is:
/// a preopen named `murmur-host<canonical path>`, read-only or read-write.
pub struct HostFolder<'a> {
    pub path: &'a Path,
    pub writable: bool,
}

/// What the model receives from one call: the status, the summary, and `data` both as the
/// exact string the component returned and parsed.
#[derive(Debug)]
pub struct Outcome {
    pub status: &'static str,
    pub summary: Option<String>,
    pub data_raw: Option<String>,
    pub data: Value,
}

impl Outcome {
    pub fn summary(&self) -> &str {
        self.summary.as_deref().unwrap_or_default()
    }

    /// The inline suspect whose `symbol_id` is `symbol_id`.
    pub fn suspect(&self, symbol_id: &str) -> Option<&Value> {
        self.data["top_suspects"]
            .as_array()?
            .iter()
            .find(|s| s["symbol_id"] == symbol_id)
    }
}

/// Call the component once, in a fresh `Store`, with `workdir` preopened at `.` (read-write,
/// the current directory) and each of `hosts` at `murmur-host<canonical path>`. `args` is sent
/// as the call's `data`, compact JSON, as murmur sends it. The fields the model never sees from
/// this tool are checked to be empty.
pub fn call(workdir: &Path, hosts: &[HostFolder<'_>], args: &Value) -> Outcome {
    let (engine, component, linker) = loaded();
    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen the work folder");
    for host in hosts {
        let canonical = host.path.canonicalize().expect("canonical host folder");
        let (dir_perms, file_perms) = if host.writable {
            (DirPerms::all(), FilePerms::all())
        } else {
            (DirPerms::READ, FilePerms::READ)
        };
        builder
            .preopened_dir(
                &canonical,
                format!("murmur-host{}", canonical.display()),
                dir_perms,
                file_perms,
            )
            .expect("preopen a host folder");
    }
    let mut store = Store::new(
        engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
        },
    );
    let tool = Tool::instantiate(&mut store, component, linker).expect("instantiate component");
    let result = tool
        .murmur_tool_run()
        .call_run(
            &mut store,
            &ToolInput {
                data: Some(args.to_string()),
                log_path: None,
            },
        )
        .expect("call run");
    assert_eq!(result.data_path, None, "data_path never reaches the model");
    assert!(!result.truncated);
    assert!(result.metadata.is_empty());
    assert!(result.attachments.is_empty());
    let data = result
        .data
        .as_deref()
        .map(|s| serde_json::from_str(s).expect("ToolResult.data is JSON"))
        .unwrap_or(Value::Null);
    Outcome {
        status: match result.status {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Error => "error",
        },
        summary: result.summary,
        data_raw: result.data,
        data,
    }
}

/// A scratch directory, removed on drop. It may live under the system temp dir: the component
/// sees a work folder only as `.`, and a host folder by the path it is preopened at.
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "tool-code-coverage-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp test directory should be created");
        Self {
            path: path.canonicalize().expect("canonical temp directory"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// ── Fixture db ─────────────────────────────────────────────────────────────────

/// A symbol row: (symbol_id, module, qualified_name, kind, start_line, end_line).
pub type Sym<'a> = (&'a str, &'a str, &'a str, &'a str, i64, i64);

pub const S_ADD: &str = "rust://fixture/#add(->i64)";
pub const S_SUB: &str = "rust://fixture/#sub(->i64)";
pub const S_UNUSED: &str = "rust://fixture/#unused(->i64)";
pub const S_TEST_ADD: &str = "rust://fixture/tests#test_add(->())";
pub const S_TEST_SUB: &str = "rust://fixture/tests#test_sub(->())";

/// The five-symbol fixture most scenarios use, all in `src/lib.rs`.
pub fn standard_symbols() -> Vec<Sym<'static>> {
    vec![
        (S_ADD, "", "add", "function", 5, 8),
        (S_SUB, "", "sub", "function", 10, 13),
        (S_UNUSED, "", "unused", "function", 40, 45),
        (S_TEST_ADD, "tests", "test_add", "function", 20, 25),
        (S_TEST_SUB, "tests", "test_sub", "function", 30, 35),
    ]
}

/// The path of the code-graph database of the repository at `repo`.
pub fn db_path(repo: &Path) -> PathBuf {
    repo.join(".murmur").join("code-graph.db")
}

/// Build `<repo>/.murmur/code-graph.db` with tool-code-graph's schema (WITHOUT the four
/// suspicion_* columns, so the tool's migration adds them) and one file `src/lib.rs` holding the
/// given symbols. The connection is closed on return.
pub fn build_graph_db(repo: &Path, symbols: &[Sym]) {
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
        INSERT INTO files (id, path, content_hash) VALUES (1, 'src/lib.rs', 'deadbeef');
        "#,
    )
    .unwrap();
    for (sid, module, qname, kind, start, end) in symbols {
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name,
                 signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture', ?2, ?3, ?3, '(->())', ?4, 1, ?5, ?6)",
            rusqlite::params![sid, module, qname, kind, start, end],
        )
        .unwrap();
    }
}

/// The four suspicion columns of `symbol_id` in `<repo>/.murmur/code-graph.db`, read on the
/// host: (ef, ep, ochiai, tarantula), `None` for SQL NULL.
pub type Scores = (Option<i64>, Option<i64>, Option<f64>, Option<f64>);

pub fn read_scores(repo: &Path, symbol_id: &str) -> Scores {
    let conn =
        Connection::open_with_flags(db_path(repo), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row(
        "SELECT suspicion_ef, suspicion_ep, suspicion_ochiai, suspicion_tarantula
           FROM symbols WHERE symbol_id = ?1",
        rusqlite::params![symbol_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

/// `{columns, rows}` of `symbols` in the database at `db`, as the native fixture recorded it:
/// every column name in order, then per symbol (by symbol_id) its id and the four suspicion
/// columns, null where a column is absent; `null` when there is no database.
pub fn snapshot(db: &Path) -> Value {
    if !db.exists() {
        return Value::Null;
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let columns: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(symbols)").unwrap();
        let names = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
        names.map(Result::unwrap).collect()
    };
    let pick = |c: &str| {
        if columns.iter().any(|n| n == c) {
            c.to_string()
        } else {
            "NULL".to_string()
        }
    };
    let sql = format!(
        "SELECT symbol_id, {}, {}, {}, {} FROM symbols ORDER BY symbol_id",
        pick("suspicion_ef"),
        pick("suspicion_ep"),
        pick("suspicion_ochiai"),
        pick("suspicion_tarantula")
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?
            ]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    json!({ "columns": columns, "rows": rows })
}

// ── Coverage ───────────────────────────────────────────────────────────────────

/// Write `<coverage_dir>/<test_name>.info` covering the given 1-based lines of `<sf_repo>/src/lib.rs`
/// (an absolute `SF:` path, as `cargo llvm-cov` emits; `sf_repo` is a host path).
pub fn write_info(coverage_dir: &Path, sf_repo: &Path, test_name: &str, hit_lines: &[i64]) {
    fs::create_dir_all(coverage_dir).unwrap();
    let sf = format!("{}/src/lib.rs", sf_repo.display());
    let mut body = format!("TN:\nSF:{sf}\n");
    for l in hit_lines {
        body.push_str(&format!("DA:{l},1\n"));
    }
    body.push_str("LF:1\nLH:1\nend_of_record\n");
    fs::write(coverage_dir.join(format!("{test_name}.info")), body).unwrap();
}

/// A `localize` call naming `repo` and `coverage_dir` as given.
pub fn localize(repo: &str, coverage_dir: &str, failing: &[&str]) -> Value {
    json!({
        "operation": "localize",
        "repo_path": repo,
        "coverage_dir": coverage_dir,
        "failing_tests": failing,
    })
}

/// Every path under `dir`, relative to it, sorted: a snapshot to compare before and after.
pub fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(path.strip_prefix(base).unwrap().display().to_string());
            if path.is_dir() && !path.is_symlink() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The entries of `<repo>/.murmur` beside the database, sorted: no lock or journal may remain.
pub fn store_entries(repo: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(repo.join(".murmur"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}
