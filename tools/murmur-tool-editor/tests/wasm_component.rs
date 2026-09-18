//! Host-target integration test for the compiled `murmur-tool-editor` **wasm32-wasip2
//! component**, run through Wasmtime.
//!
//! Every call runs in its own Wasmtime `Store`/instantiation against a preopened workdir —
//! the component-model analogue of the one-op-per-dispatch reality of this tool. Across
//! separate instantiations it proves that a repeat `read_file` of an unchanged file returns
//! the content again, that reads leave the workdir's entries exactly as they found them, and
//! that a `.murmur-tool-editor-cache/` left behind by an earlier version is neither read nor
//! modified.
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
/// is deliberately NOT inherited, so the component sees nothing of the test process's
/// environment.
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
    let (status, payload, _summary, meta) =
        run_editor_reporting(engine, component, linker, workdir, data_payload);
    (status, payload, meta)
}

/// [`run_editor`] plus `ToolResult.summary` — the field a refusal's message travels in, since a
/// failing envelope carries a null `data`.
fn run_editor_reporting(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    data_payload: &str,
) -> (Status, Value, Option<String>, Vec<(String, String)>) {
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
    (result.status, payload, result.summary, result.metadata)
}

/// The workdir's immediate entries, sorted — the "did the call leave anything behind" check.
fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read workdir")
        .map(|e| e.expect("dir entry").file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// A preopen workdir at `.test-scratch/wasm_<tag>_<pid>`, relative to the process CWD (the
/// crate root, under `cargo test`). Relative rather than under `std::env::temp_dir()` because
/// the tool refuses an absolute path input: the host-native baseline in the benchmark below
/// addresses the same tree through `logic::run`, which only accepts a workdir-relative value.
/// Gitignored; each test removes its own subtree.
fn unique_workdir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(".test-scratch").join(format!("wasm_{tag}_{}", std::process::id()));
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
    // mapped it) — for a whole-file read that is exactly `{"content":...,"total_lines":...}`.
    assert_eq!(
        payload,
        serde_json::json!({ "content": "hi there", "total_lines": 1 }),
        "payload: {payload}"
    );
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
        r#"{"operation":"write_file","dest_path":"out.txt","content":"written via wasm"}"#,
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
        r#"{"operation":"replace_in_file","dest_path":"patch.txt","old_string":"hello","new_string":"goodbye"}"#,
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

// The two absolute-path tests below are the ones that need the real preopen. On the host an
// absolute value escapes the fixture tree and fails on its own; under the single preopen a tool
// is dispatched with, `/app/results.txt` resolves to `<workdir>/app/results.txt` — which is
// exactly why the tool used to create it, report success, and read it back "correctly" after.
//
// `error_kind` is a field of the old-protocol envelope, which the WIT adapter maps to
// `Status::Error` plus the message in `summary` (the envelope's `data` is null on a failure, so
// `ToolResult.data` is None). These assert what crosses the component boundary; the
// `error_kind: "absolute_path"` value itself is pinned by the unit tests in `src/lib.rs`.
#[test]
fn component_refuses_an_absolute_dest_path_and_leaves_the_workdir_untouched() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("absolute_write");
    let before = dir_entries(&workdir);

    let (status, payload, summary, meta) = run_editor_reporting(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"write_file","dest_path":"/app/results.txt","content":"written to the wrong place"}"#,
    );
    assert!(matches!(status, Status::Error), "status: {status:?}");
    assert!(payload.is_null(), "payload: {payload}");
    assert_eq!(
        summary.as_deref(),
        Some(
            "'dest_path' must be relative to the capsule workdir; got '/app/results.txt' \
             (did you mean 'results.txt'?)"
        ),
    );
    assert!(
        meta.is_empty(),
        "a refused write declares no state_effect, got {meta:?}"
    );

    assert_eq!(
        dir_entries(&workdir),
        before,
        "a refused write must leave the workdir exactly as it found it — no `app`, no results.txt"
    );
    assert!(!workdir.join("app").exists());
    assert!(!workdir.join("results.txt").exists());

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_refuses_an_absolute_read_path_even_when_the_file_exists() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("absolute_read");
    // Created directly, not through the tool: the file an absolute write used to manufacture is
    // present, so the refusal cannot be a not-found in disguise.
    std::fs::create_dir_all(workdir.join("app")).unwrap();
    std::fs::write(workdir.join("app/results.txt"), "content from the wrong place").unwrap();

    let (status, payload, summary, _meta) = run_editor_reporting(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"read_file","path":"/app/results.txt"}"#,
    );
    assert!(matches!(status, Status::Error), "status: {status:?}");
    assert!(
        payload.is_null(),
        "a refused read must not return the file's content: {payload}"
    );
    let msg = summary.expect("a refusal carries its message in summary");
    assert!(
        msg.starts_with("'path' must be relative to the capsule workdir; got '/app/results.txt'"),
        "the refusal names `path`, the property this operation reads: {msg}"
    );

    // The same value read relatively still resolves — the refusal is about the spelling of the
    // input, not about the file being unreachable.
    let (ok_status, ok_payload, _m) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"read_file","path":"app/results.txt"}"#,
    );
    assert!(matches!(ok_status, Status::Passed), "status: {ok_status:?}");
    assert_eq!(ok_payload["content"], "content from the wrong place");

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn repeat_reads_across_instantiations_return_content_and_leave_the_workdir_untouched() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("repeat_read");
    std::fs::write(workdir.join("target.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
    let before = dir_entries(&workdir);

    let whole = r#"{"operation":"read_file","path":"target.txt"}"#;
    let ranged = r#"{"operation":"read_file","path":"target.txt","start_line":2,"end_line":3}"#;

    // Each call is a separate Store against the same preopened workdir, so nothing an earlier
    // instantiation held in memory can reach a later one.
    for _ in 0..2 {
        let (status, payload, meta) = run_editor(&eng, &component, &lnk, &workdir, whole);
        assert!(matches!(status, Status::Passed), "status: {status:?}");
        assert_eq!(
            payload,
            serde_json::json!({ "content": "l1\nl2\nl3\nl4\nl5\n", "total_lines": 5 }),
            "every whole-file read returns the bytes"
        );
        assert!(
            meta.iter().any(|(k, v)| k == "state_effect" && v == "read"),
            "expected state_effect=read, got {meta:?}"
        );

        let (status, payload, _meta) = run_editor(&eng, &component, &lnk, &workdir, ranged);
        assert!(matches!(status, Status::Passed), "status: {status:?}");
        assert_eq!(
            payload,
            serde_json::json!({
                "content": "l2\nl3",
                "total_lines": 5,
                "start_line": 2,
                "end_line": 3,
            }),
            "every ranged read returns the bytes"
        );
    }

    // A same-length rewrite with no pause is visible to the very next read.
    std::fs::write(workdir.join("target.txt"), "L1\nL2\nL3\nL4\nL5\n").unwrap();
    let (_s, payload, _m) = run_editor(&eng, &component, &lnk, &workdir, whole);
    assert_eq!(payload["content"], "L1\nL2\nL3\nL4\nL5\n");

    assert_eq!(
        dir_entries(&workdir),
        before,
        "read_file must leave the workdir's entries exactly as it found them"
    );
    assert!(!workdir.join(".murmur-tool-editor-cache").exists());

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn a_leftover_cache_directory_from_an_earlier_version_is_inert() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("stale_cache");
    std::fs::write(workdir.join("target.txt"), "fresh bytes").unwrap();

    // The shape a 0.4.0 session leaves behind: small JSON entries plus an arbitrary file.
    let stale = workdir.join(".murmur-tool-editor-cache");
    std::fs::create_dir_all(&stale).unwrap();
    let seeded: Vec<(&str, &[u8])> = vec![
        ("00000000deadbeef.json", br#"{"key":"target.txt\u0000\u0000\u00000","cache_id":"cache_b"}"#),
        ("0123456789abcdef.json", b"{ not json at all"),
        ("ffffffffffffffff.json", b""),
    ];
    for (name, bytes) in &seeded {
        std::fs::write(stale.join(name), bytes).unwrap();
    }
    let before = dir_entries(&stale);

    let (status, payload, _meta) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"read_file","path":"target.txt"}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(payload["content"], "fresh bytes", "payload: {payload}");

    assert_eq!(dir_entries(&stale), before, "the stale directory's entries are untouched");
    for (name, bytes) in &seeded {
        assert_eq!(&std::fs::read(stale.join(name)).unwrap(), bytes, "{name} was modified");
    }

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_loads_and_runs_ranged_read_file() {
    // A bounded read through the real compiled component: only the requested line span comes
    // back, alongside the file's total_lines and the resolved range.
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("ranged");
    std::fs::write(workdir.join("mod.txt"), "a\nb\nc\nd\ne\n").unwrap();

    let (status, payload, _meta) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"read_file","path":"mod.txt","start_line":2,"end_line":4}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(payload["content"], "b\nc\nd", "payload: {payload}");
    assert_eq!(payload["total_lines"], 5);
    assert_eq!(payload["start_line"], 2);
    assert_eq!(payload["end_line"], 4);

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_loads_and_runs_find_with_context_lines() {
    // context_lines=2 through the real component attaches context_before/context_after; a
    // control call with context_lines omitted carries neither key (backward-compatible shape).
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let workdir = unique_workdir("ctx");
    let sub = workdir.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("f.txt"), "L1\nL2\nL3 needle\nL4\nL5\n").unwrap();

    // With context.
    let (status, payload, _meta) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"find_in_files","pattern":"needle","dir":"src","recursive":false,"context_lines":2}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    let m = &payload["matches"][0];
    assert_eq!(m["line"], 3, "payload: {payload}");
    assert_eq!(m["context_before"], serde_json::json!(["L1", "L2"]));
    assert_eq!(m["context_after"], serde_json::json!(["L4", "L5"]));

    // Without context: no context keys at all (byte-for-byte the historical shape).
    let (status2, payload2, _m2) = run_editor(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"operation":"find_in_files","pattern":"needle","dir":"src","recursive":false}"#,
    );
    assert!(matches!(status2, Status::Passed), "status: {status2:?}");
    let m2 = &payload2["matches"][0];
    assert!(m2.get("context_before").is_none(), "default must omit context_before");
    assert!(m2.get("context_after").is_none(), "default must omit context_after");

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

    // Host-native baseline: the SAME logic, host-compiled, over the same tree. The dir is
    // workdir-relative like the wasm payload's — the tool refuses an absolute value — so both
    // sides walk the identical tree (host `logic::run` takes the stdin-envelope shape).
    let host_dir = tree.to_string_lossy().to_string();
    let envelope = serde_json::json!({
        "data": { "operation": "find_in_files", "pattern": "NEEDLE", "dir": host_dir, "recursive": true }
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
