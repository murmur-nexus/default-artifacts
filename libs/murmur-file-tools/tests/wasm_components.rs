//! Host-target integration tests for the four compiled file-tool **wasm32-wasip2
//! components** — `tool-read`, `tool-write`, `tool-edit` and `tool-find` — run through Wasmtime.
//!
//! Every call runs in its own Wasmtime `Store`/instantiation against a preopened workdir, the
//! component-model analogue of the one-call-per-dispatch reality of these tools, and against
//! the component of the tool it exercises. Across separate instantiations it proves that a
//! repeat read of an unchanged file returns the content again, that reads leave the workdir's
//! entries exactly as they found them, and that a cache directory left behind by an earlier
//! version of the editor tool is neither read nor modified.
//!
//! It also validates that each compiled artifact loads as a real component
//! (`Component::from_file` is the validation gate — there is no `wasm-tools` CLI dependency
//! here) and benchmarks a `tool-find` walk over a large synthetic tree against the identical
//! host-native `murmur_file_tools::find::run` path, to prove no order-of-magnitude regression.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

use murmur_file_tools::Tool;
use serde_json::Value;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// Host-side bindings for `world tool` (murmur:tool/run). The components import zero murmur:*
// interfaces (only wasi:*, provided by wasmtime-wasi below), so we never wire the task/text host
// traits — the linker only needs WASI.
mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Attachment, Status, ToolInput, ToolResult};
use bindings::Tool as ToolWorld;

/// Store state: just a WASI context + resource table. `WasiView` is all the linker needs.
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

static BUILD_READ: Once = Once::new();
static BUILD_WRITE: Once = Once::new();
static BUILD_EDIT: Once = Once::new();
static BUILD_FIND: Once = Once::new();

/// Path to `tool`'s compiled component, building it if absent. `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so the test cannot assume the artifact already
/// exists — it builds it into the standard workspace target dir on first use, one crate per
/// cargo invocation so its bytes are the ones a release build produces.
fn component_path(tool: Tool) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join(format!(
        "wasm32-wasip2/release/{}.wasm",
        tool.name().replace('-', "_")
    ));

    let once = match tool {
        Tool::Read => &BUILD_READ,
        Tool::Write => &BUILD_WRITE,
        Tool::Edit => &BUILD_EDIT,
        Tool::Find => &BUILD_FIND,
    };
    once.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                tool.name(),
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(manifest.join("../.."))
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(
            status.success(),
            "cargo build of the {} component failed",
            tool.name()
        );
    });

    assert!(
        wasm.exists(),
        "compiled component not found at {} (build did not produce it)",
        wasm.display()
    );
    wasm
}

/// `tool`'s compiled component, loaded once per test process into the shared [`engine`]: each
/// call still instantiates it in a fresh Store, but compiling it is paid once, not per call.
fn load(engine: &Engine, tool: Tool) -> Component {
    static COMPONENTS: [OnceLock<Component>; 4] = [const { OnceLock::new() }; 4];
    assert!(
        Engine::same(engine, &self::engine()),
        "components are compiled for the shared engine"
    );
    let slot = match tool {
        Tool::Read => &COMPONENTS[0],
        Tool::Write => &COMPONENTS[1],
        Tool::Edit => &COMPONENTS[2],
        Tool::Find => &COMPONENTS[3],
    };
    slot.get_or_init(|| Component::from_file(engine, component_path(tool)).expect("load component"))
        .clone()
}

/// The one Wasmtime engine every test in this process shares.
fn engine() -> Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.wasm_component_model(true);
            Engine::new(&config).expect("failed to create wasmtime engine")
        })
        .clone()
}

fn linker(engine: &Engine) -> Linker<HostState> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    linker
}

/// A fresh Store whose only preopen is `workdir` mapped to `.` (DirPerms/FilePerms::all) —
/// what the capsule runtime grants a tool with no host root at dispatch time — with
/// wasmtime-wasi's `allow_blocking_current_thread` set as given. Env is deliberately NOT
/// inherited, so the component sees nothing of the test process's environment. When `true`, each guest filesystem call runs on the calling thread instead of being handed
/// to a tokio `spawn_blocking` pool thread and awaited. The setter must precede
/// `preopened_dir`: the preopen captures the flag when it is created. murmur's capsule runtime
/// leaves the flag off (`false`, the builder default).
fn wasi_store(
    engine: &Engine,
    workdir: &Path,
    allow_blocking_current_thread: bool,
) -> Store<HostState> {
    wasi_store_with(engine, workdir, allow_blocking_current_thread, &[])
}

/// One preopen beyond the work folder: the host directory, the name the guest sees it under,
/// and its permissions. A host root is named `murmur-host` followed by its absolute path.
type Preopen = (PathBuf, String, DirPerms, FilePerms);

/// [`wasi_store`]'s Store with `extra` preopened after the work folder, in order — the shape
/// murmur hands a tool its host roots in.
fn wasi_store_with(
    engine: &Engine,
    workdir: &Path,
    allow_blocking_current_thread: bool,
    extra: &[Preopen],
) -> Store<HostState> {
    let mut builder = WasiCtxBuilder::new();
    builder
        .allow_blocking_current_thread(allow_blocking_current_thread)
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen workdir");
    for (host, name, dir_perms, file_perms) in extra {
        builder
            .preopened_dir(host, name, *dir_perms, *file_perms)
            .expect("preopen host root");
    }
    Store::new(
        engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
        },
    )
}

/// One independent instantiation + `run` call against `workdir`. `data_payload` is the
/// call object (what the host places in `ToolInput.data`). Returns the decoded
/// old-protocol JSON envelope carried in `ToolResult.data`, plus status and metadata.
fn run_tool(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    data_payload: &str,
) -> (Status, Value, Vec<(String, String)>) {
    let (status, payload, _summary, meta) =
        run_tool_reporting(engine, component, linker, workdir, data_payload);
    (status, payload, meta)
}

/// [`run_tool`] plus `ToolResult.summary` — the field a refusal's message travels in, since a
/// failing envelope carries a null `data`.
fn run_tool_reporting(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    data_payload: &str,
) -> (Status, Value, Option<String>, Vec<(String, String)>) {
    let result = run_tool_raw(engine, component, linker, workdir, data_payload);
    let payload: Value = result
        .data
        .as_deref()
        .map(|s| serde_json::from_str(s).expect("ToolResult.data is JSON"))
        .unwrap_or(Value::Null);
    (result.status, payload, result.summary, result.metadata)
}

/// One independent instantiation + `run` call, returning the `ToolResult` exactly as it
/// crossed the component boundary — `data` as its raw string, plus `truncated`. Every call it
/// makes attaches no image; [`call_tool`] is the one for a call that may.
fn run_tool_raw(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    data_payload: &str,
) -> ToolResult {
    let result = call_tool(engine, component, linker, workdir, data_payload);
    assert!(
        result.attachments.is_empty(),
        "this call attaches no image, so ToolResult.attachments is empty: {:?}",
        result.attachments
    );
    result
}

/// [`run_tool_raw`] without its check that `ToolResult.attachments` is empty.
fn call_tool(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    data_payload: &str,
) -> ToolResult {
    call_tool_with(engine, component, linker, workdir, &[], data_payload)
}

/// [`call_tool`] with `extra` preopened beside the work folder.
fn call_tool_with(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HostState>,
    workdir: &Path,
    extra: &[Preopen],
    data_payload: &str,
) -> ToolResult {
    let mut store = wasi_store_with(engine, workdir, false, extra);
    let tool =
        ToolWorld::instantiate(&mut store, component, linker).expect("instantiate component");
    let input = ToolInput {
        data: Some(data_payload.to_string()),
        log_path: None,
    };
    tool.murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run")
}

/// The workdir's immediate entries, sorted — the "did the call leave anything behind" check.
fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read workdir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    names.sort();
    names
}

/// A preopen workdir at `.test-scratch/wasm_<tag>_<pid>`, relative to the process CWD (the
/// crate root, under `cargo test`). Relative rather than under `std::env::temp_dir()` because
/// the host-native baseline in the benchmark below addresses the same tree through
/// `murmur_file_tools::find::run` with no host root, which only accepts a workdir-relative value.
/// Gitignored; each test removes its own subtree.
fn unique_workdir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(".test-scratch").join(format!("wasm_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

/// How many times `needle` occurs in `haystack`, as `grep -a -o <needle> | wc -l` counts.
fn count(haystack: &[u8], needle: &str) -> usize {
    let needle = needle.as_bytes();
    let mut n = 0;
    let mut at = 0;
    while let Some(i) = haystack[at..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        n += 1;
        at += i + needle.len();
    }
    n
}

/// Each component links only its own operation. `regex` and `walkdir` are named in a component
/// only through the code that calls them, by their crate names in its symbol names and by their
/// package names in its panic locations, and `tool-find` is the positive control. Each tool's
/// operation is known by a message only that operation's module produces: the tool names
/// themselves prove nothing, because the shared `Tool` descriptor puts all four in every
/// component.
#[test]
fn each_component_links_only_its_own_operation() {
    const FIND_ONLY: [&str; 5] = [
        "regex_automata",
        "regex_syntax",
        "regex-automata",
        "regex-syntax",
        "walkdir",
    ];
    let own_message = |tool: Tool| match tool {
        Tool::Read => "is named as an image but is not a PNG, JPEG, GIF or WebP file",
        Tool::Write => "failed to create directories for ",
        Tool::Edit => "edits must be an array of {old, new} objects",
        Tool::Find => "context_lines must be between 0 and ",
    };

    for tool in Tool::ALL {
        let bytes = std::fs::read(component_path(tool)).expect("read component");
        for crate_name in FIND_ONLY {
            let n = count(&bytes, crate_name);
            if tool == Tool::Find {
                assert!(n > 0, "{} names no {crate_name}", tool.name());
            } else {
                assert_eq!(n, 0, "{} names {crate_name} {n} times", tool.name());
            }
        }
        for other in Tool::ALL {
            let n = count(&bytes, own_message(other));
            if other == tool {
                assert!(n > 0, "{} lacks its own message", tool.name());
            } else {
                assert_eq!(n, 0, "{} holds {}'s message", tool.name(), other.name());
            }
        }
    }
}

#[test]
fn component_loads_and_runs_read() {
    // `Component::from_file` succeeding IS the artifact-validation gate.
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("smoke");
    std::fs::write(workdir.join("hello.txt"), "hi there").unwrap();

    let (status, payload, meta) =
        run_tool(&eng, &component, &lnk, &workdir, r#"{"path":"hello.txt"}"#);
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

    let result = run_tool_raw(&eng, &component, &lnk, &workdir, r#"{"path":"hello.txt"}"#);
    assert!(
        !result.truncated,
        "a read within every limit is not truncated"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn a_read_with_no_range_of_a_large_file_stops_at_the_line_limit() {
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("read_line_limit");
    let body: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(workdir.join("big.txt"), body).unwrap();

    let result = run_tool_raw(&eng, &component, &lnk, &workdir, r#"{"path":"big.txt"}"#);
    assert!(
        matches!(result.status, Status::Passed),
        "status: {:?}",
        result.status
    );
    assert!(result.truncated, "a cut read sets ToolResult.truncated");

    let payload: Value = serde_json::from_str(result.data.as_deref().expect("data")).unwrap();
    assert_eq!(
        payload["truncated"],
        "Read stopped at the limit of 2000 lines. Lines 1-2000 of 5000 are returned; start_line 2001 continues the read."
    );
    assert_eq!(payload["start_line"], 1);
    assert_eq!(payload["end_line"], 2000);
    assert_eq!(payload["total_lines"], 5000);
    assert!(
        payload["content"]
            .as_str()
            .unwrap()
            .ends_with("\nline 2000"),
        "{:.200}",
        payload["content"]
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_loads_and_runs_write() {
    let eng = engine();
    let component = load(&eng, Tool::Write);
    let lnk = linker(&eng);
    let workdir = unique_workdir("write");

    let (status, payload, meta) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"out.txt","content":"written via wasm"}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    // write's `data` field is null in the old protocol, so ToolResult.data is None.
    assert!(payload.is_null(), "payload: {payload}");
    assert!(
        meta.iter()
            .any(|(k, v)| k == "state_effect" && v == "mutate"),
        "expected state_effect=mutate, got {meta:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join("out.txt")).unwrap(),
        "written via wasm"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

// S1: three pairs, each applied to the result of the one before.
#[test]
fn component_applies_edits_in_order() {
    let eng = engine();
    let component = load(&eng, Tool::Edit);
    let lnk = linker(&eng);
    let workdir = unique_workdir("edit_in_order");
    std::fs::write(workdir.join("f.txt"), "one two three\n").unwrap();

    let (status, payload, summary, meta) = run_tool_reporting(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"f.txt","edits":[{"old":"one","new":"1x"},{"old":"1x two","new":"12"},{"old":"three","new":"3"}]}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert!(payload.is_null(), "payload: {payload}");
    assert_eq!(summary.as_deref(), Some("3 edits"));
    assert!(
        meta.iter()
            .any(|(k, v)| k == "state_effect" && v == "mutate"),
        "expected state_effect=mutate, got {meta:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join("f.txt")).unwrap(),
        "12 3\n"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

/// One `edit` call that must fail with `summary`, leaving `f.txt` holding `content`.
fn assert_edit_refused(tag: &str, content: &str, call: &str, summary: &str) {
    let eng = engine();
    let component = load(&eng, Tool::Edit);
    let lnk = linker(&eng);
    let workdir = unique_workdir(tag);
    std::fs::write(workdir.join("f.txt"), content).unwrap();
    let before = dir_entries(&workdir);

    let (status, payload, got, meta) = run_tool_reporting(&eng, &component, &lnk, &workdir, call);
    assert!(matches!(status, Status::Error), "{call}: {status:?}");
    assert!(payload.is_null(), "{call}: {payload}");
    assert_eq!(got.as_deref(), Some(summary), "{call}");
    assert!(
        meta.is_empty(),
        "a failed edit declares no state_effect: {meta:?}"
    );
    assert_eq!(
        std::fs::read(workdir.join("f.txt")).unwrap(),
        content.as_bytes(),
        "{call}: a failed edit leaves the file byte-identical"
    );
    assert_eq!(dir_entries(&workdir), before, "{call}");

    let _ = std::fs::remove_dir_all(&workdir);
}

// S2: the second of three pairs occurs nowhere; the first, already applied in memory, is not
// written.
#[test]
fn component_edit_with_a_missing_old_changes_nothing() {
    assert_edit_refused(
        "edit_not_found",
        "one two three\n",
        r#"{"path":"f.txt","edits":[{"old":"one","new":"1"},{"old":"absent","new":"x"},{"old":"three","new":"3"}]}"#,
        "edit 2 of 3: old occurs 0 times in f.txt; the file is unchanged",
    );
}

// S3: overlapping occurrences count, so `aa` occurs twice in `aaa`.
#[test]
fn component_edit_with_an_ambiguous_old_changes_nothing() {
    assert_edit_refused(
        "edit_multiple_overlap",
        "x aaa\n",
        r#"{"path":"f.txt","edits":[{"old":"x","new":"y"},{"old":"aa","new":"b"}]}"#,
        "edit 2 of 2: old occurs 2 times in f.txt; the file is unchanged",
    );
    assert_edit_refused(
        "edit_multiple_dup",
        "x dup dup\n",
        r#"{"path":"f.txt","edits":[{"old":"x","new":"y"},{"old":"dup","new":"one"}]}"#,
        "edit 2 of 2: old occurs 2 times in f.txt; the file is unchanged",
    );
}

#[test]
fn component_refuses_empty_edits_and_an_empty_old() {
    assert_edit_refused(
        "edit_empty_edits",
        "abc\n",
        r#"{"path":"f.txt","edits":[]}"#,
        "edits is empty: it takes at least one {old, new} object",
    );
    assert_edit_refused(
        "edit_empty_old",
        "abc\n",
        r#"{"path":"f.txt","edits":[{"old":"a","new":"z"},{"old":"","new":"x"}]}"#,
        "edit 2 of 2: old is empty; it must name the text to replace",
    );
}

#[test]
fn component_edit_of_a_missing_file_creates_nothing() {
    let eng = engine();
    let component = load(&eng, Tool::Edit);
    let lnk = linker(&eng);
    let workdir = unique_workdir("edit_missing");

    let (status, _payload, summary, meta) = run_tool_reporting(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"absent.txt","edits":[{"old":"a","new":"b"}]}"#,
    );
    assert!(matches!(status, Status::Error), "status: {status:?}");
    assert!(
        summary
            .as_deref()
            .is_some_and(|s| s.starts_with("absent.txt: ")),
        "{summary:?}"
    );
    assert!(meta.is_empty(), "{meta:?}");
    assert!(
        dir_entries(&workdir).is_empty(),
        "a failed edit creates nothing"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

// The absolute-path tests below are the ones that need the real preopen. On the host an
// absolute value escapes the fixture tree and fails on its own; under the work-folder preopen
// alone, `/app/results.txt` resolves to `<workdir>/app/results.txt` — which is exactly why the
// tool refuses it when the session opened no host folder.
//
// `error_kind` is a field of the old-protocol envelope, which the WIT adapter maps to
// `Status::Error` plus the message in `summary` (the envelope's `data` is null on a failure, so
// `ToolResult.data` is None). These assert what crosses the component boundary; the
// `error_kind: "absolute_path"` value itself is pinned by the unit tests in `src/lib.rs`.
#[test]
fn component_refuses_an_absolute_write_path_and_leaves_the_workdir_untouched() {
    let eng = engine();
    let component = load(&eng, Tool::Write);
    let lnk = linker(&eng);
    let workdir = unique_workdir("absolute_write");
    let before = dir_entries(&workdir);

    let (status, payload, summary, meta) = run_tool_reporting(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"/app/results.txt","content":"written to the wrong place"}"#,
    );
    assert!(matches!(status, Status::Error), "status: {status:?}");
    assert!(payload.is_null(), "payload: {payload}");
    assert_eq!(
        summary.as_deref(),
        Some(
            "'path' must be relative to the work folder: this session opens no host folder to \
             tool-write; got '/app/results.txt' (did you mean 'results.txt'?)"
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

// S8: every path location, through the component. Each target exists under the preopen at the
// place the absolute value resolves to, so a not-found cannot stand in for the refusal.
#[test]
fn component_refuses_an_absolute_value_at_every_path_location() {
    let eng = engine();
    let lnk = linker(&eng);
    let workdir = unique_workdir("absolute_every_location");
    std::fs::create_dir_all(workdir.join("app")).unwrap();
    std::fs::write(workdir.join("app/results.txt"), "original\n").unwrap();

    for (tool, call, location, value) in [
        (
            Tool::Read,
            r#"{"path":"/app/results.txt"}"#,
            "path",
            "/app/results.txt",
        ),
        (
            Tool::Write,
            r#"{"path":"/app/new/x.txt","content":"x"}"#,
            "path",
            "/app/new/x.txt",
        ),
        (
            Tool::Edit,
            r#"{"path":"/app/results.txt","edits":[{"old":"original","new":"x"}]}"#,
            "path",
            "/app/results.txt",
        ),
        (
            Tool::Find,
            r#"{"pattern":"original","dir":"/app"}"#,
            "dir",
            "/app",
        ),
        (
            Tool::Find,
            r#"{"pattern":"original","glob":"/app/*.txt"}"#,
            "glob",
            "/app/*.txt",
        ),
    ] {
        let component = load(&eng, tool);
        let (status, payload, summary, meta) =
            run_tool_reporting(&eng, &component, &lnk, &workdir, call);
        assert!(matches!(status, Status::Error), "{call}: {status:?}");
        assert!(payload.is_null(), "{call}: {payload}");
        let summary = summary.unwrap_or_default();
        let expected = if location == "glob" {
            format!(
                "'glob' is matched against paths below 'dir' and must be relative; got '{value}'"
            )
        } else {
            format!(
                "'{location}' must be relative to the work folder: this session opens no host \
                 folder to {}; got '{value}'",
                tool.name()
            )
        };
        assert!(summary.starts_with(&expected), "{call}: {summary}");
        assert!(meta.is_empty(), "{call}: {meta:?}");
    }
    assert_eq!(
        std::fs::read_to_string(workdir.join("app/results.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(dir_entries(&workdir.join("app")), ["results.txt"]);

    let _ = std::fs::remove_dir_all(&workdir);
}

// S4: GLM 5.3's two failure shapes, tool-editor 0.12.0's nested shape and every earlier input
// shape are refused as unknown fields, naming the fields the tool takes, and leave the workdir
// exactly as they found it.
#[test]
fn component_refuses_every_earlier_field_and_leaves_the_workdir_untouched() {
    let eng = engine();
    let lnk = linker(&eng);
    let workdir = unique_workdir("old_shapes");
    std::fs::write(workdir.join("patch.txt"), "hello\n").unwrap();
    let before = dir_entries(&workdir);

    for (tool, call, summary) in [
        (
            Tool::Read,
            r#"{"read":"path</arg_key><arg_value>patch.txt"}"#,
            "unknown field \"read\": tool-read takes path, start_line, end_line",
        ),
        (
            Tool::Write,
            r#"{"path":"new.txt","write":{"path":"new.txt"},"content":"x"}"#,
            "unknown field \"write\": tool-write takes path, content",
        ),
        (
            Tool::Read,
            r#"{"read":{"path":"patch.txt"}}"#,
            "unknown field \"read\": tool-read takes path, start_line, end_line",
        ),
        (
            Tool::Write,
            r#"{"write":{"path":"new.txt","content":"x"}}"#,
            "unknown field \"write\": tool-write takes path, content",
        ),
        (
            Tool::Edit,
            r#"{"edit":{"path":"patch.txt","edits":[{"old":"hello","new":"bye"}]}}"#,
            "unknown field \"edit\": tool-edit takes path, edits",
        ),
        (
            Tool::Find,
            r#"{"find":{"pattern":"hello"}}"#,
            "unknown field \"find\": tool-find takes pattern, dir, glob, recursive, context_lines",
        ),
        (
            Tool::Edit,
            r#"{"operation":"replace_in_file","dest_path":"patch.txt","old_string":"hello","new_string":"bye"}"#,
            "unknown field \"dest_path\", \"new_string\", \"old_string\", \"operation\": \
             tool-edit takes path, edits",
        ),
        (
            Tool::Write,
            r#"{"operation":"write_file","dest_path":"new.txt","content":"x"}"#,
            "unknown field \"dest_path\", \"operation\": tool-write takes path, content",
        ),
        (
            Tool::Edit,
            r#"{"path":"patch.txt","old_string":"hello","new_string":"bye"}"#,
            "unknown field \"new_string\", \"old_string\": tool-edit takes path, edits",
        ),
        (
            Tool::Edit,
            r#"{"path":"patch.txt","edits":[{"old_string":"hello","new_string":"bye"}]}"#,
            "unknown field \"new_string\", \"old_string\" in edit 1 of 1: an edit takes old, new",
        ),
        (
            Tool::Read,
            r#"{"operation":"read_file","path":"patch.txt"}"#,
            "unknown field \"operation\": tool-read takes path, start_line, end_line",
        ),
    ] {
        let component = load(&eng, tool);
        let (status, payload, got, meta) =
            run_tool_reporting(&eng, &component, &lnk, &workdir, call);
        assert!(matches!(status, Status::Error), "{call}: {status:?}");
        assert!(payload.is_null(), "{call}: {payload}");
        assert_eq!(got.as_deref(), Some(summary), "{call}");
        assert!(meta.is_empty(), "{call}: {meta:?}");
    }
    assert_eq!(dir_entries(&workdir), before);
    assert_eq!(
        std::fs::read_to_string(workdir.join("patch.txt")).unwrap(),
        "hello\n"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

// S5: an empty call, or a required field that is null — which murmur lets through, since
// presence is its rule. The message arrives in `ToolResult.summary`, the field a host displays.
#[test]
fn component_refuses_a_missing_or_null_required_field_and_leaves_the_workdir_untouched() {
    let eng = engine();
    let lnk = linker(&eng);
    let workdir = unique_workdir("required_fields");
    let before = dir_entries(&workdir);

    for (tool, call, field) in [
        (Tool::Read, "{}", "path"),
        (Tool::Read, r#"{"path":null}"#, "path"),
        (
            Tool::Write,
            r#"{"path":"out.txt","content":null}"#,
            "content",
        ),
        (Tool::Write, r#"{"path":null,"content":"x"}"#, "path"),
        (Tool::Edit, r#"{"path":"out.txt","edits":null}"#, "edits"),
        (Tool::Find, r#"{"pattern":null}"#, "pattern"),
    ] {
        let component = load(&eng, tool);
        let (status, payload, summary, meta) =
            run_tool_reporting(&eng, &component, &lnk, &workdir, call);
        assert!(matches!(status, Status::Error), "{call}: {status:?}");
        assert!(payload.is_null(), "{call}: {payload}");
        assert_eq!(
            summary,
            Some(format!("missing required field: {field}")),
            "{call}"
        );
        assert!(meta.is_empty(), "{call}: {meta:?}");
    }
    assert_eq!(
        dir_entries(&workdir),
        before,
        "a refused call must leave the workdir exactly as it found it"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_refuses_an_absolute_read_path_even_when_the_file_exists() {
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("absolute_read");
    // Created directly, not through the tool: the file an absolute write used to manufacture is
    // present, so the refusal cannot be a not-found in disguise.
    std::fs::create_dir_all(workdir.join("app")).unwrap();
    std::fs::write(
        workdir.join("app/results.txt"),
        "content from the wrong place",
    )
    .unwrap();

    let (status, payload, summary, _meta) = run_tool_reporting(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"/app/results.txt"}"#,
    );
    assert!(matches!(status, Status::Error), "status: {status:?}");
    assert!(
        payload.is_null(),
        "a refused read must not return the file's content: {payload}"
    );
    let msg = summary.expect("a refusal carries its message in summary");
    assert!(
        msg.starts_with(
            "'path' must be relative to the work folder: this session opens no host folder to \
             tool-read; got '/app/results.txt'"
        ),
        "the refusal names `path`, the location the call wrote: {msg}"
    );

    // The same value read relatively still resolves — the refusal is about the spelling of the
    // input, not about the file being unreachable.
    let (ok_status, ok_payload, _m) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"app/results.txt"}"#,
    );
    assert!(matches!(ok_status, Status::Passed), "status: {ok_status:?}");
    assert_eq!(ok_payload["content"], "content from the wrong place");

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn repeat_reads_across_instantiations_return_content_and_leave_the_workdir_untouched() {
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("repeat_read");
    std::fs::write(workdir.join("target.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
    let before = dir_entries(&workdir);

    let whole = r#"{"path":"target.txt"}"#;
    let ranged = r#"{"path":"target.txt","start_line":2,"end_line":3}"#;

    // Each call is a separate Store against the same preopened workdir, so nothing an earlier
    // instantiation held in memory can reach a later one.
    for _ in 0..2 {
        let (status, payload, meta) = run_tool(&eng, &component, &lnk, &workdir, whole);
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

        let (status, payload, _meta) = run_tool(&eng, &component, &lnk, &workdir, ranged);
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
    let (_s, payload, _m) = run_tool(&eng, &component, &lnk, &workdir, whole);
    assert_eq!(payload["content"], "L1\nL2\nL3\nL4\nL5\n");

    assert_eq!(
        dir_entries(&workdir),
        before,
        "read must leave the workdir's entries exactly as it found them"
    );
    assert!(!workdir.join(".tool-editor-cache").exists());

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn a_leftover_cache_directory_from_an_earlier_version_is_inert() {
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("stale_cache");
    std::fs::write(workdir.join("target.txt"), "fresh bytes").unwrap();

    // The shape a 0.4.0 session leaves behind: small JSON entries plus an arbitrary file.
    let stale = workdir.join(".tool-editor-cache");
    std::fs::create_dir_all(&stale).unwrap();
    let seeded: Vec<(&str, &[u8])> = vec![
        (
            "00000000deadbeef.json",
            br#"{"key":"target.txt\u0000\u0000\u00000","cache_id":"cache_b"}"#,
        ),
        ("0123456789abcdef.json", b"{ not json at all"),
        ("ffffffffffffffff.json", b""),
    ];
    for (name, bytes) in &seeded {
        std::fs::write(stale.join(name), bytes).unwrap();
    }
    let before = dir_entries(&stale);

    let (status, payload, _meta) =
        run_tool(&eng, &component, &lnk, &workdir, r#"{"path":"target.txt"}"#);
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(payload["content"], "fresh bytes", "payload: {payload}");

    assert_eq!(
        dir_entries(&stale),
        before,
        "the stale directory's entries are untouched"
    );
    for (name, bytes) in &seeded {
        assert_eq!(
            &std::fs::read(stale.join(name)).unwrap(),
            bytes,
            "{name} was modified"
        );
    }

    let _ = std::fs::remove_dir_all(&workdir);
}

#[path = "../src/test_png.rs"]
mod test_png;

#[test]
fn component_read_of_a_png_returns_an_attachment() {
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("read_png");
    let png = test_png::png(64, 48);
    std::fs::write(workdir.join("logo.png"), &png).unwrap();

    let result = call_tool(&eng, &component, &lnk, &workdir, r#"{"path":"logo.png"}"#);
    assert!(
        matches!(result.status, Status::Passed),
        "status: {:?}",
        result.status
    );
    let attachments: Vec<(&str, &str)> = result
        .attachments
        .iter()
        .map(|Attachment { media_type, path }| (media_type.as_str(), path.as_str()))
        .collect();
    assert_eq!(attachments, [("image/png", "logo.png")]);
    assert_eq!(
        result.summary.as_deref(),
        Some(format!("image/png, {} bytes, 64x48 pixels", png.len()).as_str())
    );
    assert_eq!(result.data, None);
    assert!(!result.truncated);
    assert_eq!(
        result.metadata,
        vec![("state_effect".to_string(), "read".to_string())]
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn component_refuses_an_image_path_outside_the_work_folder() {
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let parent = unique_workdir("outside_image");
    let workdir = parent.join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(parent.join("outside.png"), test_png::png(8, 8)).unwrap();
    std::fs::write(parent.join("outside.txt"), "outside\n").unwrap();

    let refusal = |name: &str| {
        let path = format!("../{name}");
        let result = call_tool(
            &eng,
            &component,
            &lnk,
            &workdir,
            &serde_json::json!({ "path": &path }).to_string(),
        );
        assert!(
            result.attachments.is_empty(),
            "{name}: {:?}",
            result.attachments
        );
        assert_eq!(result.data, None, "{name}");
        assert!(
            matches!(result.status, Status::Error),
            "{name}: {:?}",
            result.status
        );
        result.summary.map(|s| s.replace(&path, "<path>"))
    };
    let image = refusal("outside.png");
    assert_eq!(image, refusal("outside.txt"));
    assert!(
        !image
            .as_deref()
            .unwrap_or_default()
            .contains("named as an image"),
        "{image:?}"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn component_loads_and_runs_ranged_read() {
    // A bounded read through the real compiled component: only the requested line span comes
    // back, alongside the file's total_lines and the resolved range.
    let eng = engine();
    let component = load(&eng, Tool::Read);
    let lnk = linker(&eng);
    let workdir = unique_workdir("ranged");
    std::fs::write(workdir.join("mod.txt"), "a\nb\nc\nd\ne\n").unwrap();

    let (status, payload, _meta) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"path":"mod.txt","start_line":2,"end_line":4}"#,
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
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("ctx");
    let sub = workdir.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("f.txt"), "L1\nL2\nL3 needle\nL4\nL5\n").unwrap();

    // With context.
    let (status, payload, _meta) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"pattern":"needle","dir":"src","recursive":false,"context_lines":2}"#,
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    let m = &payload["matches"][0];
    assert_eq!(m["line"], 3, "payload: {payload}");
    assert_eq!(m["context_before"], serde_json::json!(["L1", "L2"]));
    assert_eq!(m["context_after"], serde_json::json!(["L4", "L5"]));

    // Without context: no context keys at all (byte-for-byte the historical shape).
    let (status2, payload2, _m2) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        r#"{"pattern":"needle","dir":"src","recursive":false}"#,
    );
    assert!(matches!(status2, Status::Passed), "status: {status2:?}");
    let m2 = &payload2["matches"][0];
    assert!(
        m2.get("context_before").is_none(),
        "default must omit context_before"
    );
    assert!(
        m2.get("context_after").is_none(),
        "default must omit context_after"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

// ── find at the workdir root ───────────────────────────────────────────
//
// A root search walks the whole preopen, so it can only be exercised here, where the preopen is
// a fixture workdir; under `cargo test` the unit tests' CWD is the crate root.

/// A `tool-find` call holding `fields`.
fn find_payload(fields: Value) -> String {
    assert!(fields.is_object(), "fields is an object");
    fields.to_string()
}

/// Write each `(path, content)` under `workdir`, creating parent directories.
fn write_files(workdir: &Path, files: &[(&str, &[u8])]) {
    for (path, content) in files {
        let full = workdir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
}

/// The `path` of each match in a find payload, in result order.
fn match_paths(payload: &Value) -> Vec<String> {
    payload["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("no matches array: {payload}"))
        .iter()
        .map(|m| m["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn find_searches_the_workdir_root_for_every_root_spelling() {
    let eng = engine();
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("find_root");
    write_files(
        &workdir,
        &[
            ("app.py", b"def find_me():\n    pass\n"),
            ("pkg/util.py", b"def helper():\n    pass\n"),
        ],
    );
    let expected = serde_json::json!([{ "path": "app.py", "line": 1, "text": "def find_me():" }]);

    let absent = serde_json::json!({ "pattern": "def find_me" });
    let spellings = ["", ".", "./", "src/..", "./."];
    let payloads = std::iter::once(find_payload(absent)).chain(
        spellings
            .iter()
            .map(|dir| find_payload(serde_json::json!({ "pattern": "def find_me", "dir": dir }))),
    );
    for payload in payloads {
        let result = run_tool_raw(&eng, &component, &lnk, &workdir, &payload);
        assert!(
            matches!(result.status, Status::Passed),
            "{payload}: {:?}",
            result.status
        );
        let data: Value = serde_json::from_str(result.data.as_deref().unwrap()).unwrap();
        assert_eq!(
            data,
            serde_json::json!({ "matches": expected }),
            "{payload}"
        );
        assert!(
            !result.truncated,
            "{payload}: an uncapped search is not truncated"
        );
        assert!(
            result
                .metadata
                .iter()
                .any(|(k, v)| k == "state_effect" && v == "read"),
            "{payload}: {:?}",
            result.metadata
        );
    }

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn find_root_search_skips_hidden_build_and_binary_entries() {
    let eng = engine();
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("find_skip");
    write_files(
        &workdir,
        &[
            ("a.txt", b"needle\n"),
            (".git/config", b"needle\n"),
            (".hidden/x.txt", b"needle\n"),
            ("target/out.txt", b"needle\n"),
            ("node_modules/m/i.js", b"needle\n"),
            (".env", b"needle\n"),
            ("bin.dat", b"needle\n\0\x01\x02"),
        ],
    );

    let (status, payload, _meta) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        &find_payload(serde_json::json!({ "pattern": "needle" })),
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(match_paths(&payload), [".env", "a.txt"]);

    // Naming a skipped directory searches it.
    let (status, payload, _meta) = run_tool(
        &eng,
        &component,
        &lnk,
        &workdir,
        &find_payload(serde_json::json!({ "pattern": "needle", "dir": ".git" })),
    );
    assert!(matches!(status, Status::Passed), "status: {status:?}");
    assert_eq!(
        payload,
        serde_json::json!({ "matches": [{ "path": "config", "line": 1, "text": "needle" }] })
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn find_glob_filters_a_root_search() {
    let eng = engine();
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("find_glob");
    write_files(
        &workdir,
        &[
            ("main.py", b"needle\n"),
            ("lib/helper.py", b"needle\n"),
            ("lib/notes.md", b"needle\n"),
            ("src/a/b.rs", b"needle\n"),
        ],
    );

    for (glob, expected) in [
        ("*.py", vec!["lib/helper.py", "main.py"]),
        ("src/**/*.rs", vec!["src/a/b.rs"]),
        ("lib/*", vec!["lib/helper.py", "lib/notes.md"]),
    ] {
        let (status, payload, _meta) = run_tool(
            &eng,
            &component,
            &lnk,
            &workdir,
            &find_payload(serde_json::json!({ "pattern": "needle", "glob": glob })),
        );
        assert!(matches!(status, Status::Passed), "{glob}: {status:?}");
        assert_eq!(match_paths(&payload), expected, "{glob}");
    }

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn find_root_search_caps_at_200_matches() {
    let eng = engine();
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("find_match_cap");
    let body: String = (1..=250).map(|i| format!("def f{i}(): pass\n")).collect();
    std::fs::write(workdir.join("big.py"), body).unwrap();

    let result = run_tool_raw(
        &eng,
        &component,
        &lnk,
        &workdir,
        &find_payload(serde_json::json!({ "pattern": "def " })),
    );
    assert!(
        matches!(result.status, Status::Passed),
        "status: {:?}",
        result.status
    );
    assert!(
        result.truncated,
        "a capped search sets ToolResult.truncated"
    );
    assert_eq!(
        result.summary.as_deref(),
        Some("200 matches in 1 files (capped)")
    );

    let raw = result.data.as_deref().expect("data");
    let capped_at = raw.find("\"capped\"").expect("a capped key");
    let matches_at = raw.find("\"matches\"").expect("a matches key");
    assert!(
        capped_at < matches_at,
        "the model reads the note before the matches: {raw:.200}"
    );

    let data: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(
        data["capped"],
        "Search stopped at the limit of 200 matches after reading 1 file."
    );
    let lines: Vec<i64> = data["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, (1..=200).collect::<Vec<i64>>());

    let _ = std::fs::remove_dir_all(&workdir);
}

#[test]
fn find_glob_search_stops_at_5000_files_walked() {
    let eng = engine();
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("find_walk_cap");
    // 5001 text files the `*.py` glob excludes: 10 directories of 500 and one of 1.
    for i in 0..5001 {
        let dir = workdir.join("walk").join(format!("d{:02}", i / 500));
        if i % 500 == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(dir.join(format!("f{i:04}.txt")), "needle\n").unwrap();
    }

    let result = run_tool_raw(
        &eng,
        &component,
        &lnk,
        &workdir,
        &find_payload(serde_json::json!({ "pattern": "needle", "dir": "walk", "glob": "*.py" })),
    );
    assert!(
        matches!(result.status, Status::Passed),
        "status: {:?}",
        result.status
    );
    assert!(
        result.truncated,
        "a capped search sets ToolResult.truncated"
    );
    assert_eq!(
        result.summary.as_deref(),
        Some("0 matches in 0 files (capped)")
    );
    let data: Value = serde_json::from_str(result.data.as_deref().expect("data")).unwrap();
    assert_eq!(
        data["capped"],
        "Search stopped at the limit of 5000 files walked after reading 0 files."
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
fn find_benchmark_no_orders_of_magnitude_regression() {
    const SAMPLES: usize = 5;
    const MAX_RATIO: f64 = 30.0;

    let eng = engine();
    let component = load(&eng, Tool::Find);
    let lnk = linker(&eng);
    let workdir = unique_workdir("bench");
    let tree = workdir.join("bench_tree");
    let n_files = build_tree(&tree, 60, 50); // 3000 files

    // wasm: the walk uses a workdir-relative dir under the single preopen.
    let rel_payload = r#"{"pattern":"NEEDLE","dir":"bench_tree","recursive":true}"#;

    // Host-native baseline: the SAME logic, host-compiled, over the same tree. The dir is
    // workdir-relative like the wasm payload's — with no host root the tool refuses an absolute
    // value — so both sides walk the identical tree (host `murmur_file_tools::find::run` takes the
    // stdin-envelope shape).
    let host_dir = tree.to_string_lossy().to_string();
    let envelope = serde_json::json!({
        "data": { "pattern": "NEEDLE", "dir": host_dir, "recursive": true }
    })
    .to_string();

    // Warm both paths once, then time the walk only (instantiation excluded — that is the
    // fair "latency of the walk" comparison; instantiation overhead is reported separately).
    let _ = murmur_file_tools::find::run(&envelope, &[]);

    // The sampled walks block on-thread; see the comment above the bound.
    let mut store = wasi_store(&eng, &workdir, true);
    let tool = ToolWorld::instantiate(&mut store, &component, &lnk).expect("instantiate");
    let input = ToolInput {
        data: Some(rel_payload.to_string()),
        log_path: None,
    };
    let _ = tool.murmur_tool_run().call_run(&mut store, &input).unwrap(); // warm

    // Interleaved rounds, one host walk then one wasm walk each, so both sides sample the
    // same stretch of machine load.
    let (mut host_walk_min, mut host_walk_max) = (Duration::MAX, Duration::ZERO);
    let (mut wasm_walk_min, mut wasm_walk_max) = (Duration::MAX, Duration::ZERO);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let host_out = murmur_file_tools::find::run(&envelope, &[]);
        let host_walk = t.elapsed();
        assert_eq!(host_out["ok"], true, "host find must succeed: {host_out}");
        assert_eq!(host_out["matches"].as_array().map(|a| a.len()), Some(1));
        host_walk_min = host_walk_min.min(host_walk);
        host_walk_max = host_walk_max.max(host_walk);

        let t = Instant::now();
        let wasm_res = tool.murmur_tool_run().call_run(&mut store, &input).unwrap();
        let wasm_walk = t.elapsed();
        assert!(
            matches!(wasm_res.status, Status::Passed),
            "wasm find status: {:?}",
            wasm_res.status
        );
        // ToolResult.data for find is the `data` field: `{"matches":[...]}`.
        let wasm_payload: Value = serde_json::from_str(wasm_res.data.as_deref().unwrap()).unwrap();
        assert_eq!(wasm_payload["matches"].as_array().map(|a| a.len()), Some(1));
        assert!(
            wasm_payload.get("capped").is_none(),
            "3000 files stay under every limit"
        );
        assert!(!wasm_res.truncated);
        wasm_walk_min = wasm_walk_min.min(wasm_walk);
        wasm_walk_max = wasm_walk_max.max(wasm_walk);
    }

    // Full instantiate+call in the production WASI context, for the process-spawn analogue
    // number.
    let t = Instant::now();
    let _ = run_tool(&eng, &component, &lnk, &workdir, rel_payload);
    let wasm_full = t.elapsed();

    let ratio = wasm_walk_min.as_secs_f64() / host_walk_min.as_secs_f64().max(1e-9);
    eprintln!(
        "BENCH find files={n_files} samples={SAMPLES} host_walk_min={host_walk_min:?} \
         host_walk_max={host_walk_max:?} wasm_walk_min={wasm_walk_min:?} \
         wasm_walk_max={wasm_walk_max:?} wasm_instantiate+call={wasm_full:?} ratio={ratio:.2}x"
    );

    // No order-of-magnitude regression. The bound compares the guest walk, its WASI filesystem
    // calls run on-thread, against the identical host-compiled logic. It leaves out
    // wasmtime-wasi's per-call `spawn_blocking` dispatch, whose cost scales with runqueue
    // depth and would make the bound measure the machine. murmur's capsule runtime does not
    // set `allow_blocking_current_thread`, so capsules pay that dispatch;
    // `wasm_instantiate+call` runs in the production context and is the figure that includes
    // it. The fastest of SAMPLES absorbs a one-off stall, and the max shows it. The 0.5s slack
    // keeps a sub-millisecond host time from making the ratio explode. The bound only catches
    // an order-of-magnitude regression; the recorded numbers are the real engineering gate
    // (see the build summary).
    assert!(
        wasm_walk_min.as_secs_f64() < host_walk_min.as_secs_f64() * MAX_RATIO + 0.5,
        "wasm find regressed by >{MAX_RATIO}x (fastest of {SAMPLES}): \
         host={host_walk_min:?} wasm={wasm_walk_min:?}"
    );

    let _ = std::fs::remove_dir_all(&workdir);
}

// ── Host roots ──────────────────────────────────────────────────────────────────
//
// murmur hands a tool each host folder as an extra preopen named `murmur-host` followed by the
// folder's absolute path. These tests preopen scratch directories outside the work folder the
// same way: `HOST` read-write and `RO` read-only, each under its canonical absolute path.

/// A work folder plus two host folders beside it, outside it.
struct HostFixture {
    parent: PathBuf,
    workdir: PathBuf,
    /// `HOST`'s canonical absolute path.
    host: String,
    /// `RO`'s canonical absolute path.
    ro: String,
}

impl HostFixture {
    fn new(tag: &str) -> HostFixture {
        let parent = unique_workdir(&format!("roots_{tag}"));
        let workdir = parent.join("work");
        for sub in ["work", "host", "ro"] {
            std::fs::create_dir_all(parent.join(sub)).unwrap();
        }
        let canonical = |sub: &str| {
            std::fs::canonicalize(parent.join(sub))
                .unwrap()
                .to_string_lossy()
                .to_string()
        };
        HostFixture {
            host: canonical("host"),
            ro: canonical("ro"),
            parent,
            workdir,
        }
    }

    /// `HOST` as a read-write root.
    fn host_root(&self) -> Preopen {
        (
            PathBuf::from(&self.host),
            format!("murmur-host{}", self.host),
            DirPerms::all(),
            FilePerms::all(),
        )
    }

    /// `RO` as a read-only root.
    fn ro_root(&self) -> Preopen {
        (
            PathBuf::from(&self.ro),
            format!("murmur-host{}", self.ro),
            DirPerms::READ,
            FilePerms::READ,
        )
    }

    /// `HOST` (read-write) then `RO` (read), in that preopen order.
    fn both(&self) -> Vec<Preopen> {
        vec![self.host_root(), self.ro_root()]
    }

    /// One call to `tool` holding `extra`, with the call object `data`.
    fn call(&self, tool: Tool, extra: &[Preopen], data: Value) -> ToolResult {
        let eng = engine();
        let component = load(&eng, tool);
        let lnk = linker(&eng);
        call_tool_with(
            &eng,
            &component,
            &lnk,
            &self.workdir,
            extra,
            &data.to_string(),
        )
    }

    /// The roots-held refusal for `value` at `location`, with `HOST` and `RO` held.
    fn outside_both(&self, tool: Tool, location: &str, value: &str) -> String {
        format!(
            "'{location}' is outside every host folder {} may open: {} (read-write), {} (read); \
             got '{value}'. A relative path opens in the work folder",
            tool.name(),
            self.host,
            self.ro
        )
    }

    /// The work folder's entries, recursively, relative and sorted.
    fn work_tree(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                );
                if path.is_dir() {
                    walk(&path, base, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.workdir, &self.workdir, &mut out);
        out.sort();
        out
    }
}

impl Drop for HostFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.parent);
    }
}

/// `ToolResult.data` decoded, `Null` when absent.
fn data_of(result: &ToolResult) -> Value {
    result
        .data
        .as_deref()
        .map(|d| serde_json::from_str(d).expect("ToolResult.data is JSON"))
        .unwrap_or(Value::Null)
}

/// Asserts `result` is a refusal carrying exactly `summary`, with no data, attachment or effect.
fn assert_component_refused(result: &ToolResult, summary: &str) {
    assert!(
        matches!(result.status, Status::Error),
        "{:?}: {:?}",
        result.status,
        result.summary
    );
    assert_eq!(result.summary.as_deref(), Some(summary));
    assert_eq!(result.data, None, "{summary}");
    assert!(result.attachments.is_empty(), "{summary}");
    assert!(
        result.metadata.is_empty(),
        "a refusal declares no effect: {summary}"
    );
}

fn assert_passed(result: &ToolResult) {
    assert!(
        matches!(result.status, Status::Passed),
        "{:?}: {:?}",
        result.status,
        result.summary
    );
}

// S1
#[test]
fn component_reads_a_file_in_a_read_write_root() {
    let fx = HostFixture::new("s1");
    std::fs::write(format!("{}/notes.txt", fx.host), "alpha\nbeta\n").unwrap();
    std::fs::write(fx.workdir.join("local.txt"), "local\n").unwrap();
    let before = fx.work_tree();

    let value = format!("{}/notes.txt", fx.host);
    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &value }),
    );
    assert_passed(&result);
    assert_eq!(
        data_of(&result),
        serde_json::json!({ "content": "alpha\nbeta\n", "total_lines": 2 })
    );
    assert_eq!(
        result.metadata,
        vec![("state_effect".to_string(), "read".to_string())]
    );

    // An I/O error names the value as written, never the `murmur-host…` path opened.
    let missing = format!("{}/missing.txt", fx.host);
    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &missing }),
    );
    assert!(matches!(result.status, Status::Error));
    let summary = result.summary.unwrap();
    assert!(summary.starts_with(&format!("{missing}: ")), "{summary}");
    assert!(!summary.contains("murmur-host"), "{summary}");

    assert_eq!(fx.work_tree(), before, "the work folder is unchanged");
    assert!(!fx.workdir.join("murmur-host").exists());
}

// S2
#[test]
fn component_writes_a_file_and_its_parents_in_a_read_write_root() {
    let fx = HostFixture::new("s2");
    let before = fx.work_tree();
    let value = format!("{}/new/dir/out.txt", fx.host);

    let result = fx.call(
        Tool::Write,
        &[fx.host_root()],
        serde_json::json!({ "path": &value, "content": "x" }),
    );
    assert_passed(&result);
    assert_eq!(result.summary.as_deref(), Some("1 bytes written"));
    assert_eq!(
        result.metadata,
        vec![("state_effect".to_string(), "mutate".to_string())]
    );
    assert_eq!(std::fs::read_to_string(&value).unwrap(), "x");

    let read = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &value }),
    );
    assert_passed(&read);
    assert_eq!(data_of(&read)["content"], "x");

    assert_eq!(fx.work_tree(), before, "the work folder is unchanged");
}

// S3
#[test]
fn component_edits_a_file_in_a_read_write_root() {
    let fx = HostFixture::new("s3");
    let value = format!("{}/a.txt", fx.host);
    std::fs::write(&value, "one two").unwrap();

    let result = fx.call(
        Tool::Edit,
        &[fx.host_root()],
        serde_json::json!({ "path": &value, "edits": [{ "old": "two", "new": "three" }] }),
    );
    assert_passed(&result);
    assert_eq!(std::fs::read_to_string(&value).unwrap(), "one three");
    assert!(fx.work_tree().is_empty());
}

// S4
#[test]
fn component_finds_in_a_root_with_paths_relative_to_dir() {
    let fx = HostFixture::new("s4");
    std::fs::create_dir_all(format!("{}/src", fx.host)).unwrap();
    std::fs::write(format!("{}/src/lib.rs", fx.host), "fn needle() {}\n").unwrap();

    let result = fx.call(
        Tool::Find,
        &[fx.host_root()],
        serde_json::json!({ "pattern": "needle", "dir": &fx.host }),
    );
    assert_passed(&result);
    assert_eq!(
        data_of(&result)["matches"],
        serde_json::json!([{ "path": "src/lib.rs", "line": 1, "text": "fn needle() {}" }])
    );

    // Non-recursive, and a `dir` naming a file.
    let result = fx.call(
        Tool::Find,
        &[fx.host_root()],
        serde_json::json!({ "pattern": "needle", "dir": format!("{}/src", fx.host), "recursive": false }),
    );
    assert_eq!(match_paths(&data_of(&result)), ["lib.rs"]);
    let result = fx.call(
        Tool::Find,
        &[fx.host_root()],
        serde_json::json!({ "pattern": "needle", "dir": format!("{}/src/lib.rs", fx.host) }),
    );
    assert_eq!(match_paths(&data_of(&result)), ["lib.rs"]);

    // A missing dir names the value as written.
    let missing = format!("{}/nope", fx.host);
    let result = fx.call(
        Tool::Find,
        &[fx.host_root()],
        serde_json::json!({ "pattern": "needle", "dir": &missing, "recursive": false }),
    );
    assert!(matches!(result.status, Status::Error));
    let summary = result.summary.unwrap();
    assert!(summary.starts_with(&format!("{missing}: ")), "{summary}");
}

// S5
#[test]
fn component_refuses_a_path_outside_every_root_and_touches_nothing() {
    let fx = HostFixture::new("s5");
    std::fs::write(fx.workdir.join("a.txt"), "original\n").unwrap();
    let before = fx.work_tree();
    // The work folder's own absolute path is outside both roots, and the file exists there.
    let work_abs = std::fs::canonicalize(&fx.workdir)
        .unwrap()
        .to_string_lossy()
        .to_string();
    let file = format!("{work_abs}/a.txt");
    let fresh = format!("{work_abs}/new/x.txt");

    for (tool, data, location, value) in [
        (
            Tool::Read,
            serde_json::json!({ "path": &file }),
            "path",
            file.as_str(),
        ),
        (
            Tool::Write,
            serde_json::json!({ "path": &fresh, "content": "x" }),
            "path",
            fresh.as_str(),
        ),
        (
            Tool::Edit,
            serde_json::json!({ "path": &file, "edits": [{ "old": "original", "new": "x" }] }),
            "path",
            file.as_str(),
        ),
        (
            Tool::Find,
            serde_json::json!({ "pattern": "original", "dir": &work_abs }),
            "dir",
            work_abs.as_str(),
        ),
        (Tool::Read, serde_json::json!({ "path": "/" }), "path", "/"),
    ] {
        let result = fx.call(tool, &fx.both(), data);
        assert_component_refused(&result, &fx.outside_both(tool, location, value));
    }
    assert_eq!(fx.work_tree(), before);
    assert_eq!(
        std::fs::read_to_string(fx.workdir.join("a.txt")).unwrap(),
        "original\n"
    );
}

// S6
#[test]
fn component_with_no_roots_refuses_an_absolute_path_naming_none() {
    let fx = HostFixture::new("s6");
    for (tool, data, summary) in [
        (
            Tool::Write,
            serde_json::json!({ "path": "/app/results.txt", "content": "x" }),
            "'path' must be relative to the work folder: this session opens no host folder to \
             tool-write; got '/app/results.txt' (did you mean 'results.txt'?)",
        ),
        (
            Tool::Read,
            serde_json::json!({ "path": "/" }),
            "'path' must be relative to the work folder: this session opens no host folder to \
             tool-read; got '/'",
        ),
        (
            Tool::Find,
            serde_json::json!({ "pattern": "x", "dir": "/app/.." }),
            "'dir' must be relative to the work folder: this session opens no host folder to \
             tool-find; got '/app/..'",
        ),
    ] {
        assert_component_refused(&fx.call(tool, &[], data), summary);
    }
    assert!(fx.work_tree().is_empty());
}

// S7
#[test]
fn component_refuses_a_write_or_edit_into_a_read_only_root() {
    let fx = HostFixture::new("s7");
    let keep = format!("{}/keep.txt", fx.ro);
    std::fs::write(&keep, "kept\n").unwrap();
    let sub_new = format!("{}/sub/new.txt", fx.ro);

    for (tool, data, value) in [
        (
            Tool::Write,
            serde_json::json!({ "path": &keep, "content": "x" }),
            keep.as_str(),
        ),
        (
            Tool::Write,
            serde_json::json!({ "path": &sub_new, "content": "x" }),
            sub_new.as_str(),
        ),
        (
            Tool::Edit,
            serde_json::json!({ "path": &keep, "edits": [{ "old": "kept", "new": "x" }] }),
            keep.as_str(),
        ),
    ] {
        let result = fx.call(tool, &fx.both(), data);
        assert_component_refused(
            &result,
            &format!(
                "'path' '{value}' is in {}, which this session opens read-only",
                fx.ro
            ),
        );
    }
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "kept\n");
    assert!(!Path::new(&format!("{}/sub", fx.ro)).exists());

    let read = fx.call(Tool::Read, &fx.both(), serde_json::json!({ "path": &keep }));
    assert_passed(&read);
    assert_eq!(data_of(&read)["content"], "kept\n");
    assert!(fx.work_tree().is_empty());
}

// S8
#[test]
fn component_relative_paths_behave_the_same_with_a_root_held() {
    // Each run in its own fixture, so a search of the work folder's root sees only its own run.
    let scenario = |fx: &HostFixture, extra: &[Preopen]| -> Vec<String> {
        let sub = "w";
        let calls = [
            (
                Tool::Write,
                serde_json::json!({ "path": format!("{sub}/a.txt"), "content": "one two\n" }),
            ),
            (
                Tool::Edit,
                serde_json::json!({ "path": format!("{sub}/a.txt"), "edits": [{ "old": "two", "new": "three" }] }),
            ),
            (
                Tool::Read,
                serde_json::json!({ "path": format!("{sub}/a.txt") }),
            ),
            (
                Tool::Read,
                serde_json::json!({ "path": format!("{sub}/a.txt"), "start_line": 1, "end_line": 1 }),
            ),
            (
                Tool::Find,
                serde_json::json!({ "pattern": "three", "dir": sub }),
            ),
            (
                Tool::Find,
                serde_json::json!({ "pattern": "three", "dir": format!("{sub}/..") }),
            ),
            (
                Tool::Find,
                serde_json::json!({ "pattern": "three", "dir": "../.." }),
            ),
            (
                Tool::Read,
                serde_json::json!({ "path": format!("{sub}/missing.txt") }),
            ),
            (
                Tool::Write,
                serde_json::json!({ "path": format!("{sub}/pic.png"), "content": "not a png" }),
            ),
            (
                Tool::Read,
                serde_json::json!({ "path": format!("{sub}/pic.png") }),
            ),
        ];
        calls
            .into_iter()
            .map(|(tool, data)| {
                let result = fx.call(tool, extra, data);
                format!(
                    "{:?} {:?} {:?} {:?} {:?}",
                    result.status, result.summary, result.data, result.metadata, result.truncated
                )
            })
            .collect()
    };
    let held_fx = HostFixture::new("s8_held");
    let none_fx = HostFixture::new("s8_none");
    let held = scenario(&held_fx, &[held_fx.host_root()]);
    let none = scenario(&none_fx, &[]);
    assert_eq!(held, none);
    assert!(held[2].contains("one three"), "{}", held[2]);
    assert_eq!(
        std::fs::read_dir(&held_fx.host).unwrap().count(),
        0,
        "nothing reached HOST"
    );
}

// S9
#[test]
fn component_reserves_a_leading_murmur_host_while_a_root_is_held() {
    let fx = HostFixture::new("s9");
    let value = format!("murmur-host{}/x.txt", fx.host);
    std::fs::write(format!("{}/x.txt", fx.host), "host\n").unwrap();

    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &value }),
    );
    assert_component_refused(
        &result,
        &format!(
            "'path' starts with murmur-host, where this tool's host folders are opened; name the \
             host path absolutely: '{}/x.txt'",
            fx.host
        ),
    );
    let result = fx.call(
        Tool::Find,
        &[fx.host_root()],
        serde_json::json!({ "pattern": "host", "dir": "./murmur-host" }),
    );
    assert_component_refused(
        &result,
        "'dir' starts with murmur-host, where this tool's host folders are opened; name the host \
         path absolutely: '/'",
    );
    assert!(fx.work_tree().is_empty());

    // With no roots the same values are ordinary work-folder paths.
    let local = fx.workdir.join(&value);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, "local\n").unwrap();
    let result = fx.call(Tool::Read, &[], serde_json::json!({ "path": &value }));
    assert_passed(&result);
    assert_eq!(data_of(&result)["content"], "local\n");
    std::fs::create_dir_all(fx.workdir.join("murmur-host/plain")).unwrap();
    std::fs::write(fx.workdir.join("murmur-host/plain/y.txt"), "local\n").unwrap();
    let result = fx.call(
        Tool::Find,
        &[],
        serde_json::json!({ "pattern": "local", "dir": "./murmur-host", "glob": "plain/*" }),
    );
    assert_passed(&result);
    assert_eq!(match_paths(&data_of(&result)), ["plain/y.txt"]);
}

// S10
#[test]
fn component_dotdot_cannot_climb_out_of_a_root() {
    let fx = HostFixture::new("s10");
    let sibling = format!("{}-sibling", fx.host);
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(format!("{sibling}/secret.txt"), "secret\n").unwrap();
    let base = Path::new(&fx.host)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let value = format!("{}/../{base}-sibling/secret.txt", fx.host);

    let result = fx.call(
        Tool::Read,
        &fx.both(),
        serde_json::json!({ "path": &value }),
    );
    assert_component_refused(&result, &fx.outside_both(Tool::Read, "path", &value));
    let result = fx.call(
        Tool::Write,
        &fx.both(),
        serde_json::json!({ "path": &value, "content": "x" }),
    );
    assert_component_refused(&result, &fx.outside_both(Tool::Write, "path", &value));
    assert_eq!(
        std::fs::read_to_string(format!("{sibling}/secret.txt")).unwrap(),
        "secret\n"
    );
    let _ = std::fs::remove_dir_all(&sibling);
}

// S11
#[test]
fn component_root_covers_whole_components_only() {
    let fx = HostFixture::new("s11");
    let longer = format!("{}2", fx.host);
    std::fs::create_dir_all(&longer).unwrap();
    std::fs::write(format!("{longer}/file"), "not covered\n").unwrap();
    let value = format!("{longer}/file");

    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &value }),
    );
    assert_component_refused(
        &result,
        &format!(
            "'path' is outside every host folder tool-read may open: {} (read-write); got \
             '{value}'. A relative path opens in the work folder",
            fx.host
        ),
    );
    let _ = std::fs::remove_dir_all(&longer);
}

// S12
#[test]
fn component_with_the_whole_host_read_only_reads_anywhere_and_writes_nowhere() {
    let fx = HostFixture::new("s12");
    let file = format!("{}/fixture.txt", fx.host);
    std::fs::write(&file, "whole host\n").unwrap();
    let whole: Vec<Preopen> = vec![(
        PathBuf::from("/"),
        "murmur-host".to_string(),
        DirPerms::READ,
        FilePerms::READ,
    )];

    let result = fx.call(Tool::Read, &whole, serde_json::json!({ "path": &file }));
    assert_passed(&result);
    assert_eq!(data_of(&result)["content"], "whole host\n");

    let beside = format!("{}/beside.txt", fx.host);
    let result = fx.call(
        Tool::Write,
        &whole,
        serde_json::json!({ "path": &beside, "content": "x" }),
    );
    assert_component_refused(
        &result,
        &format!("'path' '{beside}' is in /, which this session opens read-only"),
    );
    assert!(!Path::new(&beside).exists());
    assert!(fx.work_tree().is_empty());
}

// S13
#[test]
fn component_describes_but_does_not_attach_an_image_under_a_root() {
    let fx = HostFixture::new("s13");
    let png = test_png::png(64, 48);
    let value = format!("{}/pic.png", fx.host);
    std::fs::write(&value, &png).unwrap();
    std::fs::write(fx.workdir.join("pic.png"), &png).unwrap();

    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &value }),
    );
    assert_passed(&result);
    assert!(result.attachments.is_empty(), "{:?}", result.attachments);
    assert_eq!(
        result.summary.as_deref(),
        Some(
            format!(
                "image/png, {} bytes, 64x48 pixels; not shown: only an image named by a path \
                 relative to the work folder is shown to the model",
                png.len()
            )
            .as_str()
        )
    );
    assert_eq!(result.data, None);

    // A text file named as an image under a root is still `not_an_image`.
    let fake = format!("{}/fake.png", fx.host);
    std::fs::write(&fake, "plain text").unwrap();
    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": &fake }),
    );
    assert_component_refused(
        &result,
        &format!("'{fake}' is named as an image but is not a PNG, JPEG, GIF or WebP file"),
    );

    // The same PNG in the work folder, read relatively, is attached.
    let result = fx.call(
        Tool::Read,
        &[fx.host_root()],
        serde_json::json!({ "path": "pic.png" }),
    );
    assert_passed(&result);
    let attachments: Vec<(&str, &str)> = result
        .attachments
        .iter()
        .map(|Attachment { media_type, path }| (media_type.as_str(), path.as_str()))
        .collect();
    assert_eq!(attachments, [("image/png", "pic.png")]);
}

// S14
#[test]
fn component_refuses_an_absolute_glob_with_or_without_roots() {
    let fx = HostFixture::new("s14");
    let glob = format!("{}/*.rs", fx.host);
    for extra in [vec![], vec![fx.host_root()], fx.both()] {
        let result = fx.call(
            Tool::Find,
            &extra,
            serde_json::json!({ "pattern": "x", "dir": &fx.host, "glob": &glob }),
        );
        let expected = if extra.is_empty() {
            // With no root the absolute `dir` is refused first.
            format!(
                "'dir' must be relative to the work folder: this session opens no host folder to \
                 tool-find; got '{}' (did you mean 'host'?)",
                fx.host
            )
        } else {
            format!(
                "'glob' is matched against paths below 'dir' and must be relative; got '{glob}' \
                 (did you mean '*.rs'?). Name a host folder in 'dir'"
            )
        };
        assert_component_refused(&result, &expected);

        let result = fx.call(
            Tool::Find,
            &extra,
            serde_json::json!({ "pattern": "x", "glob": "/src/*.rs" }),
        );
        assert_component_refused(
            &result,
            "'glob' is matched against paths below 'dir' and must be relative; got '/src/*.rs' \
             (did you mean '*.rs'?). Name a host folder in 'dir'",
        );
    }
}

// S15
#[test]
fn component_find_of_the_work_folder_skips_a_murmur_host_entry() {
    let fx = HostFixture::new("s15");
    std::fs::write(format!("{}/host.txt", fx.host), "needle\n").unwrap();
    std::fs::create_dir_all(fx.workdir.join("murmur-host")).unwrap();
    std::fs::write(fx.workdir.join("murmur-host/decoy.txt"), "needle\n").unwrap();
    std::fs::write(fx.workdir.join("real.txt"), "needle\n").unwrap();

    // HOST under its own name, and HOST standing in for the whole host at `murmur-host`, where
    // a walk into `./murmur-host` would reach it.
    let as_whole_host: Vec<Preopen> = vec![(
        PathBuf::from(&fx.host),
        "murmur-host".to_string(),
        DirPerms::all(),
        FilePerms::all(),
    )];
    for extra in [vec![fx.host_root()], as_whole_host] {
        for dir in [
            Value::Null,
            Value::from(""),
            Value::from("."),
            Value::from("./"),
        ] {
            for recursive in [true, false] {
                let result = fx.call(
                    Tool::Find,
                    &extra,
                    serde_json::json!({ "pattern": "needle", "dir": &dir, "recursive": recursive }),
                );
                assert_passed(&result);
                assert_eq!(
                    match_paths(&data_of(&result)),
                    ["real.txt"],
                    "dir {dir}, recursive {recursive}"
                );
            }
        }
    }

    // With no root the work folder's `murmur-host` folder is searched like any other.
    let result = fx.call(Tool::Find, &[], serde_json::json!({ "pattern": "needle" }));
    assert_eq!(
        match_paths(&data_of(&result)),
        ["murmur-host/decoy.txt", "real.txt"]
    );
}
