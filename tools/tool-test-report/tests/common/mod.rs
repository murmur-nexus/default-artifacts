//! Shared harness for the suites that drive the compiled `tool-test-report` **wasm32-wasip2
//! component** in Wasmtime: building and loading it once, calling it in a fresh `Store` per call
//! with a scratch work folder at `.` and host folders at `murmur-host<path>` (the preopens murmur
//! gives a WASM tool), scratch directories, and code-graph fixture databases.
#![allow(dead_code)] // each suite uses its own subset

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::Value;
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
    let wasm = target.join("wasm32-wasip2/release/tool_test_report.wasm");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-test-report",
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

    pub fn failures(&self) -> &Vec<Value> {
        self.data["failures"].as_array().expect("failures array")
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
            "tool-test-report-{tag}-{}-{nanos}-{n}",
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

/// Copy the fixture `name` into `dir` under the same name.
pub fn copy_fixture(name: &str, dir: &Path) {
    fs::copy(fixtures_dir().join(name), dir.join(name)).expect("copy fixture");
}

/// Build a code-graph.db under `repo/.murmur/` with tool-code-graph's schema and the given
/// `(module, qualified_name, symbol_id)` function rows. The connection is closed on return.
pub fn build_graph_db(repo: &Path, rows: &[(&str, &str, &str)]) {
    let dir = repo.join(".murmur");
    fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join("code-graph.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY, path TEXT UNIQUE, content_hash TEXT,
            language TEXT DEFAULT 'rust'
        );
        CREATE TABLE symbols (
            id INTEGER PRIMARY KEY, symbol_id TEXT UNIQUE, language TEXT, package TEXT,
            module TEXT, qualified_name TEXT, simple_name TEXT, signature TEXT, kind TEXT,
            file_id INTEGER, start_line INTEGER, end_line INTEGER, doc_comment TEXT DEFAULT ''
        );
        "#,
    )
    .unwrap();
    for (module, qname, symbol_id) in rows {
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name, signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture_crate', ?2, ?3, ?3, '(->())', 'function', 1, 1, 3)",
            rusqlite::params![symbol_id, module, qname],
        )
        .unwrap();
    }
}

/// A cargo test run of `n` failing tests `tests::t0`… with no passing one.
pub fn many_cargo_failures(n: usize) -> String {
    let mut raw = format!("\nrunning {n} tests\n");
    for i in 0..n {
        raw.push_str(&format!("test tests::t{i} ... FAILED\n"));
    }
    raw.push_str("\nfailures:\n\n");
    for i in 0..n {
        raw.push_str(&format!(
            "---- tests::t{i} stdout ----\nthread 'tests::t{i}' panicked at src/lib.rs:{}:5:\nboom {i}\n\n",
            i + 1
        ));
    }
    raw.push_str(&format!(
        "test result: FAILED. 0 passed; {n} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
    ));
    raw
}

/// Every path under `dir`, relative to it, sorted: a snapshot to compare before and after.
pub fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(path.strip_prefix(base).unwrap().display().to_string());
            if path.is_dir() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}
