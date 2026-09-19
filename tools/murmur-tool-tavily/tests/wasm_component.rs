//! Host-target integration test for the compiled `murmur-tool-tavily` **wasm32-wasip2
//! component**, run through Wasmtime with `wasi:http` linked.
//!
//! Three things can only be proved here and not by the host unit tests:
//!
//! * The component's `wasi:http` transport sends one real `POST /search` to the address in
//!   `MURMUR_GATEWAY_ENDPOINT`, carrying no credential header of its own making.
//! * The gateway address is read out of the guest environment, and its absence — the
//!   variable unset, or set empty — fails closed with nothing sent.
//! * A truncated search's full response lands in the workdir preopen at `.`.
//!
//! The fake Tavily is a loopback `TcpListener` standing where the runtime's gateway would:
//! it records every request, headers included, and answers each with the scripted body.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
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
use bindings::exports::murmur::tool::run::{Status, ToolInput, ToolResult};
use bindings::Tool;

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

/// Path to the compiled component, building it if absent. `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so the test cannot assume the artifact
/// already exists.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/murmur_tool_tavily.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "murmur-tool-tavily",
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
        "compiled component not found at {}",
        wasm.display()
    );
    wasm
}

/// Instantiate the component once and call `run`. `gateway` is the value of
/// `MURMUR_GATEWAY_ENDPOINT`, `None` leaving it unset; the workdir is preopened at `.`.
fn run_component(
    workdir: &Path,
    gateway: Option<&str>,
    config: Option<&str>,
    data: &str,
) -> ToolResult {
    let mut engine_config = Config::new();
    engine_config.wasm_component_model(true);
    let engine = Engine::new(&engine_config).expect("engine");
    let component = Component::from_file(&engine, component_path()).expect("load component");

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker).expect("add wasi:http");

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen");
    if let Some(gateway) = gateway {
        builder.env(murmur_tool_tavily::GATEWAY_ENDPOINT_ENV, gateway);
    }
    if let Some(config) = config {
        builder.env(murmur_tool_tavily::ARTIFACT_CONFIG_ENV, config);
    }
    let mut store = Store::new(
        &engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
            http: WasiHttpCtx::new(),
        },
    );
    let tool = Tool::instantiate(&mut store, &component, &linker).expect("instantiate");
    let input = ToolInput {
        data: Some(data.to_string()),
        log_path: None,
    };
    tool.murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run")
}

/// One request as the fake saw it.
#[derive(Debug, Clone)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// A loopback fake Tavily answering every request with `body`.
struct FakeTavily {
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeTavily {
    fn start(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                if let Some(request) = read_request(&mut stream) {
                    recorded.lock().unwrap().push(request);
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { endpoint, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Recorded {
        request_line,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn tavily_body(results: usize, content_len: usize) -> String {
    json!({
        "query": "probe",
        "results": (0..results)
            .map(|i| json!({"title": format!("Fake title {i}"), "url": format!("https://example.org/{i}"), "content": "c".repeat(content_len)}))
            .collect::<Vec<_>>(),
        "usage": {"credits": 1},
        "request_id": "fake-request-1"
    })
    .to_string()
}

#[test]
fn a_search_is_one_post_to_the_gateway_carrying_no_credential() {
    let fake = FakeTavily::start(tavily_body(2, 40));
    let workdir = tempfile::tempdir().unwrap();
    let result = run_component(
        workdir.path(),
        Some(&fake.endpoint),
        None,
        r#"{"query":"probe"}"#,
    );

    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    let data = result.data.unwrap();
    assert!(data.contains("[1] Fake title 0\n"), "{data}");
    assert!(!result.truncated);
    assert_eq!(result.data_path, None);

    let requests = fake.requests();
    assert_eq!(requests.len(), 1, "exactly one request");
    let request = &requests[0];
    assert!(
        request.request_line.starts_with("POST /search "),
        "{}",
        request.request_line
    );
    for forbidden in ["authorization", "x-api-key"] {
        assert!(
            !request.headers.iter().any(|(name, _)| name == forbidden),
            "the tool must send no {forbidden} header: {:?}",
            request.headers
        );
    }
    let keys: Vec<&str> = request
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "auto_parameters",
            "include_answer",
            "include_favicon",
            "include_image_descriptions",
            "include_images",
            "include_raw_content",
            "include_usage",
            "max_results",
            "query",
            "search_depth",
            "topic",
        ]
    );
    assert_eq!(request.body["query"], json!("probe"));
}

#[test]
fn an_unset_or_empty_gateway_is_refused_with_nothing_sent() {
    let fake = FakeTavily::start(tavily_body(1, 10));
    for gateway in [None, Some("")] {
        let workdir = tempfile::tempdir().unwrap();
        let result = run_component(workdir.path(), gateway, None, r#"{"query":"probe"}"#);
        assert!(matches!(result.status, Status::Error));
        let data: Value = serde_json::from_str(&result.data.unwrap()).unwrap();
        assert_eq!(data["error_kind"], json!("gateway_missing"), "{gateway:?}");
    }
    assert!(fake.requests().is_empty(), "nothing may reach the upstream");
}

#[test]
fn an_oversized_response_spills_into_the_workdir() {
    let fake = FakeTavily::start(tavily_body(20, 3000));
    let workdir = tempfile::tempdir().unwrap();
    let config =
        r#"{"config_version":1,"max_results":{"default":20,"max":20},"max_output_bytes":4096}"#;
    let result = run_component(
        workdir.path(),
        Some(&fake.endpoint),
        Some(config),
        r#"{"query":"probe"}"#,
    );

    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    assert!(result.truncated);
    assert!(result.data.unwrap().len() <= 4096);
    let data_path = result.data_path.expect("data_path set");
    assert_eq!(data_path, "tavily-results/fake-request-1.json");
    let spilled: Value =
        serde_json::from_slice(&std::fs::read(workdir.path().join(&data_path)).unwrap()).unwrap();
    assert_eq!(spilled["results"].as_array().unwrap().len(), 20);
}
