//! driver-xai proved in the component under a real `mur run`, against a loopback fake xAI
//! Responses server.
//!
//! The unit tests in `src/lib.rs` drive `process_xai_sse_line` and the host-side body parser,
//! neither of which owns a WASI resource or sends a header. Only a launch shows what the guest
//! puts on the wire through murmur's credential gateway — the body, the headers, the reasoning
//! item sent back verbatim — that murmur stores the `xai_reasoning` member of a `thinking` block
//! and hands it back, that it takes the cap and window from the driver's `models:` table, and
//! that a stream cut before its terminal event or carrying an `error` event ends the turn with
//! the driver's error rather than trapping the guest.
//!
//! Each test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake provider that records every request's method, path,
//! headers and body. The driver is built out of this workspace on every run; `mur` comes from
//! `MUR_BIN` or `PATH`, and a run that finds neither fails rather than skipping.
//!
//! They are `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=… cargo test -p driver-xai --test mur_run_fake_xai -- --ignored` runs them. With
//! `DRIVER_XAI_KEEP=<dir>` set, the round-trip test copies its session records and the requests
//! the fake recorded into `<dir>`, which must not exist yet.
//!
//! Unix only: it spawns a host `mur` and reads Unix permission bits, so a `wasm32-wasip2` build
//! of the test targets compiles it to nothing.
#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex, Once},
    thread,
    time::Duration,
};

use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_CRATE: &str = "drivers/driver-xai";

const GROK_47: &str = "grok-4.7";
const GROK_BUILD: &str = "grok-build-0.1";

/// The capsule's name and version. The host's `prompt_cache_key` opens with
/// `<name>:<version>:`, and this driver forwards it.
const CAPSULE_NAME: &str = "xai-fake-proof";
const CAPSULE_VERSION: &str = "0.1.0";

/// The key the capsule hands the gateway; the fake must see it as the runtime's Bearer header.
const API_KEY: &str = "test-key";

/// Body members the driver never sends.
const FORBIDDEN: &[&str] = &[
    "previous_response_id",
    "max_tokens",
    "max_completion_tokens",
    "tool_choice",
    "parallel_tool_calls",
    "frequency_penalty",
    "presence_penalty",
    "stop",
    "background",
    "reasoning_effort",
];

// ── the runtime under test ───────────────────────────────────────────────────

/// The `mur` binary these tests launch: `MUR_BIN` when set, otherwise the first executable
/// `mur` on `PATH`. Panics when neither resolves.
fn mur_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os("MUR_BIN") {
        let path = PathBuf::from(explicit);
        assert!(
            is_executable(&path),
            "MUR_BIN names {}, which is not executable",
            path.display()
        );
        return path;
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("mur"))
        .find(|candidate| is_executable(candidate))
        .unwrap_or_else(|| {
            panic!(
                "no `mur` binary: set MUR_BIN to one, or put it on PATH. These tests launch the \
                 runtime as a subprocess, so a missing binary is a failure rather than a skip."
            )
        })
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn mur(home: &TempDir) -> Command {
    let mut command = Command::new(mur_binary());
    command.env("HOME", home.path());
    command
}

fn run_to_success(mut command: Command, what: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("spawning `mur` for {what}: {err}"));
    assert!(
        output.status.success(),
        "{what} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

// ── building and packing ─────────────────────────────────────────────────────

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn build_driver() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                "driver-xai",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for driver-xai");
        assert!(status.success(), "cargo build of driver-xai failed");
    });
}

/// A top-level scalar in the driver's `murmur.yaml`, by line scan.
fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

/// Pack the driver with `scripts/package-artifact.sh`; returns its version and zip.
fn pack(out_dir: &Path) -> (String, PathBuf) {
    build_driver();
    let crate_path = workspace_root().join(DRIVER_CRATE);
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let name = manifest_field(&manifest, "name");
    let version = manifest_field(&manifest, "version");
    let wasm = target_dir()
        .join("wasm32-wasip2/release")
        .join(format!("{}.wasm", name.replace('-', "_")));
    assert!(
        wasm.exists(),
        "{} not found after a successful build",
        wasm.display()
    );
    let zip = out_dir.join(format!("{name}-{version}.mur.zip"));
    let status = Command::new("bash")
        .arg(workspace_root().join("scripts/package-artifact.sh"))
        .arg(&crate_path)
        .arg(out_dir)
        .arg("--payload")
        .arg(&wasm)
        .env("MUR_BIN", mur_binary())
        .stdout(std::process::Stdio::null())
        .status()
        .expect("failed to spawn scripts/package-artifact.sh");
    assert!(
        status.success() && zip.is_file(),
        "packing {name}-{version}.mur.zip failed"
    );
    (version, zip)
}

// ── staging ──────────────────────────────────────────────────────────────────

/// What a capsule manifest varies by.
struct Capsule<'a> {
    model: &'a str,
    /// `inference.driver.config` entries, `key: value` each.
    config: &'a [&'a str],
    /// `inference.max_tokens` and `context.max_tokens`, or neither.
    limits: Option<(u64, u64)>,
}

impl<'a> Capsule<'a> {
    fn of(model: &'a str) -> Self {
        Self {
            model,
            config: &[],
            limits: None,
        }
    }
}

struct Staging {
    home: TempDir,
    project: TempDir,
    version: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    /// The capsule manifest: `capsule.model` behind the endpoint, with content tracing on so the
    /// session's request and response blobs are kept.
    fn write_manifest(&self, endpoint: &str, capsule: &Capsule) {
        let config = if capsule.config.is_empty() {
            " {}\n".to_string()
        } else {
            let entries: String = capsule
                .config
                .iter()
                .map(|entry| format!("      {entry}\n"))
                .collect();
            format!("\n{entries}")
        };
        let (max_tokens, context) = match capsule.limits {
            Some((output, window)) => (
                format!("  max_tokens: {output}\n"),
                format!("context:\n  max_tokens: {window}\n"),
            ),
            None => (String::new(), String::new()),
        };
        fs::write(
            self.manifest(),
            format!(
                "name: {CAPSULE_NAME}\nversion: {CAPSULE_VERSION}\nartifacts:\n\
                 \x20 - name: local/driver-xai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: {API_KEY}\n\
                 inference:\n  transport: http\n  model: \"{}\"\n{max_tokens}  \
                 driver:\n    artifact: local/driver-xai\n    config:{config}\
                 {context}trace:\n  capture: content\n",
                self.version, capsule.model
            ),
        )
        .unwrap();
    }

    fn records(&self) -> PathBuf {
        self.home.path().join(".murmur/records")
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (version, zip) = pack(artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        version,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1", &Capsule::of(GROK_47));

    let mut publish = mur(&staging.home);
    publish.arg("publish").arg(&zip);
    run_to_success(publish, "publishing driver-xai");

    let mut install = mur(&staging.home);
    install
        .current_dir(staging.project.path())
        .arg("install")
        .arg(&zip);
    run_to_success(install, "installing driver-xai");
    staging
}

/// Run one task to completion and return its output, whatever its exit status.
fn run_session(staging: &Staging) -> Output {
    let workdir = staging.project.path().join("work");
    fs::create_dir_all(&workdir).unwrap();
    mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "call the probe tool once, then say done",
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .output()
        .expect("spawning `mur run`")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ── the fake provider ────────────────────────────────────────────────────────

/// The encrypted content the fake returns on the tool-call turn's reasoning item.
const ENCRYPTED: &str = "gAAAAABpZmFrZS1lbmNyeXB0ZWQtcmVhc29uaW5n+/=";

/// The summary the fake streams on the tool-call turn, split across two deltas.
const SUMMARY: [&str; 2] = [
    "The user wants one probe call. ",
    "I'll call it with \"ls\".",
];

/// The reasoning item exactly as the fake sends it on `response.output_item.done`. Request 2
/// must carry it back unchanged.
fn reasoning_item() -> Value {
    json!({
        "id": "rs_fake_1",
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": SUMMARY.concat()}],
        "status": "completed",
        "encrypted_content": ENCRYPTED,
    })
}

fn event(data: Value) -> String {
    format!(
        "event: {}\ndata: {data}\n\n",
        data["type"].as_str().unwrap_or("message")
    )
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": input + output + reasoning,
        "cost_in_usd_ticks": 1234,
    })
}

fn completed(usage: Value) -> String {
    event(json!({"type": "response.completed", "sequence_number": 99,
                 "response": {"id": "resp_fake", "object": "response", "status": "completed",
                              "store": false, "usage": usage}}))
}

/// Summary deltas, the reasoning item, one `probe` call whose arguments arrive in two deltas,
/// and `response.completed` with cached and reasoning tokens.
fn reasoning_tool_call_stream() -> String {
    let call = json!({"type": "function_call", "id": "fc_fake_1", "call_id": "call_probe_1",
                      "name": "probe", "arguments": "", "status": "in_progress"});
    let mut done_call = call.clone();
    done_call["arguments"] = json!("{\"cmd\":\"ls\"}");
    done_call["status"] = json!("completed");
    [
        event(json!({"type": "response.created", "response": {"id": "resp_fake", "status": "in_progress"}})),
        event(json!({"type": "response.output_item.added", "output_index": 0,
                     "item": {"id": "rs_fake_1", "type": "reasoning", "summary": []}})),
        event(json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                     "summary_index": 0, "delta": SUMMARY[0]})),
        event(json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                     "summary_index": 0, "delta": SUMMARY[1]})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()})),
        event(json!({"type": "response.output_item.added", "output_index": 1, "item": call})),
        event(json!({"type": "response.function_call_arguments.delta", "output_index": 1,
                     "delta": "{\"cmd\":"})),
        event(json!({"type": "response.function_call_arguments.delta", "output_index": 1,
                     "delta": "\"ls\"}"})),
        event(json!({"type": "response.function_call_arguments.done", "output_index": 1,
                     "arguments": "{\"cmd\":\"ls\"}"})),
        event(json!({"type": "response.output_item.done", "output_index": 1, "item": done_call})),
        completed(usage(1200, 1000, 15, 25)),
    ]
    .concat()
}

fn text_stop_stream() -> String {
    [
        event(
            json!({"type": "response.output_text.delta", "output_index": 0,
                     "delta": "done after "}),
        ),
        event(
            json!({"type": "response.output_text.delta", "output_index": 0,
                     "delta": "the tool"}),
        ),
        completed(usage(1300, 1152, 12, 4)),
    ]
    .concat()
}

/// Some text, then the connection closes with no terminal event.
fn stream_without_terminal_event() -> String {
    event(
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "The result is"}),
    )
}

/// Some text, then an `error` event, then the connection closes.
fn stream_with_error_event() -> String {
    event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"}))
        + &event(json!({"type": "error", "code": "invalid_request_error",
                        "message": "the fake refuses", "param": null}))
}

#[derive(Clone, Copy)]
enum Script {
    /// Request 1 gets a reasoning plus tool-call turn; a request carrying a tool result gets text.
    RoundTrip,
    /// Every request gets [`stream_without_terminal_event`].
    NoTerminal,
    /// Every request gets [`stream_with_error_event`].
    ErrorEvent,
    /// Every request gets [`text_stop_stream`].
    TextStop,
}

/// A loopback Responses server that answers by `script` and records every request.
struct Provider {
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Provider {
    fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let follow_up = request
                    .body
                    .replace(' ', "")
                    .contains("\"type\":\"function_call_output\"");
                recorded.lock().unwrap().push(request);
                let payload = match script {
                    Script::RoundTrip if follow_up => text_stop_stream(),
                    Script::RoundTrip => reasoning_tool_call_stream(),
                    Script::NoTerminal => stream_without_terminal_event(),
                    Script::ErrorEvent => stream_with_error_event(),
                    Script::TextStop => text_stop_stream(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{payload}",
                    payload.len()
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

/// One request as the fake received it, header names lowercased.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|err| panic!("request body is not JSON ({err}): {}", self.body))
    }

    fn to_json(&self) -> Value {
        json!({
            "method": self.method,
            "path": self.path,
            "headers": self.headers.iter().map(|(n, v)| json!([n, v])).collect::<Vec<_>>(),
            "body": self.json(),
        })
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
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
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Copy the session records and the recorded requests into `DRIVER_XAI_KEEP`, when set.
fn keep(staging: &Staging, requests: &[Recorded]) {
    let Some(dir) = std::env::var_os("DRIVER_XAI_KEEP") else {
        return;
    };
    let dir = PathBuf::from(dir);
    fs::create_dir(&dir).expect("DRIVER_XAI_KEEP must name a directory that does not exist");
    let status = Command::new("cp")
        .arg("-r")
        .arg(staging.records())
        .arg(dir.join("records"))
        .status()
        .expect("spawning cp");
    assert!(status.success(), "copying the session records failed");
    let recorded: Vec<Value> = requests.iter().map(Recorded::to_json).collect();
    fs::write(
        dir.join("requests.json"),
        serde_json::to_string_pretty(&recorded).unwrap(),
    )
    .unwrap();
}

/// Every `trace.jsonl` under `dir`.
fn traces_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(traces_under(&path));
        } else if path.file_name().is_some_and(|name| name == "trace.jsonl") {
            found.push(path);
        }
    }
    found
}

fn assert_no_trap(text: &str) {
    assert!(
        !text.contains("E-RUN-007") && !text.contains("trapped"),
        "the guest must not trap:\n{text}"
    );
}

/// Every trace line of the session's one trace with `event_type` `kind`.
fn trace_events(staging: &Staging, kind: &str) -> Vec<Value> {
    let traces = traces_under(&staging.records());
    assert_eq!(traces.len(), 1, "one session trace: {traces:?}");
    fs::read_to_string(&traces[0])
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event_type"] == kind)
        .collect()
}

/// Every request goes to `/v1/responses` with the runtime's Bearer header, `store: false`, the
/// encrypted-reasoning `include`, `stream: true`, the model verbatim, the capsule's cache key and
/// no forbidden member.
fn assert_wire_shape(requests: &[Recorded], model: &str) {
    let cache_key_prefix = format!("{CAPSULE_NAME}:{CAPSULE_VERSION}:");
    for (n, request) in requests.iter().enumerate() {
        let n = n + 1;
        eprintln!(
            "request {n}: {} {}\n  headers: {:?}\n  body: {}",
            request.method, request.path, request.headers, request.body
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {API_KEY}").as_str()),
            "request {n}: the runtime attaches upstream_auth"
        );
        assert!(
            request
                .header("content-type")
                .is_some_and(|v| v.starts_with("application/json")),
            "request {n}: {:?}",
            request.headers
        );
        let body = request.json();
        assert_eq!(body["model"], json!(model), "request {n}");
        assert_eq!(body["store"], json!(false), "request {n}");
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "request {n}"
        );
        assert_eq!(body["stream"], json!(true), "request {n}");
        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| key.starts_with(&cache_key_prefix)),
            "request {n}: the cache key is forwarded: {}",
            body["prompt_cache_key"]
        );
        for forbidden in FORBIDDEN {
            assert!(
                !request.body.contains(&format!("\"{forbidden}\"")),
                "request {n} must not carry {forbidden}: {}",
                request.body
            );
        }
    }
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// (1) grok-4.7, one tool round trip: request 2 sends back request 1's reasoning item byte for
/// byte, before the `function_call` and its `function_call_output`; the trace carries the cached
/// and reasoning counts, and the limits come from `models:`.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn grok_4_7_tool_round_trip_sends_the_encrypted_reasoning_back() {
    let staging = stage();
    let provider = Provider::start(Script::RoundTrip);
    staging.write_manifest(&provider.endpoint, &Capsule::of(GROK_47));

    let output = run_session(&staging);
    let text = combined(&output);
    let requests = provider.requests();
    keep(&staging, &requests);
    eprintln!("--- mur run ---\n{text}");

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(text.contains("status:  ok"), "{text}");
    assert_no_trap(&text);
    assert_eq!(requests.len(), 2, "one tool round trip:\n{text}");
    assert_wire_shape(&requests, GROK_47);
    for request in &requests {
        assert_eq!(request.json()["reasoning"], json!({"effort": "high"}));
        assert_eq!(request.json()["max_output_tokens"], json!(128_000));
    }

    let follow_up = requests[1].json();
    let input = follow_up["input"].as_array().unwrap();
    let at = |kind: &str| {
        input
            .iter()
            .position(|item| item["type"] == kind)
            .unwrap_or_else(|| panic!("request 2 must carry a {kind} item: {follow_up}"))
    };
    let (reasoning, call, result) = (
        at("reasoning"),
        at("function_call"),
        at("function_call_output"),
    );
    assert!(reasoning < call && call < result, "{follow_up}");
    assert_eq!(
        serde_json::to_string(&input[reasoning]).unwrap(),
        serde_json::to_string(&reasoning_item()).unwrap(),
        "request 1's reasoning item goes back byte for byte"
    );
    assert!(
        requests[1].body.contains(ENCRYPTED),
        "the encrypted content is on the wire verbatim"
    );
    assert_eq!(
        input[call],
        json!({"type": "function_call", "call_id": "call_probe_1", "name": "probe",
               "arguments": "{\"cmd\":\"ls\"}"})
    );
    assert_eq!(input[result]["call_id"], "call_probe_1");
    assert!(
        !requests[0].body.contains("\"reasoning\",") && !requests[0].body.contains(ENCRYPTED),
        "request 1 has no reasoning to send back"
    );

    let inferences = trace_events(&staging, "inference");
    eprintln!("inference events: {inferences:#?}");
    let reporting: Vec<&Value> = inferences
        .iter()
        .filter(|event| event.get("cached_tokens").is_some())
        .collect();
    assert_eq!(
        reporting.len(),
        2,
        "both turns report usage: {inferences:#?}"
    );
    assert_eq!(reporting[0]["cached_tokens"], json!(1000));
    assert_eq!(reporting[0]["input_tokens_actual"], json!(200));
    assert_eq!(reporting[0]["output_tokens_actual"], json!(40));
    assert_eq!(reporting[0]["thinking_tokens"], json!(25));
    assert_eq!(reporting[1]["cached_tokens"], json!(1152));
    assert_eq!(reporting[1]["thinking_tokens"], json!(4));

    let starts = trace_events(&staging, "session_start");
    let limits = &starts[0]["model_limits"];
    eprintln!("session_start.model_limits = {limits}");
    assert_eq!(limits["max_output_tokens"], json!(128_000), "{limits}");
    assert_eq!(
        limits["max_output_tokens_source"],
        json!("driver"),
        "{limits}"
    );
    assert_eq!(limits["context_window"], json!(500_000), "{limits}");
    assert_eq!(limits["context_window_source"], json!("driver"), "{limits}");
}

/// (2) A stream that ends with no terminal event fails the turn with the driver's error, which
/// is transient, so murmur sends the same request again; the guest does not trap.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_stream_with_no_terminal_event_fails_the_turn_and_is_retried() {
    let staging = stage();
    let provider = Provider::start(Script::NoTerminal);
    staging.write_manifest(&provider.endpoint, &Capsule::of(GROK_47));

    let output = run_session(&staging);
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains("driver: xAI Responses stream ended with no terminal event"),
        "the task must end with the driver error:\n{text}"
    );
    let requests = provider.requests();
    eprintln!("requests the fake received: {}", requests.len());
    assert!(
        requests.len() > 1,
        "murmur retries a transient failure:\n{text}"
    );
    assert!(
        requests.windows(2).all(|pair| pair[0].body == pair[1].body),
        "a retry sends the same request"
    );
}

/// (3) A stream carrying an `error` event fails the task with the provider's error; the guest
/// does not trap.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn an_error_event_fails_the_task_in_the_component() {
    let staging = stage();
    let provider = Provider::start(Script::ErrorEvent);
    staging.write_manifest(&provider.endpoint, &Capsule::of(GROK_47));

    let output = run_session(&staging);
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains("xAI error: invalid_request_error: the fake refuses"),
        "the task must end with the provider error:\n{text}"
    );
    let requests = provider.requests().len();
    eprintln!("requests the fake received: {requests}");
    assert!(requests > 0, "{text}");
}

/// (4) grok-build-0.1 with `reasoning_effort: high` is refused before the fake receives any
/// request.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn grok_build_with_an_effort_is_refused_before_any_request() {
    let staging = stage();
    let provider = Provider::start(Script::TextStop);
    staging.write_manifest(
        &provider.endpoint,
        &Capsule {
            config: &["reasoning_effort: high"],
            ..Capsule::of(GROK_BUILD)
        },
    );

    let output = run_session(&staging);
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains(
            "driver: inference.driver.config 'reasoning_effort' is not accepted for model \
             'grok-build-0.1': xAI documents no reasoning effort for it. Remove the key."
        ),
        "the task must end with the refusal:\n{text}"
    );
    assert_eq!(provider.requests().len(), 0, "no request reaches xAI");
}
