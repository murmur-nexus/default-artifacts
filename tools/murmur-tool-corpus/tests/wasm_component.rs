//! Host-target integration test for the compiled `murmur-tool-corpus` **wasm32-wasip2
//! component**, run through Wasmtime.
//!
//! Two things can only be proved here and not by the host unit tests:
//!
//! * The corpus is genuinely durable across *independent* instantiations. A record
//!   appended in one `Store` is readable from a fresh `Store` against the same state
//!   directory — the component-model analogue of two separate tool dispatches.
//! * The tool writes nothing outside the durable-state preopen, and fails closed when
//!   that preopen is absent instead of quietly building a corpus inside the workdir the
//!   agent can rewrite.
//!
//! The runtime does not grant a `state/` preopen today (`capsule-runtime` gives a tool
//! exactly one, the workdir at `.`), which is why this test builds its own `WasiCtx` with
//! two. `Component::from_file` succeeding is also the artifact-validation gate — there is
//! no `wasm-tools` CLI dependency here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use serde_json::{json, Value};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// Host-side bindings for `world tool` (murmur:tool/run). The component imports zero
// murmur:* interfaces (only wasi:*, provided by wasmtime-wasi below), so the linker only
// needs WASI.
mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput};
use bindings::Tool;

/// Guest path the component mounts its durable state at. Mirrors
/// `murmur_tool_corpus::STATE_DIR`, which the cdylib exports but this host-target test
/// links against directly.
const STATE_GUEST_PATH: &str = murmur_tool_corpus::STATE_DIR;

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
static DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Path to the compiled component, building it if absent. `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so the test cannot assume the artifact
/// already exists.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/murmur_tool_corpus.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "murmur-tool-corpus",
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

/// A fresh Store with the workdir at `.` — what the capsule runtime grants today — plus,
/// when `state` is `Some`, a second preopen at the guest path the corpus lives under.
/// That second preopen stands in for the `capabilities.state` grant.
fn store_for(engine: &Engine, workdir: &Path, state: Option<&Path>) -> Store<HostState> {
    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen workdir");
    if let Some(state) = state {
        builder
            .preopened_dir(state, STATE_GUEST_PATH, DirPerms::all(), FilePerms::all())
            .expect("preopen state dir");
    }
    Store::new(
        engine,
        HostState { table: ResourceTable::new(), wasi: builder.build() },
    )
}

/// One independent instantiation + `run` call. Returns the status, the decoded envelope
/// from `ToolResult.data`, and the metadata list.
fn run_corpus(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    state: Option<&Path>,
    payload: Value,
) -> (Status, Value, Vec<(String, String)>) {
    let mut store = store_for(engine, workdir, state);
    let tool = Tool::instantiate(&mut store, component, linker).expect("instantiate component");
    let input = ToolInput { data: Some(payload.to_string()), log_path: None };
    let result = tool
        .murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run");
    let envelope: Value = result
        .data
        .as_deref()
        .map(|s| serde_json::from_str(s).expect("ToolResult.data is JSON"))
        .unwrap_or(Value::Null);
    (result.status, envelope, result.metadata)
}

struct Fixture {
    root: PathBuf,
    workdir: PathBuf,
    state: PathBuf,
}

/// A throwaway workdir plus a separate persistent state directory carrying the operator
/// config. The two are siblings, so anything the tool writes into the workdir is visible
/// as a stray entry.
fn fixture(tag: &str, with_state: bool) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "murmur_corpus_wasm_{tag}_{}_{}",
        std::process::id(),
        DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let workdir = root.join("workdir");
    let state = root.join("state");
    std::fs::create_dir_all(&workdir).expect("create workdir");
    if with_state {
        std::fs::create_dir_all(&state).expect("create state dir");
        std::fs::write(
            state.join("corpus.config.json"),
            json!({
                "config_version": 1,
                "read_recent": { "default": 5, "max": 20 },
                "search": { "default_k": 3, "max_k": 10 },
                "types": {
                    "note": {
                        "schema_version": 1,
                        "schema": {
                            "type": "object",
                            "required": ["text"],
                            "properties": { "text": { "type": "string" } },
                            "additionalProperties": false
                        }
                    },
                    "withdrawal": {
                        "schema_version": 1,
                        "schema": {
                            "type": "object",
                            "required": ["reason"],
                            "properties": { "reason": { "type": "string" } },
                            "additionalProperties": false
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write operator config");
    }
    Fixture { root, workdir, state }
}

fn workdir_entries(workdir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(workdir)
        .expect("read workdir")
        .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_corpus_survives_independent_instantiations() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("durable", true);

    // Instantiation #1 — append.
    let (status, envelope, meta) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "append", "type": "note", "body": { "text": "durable across stores" } }),
    );
    assert!(matches!(status, Status::Passed), "append status: {status:?} {envelope}");
    let id = envelope["id"].as_str().expect("an id").to_string();
    assert_eq!(envelope["deduped"], false);
    assert!(
        meta.iter().any(|(k, v)| k == "state_effect" && v == "mutate"),
        "expected state_effect=mutate, got {meta:?}"
    );
    assert!(
        meta.iter().any(|(k, v)| k == "resource_id" && v == &format!("corpus:{id}")),
        "expected resource_id=corpus:{id}, got {meta:?}"
    );

    // Instantiation #2 — a *separate* Store reads it back. An in-memory store could never
    // satisfy this.
    let (status, envelope, _meta) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "read_recent", "type": "note", "n": 5 }),
    );
    assert!(matches!(status, Status::Passed), "read_recent status: {status:?} {envelope}");
    assert_eq!(envelope["returned"], 1, "{envelope}");
    assert_eq!(envelope["records"][0]["id"], id);
    assert_eq!(envelope["records"][0]["body"]["text"], "durable across stores");

    // Instantiation #3 — `get` resolves the same record.
    let (status, envelope, _meta) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "get", "id": id }),
    );
    assert!(matches!(status, Status::Passed), "get status: {status:?} {envelope}");
    assert_eq!(envelope["record"]["id"], id);
    assert_eq!(envelope["record"]["schema_version"], 1);

    // Every byte the tool wrote landed in the state directory.
    assert_eq!(
        workdir_entries(&f.workdir),
        Vec::<String>::new(),
        "the tool must write nothing into the workdir"
    );
    assert!(f.state.join("corpus.jsonl").exists(), "the corpus lives under the state grant");

    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn a_withdrawal_in_one_instantiation_is_visible_to_the_next() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("withdraw", true);

    let (_s, envelope, _m) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "append", "type": "note", "body": { "text": "to be retracted" } }),
    );
    let target = envelope["id"].as_str().expect("an id").to_string();

    let (status, envelope, _m) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "append", "type": "withdrawal",
                "body": { "reason": "superseded" }, "withdraws": target }),
    );
    assert!(matches!(status, Status::Passed), "withdrawal status: {status:?} {envelope}");
    let withdrawal_id = envelope["id"].as_str().expect("an id").to_string();

    // A fourth, independent instantiation sees the withdrawal.
    let (status, envelope, _m) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "get", "id": target }),
    );
    assert!(matches!(status, Status::Passed), "get status: {status:?} {envelope}");
    assert_eq!(envelope["record"]["body"], Value::Null);
    assert_eq!(envelope["record"]["withdrawn_by"], withdrawal_id);
    assert!(envelope["record"]["withdrawn_at"].is_string());

    let (_s, envelope, _m) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "read_recent", "type": "note", "n": 5 }),
    );
    assert_eq!(envelope["returned"], 0, "a withdrawn record drops out of read_recent");

    assert_eq!(workdir_entries(&f.workdir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn without_a_state_preopen_every_operation_fails_closed_and_creates_nothing() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("no_state", false);

    for payload in [
        json!({ "operation": "append", "type": "note", "body": { "text": "x" } }),
        json!({ "operation": "get", "id": "not_00000000000000000000000000000000" }),
        json!({ "operation": "read_recent", "type": "note", "n": 5 }),
        json!({ "operation": "search", "query": "anything" }),
    ] {
        let (status, envelope, _meta) =
            run_corpus(&eng, &component, &lnk, &f.workdir, None, payload.clone());
        assert!(
            matches!(status, Status::Error),
            "{payload} expected status error, got {status:?}: {envelope}"
        );
        assert_eq!(envelope["error_kind"], "state_unavailable", "{payload} -> {envelope}");
        assert_eq!(envelope["ok"], false);
        assert!(
            envelope["message"].as_str().unwrap_or_default().contains("capabilities.state"),
            "message must name the grant: {envelope}"
        );
    }

    // Without the grant, `state/corpus.jsonl` would resolve *inside* the workdir preopen.
    // Nothing may appear there — a corpus the agent can rewrite is the failure this
    // artifact exists to prevent.
    assert_eq!(
        workdir_entries(&f.workdir),
        Vec::<String>::new(),
        "the tool created something in the workdir"
    );
    assert!(!f.workdir.join(STATE_GUEST_PATH).exists(), "the tool created a state directory");

    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn search_runs_through_the_real_component() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("search", true);

    for text in ["the rollback plan for the release", "rollback only", "unrelated entirely"] {
        let (status, envelope, _m) = run_corpus(
            &eng,
            &component,
            &lnk,
            &f.workdir,
            Some(&f.state),
            json!({ "operation": "append", "type": "note", "body": { "text": text } }),
        );
        assert!(matches!(status, Status::Passed), "append status: {status:?} {envelope}");
    }

    let (status, envelope, meta) = run_corpus(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        json!({ "operation": "search", "query": "rollback plan", "k": 5 }),
    );
    assert!(matches!(status, Status::Passed), "search status: {status:?} {envelope}");
    let hits = envelope["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 2, "{envelope}");
    assert_eq!(hits[0]["score"], 1.0);
    assert_eq!(hits[1]["score"], 0.5);
    assert!(
        meta.iter().any(|(k, v)| k == "resource_id" && v == "corpus:search:rollback plan"),
        "{meta:?}"
    );
    assert!(meta.iter().any(|(k, v)| k == "state_effect" && v == "read"), "{meta:?}");

    assert_eq!(workdir_entries(&f.workdir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&f.root);
}
