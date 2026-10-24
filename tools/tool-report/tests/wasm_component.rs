//! Host-target integration test for the compiled `tool-report` **wasm32-wasip2
//! component**, run through Wasmtime.
//!
//! Three things can only be proved here and not by the host unit tests:
//!
//! * The report survives *independent* instantiations. A verdict filed in one call is read
//!   back and superseded by the next — the component-model analogue of two separate tool
//!   dispatches against a store that outlives both — and two sessions sharing one store
//!   each write their own file.
//! * The tool writes nothing outside the durable-state preopen, and fails closed when that
//!   preopen is absent instead of quietly writing a report inside the workdir the agent can
//!   rewrite.
//! * The operator configuration and the run identity are read out of the guest environment,
//!   from the variables the runtime delivers them in — a path the host tests, which pass
//!   both in as parameters, never take. Without `MURMUR_SESSION_ID` the call fails closed.
//!
//! The `WasiCtx` here is built by hand: the workdir preopen at `.` the runtime always
//! grants, plus the second preopen `capabilities.state` mounts and the environment
//! variables. `Component::from_file` succeeding is also the artifact-validation gate —
//! there is no `wasm-tools` CLI dependency here.

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
/// `tool_report::STATE_DIR`, which the cdylib exports but this host-target test
/// links against directly.
const STATE_GUEST_PATH: &str = tool_report::STATE_DIR;
const REPORTS_DIR: &str = tool_report::store::REPORTS_DIR;

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
static DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Path to the compiled component, building it if absent. `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so the test cannot assume the artifact
/// already exists.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/tool_report.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-report",
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

/// A fresh Store with the workdir at `.` — what the capsule runtime always grants — plus,
/// when `state` is `Some`, the second preopen standing in for the `capabilities.state`
/// grant, the environment variable an artifact's `config:` block arrives in, and the
/// session and capsule the runtime stamps into every guest environment. `session` of `None`
/// leaves `MURMUR_SESSION_ID` unset.
fn store_for(
    engine: &Engine,
    workdir: &Path,
    state: Option<&Path>,
    config: Option<&str>,
    session: Option<&str>,
) -> Store<HostState> {
    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen workdir");
    if let Some(state) = state {
        builder
            .preopened_dir(state, STATE_GUEST_PATH, DirPerms::all(), FilePerms::all())
            .expect("preopen state dir");
    }
    if let Some(config) = config {
        builder.env(tool_report::ARTIFACT_CONFIG_ENV, config);
    }
    if let Some(session) = session {
        builder.env(tool_report::SESSION_ID_ENV, session);
    }
    builder.env(tool_report::CAPSULE_NAME_ENV, CAPSULE_NAME);
    Store::new(
        engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
        },
    )
}

const SESSION_ID: &str = "sess-wasm-1";
const CAPSULE_NAME: &str = "report-proof-capsule";

/// One independent instantiation + `run` call as [`SESSION_ID`]. Returns the status, the
/// decoded envelope from `ToolResult.data`, and the metadata list.
fn run_report(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    state: Option<&Path>,
    config: Option<&str>,
    payload: Value,
) -> (Status, Value, Vec<(String, String)>) {
    run_as(
        engine,
        component,
        linker,
        workdir,
        state,
        config,
        Some(SESSION_ID),
        payload,
    )
}

/// [`run_report`] under an explicit `MURMUR_SESSION_ID`, or with none.
#[allow(clippy::too_many_arguments)]
fn run_as(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    state: Option<&Path>,
    config: Option<&str>,
    session: Option<&str>,
    payload: Value,
) -> (Status, Value, Vec<(String, String)>) {
    let mut store = store_for(engine, workdir, state, config, session);
    let tool = Tool::instantiate(&mut store, component, linker).expect("instantiate component");
    let input = ToolInput {
        data: Some(payload.to_string()),
        log_path: None,
    };
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

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A throwaway workdir plus a separate persistent state directory. The two are siblings, so
/// anything the tool writes into the workdir is visible as a stray entry.
fn fixture(tag: &str, with_state: bool) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "murmur_report_wasm_{tag}_{}_{}",
        std::process::id(),
        DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let workdir = root.join("workdir");
    let state = root.join("state");
    std::fs::create_dir_all(&workdir).expect("create workdir");
    if with_state {
        std::fs::create_dir_all(&state).expect("create state dir");
    }
    Fixture {
        root,
        workdir,
        state,
    }
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// `session`'s report under the host-side state directory, `reports/<session>.json`.
fn report(state: &Path, session: &str) -> Value {
    let path = state
        .join(REPORTS_DIR)
        .join(tool_report::store::report_file_name(session));
    let text = std::fs::read_to_string(path).expect("read the report");
    serde_json::from_str(&text).expect("the report file is one JSON object")
}

#[test]
fn the_report_survives_independent_instantiations_and_the_verdict_is_revisable() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("durable", true);

    // Instantiation #1 — a progress note, which concludes nothing.
    let (status, envelope, meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        None,
        json!({ "operation": "progress", "note": "Indexed 4 of 9 crates." }),
    );
    assert!(
        matches!(status, Status::Passed),
        "progress status: {status:?} {envelope}"
    );
    assert_eq!(envelope["concluded"], false);
    assert!(
        meta.iter()
            .any(|(k, v)| k == "state_effect" && v == "mutate"),
        "expected state_effect=mutate, got {meta:?}"
    );
    let expected_resource = format!("report:{STATE_GUEST_PATH}/{REPORTS_DIR}/{SESSION_ID}.json");
    assert!(
        meta.iter()
            .any(|(k, v)| k == "resource_id" && v == &expected_resource),
        "expected resource_id={expected_resource}, got {meta:?}"
    );

    // Instantiation #2 — a *separate* Store files the verdict over the same file.
    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        None,
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "Ported the parser and all tests pass.",
            "deliverables": [{ "name": "patch", "kind": "diff", "uri": "out/parser.patch" }]
        }),
    );
    assert!(
        matches!(status, Status::Passed),
        "report status: {status:?} {envelope}"
    );
    assert_eq!(envelope["concluded"], true);
    assert_eq!(envelope["revision"], 1);

    // Instantiation #3 — a third call revises the verdict and keeps the first.
    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        None,
        json!({ "operation": "report", "outcome": "blocked", "summary": "The port regressed." }),
    );
    assert!(
        matches!(status, Status::Passed),
        "revision status: {status:?} {envelope}"
    );
    assert_eq!(envelope["revision"], 2);

    let doc = report(&f.state, SESSION_ID);
    assert_eq!(doc["outcome"], "blocked");
    assert_eq!(doc["superseded"][0]["outcome"], "success");
    assert_eq!(doc["progress"][0]["note"], "Indexed 4 of 9 crates.");
    // The runtime's own environment reaches the file, which is what tells a stale report
    // from this run's.
    assert_eq!(doc["session_id"], SESSION_ID);
    assert_eq!(doc["capsule"], CAPSULE_NAME);

    // Every byte the tool wrote landed in the state directory, and no temp file survived.
    assert_eq!(
        entries(&f.workdir),
        Vec::<String>::new(),
        "the tool must write nothing into the workdir"
    );
    assert_eq!(entries(&f.state), vec![REPORTS_DIR.to_string()]);
    assert_eq!(
        entries(&f.state.join(REPORTS_DIR)),
        vec![format!("{SESSION_ID}.json")]
    );
    assert_eq!(
        envelope["report_path"],
        format!("{STATE_GUEST_PATH}/{REPORTS_DIR}/{SESSION_ID}.json")
    );
}

#[test]
fn without_the_state_preopen_the_component_fails_closed() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("no-grant", false);

    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        None,
        None,
        json!({ "operation": "report", "outcome": "success", "summary": "done" }),
    );

    assert!(
        matches!(status, Status::Error),
        "status: {status:?} {envelope}"
    );
    assert_eq!(envelope["error_kind"], "state_unavailable");
    assert_eq!(
        entries(&f.workdir),
        Vec::<String>::new(),
        "no state/ directory may appear inside the workdir"
    );
    assert!(
        !f.state.exists(),
        "the tool must not create the state directory"
    );
}

#[test]
fn the_operator_declaration_is_read_from_the_guest_environment() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let config = json!({
        "config_version": 1,
        "require_report": true,
        "deliverables": { "kinds": ["diff", "dataset"] },
        "notes_for": { "stages": ["review"] }
    })
    .to_string();

    let f = fixture("declared", true);
    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        Some(&config),
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "done",
            "deliverables": [{ "name": "d", "kind": "screenshot", "uri": "out/a.png" }]
        }),
    );
    assert!(
        matches!(status, Status::Failed),
        "status: {status:?} {envelope}"
    );
    assert_eq!(envelope["error_kind"], "undeclared_kind");
    assert_eq!(
        entries(&f.state),
        Vec::<String>::new(),
        "a refusal writes nothing"
    );

    // The same call with no block in the environment is accepted: adoption is incremental.
    let g = fixture("undeclared", true);
    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &g.workdir,
        Some(&g.state),
        None,
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "done",
            "deliverables": [{ "name": "d", "kind": "screenshot", "uri": "out/a.png" }]
        }),
    );
    assert!(
        matches!(status, Status::Passed),
        "status: {status:?} {envelope}"
    );
}

#[test]
fn a_deliverable_carrying_an_inline_payload_is_refused_by_the_component() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("inline", true);

    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        None,
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "Generated the dataset.",
            "deliverables": [{
                "name": "rows", "kind": "dataset", "uri": "out/rows.csv",
                "content": "id,name\n1,alice\n"
            }]
        }),
    );
    assert!(
        matches!(status, Status::Failed),
        "status: {status:?} {envelope}"
    );
    assert_eq!(envelope["error_kind"], "inline_payload_refused");
    assert_eq!(entries(&f.state), Vec::<String>::new());
    assert_eq!(entries(&f.workdir), Vec::<String>::new());
}

#[test]
fn two_sessions_sharing_one_state_preopen_each_write_their_own_file() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("two-sessions", true);
    let first = "ses_0192f3a4b5c67d8e9f0a1b2c3d4e5f6a";
    let second = "ses_0192f3a4b5c67d8e9f0a1b2c3d4e5f6b";

    for (session, outcome) in [(first, "success"), (second, "blocked")] {
        let (status, envelope, meta) = run_as(
            &eng,
            &component,
            &lnk,
            &f.workdir,
            Some(&f.state),
            None,
            Some(session),
            json!({ "operation": "report", "outcome": outcome, "summary": session }),
        );
        assert!(
            matches!(status, Status::Passed),
            "{session}: {status:?} {envelope}"
        );
        assert_eq!(envelope["revision"], 1, "{session} starts its own file");
        let expected = format!("report:{STATE_GUEST_PATH}/{REPORTS_DIR}/{session}.json");
        assert!(
            meta.iter()
                .any(|(k, v)| k == "resource_id" && v == &expected),
            "expected resource_id={expected}, got {meta:?}"
        );
    }

    assert_eq!(
        entries(&f.state.join(REPORTS_DIR)),
        vec![format!("{first}.json"), format!("{second}.json")]
    );
    let first_doc = report(&f.state, first);
    assert_eq!(first_doc["session_id"], first);
    assert_eq!(first_doc["outcome"], "success");
    let second_doc = report(&f.state, second);
    assert_eq!(second_doc["session_id"], second);
    assert_eq!(second_doc["outcome"], "blocked");
    assert_eq!(
        entries(&f.workdir),
        Vec::<String>::new(),
        "the tool must write nothing into the workdir"
    );
}

#[test]
fn without_a_session_id_the_component_fails_closed() {
    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("no-session", true);

    let (status, envelope, meta) = run_as(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        None,
        None,
        json!({ "operation": "progress", "note": "n" }),
    );
    assert!(
        matches!(status, Status::Error),
        "status: {status:?} {envelope}"
    );
    assert_eq!(envelope["error_kind"], "session_unavailable");
    let expected = format!("report:{STATE_GUEST_PATH}/{REPORTS_DIR}");
    assert!(
        meta.iter()
            .any(|(k, v)| k == "resource_id" && v == &expected),
        "expected resource_id={expected}, got {meta:?}"
    );
    assert_eq!(entries(&f.state), Vec::<String>::new(), "nothing written");
    assert_eq!(entries(&f.workdir), Vec::<String>::new());
}

#[test]
fn retention_runs_inside_the_component_and_spares_the_callers_file() {
    use std::time::{Duration, SystemTime};
    use tool_report::retention::{MAX_REPORT_FILES, REPORT_RETENTION_GRACE};

    let eng = engine();
    let component = Component::from_file(&eng, component_path()).expect("load component");
    let lnk = linker(&eng);
    let f = fixture("retention", true);

    // MAX + 3 files past the grace, one minute apart, file 0 the oldest.
    let reports = f.state.join(REPORTS_DIR);
    std::fs::create_dir(&reports).expect("create reports/");
    let count = MAX_REPORT_FILES + 3;
    let planted: Vec<String> = (0..count)
        .map(|n| {
            let name = format!("ses_{n:032x}.json");
            let path = reports.join(&name);
            std::fs::write(&path, "{}").expect("plant a report");
            let age = REPORT_RETENTION_GRACE + Duration::from_secs(60 * (count - n) as u64 + 3600);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now() - age))
                .expect("age the report");
            name
        })
        .collect();

    let (status, envelope, _meta) = run_report(
        &eng,
        &component,
        &lnk,
        &f.workdir,
        Some(&f.state),
        None,
        json!({ "operation": "progress", "note": "n" }),
    );
    assert!(
        matches!(status, Status::Passed),
        "status: {status:?} {envelope}"
    );

    let remaining = entries(&reports);
    assert_eq!(remaining.len(), MAX_REPORT_FILES);
    assert!(remaining.contains(&format!("{SESSION_ID}.json")));
    for gone in &planted[..4] {
        assert!(!remaining.contains(gone), "{gone} was among the 4 oldest");
    }
}
