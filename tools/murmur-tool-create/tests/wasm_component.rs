//! Host-target integration test for the compiled `murmur-tool-create` **wasm32-wasip2
//! component**, run through Wasmtime.
//!
//! Proves the scaffold operation runs end-to-end through the `murmur:tool/run` export of
//! the real compiled artifact (not just the host-side `logic`): instantiating the
//! component against a preopened workdir and calling `run` with a scaffold request creates
//! the expected `tools/<name>/` directory inside that workdir. `Component::from_file`
//! succeeding is itself the artifact-validation gate.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use serde_json::Value;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

static BUILD: Once = Once::new();

fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/murmur_tool_create.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "murmur-tool-create",
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

fn run_create(workdir: &Path, data_payload: &str) -> (Status, Option<String>, Value) {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");
    let component = Component::from_file(&engine, component_path()).expect("load component");

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi");

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen");
    let mut store = Store::new(
        &engine,
        HostState { table: ResourceTable::new(), wasi: builder.build() },
    );

    let tool = Tool::instantiate(&mut store, &component, &linker).expect("instantiate");
    let input = ToolInput { data: Some(data_payload.to_string()), log_path: None };
    let result = tool.murmur_tool_run().call_run(&mut store, &input).expect("run");
    let summary = result.summary.clone();
    let data: Value = result
        .data
        .as_deref()
        .map(|s| serde_json::from_str(s).expect("data JSON"))
        .unwrap_or(Value::Null);
    (result.status, summary, data)
}

fn unique_workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("murmur_create_wasm_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

#[test]
fn scaffold_wasm_tool_through_component() {
    let workdir = unique_workdir("scaffold");
    let (status, summary, data) = run_create(
        &workdir,
        r#"{"type":"tool","name":"csv-parser","runtime":"wasm"}"#,
    );

    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(summary.as_deref(), Some("Created tools/csv-parser/"));
    assert_eq!(data["path"], "tools/csv-parser");

    let base = workdir.join("tools").join("csv-parser");
    assert!(base.join("murmur.yaml").exists(), "murmur.yaml missing");
    assert!(base.join("component.wat").exists(), "component.wat missing");
    assert!(base.join("README.md").exists(), "README.md missing");

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn scaffold_native_tool_through_component() {
    // The literal request from the slice design's Verification Scenarios: a native-runtime
    // scaffold, proven through the real compiled component (not just host-side `logic`).
    let workdir = unique_workdir("scaffold_native");
    let (status, summary, data) = run_create(
        &workdir,
        r#"{"type":"tool","name":"probe-tool","runtime":"native"}"#,
    );

    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(summary.as_deref(), Some("Created tools/probe-tool/"));
    assert_eq!(data["path"], "tools/probe-tool");

    let base = workdir.join("tools").join("probe-tool");
    let manifest = std::fs::read_to_string(base.join("murmur.yaml")).expect("murmur.yaml missing");
    assert!(manifest.contains("name: probe-tool"));
    assert!(manifest.contains("runtime: native"));
    assert!(base.join("bin").join("run").exists(), "bin/run missing");
    assert!(base.join("README.md").exists(), "README.md missing");

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn scaffold_failure_reports_error_status() {
    let workdir = unique_workdir("scaffold_err");
    // Unknown runtime is rejected by scaffold_tool_in and surfaces as an error result.
    let (status, summary, _data) = run_create(
        &workdir,
        r#"{"type":"tool","name":"bad","runtime":"quantum"}"#,
    );
    assert!(matches!(status, Status::Error), "status: {status:?}");
    assert!(
        summary.as_deref().unwrap_or("").contains("scaffold failed"),
        "summary: {summary:?}"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}
