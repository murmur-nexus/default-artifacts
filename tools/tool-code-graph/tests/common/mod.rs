//! Shared harness for the suites that drive the compiled `tool-code-graph` **wasm32-wasip2
//! component** in Wasmtime: building and loading it once, calling it in a fresh `Store` per call
//! with a scratch work folder at `.` and host folders at `murmur-host<path>` (the preopens murmur
//! gives a WASM tool), the fixture repositories, scratch directories and database snapshots.
#![allow(dead_code)] // each suite uses its own subset

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
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
/// built alone, as a release builds it. The build compiles bundled SQLite and tree-sitter with
/// the pinned wasi-sdk, which a test never downloads: without it the suite stops here, naming the
/// script that installs it.
///
/// `CODE_GRAPH_COMPONENT=<path to a .wasm>` uses that build instead and builds nothing, to run a
/// suite against another build of the component.
pub fn component_path() -> PathBuf {
    if let Some(wasm) = std::env::var_os("CODE_GRAPH_COMPONENT") {
        return PathBuf::from(wasm);
    }
    static BUILD: Once = Once::new();
    let root = workspace_root();
    assert!(
        wasm_c_compiler_present(&root),
        "the pinned wasi-sdk is not installed: run ./scripts/wasi-sdk.sh (see BUILD.md, Prerequisites)"
    );
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let wasm = target.join("wasm32-wasip2/release/tool_code_graph.wasm");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-code-graph",
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

type Loaded = (Engine, Component, Linker<HostState>);

/// The one engine every test in the process shares, and the component compiled into it once.
fn loaded() -> &'static Loaded {
    static LOADED: OnceLock<&'static Loaded> = OnceLock::new();
    LOADED.get_or_init(|| loaded_from(&component_path()))
}

/// The component at `wasm`, compiled once per process into its own engine.
fn loaded_from(wasm: &Path) -> &'static Loaded {
    static ALL: Mutex<Vec<(PathBuf, &'static Loaded)>> = Mutex::new(Vec::new());
    let mut all = ALL.lock().unwrap();
    if let Some((_, loaded)) = all.iter().find(|(path, _)| path == wasm) {
        return loaded;
    }
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("failed to create wasmtime engine");
    let component = Component::from_file(&engine, wasm).expect("load the component");
    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    let loaded: &'static Loaded = Box::leak(Box::new((engine, component, linker)));
    all.push((wasm.to_path_buf(), loaded));
    loaded
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
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} | {} | {}",
            self.status,
            self.summary(),
            self.data_raw.as_deref().unwrap_or("null")
        )
    }
}

/// Call the component once, in a fresh `Store`, with `workdir` preopened at `.` (read-write,
/// the current directory) and each of `hosts` at `murmur-host<canonical path>`. `args` is sent
/// as the call's `data`, compact JSON, as murmur sends it. The fields the model never sees from
/// this tool are checked to be empty, and the repository the call named is checked to hold no
/// SQLite lock directory or journal afterwards.
pub fn call(workdir: &Path, hosts: &[HostFolder<'_>], args: &Value) -> Outcome {
    let outcome = call_unchecked(workdir, hosts, args);
    let repo = match args.get("repo_path").and_then(Value::as_str) {
        Some(p) if p.starts_with('/') => PathBuf::from(p),
        Some(p) if !p.is_empty() => workdir.join(p),
        _ => workdir.to_path_buf(),
    };
    assert_no_lock_or_journal(&repo);
    outcome
}

/// Assert `<repo>/.murmur/` holds no `code-graph.db.lock` and no `code-graph.db-journal`.
pub fn assert_no_lock_or_journal(repo: &Path) {
    for leftover in ["code-graph.db.lock", "code-graph.db-journal"] {
        let path = repo.join(".murmur").join(leftover);
        assert!(
            !path.exists(),
            "{} is left behind after the call",
            path.display()
        );
    }
}

/// [`call`] without the lock and journal check, for a test that plants a lock itself.
pub fn call_unchecked(workdir: &Path, hosts: &[HostFolder<'_>], args: &Value) -> Outcome {
    call_loaded(loaded(), workdir, hosts, args)
}

/// [`call_unchecked`] of the component at `wasm` rather than the workspace build.
pub fn call_component(wasm: &Path, workdir: &Path, args: &Value) -> Outcome {
    call_loaded(loaded_from(wasm), workdir, &[], args)
}

fn call_loaded(
    (engine, component, linker): &'static Loaded,
    workdir: &Path,
    hosts: &[HostFolder<'_>],
    args: &Value,
) -> Outcome {
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

/// Call the component with `dir` as the work folder (read-write at `.`). A `repo_path` equal to
/// `dir`'s absolute path is sent as `"."`, the work folder itself: the component cannot open an
/// absolute path outside a host folder.
pub fn run_tool(dir: &Path, mut op: Value) -> Outcome {
    if op.get("repo_path").and_then(Value::as_str) == dir.to_str() {
        op["repo_path"] = json!(".");
    }
    call(dir, &[], &op)
}

/// `index_repository` of the work folder `dir` itself.
pub fn index(dir: &Path) -> Outcome {
    run_tool(
        dir,
        json!({ "operation": "index_repository", "repo_path": "." }),
    )
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
            "tool-code-graph-{tag}-{}-{nanos}-{n}",
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

/// The fixture repositories of `tests/fixtures/repos.json`: name → (relative path → content).
pub fn repos() -> &'static BTreeMap<String, BTreeMap<String, String>> {
    static REPOS: OnceLock<BTreeMap<String, BTreeMap<String, String>>> = OnceLock::new();
    REPOS.get_or_init(|| {
        let text = fs::read_to_string(fixtures_dir().join("repos.json")).expect("repos.json");
        serde_json::from_str(&text).expect("repos.json is name -> path -> content")
    })
}

/// Write the fixture repository `name` (`rust`, `impact`, `python` or `mixed`) into `dir`.
pub fn write_repo(name: &str, dir: &Path) {
    let files = repos()
        .get(name)
        .unwrap_or_else(|| panic!("no fixture repository {name:?} in repos.json"));
    for (rel, content) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
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

/// The graph database's rows, keyed so two databases compare regardless of row ids:
/// `files(path, content_hash, language)` by path; `symbols` (every column but `id`/`file_id`,
/// plus the file's path) by `symbol_id`; `edges` (every column but `id`) by
/// `(src_symbol_id, dst_name, edge_kind)`; `symbols_fts(symbol_id, qualified_name, doc_comment)`
/// by `symbol_id`. `Value::Null` when there is no database.
pub fn snapshot(db: &Path) -> Value {
    if !db.exists() {
        return Value::Null;
    }
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open the database read-only");
    let rows = |sql: &str, key_cols: usize| -> Value {
        let mut stmt = conn.prepare(sql).expect("prepare snapshot query");
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut map = serde_json::Map::new();
        let mut q = stmt.query([]).unwrap();
        while let Some(r) = q.next().unwrap() {
            let mut obj = serde_json::Map::new();
            let mut key = Vec::new();
            for (i, name) in names.iter().enumerate() {
                let v: Value = match r.get_ref(i).unwrap() {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(x) => json!(x),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(b) => json!(b),
                };
                if i < key_cols {
                    key.push(match &v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                }
                obj.insert(name.clone(), v);
            }
            let key = key.join(" | ");
            assert!(
                map.insert(key.clone(), Value::Object(obj)).is_none(),
                "duplicate snapshot key {key}"
            );
        }
        Value::Object(map)
    };
    json!({
        "files": rows("SELECT path, content_hash, language FROM files", 1),
        "symbols": rows(
            "SELECT s.symbol_id, s.language, s.package, s.module, s.qualified_name, s.simple_name,
                    s.signature, s.kind, f.path AS file, s.start_line, s.end_line, s.doc_comment,
                    s.visibility, s.attributes
               FROM symbols s JOIN files f ON f.id = s.file_id",
            1,
        ),
        "edges": rows(
            "SELECT src_symbol_id, dst_name, edge_kind, dst_symbol_id, call_style, confidence
               FROM edges",
            3,
        ),
        "symbols_fts": rows(
            "SELECT symbol_id, qualified_name, doc_comment FROM symbols_fts",
            1,
        ),
    })
}

/// `summary` with every ` (os error N)` removed: the native binary and WASI number the same
/// error differently.
pub fn strip_os_error(summary: &str) -> String {
    let mut out = String::with_capacity(summary.len());
    let mut rest = summary;
    while let Some(i) = rest.find(" (os error ") {
        out.push_str(&rest[..i]);
        match rest[i..].find(')') {
            Some(j) => rest = &rest[i + j + 1..],
            None => {
                rest = &rest[i..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}
