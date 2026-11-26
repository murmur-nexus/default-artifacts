//! driver-openai-compatible proved in the component under a real `mur run`, against a loopback
//! fake OpenAI compatible server.
//!
//! The unit tests in `src/lib.rs` drive `process_sse_line` and the host-side body parser, neither
//! of which owns a WASI resource or sends a header. Only a launch shows what the guest puts on the
//! wire through murmur's credential gateway — the path, the body, the headers with and without a
//! key — that murmur launches a driver with no `models:` on the capsule's own limits and passes a
//! model id with `/` and `:` through, and that a stream cut before `data: [DONE]`, an `error`
//! chunk or a model `404` ends the task with the driver's error rather than trapping the guest
//! while it releases the response body.
//!
//! Each test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake server that records every request's method, path, headers
//! and body. The driver is built out of this workspace on every run; `mur` comes from `MUR_BIN`
//! or `PATH`, and a run that finds neither fails rather than skipping.
//!
//! They are `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=… cargo test -p driver-openai-compatible --test mur_run_fake_openai_compatible --
//! --ignored` runs them.
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

const DRIVER_CRATE: &str = "drivers/driver-openai-compatible";

/// A served-model name with a `/` and a `:`; it must reach the server byte for byte.
const MODEL: &str = "org/Llama-3.1-8B-Instruct:q4";

/// The capsule's name and version. The host's `prompt_cache_key` opens with
/// `<name>:<version>:`, so that prefix appearing anywhere on the wire means the key was sent.
const CAPSULE_NAME: &str = "openai-compatible-fake-proof";
const CAPSULE_VERSION: &str = "0.1.0";

/// The key an `api_key` capsule hands the gateway; the fake must see it as the runtime's Bearer
/// header.
const API_KEY: &str = "test-key";

/// Body members the driver never sends.
const FORBIDDEN: &[&str] = &[
    "prompt_cache_key",
    "max_completion_tokens",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning_effort",
    "reasoning",
    "reasoning_content",
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
                "driver-openai-compatible",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for driver-openai-compatible");
        assert!(
            status.success(),
            "cargo build of driver-openai-compatible failed"
        );
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

/// How the capsule hands the gateway its credential.
#[derive(Clone, Copy)]
enum Auth {
    Keyless,
    ApiKey,
}

/// What a capsule manifest varies by.
struct Capsule<'a> {
    model: &'a str,
    auth: Auth,
    /// `inference.driver.config` entries, `key: value` each.
    config: &'a [&'a str],
}

impl<'a> Capsule<'a> {
    fn keyless(model: &'a str) -> Self {
        Self {
            model,
            auth: Auth::Keyless,
            config: &[],
        }
    }
}

/// `inference.max_tokens` and `context.max_tokens` on every capsule: the driver declares no
/// `models:`, so murmur takes both from the manifest.
const OUTPUT_CAP: u64 = 256;
const CONTEXT_WINDOW: u64 = 8192;

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
        let auth = match capsule.auth {
            Auth::Keyless => "keyless: true".to_string(),
            Auth::ApiKey => format!("api_key: {API_KEY}"),
        };
        fs::write(
            self.manifest(),
            format!(
                "name: {CAPSULE_NAME}\nversion: {CAPSULE_VERSION}\nartifacts:\n\
                 \x20 - name: local/driver-openai-compatible\n    version: {}\n    \
                 runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      {auth}\n\
                 inference:\n  transport: http\n  model: \"{}\"\n  max_tokens: {OUTPUT_CAP}\n  \
                 driver:\n    artifact: local/driver-openai-compatible\n    config:{config}\
                 context:\n  max_tokens: {CONTEXT_WINDOW}\ntrace:\n  capture: content\n",
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
    staging.write_manifest("http://127.0.0.1:1/v1", &Capsule::keyless(MODEL));

    let mut publish = mur(&staging.home);
    publish.arg("publish").arg(&zip);
    run_to_success(publish, "publishing driver-openai-compatible");

    let mut install = mur(&staging.home);
    install
        .current_dir(staging.project.path())
        .arg("install")
        .arg(&zip);
    run_to_success(install, "installing driver-openai-compatible");
    staging
}

const TOOL_TASK: &str = "call the probe tool once, then say done";

/// Run one task to completion and return its output, whatever its exit status.
fn run_session(staging: &Staging, task: &str) -> Output {
    let workdir = staging.project.path().join("work");
    fs::create_dir_all(&workdir).unwrap();
    mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            task,
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

/// Every file named `name` under `dir`.
fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_named(&path, name));
        } else if path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        }
    }
    found
}

/// The session's `out/result.txt`, under the scratch records directory.
fn result_text(staging: &Staging) -> String {
    let results: Vec<PathBuf> = files_named(&staging.records(), "result.txt")
        .into_iter()
        .filter(|path| path.parent().is_some_and(|dir| dir.ends_with("out")))
        .collect();
    assert_eq!(results.len(), 1, "one session result: {results:?}");
    fs::read_to_string(&results[0]).unwrap()
}

// ── the fake server ──────────────────────────────────────────────────────────

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": MODEL,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn text_chunk(text: &str, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "cmpl-fake", "object": "text_completion", "model": MODEL,
               "choices": [{"index": 0, "text": text, "finish_reason": finish_reason}]})
    )
}

fn usage_chunk(prompt: u64, completion: u64) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-fake", "model": MODEL, "choices": [],
               "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
                         "total_tokens": prompt + completion}})
    )
}

/// One `probe` tool call in two argument fragments, `finish_reason: "tool_calls"`, a usage-only
/// chunk and `[DONE]`.
fn tool_call_stream() -> String {
    let head = json!({
        "index": 0, "id": "call_probe_1", "type": "function",
        "function": {"name": "probe", "arguments": "{\"cmd\":"}
    });
    let tail = json!({"index": 0, "function": {"arguments": "\"ls\"}"}});
    chat_chunk(
        json!({"role": "assistant", "reasoning": "One probe call."}),
        None,
    ) + &chat_chunk(json!({"tool_calls": [head]}), None)
        + &chat_chunk(json!({"tool_calls": [tail]}), None)
        + &chat_chunk(json!({}), Some("tool_calls"))
        + &usage_chunk(1200, 40)
        + "data: [DONE]\n\n"
}

fn text_stop_stream() -> String {
    chat_chunk(json!({"content": "done after the tool"}), None)
        + &chat_chunk(json!({}), Some("stop"))
        + &usage_chunk(1300, 12)
        + "data: [DONE]\n\n"
}

/// The completions answer the fake gives: `A`.
const CHOSEN: &str = "A";

fn completions_stream() -> String {
    text_chunk(CHOSEN, None)
        + &text_chunk("", Some("stop"))
        + &usage_chunk(40, 1)
        + "data: [DONE]\n\n"
}

/// A complete turn whose stream stops after its finish reason, without `data: [DONE]`.
fn stream_without_done() -> String {
    chat_chunk(json!({"content": "The result is"}), None) + &chat_chunk(json!({}), Some("stop"))
}

const BACKEND_DOWN: &str =
    r#"{"code":503,"message":"backend unavailable","param":null,"type":"ServiceUnavailableError"}"#;

/// Some text, then an `error` chunk naming 503, then `[DONE]`, as vLLM streams an error.
fn stream_with_error_chunk() -> String {
    chat_chunk(json!({"content": "partial"}), None)
        + &format!("data: {{\"error\":{BACKEND_DOWN}}}\n\n")
        + "data: [DONE]\n\n"
}

fn model_not_found_body(model: &str) -> String {
    json!({"error": {"message": format!("The model `{model}` does not exist."),
                     "type": "NotFoundError", "param": "model", "code": 404}})
    .to_string()
}

#[derive(Clone, Copy)]
enum Script {
    /// Request 1 gets a tool-call turn; a request carrying a tool result gets text.
    RoundTrip,
    /// Every request gets [`text_stop_stream`].
    TextStop,
    /// Every request gets [`completions_stream`].
    Completions,
    /// Every request gets [`stream_without_done`].
    NoDone,
    /// Every request gets [`stream_with_error_chunk`].
    ErrorChunk,
    /// Every request gets a `404` naming the model.
    ModelNotFound,
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
}

/// A loopback OpenAI compatible server that answers by `script` and records every request.
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
                let follow_up = request.body.replace(' ', "").contains("\"role\":\"tool\"");
                let model = serde_json::from_str::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body["model"].as_str().map(str::to_string))
                    .unwrap_or_default();
                recorded.lock().unwrap().push(request);
                let (status, content_type, payload) = match script {
                    Script::RoundTrip if follow_up => {
                        ("200 OK", "text/event-stream", text_stop_stream())
                    }
                    Script::RoundTrip => ("200 OK", "text/event-stream", tool_call_stream()),
                    Script::TextStop => ("200 OK", "text/event-stream", text_stop_stream()),
                    Script::Completions => ("200 OK", "text/event-stream", completions_stream()),
                    Script::NoDone => ("200 OK", "text/event-stream", stream_without_done()),
                    Script::ErrorChunk => {
                        ("200 OK", "text/event-stream", stream_with_error_chunk())
                    }
                    Script::ModelNotFound => (
                        "404 Not Found",
                        "application/json",
                        model_not_found_body(&model),
                    ),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
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

fn assert_no_trap(text: &str) {
    assert!(
        !text.contains("E-RUN-007") && !text.contains("trapped"),
        "the guest must not trap:\n{text}"
    );
}

/// Every request is a `POST` to `path` with the stamped streaming flags, the model id byte for
/// byte, the capsule's output cap, JSON content, no forbidden member and no cache key.
fn assert_wire_shape(requests: &[Recorded], path: &str, model: &str) {
    let cache_key_prefix = format!("{CAPSULE_NAME}:{CAPSULE_VERSION}:");
    for (n, request) in requests.iter().enumerate() {
        let n = n + 1;
        eprintln!(
            "request {n}: {} {}\n  headers: {:?}\n  body: {}",
            request.method, request.path, request.headers, request.body
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, path, "request {n}");
        assert!(
            request
                .header("content-type")
                .is_some_and(|v| v.starts_with("application/json")),
            "request {n}: {:?}",
            request.headers
        );
        let body = request.json();
        assert_eq!(
            body["model"].as_str().map(str::as_bytes),
            Some(model.as_bytes()),
            "request {n}"
        );
        assert_eq!(body["max_tokens"], json!(OUTPUT_CAP), "request {n}");
        assert_eq!(body["stream"], json!(true), "request {n}");
        assert_eq!(
            body["stream_options"],
            json!({"include_usage": true}),
            "request {n}"
        );
        for forbidden in FORBIDDEN {
            assert!(
                !request.body.contains(&format!("\"{forbidden}\"")),
                "request {n} must not carry {forbidden}: {}",
                request.body
            );
        }
        assert!(
            !request.body.contains(&cache_key_prefix),
            "request {n} carries the cache key: {}",
            request.body
        );
    }
}

fn authorization_headers(request: &Recorded) -> Vec<&str> {
    request
        .headers
        .iter()
        .filter(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str())
        .collect()
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// (1) A keyless chat capsule: a tool-call turn, then a text turn. Two requests to
/// `/v1/chat/completions`, neither with an `authorization` header, the model id unchanged, and
/// the second carrying the tool call and its result.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn keyless_chat_tool_round_trip() {
    let staging = stage();
    let provider = Provider::start(Script::RoundTrip);
    staging.write_manifest(&provider.endpoint, &Capsule::keyless(MODEL));

    let output = run_session(&staging, TOOL_TASK);
    let text = combined(&output);
    let requests = provider.requests();

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(text.contains("status:  ok"), "{text}");
    assert_no_trap(&text);
    assert_eq!(requests.len(), 2, "one tool round trip:\n{text}");
    assert_wire_shape(&requests, "/v1/chat/completions", MODEL);
    for (n, request) in requests.iter().enumerate() {
        assert!(
            authorization_headers(request).is_empty(),
            "request {}: a keyless capsule sends no credential: {:?}",
            n + 1,
            request.headers
        );
        let body = request.json();
        assert!(
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "the runtime's tools go out on chat: {body}"
        );
    }

    let follow_up = requests[1].json();
    let messages = follow_up["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_probe_1"),
        "request 2 carries the tool result: {follow_up}"
    );
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap_or_else(|| panic!("request 2 must carry the assistant turn: {follow_up}"));
    assert_eq!(
        assistant,
        &json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_probe_1", "type": "function",
             "function": {"name": "probe", "arguments": "{\"cmd\":\"ls\"}"}}
        ]}),
        "the tool-call turn goes back without its reasoning"
    );
}

/// (2) An `api_key` chat capsule: the runtime attaches exactly one `authorization: Bearer <key>`.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn api_key_chat_run_sends_one_bearer_header() {
    let staging = stage();
    let provider = Provider::start(Script::TextStop);
    staging.write_manifest(
        &provider.endpoint,
        &Capsule {
            auth: Auth::ApiKey,
            ..Capsule::keyless(MODEL)
        },
    );

    let output = run_session(&staging, "say done");
    let text = combined(&output);
    let requests = provider.requests();

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert_no_trap(&text);
    assert_eq!(requests.len(), 1, "one text turn:\n{text}");
    assert_wire_shape(&requests, "/v1/chat/completions", MODEL);
    assert_eq!(
        authorization_headers(&requests[0]),
        [format!("Bearer {API_KEY}").as_str()],
        "exactly one Bearer header, from upstream_auth"
    );
    assert_eq!(result_text(&staging).trim(), "done after the tool");
}

/// (3) A completions capsule with `structured_outputs.choice` and `temperature: 1`: one request to
/// `/v1/completions` whose `prompt` ends with the task's `<<`, carrying both members and no
/// `tools`; the session's `out/result.txt` holds the fake's answer.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn completions_with_structured_outputs_answers_from_the_prompt() {
    let staging = stage();
    let provider = Provider::start(Script::Completions);
    staging.write_manifest(
        &provider.endpoint,
        &Capsule {
            config: &[
                "surface: completions",
                "temperature: 1",
                "structured_outputs: { choice: [\"A\", \"B\"] }",
            ],
            ..Capsule::keyless("minitaur-8b")
        },
    );

    let output = run_session(&staging, "Offer: 3 of 10. You choose option <<");
    let text = combined(&output);
    let requests = provider.requests();

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(text.contains("status:  ok"), "{text}");
    assert_no_trap(&text);
    assert_eq!(requests.len(), 1, "one completions turn:\n{text}");
    assert_wire_shape(&requests, "/v1/completions", "minitaur-8b");

    let body = requests[0].json();
    let prompt = body["prompt"].as_str().expect("a prompt string");
    assert!(
        prompt.ends_with("Offer: 3 of 10. You choose option <<"),
        "the prompt ends with the task byte for byte: {prompt:?}"
    );
    assert_eq!(body["structured_outputs"], json!({"choice": ["A", "B"]}));
    assert!(
        requests[0].body.contains("\"temperature\":1"),
        "temperature as written: {}",
        requests[0].body
    );
    assert!(
        body.get("tools").is_none(),
        "no tools on completions: {body}"
    );
    assert!(
        body.get("messages").is_none(),
        "no messages on completions: {body}"
    );
    assert_eq!(result_text(&staging).trim(), CHOSEN);
}

/// Run a session against `script` and assert it fails with `message` and does not trap.
fn assert_task_fails_with(script: Script, message: &str) -> Vec<Recorded> {
    let staging = stage();
    let provider = Provider::start(script);
    staging.write_manifest(&provider.endpoint, &Capsule::keyless(MODEL));

    let output = run_session(&staging, "say done");
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains(message),
        "the task must end with the driver error {message:?}:\n{text}"
    );
    let requests = provider.requests();
    eprintln!("requests the fake received: {}", requests.len());
    requests
}

/// (4a) A stream that stops after its finish reason without `data: [DONE]`.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_stream_cut_before_done_fails_the_task_in_the_component() {
    let requests =
        assert_task_fails_with(Script::NoDone, "driver: stream ended before data: [DONE]");
    assert!(!requests.is_empty());
}

/// (4b) A stream carrying an `error` chunk with code 503.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn an_error_chunk_naming_503_fails_the_task_in_the_component() {
    let requests = assert_task_fails_with(
        Script::ErrorChunk,
        &format!("driver: server stream error: {BACKEND_DOWN}"),
    );
    assert!(!requests.is_empty());
}

/// (4c) A `404` naming the model: the driver says the id went through unchanged.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_model_404_fails_the_task_naming_the_model() {
    let requests = assert_task_fails_with(
        Script::ModelNotFound,
        &format!("driver: the server refused model '{MODEL}': HTTP 404: "),
    );
    assert_eq!(requests.len(), 1, "a 404 is not retried");
    assert_eq!(requests[0].path, "/v1/chat/completions");
}

/// (5) An unknown config key fails the task by name before any request reaches the server.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn an_unknown_config_key_fails_the_task_before_any_request() {
    let staging = stage();
    let provider = Provider::start(Script::TextStop);
    staging.write_manifest(
        &provider.endpoint,
        &Capsule {
            config: &["extra_body: { temperature: 1 }"],
            ..Capsule::keyless(MODEL)
        },
    );

    let output = run_session(&staging, "say done");
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains(
            "driver: unrecognised inference.driver.config key(s): extra_body. Accepted keys: \
             structured_outputs, surface, temperature"
        ),
        "{text}"
    );
    assert_eq!(
        provider.requests().len(),
        0,
        "no request reaches the server"
    );
}
