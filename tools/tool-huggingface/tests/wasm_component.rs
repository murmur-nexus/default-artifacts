//! Host-target integration test for the compiled `tool-huggingface` **wasm32-wasip2
//! component**, run through Wasmtime with `wasi:http` linked.
//!
//! What only this test proves, against loopback fakes standing where the Hub (behind the
//! gateway) and the CDN would be:
//!
//! * The component's `wasi:http` transport sends real `HEAD` and `GET` requests to the address
//!   in `MURMUR_GATEWAY_ENDPOINT`, follows the Hub's `302` to the CDN as a direct request, and
//!   streams the file into the work folder with the right sha256. Neither fake sees an
//!   `authorization` header.
//! * An unset or empty gateway fails closed with nothing sent.
//! * A truncated result's full response lands in the work folder preopen at `.`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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

/// Path to the compiled component, building it if absent.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/tool_huggingface.wasm");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-huggingface",
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
        builder.env(tool_huggingface::GATEWAY_ENDPOINT_ENV, gateway);
    }
    if let Some(config) = config {
        builder.env(tool_huggingface::ARTIFACT_CONFIG_ENV, config);
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
    let result = tool
        .murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run");
    assert!(result.attachments.is_empty());
    result
}

/// One request as a fake saw it.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

type Handler = dyn Fn(&Recorded) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync;

/// A loopback HTTP server answering each request through `handler` and recording it.
struct Fake {
    endpoint: String,
    authority: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Fake {
    fn start(handler: Arc<Handler>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                recorded.lock().unwrap().push(request.clone());
                let (status, headers, body) = handler(&request);
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nconnection: close\r\ncontent-length: {}\r\n",
                    body.len()
                );
                for (name, value) in headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                if request.method != "HEAD" {
                    let _ = stream.write_all(&body);
                }
            }
        });
        Self {
            endpoint: format!("http://{authority}"),
            authority,
            requests,
        }
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
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Some(Recorded {
        method,
        target,
        headers,
    })
}

fn has_header(request: &Recorded, name: &str) -> bool {
    request.headers.iter().any(|(n, _)| n == name)
}

#[test]
fn a_download_follows_the_hub_redirect_to_the_cdn_without_a_credential() {
    let blob: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let digest: String = Sha256::digest(&blob)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let cdn_blob = blob.clone();
    let cdn = Fake::start(Arc::new(move |request: &Recorded| {
        assert!(request.target.starts_with("/blob"));
        (200, vec![], cdn_blob.clone())
    }));
    let location = format!("{}/blob?sig=secret", cdn.endpoint);
    let size = blob.len().to_string();
    let etag = format!("\"{digest}\"");
    let hub = Fake::start(Arc::new(move |request: &Recorded| {
        assert_eq!(request.target, "/org/model/resolve/main/weights.bin");
        (
            302,
            vec![
                ("location".to_string(), location.clone()),
                ("x-linked-size".to_string(), size.clone()),
                ("x-linked-etag".to_string(), etag.clone()),
            ],
            Vec::new(),
        )
    }));

    let workdir = tempfile::tempdir().unwrap();
    let result = run_component(
        workdir.path(),
        Some(&hub.endpoint),
        None,
        r#"{"op":"download","repo":"org/model","repo_file":"weights.bin","dest":"dl/weights.bin"}"#,
    );
    assert!(
        matches!(result.status, Status::Passed),
        "{:?} {:?}",
        result.summary,
        result.data
    );
    let data = result.data.unwrap();
    assert!(
        data.contains(&format!(
            "sha256: {digest} (matches the Hub's x-linked-etag)"
        )),
        "{data}"
    );
    assert!(
        data.contains(&format!("→ {} direct (no credential)", cdn.authority)),
        "{data}"
    );
    let written = std::fs::read(workdir.path().join("dl/weights.bin")).unwrap();
    assert_eq!(written, blob);
    assert!(!workdir.path().join("dl/weights.bin.partial").exists());

    let hub_requests = hub.requests();
    let methods: Vec<&str> = hub_requests.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(
        methods,
        ["HEAD", "GET"],
        "HEAD before GET, both to the gateway address"
    );
    let cdn_requests = cdn.requests();
    assert_eq!(cdn_requests.len(), 1);
    assert_eq!(cdn_requests[0].target, "/blob?sig=secret");
    for request in hub_requests.iter().chain(&cdn_requests) {
        assert!(!has_header(request, "authorization"), "{request:?}");
        assert!(!has_header(request, "accept-encoding"), "{request:?}");
    }
}

#[test]
fn an_unset_or_empty_gateway_is_refused_with_nothing_sent() {
    let fake = Fake::start(Arc::new(|_: &Recorded| (200, vec![], b"[]".to_vec())));
    for gateway in [None, Some("")] {
        let workdir = tempfile::tempdir().unwrap();
        let config = format!(
            r#"{{"config_version":1,"datasets_server_endpoint":"{}"}}"#,
            fake.endpoint
        );
        for input in [
            r#"{"op":"search","kind":"model","query":"bert"}"#,
            r#"{"op":"dataset","repo":"org/data"}"#,
        ] {
            let result = run_component(workdir.path(), gateway, Some(&config), input);
            assert!(matches!(result.status, Status::Error));
            let data: Value = serde_json::from_str(&result.data.unwrap()).unwrap();
            assert_eq!(data["error_kind"], json!("gateway_missing"), "{gateway:?}");
        }
    }
    assert!(fake.requests().is_empty(), "nothing may be sent");
}

#[test]
fn an_oversized_result_spills_into_the_work_folder() {
    let items: Vec<Value> = (0..50)
        .map(|i| json!({"id": format!("org/model-{i}"), "downloads": i, "likes": i, "pipeline_tag": "fill-mask", "library_name": "transformers", "tags": ["license:mit"]}))
        .collect();
    let body = Value::Array(items).to_string().into_bytes();
    let hub = Fake::start(Arc::new(move |request: &Recorded| {
        assert!(
            request
                .target
                .starts_with("/api/models?search=bert&limit=50"),
            "{}",
            request.target
        );
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            body.clone(),
        )
    }));
    let workdir = tempfile::tempdir().unwrap();
    let result = run_component(
        workdir.path(),
        Some(&hub.endpoint),
        Some(r#"{"config_version":1,"max_output_bytes":2048}"#),
        r#"{"op":"search","kind":"model","query":"bert","limit":50}"#,
    );
    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    assert!(result.truncated);
    assert!(result.data.unwrap().len() <= 2048);
    let data_path = result.data_path.expect("data_path set");
    assert_eq!(data_path, "hf-results/search-1.json");
    let spilled: Value =
        serde_json::from_slice(&std::fs::read(workdir.path().join(&data_path)).unwrap()).unwrap();
    assert_eq!(spilled.as_array().unwrap().len(), 50);
    let requests = hub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert!(!has_header(&requests[0], "authorization"));
}
