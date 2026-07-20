//! Host-target integration test for the compiled `murmur-tool-editor` **wasm32-wasip2
//! component**, run through Wasmtime.
//!
//! This replaces the old `tests/cross_process_cache.rs`, which spawned the native binary
//! as two OS processes via `env!("CARGO_BIN_EXE_murmur-tool-editor")` — a symbol that no
//! longer exists once the `[[bin]]` target is dropped in the wasm port. The on-disk
//! read-cache invariant it proved (cache hit / miss-on-mtime-change across two *separate*
//! invocations) is reproduced here across two independent Wasmtime `Store`/instantiation
//! calls against the same preopened workdir — the component-model analogue of two OS
//! processes.
//!
//! It also validates the compiled artifact loads as a real component
//! (`Component::from_file` is the validation gate — there is no `wasm-tools` CLI dependency
//! here) and benchmarks a `find_in_files` walk over a large synthetic tree against the
//! identical host-native `logic::run` path, to prove no order-of-magnitude regression.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::Instant;

use serde_json::Value;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// Host-side bindings for `world tool` (murmur:tool/run). The component imports zero
// murmur:* interfaces (only wasi:*, provided by wasmtime-wasi below), so we never wire the
// task/text host traits — the linker only needs WASI.
mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput};
use bindings::Tool;

/// Store state: just a WASI context + resource table. `WasiView` is all the linker needs.
struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

static BUILD: Once = Once::new();

/// Path to the compiled component, building it if absent. `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so the test cannot assume the artifact
/// already exists — it builds it into the standard workspace target dir on first use.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/murmur_tool_editor.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "murmur-tool-editor",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(manifest.join("../.."))
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

fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config).expect("failed to create wasmtime engine")
}

fn linker(engine: &Engine) -> Linker<HostState> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    linker
}

/// A fresh Store whose only preopen is `workdir` mapped to `.` (DirPerms/FilePerms::all) —
/// exactly what the capsule runtime's `build_wasi_ctx` grants a tool at dispatch time. Env
/// is deliberately NOT inherited, so the editor's cache resolves to its default
/// workdir-relative `.murmur-tool-editor-cache`.
fn store_for(engine: &Engine, workdir: &Path) -> Store<HostState> {
    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen workdir");
    Store::new(
        engine,
        HostState { table: ResourceTable::new(), wasi: builder.build() },
    )
}

/// One independent instantiation + `run` call against `workdir`. `data_payload` is the
/// operation object (what the host places in `ToolInput.data`). Returns the decoded
/// old-protocol JSON envelope carried in `ToolResult.data`, plus status and metadata.
fn run_editor(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    data_payload: &str,
) -> (Status, Value, Vec<(String, String)>) {
    let mut store = store_for(engine, workdir);
    let tool = Tool::instantiate(&mut store, component, linker).expect("instantiate component");
    let input = ToolInput { data: Some(data_payload.to_string()), log_path: None };
    let result = tool
        .murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run");
    let payload: Value = result
        .data
        .as_deref()
        .map(|s| serde_json::from_str(s).expect("ToolResult.data is JSON"))
        .unwrap_or(Value::Null);
    (result.status, payload, result.metadata)
}

fn unique_workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("murmur_editor_wasm_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

#[test]
fn component_loads_and_runs_read_file() {
    // `Component::from_file` succeeding IS the artifact-validation gate.
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("smoke");
    std::fs::write(workdir.join("hello.txt"), "hi there").unwrap();

    let (status, payload, meta) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"read_file","path":"hello.txt"}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    // ToolResult.data carries the operation's `data` field (as the host's native dispatch
    // mapped it) — for a read miss that is `{"content":...,"cache_ref":...}`.
    assert_eq!(payload["content"], "hi there", "payload: {payload}");
    assert!(payload["cache_ref"].is_string());
    assert!(
        meta.iter().any(|(k, v)| k == "state_effect" && v == "read"),
        "expected state_effect=read, got {meta:?}"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_loads_and_runs_write_file() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("write");

    let (status, payload, meta) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"write_file","path":"out.txt","content":"written via wasm"}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    // write_file's `data` field is null in the old protocol, so ToolResult.data is None.
    assert!(payload.is_null(), "payload: {payload}");
    assert!(
        meta.iter().any(|(k, v)| k == "state_effect" && v == "mutate"),
        "expected state_effect=mutate, got {meta:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join("out.txt")).unwrap(),
        "written via wasm"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_loads_and_runs_replace_in_file() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("replace");
    std::fs::write(workdir.join("patch.txt"), "hello world, hello again").unwrap();

    let (status, payload, meta) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"replace_in_file","path":"patch.txt","old_string":"hello","new_string":"goodbye"}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(payload["count"], 2, "payload: {payload}");
    assert!(
        meta.iter().any(|(k, v)| k == "state_effect" && v == "mutate"),
        "expected state_effect=mutate, got {meta:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join("patch.txt")).unwrap(),
        "goodbye world, goodbye again"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn read_cache_persists_across_component_instantiations() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("cache");
    std::fs::write(workdir.join("target.txt"), "hello from the disk cache").unwrap();

    let payload = r#"{"operation":"read_file","path":"target.txt"}"#;

    // Instantiation #1 — cache miss: full content plus a cache_ref.
    let (s1, first, _m) = run_editor(&eng, &component, &lnk, &workdir, payload);
    assert!(matches!(s1, Status::Passed), "first read status: {s1:?}");
    assert_eq!(first["content"], "hello from the disk cache");
    let cache_ref = first["cache_ref"]
        .as_str()
        .expect("first read must return a cache_ref")
        .to_string();

    // Instantiation #2 — a *separate* Store against the unchanged file: ref, no content.
    // This is the scenario an in-memory cache could never satisfy.
    let (s2, second, _m) = run_editor(&eng, &component, &lnk, &workdir, payload);
    assert!(matches!(s2, Status::Passed), "second read status: {s2:?}");
    assert_eq!(
        second["cache_ref"].as_str().unwrap(),
        cache_ref,
        "cache_ref must be stable across instantiations"
    );
    assert!(
        second["content"].is_null(),
        "cache hit must NOT resend content, got: {}",
        second["content"]
    );

    // Rewrite the file; mtime granularity can be 1s, so wait first. The (path, range,
    // mtime) key changes, so instantiation #3 is a miss returning fresh content.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    std::fs::write(workdir.join("target.txt"), "changed content").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let (s3, third, _m) = run_editor(&eng, &component, &lnk, &workdir, payload);
    assert!(matches!(s3, Status::Passed), "third read status: {s3:?}");
    assert_eq!(
        third["content"], "changed content",
        "post-rewrite read must return fresh content, not a stale pointer"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

/// Build a synthetic tree of `dirs * files_per_dir` small text files under `root`, one of
/// which contains the marker `NEEDLE`. Returns the file count.
fn build_tree(root: &Path, dirs: usize, files_per_dir: usize) -> usize {
    let mut n = 0;
    for d in 0..dirs {
        let dd = root.join(format!("dir{d:03}"));
        std::fs::create_dir_all(&dd).unwrap();
        for f in 0..files_per_dir {
            let mut body = String::with_capacity(40 * 60);
            for l in 0..40 {
                if d == dirs / 2 && f == 3 && l == 5 {
                    body.push_str("this line contains the NEEDLE marker here\n");
                } else {
                    body.push_str(&format!(
                        "ordinary line {l} in file {f} dir {d} with filler lorem ipsum dolor\n"
                    ));
                }
            }
            std::fs::write(dd.join(format!("file{f:03}.txt")), body).unwrap();
            n += 1;
        }
    }
    n
}

#[test]
fn find_in_files_benchmark_no_orders_of_magnitude_regression() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("bench");
    let tree = workdir.join("bench_tree");
    let n_files = build_tree(&tree, 60, 50); // 3000 files

    // wasm: the walk uses a workdir-relative dir under the single preopen.
    let rel_payload = r#"{"operation":"find_in_files","pattern":"NEEDLE","dir":"bench_tree","recursive":true}"#;

    // Host-native baseline: the SAME logic, host-compiled, over the same tree via an
    // absolute path (host `logic::run` takes the stdin-envelope shape).
    let abs_dir = tree.to_string_lossy().to_string();
    let envelope = serde_json::json!({
        "data": { "operation": "find_in_files", "pattern": "NEEDLE", "dir": abs_dir, "recursive": true }
    })
    .to_string();

    // Warm both paths once, then time the walk only (instantiation excluded — that is the
    // fair "latency of the walk" comparison; instantiation overhead is reported separately).
    let _ = murmur_tool_editor::logic::run(&envelope);
    let t = Instant::now();
    let host_out = murmur_tool_editor::logic::run(&envelope);
    let host_walk = t.elapsed();
    assert_eq!(host_out["ok"], true, "host find must succeed: {host_out}");
    assert_eq!(host_out["matches"].as_array().map(|a| a.len()), Some(1));

    let mut store = store_for(&eng, &workdir);
    let tool = Tool::instantiate(&mut store, &component, &lnk).expect("instantiate");
    let input = ToolInput { data: Some(rel_payload.to_string()), log_path: None };
    let _ = tool.murmur_tool_run().call_run(&mut store, &input).unwrap(); // warm
    let t = Instant::now();
    let wasm_res = tool.murmur_tool_run().call_run(&mut store, &input).unwrap();
    let wasm_walk = t.elapsed();
    assert!(matches!(wasm_res.status, Status::Passed), "wasm find status: {:?}", wasm_res.status);
    // ToolResult.data for find is the `data` field: `{"matches":[...]}`.
    let wasm_payload: Value = serde_json::from_str(wasm_res.data.as_deref().unwrap()).unwrap();
    assert_eq!(wasm_payload["matches"].as_array().map(|a| a.len()), Some(1));

    // Full instantiate+call, for the process-spawn analogue number.
    let t = Instant::now();
    let _ = run_editor(&eng, &component, &lnk, &workdir, rel_payload);
    let wasm_full = t.elapsed();

    let ratio = wasm_walk.as_secs_f64() / host_walk.as_secs_f64().max(1e-9);
    eprintln!(
        "BENCH find_in_files files={n_files} host_walk={host_walk:?} \
         wasm_walk={wasm_walk:?} wasm_instantiate+call={wasm_full:?} ratio={ratio:.2}x"
    );

    // No order-of-magnitude regression. A generous 30x bound (plus a 0.5s floor slack so a
    // sub-millisecond host time can't make the ratio explode on a loaded CI box) catches a
    // catastrophic regression while staying non-flaky; the recorded numbers are the real
    // engineering gate (see the build summary).
    assert!(
        wasm_walk.as_secs_f64() < host_walk.as_secs_f64() * 30.0 + 0.5,
        "wasm find_in_files regressed by >30x: host={host_walk:?} wasm={wasm_walk:?}"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}
