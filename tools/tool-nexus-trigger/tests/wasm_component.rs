//! The compiled `tool-nexus-trigger` wasm32-wasip2 component, run through Wasmtime with a temp
//! folder preopened as `.`, the way the runtime mounts an entry's
//! `capabilities.filesystem.scope`.
//!
//! What only the guest can show: that its relative paths land in the `.` preopen, that
//! `wasi:random` and `wasi:clocks` drive the request id and the wait (a no-answer call really
//! waits about [`ANSWER_WAIT`]), and that a `murmur-runtime` preopen, which the runtime hands only
//! an entry without a scope, makes it refuse before writing.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tool_nexus_trigger::answer::{self, AWAITS_APPROVAL};
use tool_nexus_trigger::inbox::{ANSWER_WAIT, POLL_INTERVAL};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use common::{listing, FakeNexus, Mode};

mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput, ToolResult};
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

static BUILD: Once = Once::new();

/// The compiled component, built if absent: `cargo test` runs before any wasm build step.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-nexus-trigger",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(manifest.join("../.."))
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(status.success(), "cargo build of the wasm component failed");
    });
    let wasm = target.join("wasm32-wasip2/release/tool_nexus_trigger.wasm");
    assert!(wasm.exists(), "no component at {}", wasm.display());
    wasm
}

struct Guest {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
}

impl Guest {
    fn load() -> Self {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        let component = Component::from_file(&engine, component_path()).expect("component loads");
        let mut linker: Linker<HostState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
        Self {
            engine,
            component,
            linker,
        }
    }

    /// One instantiation and one `run`, with `inbox` at `.` and each `(host, name)` extra preopen
    /// read-only.
    fn run(&self, inbox: &Path, extra: &[(&Path, &str)], input: Value) -> ToolResult {
        let mut builder = WasiCtxBuilder::new();
        builder
            .preopened_dir(inbox, ".", DirPerms::all(), FilePerms::all())
            .expect("preopen the inbox");
        for (host, name) in extra {
            builder
                .preopened_dir(host, name, DirPerms::READ, FilePerms::READ)
                .expect("preopen an extra root");
        }
        let mut store = Store::new(
            &self.engine,
            HostState {
                table: ResourceTable::new(),
                wasi: builder.build(),
            },
        );
        let tool =
            Tool::instantiate(&mut store, &self.component, &self.linker).expect("instantiate");
        let input = ToolInput {
            data: Some(input.to_string()),
            log_path: None,
        };
        tool.murmur_tool_run()
            .call_run(&mut store, &input)
            .expect("call run")
    }
}

fn data(result: &ToolResult) -> Value {
    serde_json::from_str(result.data.as_deref().expect("data")).expect("data is JSON")
}

#[test]
fn an_add_is_answered_and_the_inbox_is_left_empty() {
    let guest = Guest::load();
    let dir = tempfile::tempdir().unwrap();
    let nexus = FakeNexus::start(dir.path(), Mode::Answer);

    let added = guest.run(
        dir.path(),
        &[],
        json!({"op": "add", "label": "ready", "every": "30s", "for": "1h", "until": "test -e ready.txt"}),
    );
    assert!(matches!(added.status, Status::Passed), "{added:?}");
    let summary = added.summary.clone().unwrap();
    assert!(summary.starts_with(AWAITS_APPROVAL), "{summary}");
    assert!(summary.contains("trigger t1 awaits approval"));
    assert_eq!(data(&added)["op"], "add");
    assert_eq!(data(&added)["summary"], summary.as_str());
    assert!(added.attachments.is_empty());

    let raw = nexus.raw_requests();
    assert_eq!(raw.len(), 1);
    assert_eq!(
        String::from_utf8(raw[0].clone()).unwrap(),
        r#"{"v":1,"op":"add","until":"test -e ready.txt","every":"30s","for":"1h","label":"ready"}"#
    );

    let listed = guest.run(dir.path(), &[], json!({"op": "list"}));
    assert!(matches!(listed.status, Status::Passed));
    assert!(listed
        .summary
        .unwrap()
        .starts_with("t1  awaiting-approval  ready"));
    let cancelled = guest.run(
        dir.path(),
        &[],
        json!({"op": "cancel", "id": "t1", "yes": true}),
    );
    assert!(matches!(cancelled.status, Status::Passed));
    assert_eq!(cancelled.summary.unwrap(), "cancelled trigger t1 · ready");

    assert!(listing(dir.path()).is_empty());
}

#[test]
fn no_answer_waits_the_answer_wait_on_the_guest_clock_and_cleans_up() {
    let guest = Guest::load();
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let result = guest.run(dir.path(), &[], json!({"op": "add", "until": "true"}));
    let took = started.elapsed();
    eprintln!("guest no-answer call took {took:?}");
    assert!(matches!(result.status, Status::Error), "{result:?}");
    assert_eq!(result.summary.as_deref(), Some(answer::NO_ANSWER));
    assert!(took >= ANSWER_WAIT, "the guest waited {took:?}");
    assert!(
        took < ANSWER_WAIT + POLL_INTERVAL + Duration::from_millis(1500),
        "the guest waited {took:?}"
    );
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn a_murmur_runtime_preopen_means_the_whole_work_folder() {
    let guest = Guest::load();
    let dir = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let nexus = FakeNexus::start(dir.path(), Mode::Answer);
    let result = guest.run(
        dir.path(),
        &[(runtime.path(), "murmur-runtime")],
        json!({"op": "add", "until": "true"}),
    );
    assert!(matches!(result.status, Status::Error));
    assert_eq!(result.summary.as_deref(), Some(answer::WHOLE_WORKDIR));
    std::thread::sleep(Duration::from_millis(50));
    assert!(listing(dir.path()).is_empty());
    assert!(listing(runtime.path()).is_empty());
    assert!(nexus.requests().is_empty());
}
