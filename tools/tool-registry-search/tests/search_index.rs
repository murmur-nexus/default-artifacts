//! The compiled `tool-registry-search` **wasm32-wasip2 component**, driven through Wasmtime with
//! `wasi:http` linked, the way the runtime drives it: `ToolInput.data` in, `ToolResult` out, the
//! test's chosen directory preopened at `.`, and `murmur-host<dir>` preopens standing for host
//! folders the session opened.
//!
//! Every assertion reads what reaches the model — `status`, `summary` and the parsed `data` —
//! never `error_kind`, which stays inside the tool; the unit tests of `search`, `fetch` and
//! `paths` assert kinds where they are made.
//!
//! HTTP is served by `std`-only loopback stubs. Wasmtime's `wasi:http` admits every request, so
//! the network allow-list is proved under a real `mur run` in `tests/mur_run_network.rs`. The
//! `https_*` tests reach github.com and are `#[ignore]`d;
//! `cargo test -p tool-registry-search --test search_index -- --ignored https_` runs them.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Once, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_http::WasiHttpCtx;

mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput};
use bindings::Tool;

const FIXTURE: &str = "tests/fixtures/artifacts-index.json";
const SCHEMA_1_FIXTURE: &str = "tests/fixtures/artifacts-index-schema1.json";
const NATIVE_RESULTS: &str = "tests/fixtures/native-0.5.0-results.json";
const SCHEMA_1_HINT: &str = "`mur index <dir> --repo <owner>/<repo> --release <tag>`";
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;

/// Keys that would make the tool a judge of fit or a source of install material.
const FORBIDDEN_KEYS: &[&str] = &[
    "fit",
    "newest_fitting",
    "unserved",
    "wit_level",
    "platform",
    "interfaces",
    "assets",
    "file",
    "sha256",
    "published_date",
];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ── the Wasmtime harness ─────────────────────────────────────────────────────

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

static BUILD: Once = Once::new();

/// Path to the compiled component, building it once per test process. `cargo test --workspace`
/// (CI) runs before the separate wasm build step, so the test cannot assume it exists.
fn component_path() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate_dir().join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/tool_registry_search.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-registry-search",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(crate_dir().join("../.."))
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(status.success(), "cargo build of the wasm component failed");
    });

    assert!(
        wasm.exists(),
        "compiled component not found at {}",
        wasm.display()
    );
    wasm
}

fn engine_and_component() -> &'static (Engine, Component) {
    static LOADED: OnceLock<(Engine, Component)> = OnceLock::new();
    LOADED.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        let component = Component::from_file(&engine, component_path()).expect("load component");
        (engine, component)
    })
}

/// What reaches the model from one call: the runtime fences `summary` and `data` and passes
/// `status` on; `data` is parsed here.
#[derive(Debug)]
struct Seen {
    status: &'static str,
    summary: String,
    data: Option<Value>,
}

impl std::fmt::Display for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} | {} | {:?}", self.status, self.summary, self.data)
    }
}

/// Runs the component once on `data` (sent as `ToolInput.data`, compact JSON), with `cwd`
/// preopened at `.` and each of `host_roots` preopened read-only as `murmur-host<dir>`.
fn tool_with(cwd: &Path, host_roots: &[&Path], data: &Value) -> Seen {
    let (engine, component) = engine_and_component();
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker).expect("add wasi:http");

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(cwd, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen the work folder");
    for root in host_roots {
        let name = format!("murmur-host{}", root.display());
        builder
            .preopened_dir(root, &name, DirPerms::READ, FilePerms::READ)
            .expect("preopen a host root");
    }
    let mut store = Store::new(
        engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
            http: WasiHttpCtx::new(),
        },
    );
    let tool = Tool::instantiate(&mut store, component, &linker).expect("instantiate");
    let input = ToolInput {
        data: Some(data.to_string()),
        log_path: None,
    };
    let result = tool
        .murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run");

    assert!(result.attachments.is_empty(), "this tool attaches nothing");
    assert!(result.metadata.is_empty(), "this tool sets no metadata");
    assert_eq!(result.data_path, None);
    assert!(!result.truncated);
    Seen {
        status: match result.status {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Error => "error",
        },
        summary: result.summary.expect("every result carries a summary"),
        data: result.data.map(|d| {
            serde_json::from_str(&d).unwrap_or_else(|e| panic!("data not JSON ({e}): {d}"))
        }),
    }
}

fn tool_in(cwd: &Path, data: Value) -> Seen {
    tool_with(cwd, &[], &data)
}

fn tool(data: Value) -> Seen {
    tool_in(&crate_dir(), data)
}

/// A passed search's `data`.
fn passed(out: &Seen) -> &Value {
    assert_eq!(out.status, "passed", "{out}");
    out.data.as_ref().expect("a passed search carries data")
}

fn search_fixture(query: &str) -> Value {
    let out = tool(json!({ "query": query, "index": FIXTURE }));
    passed(&out).clone()
}

fn result_names(data: &Value) -> Vec<String> {
    data["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

/// A failure: `error`, no `data`, and a summary holding every needle.
fn assert_error(out: &Seen, needles: &[&str]) {
    assert_eq!(out.status, "error", "{out}");
    assert_eq!(out.data, None, "{out}");
    for needle in needles {
        assert!(out.summary.contains(needle), "{needle:?} not in {out}");
    }
}

fn assert_no_forbidden_keys(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    !FORBIDDEN_KEYS.contains(&key.as_str()),
                    "output carries the forbidden key {key:?}"
                );
                assert_no_forbidden_keys(child);
            }
        }
        Value::Array(items) => items.iter().for_each(assert_no_forbidden_keys),
        _ => {}
    }
}

fn the_acceptance_result(out: &Seen) {
    let data = passed(out);
    assert_eq!(out.summary, "1 result(s)");
    assert_eq!(data["scope"], "murmur");
    assert_eq!(data["count"], 1);
    assert_eq!(data["results"][0]["name"], "murmur/tool-editor");
    assert_eq!(
        data["results"][0]["versions"],
        json!([
            {"version": "0.7.0", "release": "v0.26.0", "built_at_level": 1, "platforms": ["any"]},
            {"version": "0.6.0", "release": "v0.25.0", "built_at_level": 1, "platforms": ["any"]},
        ])
    );
    assert_no_forbidden_keys(data);
}

// ── loopback HTTP stub ───────────────────────────────────────────────────────

/// Serves every connection to a fresh loopback port with the raw response `respond` gives
/// for the request path, for as long as the test process lives. Returns the base URL.
fn serve(respond: fn(&str) -> Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer(stream, respond));
        }
    });
    base
}

fn answer(mut stream: TcpStream, respond: fn(&str) -> Vec<u8>) {
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&request);
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    // The client may hang up mid-body (the size cap); that is not the stub's failure.
    let _ = stream.write_all(&respond(&path));
}

fn http_response(status: &str, extra_headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

// ── 1. acceptance ────────────────────────────────────────────────────────────

#[test]
fn acceptance_tool_editor_lists_every_version_newest_first() {
    let out = tool(json!({ "query": "tool-editor", "index": FIXTURE }));
    the_acceptance_result(&out);
    let data = out.data.unwrap();
    assert_eq!(data["index"], FIXTURE);
    // Only `data` reaches the model: nothing of the native top-level mirror is inside it.
    for key in ["ok", "message", "status", "summary", "data", "error_kind"] {
        assert!(data.get(key).is_none(), "data carries {key:?}: {data}");
    }
}

// ── 2. platforms ─────────────────────────────────────────────────────────────

#[test]
fn native_tool_lists_each_payload_platform() {
    let data = search_fixture("tool-git");
    assert_eq!(data["results"][0]["name"], "murmur/tool-git");
    assert_eq!(
        data["results"][0]["versions"],
        json!([{"version": "0.2.0", "release": "v0.25.0", "built_at_level": 1,
                "platforms": ["darwin-aarch64", "linux-x86_64"]}])
    );
}

#[test]
fn skill_payload_is_platform_independent() {
    let data = search_fixture("skill-create-manifest");
    assert_eq!(data["results"][0]["name"], "murmur/skill-create-manifest");
    assert_eq!(
        data["results"][0]["versions"][0]["platforms"],
        json!(["any"])
    );
}

// ── 3. ranking ───────────────────────────────────────────────────────────────

#[test]
fn full_bare_and_upper_case_names_rank_the_artifact_first() {
    for query in ["murmur/tool-editor", "tool-editor", "TOOL-EDITOR"] {
        let data = search_fixture(query);
        assert_eq!(
            result_names(&data)[0],
            "murmur/tool-editor",
            "query {query}"
        );
    }
}

#[test]
fn limit_caps_the_results() {
    let out = tool(json!({ "query": "tool", "index": FIXTURE, "limit": 1 }));
    let data = passed(&out);
    assert_eq!(data["count"], 1, "{out}");
    assert_eq!(data["results"].as_array().unwrap().len(), 1);
    assert_eq!(out.summary, "1 result(s)");
}

#[test]
fn description_only_match_is_returned() {
    // "messages" is in driver-anthropic's description, and in no name or tag.
    let data = search_fixture("Messages");
    assert_eq!(result_names(&data), ["murmur/driver-anthropic"]);
}

#[test]
fn tag_only_match_is_returned() {
    let dir = TempDir::new().unwrap();
    let index = json!({"schema_version": 2, "scope": "acme", "artifacts": [
        {"name": "tool-a", "runtime": "tool", "description": "Does a.", "tags": ["zebra"],
         "versions": [{"version": "0.1.0", "release": "v1", "interfaces": [],
                       "assets": [{"file": "tool-a-0.1.0.mur.zip", "sha256": "00"}]}]},
        {"name": "tool-b", "runtime": "tool", "tags": [], "versions": []},
    ]});
    fs::write(dir.path().join("artifacts-index.json"), index.to_string()).unwrap();
    let out = tool_in(
        dir.path(),
        json!({ "query": "zebra", "index": "artifacts-index.json" }),
    );
    let data = passed(&out);
    assert_eq!(result_names(data), ["acme/tool-a"], "{out}");
    assert_eq!(
        data["results"][0]["versions"][0]["built_at_level"],
        Value::Null
    );
}

#[test]
fn no_match_is_passed_with_zero_results() {
    let out = tool(json!({ "query": "zzz", "index": FIXTURE }));
    assert_eq!(passed(&out)["count"], 0);
    assert_eq!(out.summary, "0 result(s)");
}

// ── 4. schema refusals ───────────────────────────────────────────────────────

#[test]
fn schema_1_index_is_refused_with_the_regeneration_command() {
    let out = tool(json!({ "query": "git", "index": SCHEMA_1_FIXTURE }));
    assert_error(
        &out,
        &[SCHEMA_1_FIXTURE, "schema 1", "schema 2", SCHEMA_1_HINT],
    );
}

/// Writes the fixture's text with `from` replaced by `to` into `dir` as `artifacts-index.json`;
/// the call then runs with `dir` as its work folder and names the file relatively.
fn edited_fixture(dir: &TempDir, fixture: &str, from: &str, to: &str) -> &'static str {
    let text = fs::read_to_string(crate_dir().join(fixture)).unwrap();
    assert!(text.contains(from), "{fixture} has no {from:?}");
    fs::write(
        dir.path().join("artifacts-index.json"),
        text.replacen(from, to, 1),
    )
    .unwrap();
    "artifacts-index.json"
}

#[test]
fn integer_schema_1_index_is_refused_the_same_way() {
    let dir = TempDir::new().unwrap();
    let path = edited_fixture(
        &dir,
        SCHEMA_1_FIXTURE,
        r#""schema_version": "1""#,
        r#""schema_version": 1"#,
    );
    let out = tool_in(dir.path(), json!({ "query": "git", "index": path }));
    assert_error(&out, &[path, "schema 1", "schema 2", SCHEMA_1_HINT]);
}

#[test]
fn other_schema_version_is_refused_naming_schema_2() {
    let dir = TempDir::new().unwrap();
    let path = edited_fixture(
        &dir,
        FIXTURE,
        r#""schema_version": 2"#,
        r#""schema_version": 3"#,
    );
    let out = tool_in(dir.path(), json!({ "query": "git", "index": path }));
    assert_error(&out, &["schema_version 3", "schema 2"]);
}

#[test]
fn missing_schema_version_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = edited_fixture(&dir, FIXTURE, r#""schema_version": 2,"#, "");
    let out = tool_in(dir.path(), json!({ "query": "git", "index": path }));
    assert_error(&out, &["no schema_version", "schema 2"]);
}

// ── 5. malformed ─────────────────────────────────────────────────────────────

#[test]
fn malformed_indexes_are_parse_errors_naming_the_source() {
    let dir = TempDir::new().unwrap();
    let cases = [
        ("not-json.json", "this is not json".to_string()),
        ("array.json", "[]".to_string()),
        (
            "no-versions.json",
            json!({"schema_version": 2, "scope": "murmur",
                   "artifacts": [{"name": "tool-x", "runtime": "tool", "tags": []}]})
            .to_string(),
        ),
    ];
    for (file, text) in cases {
        fs::write(dir.path().join(file), text).unwrap();
        let out = tool_in(dir.path(), json!({ "query": "x", "index": file }));
        assert_error(&out, &["failed to parse the artifact index from", file]);
        if file == "no-versions.json" {
            assert!(out.summary.contains("versions"), "{out}");
        }
    }
}

// ── 6. old inputs ────────────────────────────────────────────────────────────

#[test]
fn registry_input_is_refused_naming_index() {
    let out = tool(json!({ "query": "x", "registry": "local" }));
    assert_error(&out, &["'registry' input was removed", "'index'"]);
}

#[test]
fn local_index_is_refused_naming_mur_search() {
    let out = tool(json!({ "query": "editor", "index": "local" }));
    assert_error(&out, &["mur search editor --index local"]);
}

#[test]
fn double_encoded_data_reads_the_fixture() {
    let inner = json!({ "query": "tool-editor", "index": FIXTURE }).to_string();
    let out = tool(Value::String(inner));
    the_acceptance_result(&out);
}

#[test]
fn missing_data_and_missing_query_are_refused() {
    let out = tool(json!({}));
    assert_error(&out, &["invalid search parameters: ", "query"]);
    let out = tool(json!({ "query": "" }));
    assert_error(&out, &["query must not be empty"]);
}

// ── 7. paths ─────────────────────────────────────────────────────────────────

#[test]
fn relative_path_resolves_against_the_working_directory() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::copy(crate_dir().join(FIXTURE), dir.path().join("sub/index.json")).unwrap();
    the_acceptance_result(&tool_in(
        dir.path(),
        json!({ "query": "tool-editor", "index": "sub/index.json" }),
    ));
    // The same relative path from another directory names no file.
    let out = tool(json!({ "query": "tool-editor", "index": "sub/index.json" }));
    assert_error(&out, &["failed to read sub/index.json: "]);
}

#[test]
fn absolute_path_is_read_under_a_held_root() {
    let fixtures = fs::canonicalize(crate_dir().join("tests/fixtures")).unwrap();
    let path = fixtures.join("artifacts-index.json");
    let work = TempDir::new().unwrap();
    let out = tool_with(
        work.path(),
        &[&fixtures],
        &json!({ "query": "tool-editor", "index": path }),
    );
    the_acceptance_result(&out);
    assert_eq!(out.data.unwrap()["index"], path.to_str().unwrap());

    // Lexical `.` and `..` resolve before the root is matched.
    let dotted = format!("{}/./sub/../artifacts-index.json", fixtures.display());
    let out = tool_with(
        work.path(),
        &[&fixtures],
        &json!({ "query": "tool-editor", "index": dotted }),
    );
    the_acceptance_result(&out);
}

#[test]
fn absolute_path_is_refused_with_no_root() {
    let path = fs::canonicalize(crate_dir().join(FIXTURE)).unwrap();
    let path = path.to_str().unwrap();
    let out = tool(json!({ "query": "tool-editor", "index": path }));
    assert_error(
        &out,
        &[
            &format!("failed to read {path}: "),
            "opens no host folder",
            "a relative path opens in the work folder",
        ],
    );
}

#[test]
fn absolute_path_is_refused_outside_every_root() {
    let path = fs::canonicalize(crate_dir().join(FIXTURE)).unwrap();
    let other = TempDir::new().unwrap();
    let other_root = fs::canonicalize(other.path()).unwrap();
    let out = tool_with(
        &crate_dir(),
        &[&other_root],
        &json!({ "query": "tool-editor", "index": path }),
    );
    assert_error(
        &out,
        &[
            &format!("failed to read {}: ", path.display()),
            "outside every host folder",
            other_root.to_str().unwrap(),
            "a relative path opens in the work folder",
        ],
    );
    // A `..` that climbs out of the root lexically is refused, never opened.
    let climbing = format!("{}/../../etc/passwd", other_root.display());
    let out = tool_with(
        &crate_dir(),
        &[&other_root],
        &json!({ "query": "x", "index": climbing }),
    );
    assert_error(&out, &["outside every host folder"]);
}

#[test]
fn missing_file_is_refused_naming_the_path() {
    let out = tool(json!({ "query": "x", "index": "no/such/artifacts-index.json" }));
    assert_error(&out, &["failed to read no/such/artifacts-index.json"]);
}

// ── 8. loopback HTTP ─────────────────────────────────────────────────────────

#[test]
fn http_redirect_is_followed() {
    let base = serve(|path| match path {
        "/latest" => http_response("302 Found", "Location: /v1/artifacts-index.json\r\n", b""),
        "/v1/artifacts-index.json" => {
            let body = fs::read(crate_dir().join(FIXTURE)).unwrap();
            http_response("200 OK", "Content-Type: application/json\r\n", &body)
        }
        _ => http_response("404 Not Found", "", b""),
    });
    let url = format!("{base}/latest");
    let out = tool(json!({ "query": "tool-editor", "index": url }));
    the_acceptance_result(&out);
    assert_eq!(out.data.unwrap()["index"], url);
}

/// `/r<N>` redirects to `/r<N+1>`, so `/r0` needs a sixth redirect to reach `/r6`, and `/r1`
/// reaches it in exactly five.
fn redirect_chain(path: &str) -> Vec<u8> {
    match path.strip_prefix("/r").and_then(|n| n.parse::<u32>().ok()) {
        Some(6) => {
            let body = fs::read(crate_dir().join(FIXTURE)).unwrap();
            http_response("200 OK", "", &body)
        }
        Some(n) => http_response("302 Found", &format!("Location: /r{}\r\n", n + 1), b""),
        None => http_response("404 Not Found", "", b""),
    }
}

#[test]
fn five_redirects_are_followed_and_a_sixth_is_refused() {
    let base = serve(redirect_chain);
    the_acceptance_result(&tool(
        json!({ "query": "tool-editor", "index": format!("{base}/r1") }),
    ));
    let url = format!("{base}/r0");
    let out = tool(json!({ "query": "tool-editor", "index": url }));
    assert_error(
        &out,
        &[&format!("failed to fetch {url}: more than 5 redirects")],
    );
}

#[test]
fn http_404_is_refused() {
    let base = serve(|_| http_response("404 Not Found", "", b"not here"));
    let url = format!("{base}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, &[&format!("HTTP 404 fetching {url}")]);
}

#[test]
fn oversized_body_is_refused() {
    let base = serve(|_| http_response("200 OK", "", &vec![b' '; MAX_INDEX_BYTES + 1]));
    let url = format!("{base}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, &[&url, "more than 16 MiB"]);
}

#[test]
fn non_json_body_is_a_parse_error() {
    let base = serve(|_| http_response("200 OK", "", b"<html>hello</html>"));
    let url = format!("{base}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, &["failed to parse the artifact index from", &url]);
}

#[test]
fn a_refused_connection_names_the_url() {
    // Bind and drop: nothing listens on the port afterwards.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, &[&format!("failed to fetch {url}: ")]);
}

// ── 9. TLS ───────────────────────────────────────────────────────────────────

/// An `https://` index makes the guest send `Scheme::Https`, so the host's `wasi:http` opens a
/// TLS session: the first bytes on the wire are a handshake record.
#[test]
fn https_index_starts_a_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stub = thread::spawn(move || -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(_) => return Vec::new(),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut first = Vec::new();
        let mut buf = [0u8; 512];
        while first.len() < 2 {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => first.extend_from_slice(&buf[..n]),
            }
        }
        first
    });

    let url = format!("https://127.0.0.1:{port}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    let first = stub.join().unwrap();

    assert!(
        first.starts_with(&[0x16, 0x03]),
        "expected a TLS handshake record, got {first:02x?}; tool said {out}"
    );
    assert_error(&out, &[&format!("failed to fetch {url}: ")]);
}

// ── 10. the native 0.5.0 build's answers ─────────────────────────────────────

/// `summary` with a trailing ` (os error N)` cut: WASI numbers errno differently from the host
/// the native build ran on.
fn without_os_error(summary: &str) -> &str {
    match summary.rfind(" (os error ") {
        Some(at) if summary.ends_with(')') => &summary[..at],
        _ => summary,
    }
}

#[test]
fn native_0_5_0_results() {
    let recorded: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(crate_dir().join(NATIVE_RESULTS)).unwrap())
            .unwrap();
    assert_eq!(recorded.len(), 14, "the recorded case list");
    for case in &recorded {
        let out = tool(case["input"].clone());
        let what = &case["input"];
        assert_eq!(out.status, case["status"], "status for {what}");
        let want = case["summary"].as_str().unwrap();
        if what["index"] == "no/such/artifacts-index.json" {
            assert_eq!(
                without_os_error(&out.summary),
                without_os_error(want),
                "summary for {what}"
            );
        } else {
            assert_eq!(out.summary, want, "summary for {what}");
        }
        let want_data = match &case["data"] {
            Value::Null => None,
            data => Some(data.clone()),
        };
        assert_eq!(out.data, want_data, "data for {what}");
    }
}

// ── 11. real HTTPS ───────────────────────────────────────────────────────────

const V0_24_0_RAW_URL: &str =
    "https://raw.githubusercontent.com/murmur-nexus/default-artifacts/v0.24.0/artifacts-index.json";

#[test]
#[ignore = "needs network: fetches from github.com"]
fn https_schema_1_index_on_github_is_read_and_refused() {
    let out = tool(json!({ "query": "git", "index": V0_24_0_RAW_URL }));
    assert_error(
        &out,
        &[V0_24_0_RAW_URL, "schema 1", "schema 2", SCHEMA_1_HINT],
    );
}

#[test]
#[ignore = "needs network: fetches from github.com"]
fn https_default_index_is_fetched() {
    let out = tool(json!({ "query": "git" }));
    let data = passed(&out);
    assert_eq!(data["scope"], "murmur", "{out}");
    assert_eq!(
        data["index"],
        "https://github.com/murmur-nexus/default-artifacts/releases/latest/download/artifacts-index.json"
    );
    assert_no_forbidden_keys(data);
}
