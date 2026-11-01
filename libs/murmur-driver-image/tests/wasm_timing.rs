//! How long the image pre-pass takes inside a wasm32-wasip2 component, beside the same code
//! native.
//!
//! Builds examples/timing.rs twice in release — for wasm32-wasip2 and for the host — runs the
//! wasm command under wasmtime 46 and the host binary directly, and prints the median and range of
//! each case. It asserts nothing about speed: it is a measurement, run by hand with
//!
//!     cargo test -p murmur-driver-image --test wasm_timing -- --ignored --nocapture
//!
//! `MURMUR_IMAGE_TIMING_RUNS` sets the runs per case (default 7).
#![cfg(not(target_arch = "wasm32"))]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::bindings::sync::Command as WasiCommand;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn build_example(target: Option<&str>) {
    let mut cargo = Command::new(env!("CARGO"));
    cargo.args([
        "build",
        "--release",
        "-p",
        "murmur-driver-image",
        "--example",
        "timing",
        "--features",
        "fixtures",
    ]);
    if let Some(target) = target {
        cargo.args(["--target", target]);
    }
    let status = cargo
        .current_dir(workspace_root())
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "cargo build of the timing example failed");
}

fn run_wasm(runs: &str) -> String {
    build_example(Some("wasm32-wasip2"));
    let wasm = target_dir().join("wasm32-wasip2/release/examples/timing.wasm");
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");
    let component = Component::from_file(&engine, &wasm).expect("load the timing component");
    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");

    let stdout = MemoryOutputPipe::new(1 << 20);
    let mut builder = WasiCtxBuilder::new();
    builder
        .args(&["timing", runs])
        .stdout(stdout.clone())
        .inherit_stderr();
    let mut store = Store::new(
        &engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
        },
    );
    let command = WasiCommand::instantiate(&mut store, &component, &linker).expect("instantiate");
    command
        .wasi_cli_run()
        .call_run(&mut store)
        .expect("call run")
        .expect("the timing command exits 0");
    drop(store);
    String::from_utf8(stdout.contents().to_vec()).expect("utf-8 output")
}

fn run_native(runs: &str) -> String {
    build_example(None);
    let output = Command::new(target_dir().join("release/examples/timing"))
        .arg(runs)
        .output()
        .expect("run the native timing binary");
    assert!(output.status.success(), "the native timing binary failed");
    String::from_utf8(output.stdout).expect("utf-8 output")
}

fn summary(line: &Value) -> String {
    let mut runs: Vec<f64> = line["runs_ms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run.as_f64().unwrap())
        .collect();
    runs.sort_by(f64::total_cmp);
    let median = if runs.len() % 2 == 1 {
        runs[runs.len() / 2]
    } else {
        (runs[runs.len() / 2 - 1] + runs[runs.len() / 2]) / 2.0
    };
    format!(
        "{median:.1} ms ({:.1}–{:.1})",
        runs[0],
        runs[runs.len() - 1]
    )
}

#[test]
#[ignore = "a measurement: builds two release binaries and takes about a minute"]
fn image_prepass_timing_under_wasmtime() {
    let runs = std::env::var("MURMUR_IMAGE_TIMING_RUNS").unwrap_or_else(|_| "7".to_string());
    let wasm = run_wasm(&runs);
    let native = run_native(&runs);
    let parse = |text: &str| -> Vec<Value> {
        text.lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect()
    };
    let (wasm, native) = (parse(&wasm), parse(&native));
    assert_eq!(wasm.len(), 4);
    assert_eq!(native.len(), 4);

    println!("runs per case: {runs}");
    println!("| Case | Sent | Sent bytes | wasm32-wasip2 (wasmtime 46) median (range) | native median (range) |");
    println!("|---|---|---|---|---|");
    for (w, n) in wasm.iter().zip(&native) {
        assert_eq!(w["case"], n["case"]);
        assert_eq!(
            w["sent_bytes"], n["sent_bytes"],
            "wasm and native output differ"
        );
        println!(
            "| {} | {} | {} | {} | {} |",
            w["case"].as_str().unwrap(),
            w["sent"].as_str().unwrap(),
            w["sent_bytes"],
            summary(w),
            summary(n)
        );
    }
    println!("input PNG bytes: {}", wasm[0]["input_png_bytes"]);
}
