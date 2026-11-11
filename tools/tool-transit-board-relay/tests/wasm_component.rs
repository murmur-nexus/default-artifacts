//! Host-target integration test for the compiled `tool-transit-board-relay` **wasm32-wasip2
//! component**, run through Wasmtime with `wasi:http` linked.
//!
//! Three things can only be proved here and not by the host unit tests:
//!
//! * The component's `wasi:http` transport sends one real `POST /v1/call` to the address in
//!   `MURMUR_GATEWAY_ENDPOINT`, carrying the envelope and no credential header of its own.
//! * The four variables are read out of the guest environment, and a missing gateway — unset or
//!   empty — fails closed with nothing sent.
//! * Serve's answer, a refusal status and a dead port each come back as the result the README
//!   promises.
//!
//! The fake serve is a loopback `TcpListener` standing where the runtime's gateway would: it
//! records every request, headers included, and answers each with the scripted status and body.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use tool_transit_board_relay::{
    ARTIFACT_CONFIG_ENV, CAPSULE_NAME_ENV, GATEWAY_ENDPOINT_ENV, SESSION_ID_ENV,
};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
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

const CAPSULE: &str = "board-fixer";
const SESSION: &str = "ses_00112233445566778899aabbccddeeff";
const CONFIG: &str = r#"{"board":"/abs/board","verbs":["ls","show","add","dep"],"protect":["Plan","Design","Done"]}"#;
const PREFIX: &str = "tool-transit-board-relay: ";

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

/// Path to the compiled component, building it if absent. `cargo test --workspace` (CI) runs
/// before the separate wasm build step, so the test cannot assume the artifact already exists.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/tool_transit_board_relay.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-transit-board-relay",
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

/// The guest environment: each variable set to its value, `None` leaving it unset.
struct GuestEnv<'a> {
    gateway: Option<&'a str>,
    config: Option<&'a str>,
    capsule: Option<&'a str>,
    session: Option<&'a str>,
}

impl<'a> GuestEnv<'a> {
    fn full(gateway: &'a str) -> Self {
        Self {
            gateway: Some(gateway),
            config: Some(CONFIG),
            capsule: Some(CAPSULE),
            session: Some(SESSION),
        }
    }
}

/// Instantiate the component once and call `run`.
fn run_component(env: &GuestEnv<'_>, data: &str) -> ToolResult {
    let mut engine_config = Config::new();
    engine_config.wasm_component_model(true);
    let engine = Engine::new(&engine_config).expect("engine");
    let component = Component::from_file(&engine, component_path()).expect("load component");

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker).expect("add wasi:http");

    let mut builder = WasiCtxBuilder::new();
    for (name, value) in [
        (GATEWAY_ENDPOINT_ENV, env.gateway),
        (ARTIFACT_CONFIG_ENV, env.config),
        (CAPSULE_NAME_ENV, env.capsule),
        (SESSION_ID_ENV, env.session),
    ] {
        if let Some(value) = value {
            builder.env(name, value);
        }
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
    assert!(
        result.attachments.is_empty(),
        "this tool attaches no image, so ToolResult.attachments is empty"
    );
    assert_eq!(result.data_path, None);
    assert!(!result.truncated);
    result
}

/// One request as the fake saw it.
#[derive(Debug, Clone)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// A loopback fake serve answering every request with `status` and `body`.
struct FakeServe {
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeServe {
    fn start(status: u16, body: String) -> Self {
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
                    "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
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

fn help_answer() -> String {
    json!({
        "ok": true, "verb": "help", "effect": "read",
        "result": {
            "verbs": [{"verb": "ls", "fields": []}, {"verb": "show", "fields": [{"name": "card"}]}],
            "protect": ["Plan", "Design", "Done"]
        },
        "output": null, "logged": []
    })
    .to_string()
}

#[test]
fn help_is_one_post_carrying_the_envelope_and_no_credential() {
    let serve = FakeServe::start(200, help_answer());
    let result = run_component(&GuestEnv::full(&serve.endpoint), r#"{"verb":"help"}"#);

    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    assert_eq!(result.data.as_deref(), Some(help_answer().as_str()));
    assert_eq!(result.summary.as_deref(), Some("transit help: ok (read)"));
    assert_eq!(
        result.metadata,
        vec![("state_effect".to_string(), "read".to_string())]
    );

    let requests = serve.requests();
    assert_eq!(requests.len(), 1, "exactly one request");
    let request = &requests[0];
    assert!(
        request.request_line.starts_with("POST /v1/call "),
        "{}",
        request.request_line
    );
    assert!(
        !request
            .headers
            .iter()
            .any(|(name, _)| name == "authorization"),
        "the relay must send no authorization header: {:?}",
        request.headers
    );
    assert_eq!(
        request.body,
        json!({
            "envelope": 1,
            "capsule": CAPSULE,
            "session": SESSION,
            "config": serde_json::from_str::<Value>(CONFIG).unwrap(),
            "input": {"verb": "help"},
        })
    );
}

#[test]
fn a_refused_answer_fails_with_the_body_verbatim() {
    let body = json!({
        "ok": false, "verb": "mv", "effect": "none",
        "error": {"code": "not-granted", "message": "nexus transit tool: mv is not granted to this capsule"}
    })
    .to_string();
    let serve = FakeServe::start(200, body.clone());
    let result = run_component(
        &GuestEnv::full(&serve.endpoint),
        r#"{"verb":"mv","card":"c1","to":"Done","reason":"r"}"#,
    );
    assert!(matches!(result.status, Status::Failed));
    assert_eq!(result.data.as_deref(), Some(body.as_str()));
    assert_eq!(
        result.summary.as_deref(),
        Some("transit mv: refused not-granted: nexus transit tool: mv is not granted to this capsule")
    );
    assert!(result.metadata.is_empty());
    assert_eq!(serve.requests().len(), 1);
}

#[test]
fn a_401_gives_the_key_line() {
    let serve = FakeServe::start(
        401,
        r#"{"error":{"code":"unauthorized","message":"nexus transit serve: the call carries no valid key"}}"#
            .to_string(),
    );
    let result = run_component(&GuestEnv::full(&serve.endpoint), r#"{"verb":"help"}"#);
    assert!(matches!(result.status, Status::Error));
    let summary = result.summary.unwrap();
    assert!(
        summary.starts_with(&format!(
            "{PREFIX}nexus transit serve refused this entry's key (401): "
        )),
        "{summary}"
    );
    assert!(
        summary.contains("credentials.NEXUS_TRANSIT_TOOL_KEY"),
        "{summary}"
    );
    assert_eq!(result.data.as_deref(), Some(summary.as_str()));
    assert_eq!(serve.requests().len(), 1, "one request, no retry");
}

#[test]
fn an_unset_or_empty_gateway_is_refused_with_nothing_sent() {
    let serve = FakeServe::start(200, help_answer());
    for gateway in [None, Some("")] {
        let env = GuestEnv {
            gateway,
            ..GuestEnv::full(&serve.endpoint)
        };
        let result = run_component(&env, r#"{"verb":"help"}"#);
        assert!(matches!(result.status, Status::Error), "{gateway:?}");
        let summary = result.summary.unwrap();
        assert!(
            summary.starts_with(&format!("{PREFIX}this entry has no gateway")),
            "{summary}"
        );
    }
    assert!(serve.requests().is_empty(), "nothing may reach serve");
}

#[test]
fn a_dead_port_gives_the_transport_line() {
    let endpoint = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let result = run_component(&GuestEnv::full(&endpoint), r#"{"verb":"help"}"#);
    assert!(matches!(result.status, Status::Error));
    let summary = result.summary.unwrap();
    assert!(
        summary.starts_with(&format!("{PREFIX}no answer through the gateway (")),
        "{summary}"
    );
    assert!(
        summary.ends_with(
            "); check that nexus transit serve is running on the port this entry's gateway.endpoint names"
        ),
        "{summary}"
    );
}
