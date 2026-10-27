//! driver-baseten proved in the component under a real `mur run`, against a loopback fake
//! Baseten.
//!
//! The unit tests in `src/lib.rs` drive `process_baseten_sse_line` and the host-side body
//! parser, neither of which owns a WASI resource or sends a header. Only a launch shows what the
//! guest puts on the wire through murmur's credential gateway — the body, the
//! `x-session-affinity` header, the replayed reasoning — and that a stream cut before
//! `data: [DONE]` or carrying an `error` chunk ends the turn with the driver's error rather than
//! trapping the guest while it releases the response body.
//!
//! Each test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake provider that records every request's method, path,
//! headers and body. The driver is built out of this workspace on every run; `mur` comes from
//! `MUR_BIN` or `PATH`, and a run that finds neither fails rather than skipping.
//!
//! They are `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=… cargo test -p driver-baseten --test mur_run_fake_baseten -- --ignored` runs them.
//! With `DRIVER_BASETEN_KEEP=<dir>` set, the round-trip test copies its session records and the
//! requests the fake recorded into `<dir>`, which must not exist yet.
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
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const DRIVER_CRATE: &str = "drivers/driver-baseten";

/// The capsule's name and version. The host's `prompt_cache_key` opens with
/// `<name>:<version>:`, so that prefix appearing anywhere on the wire means the raw key left the
/// guest. The bare name may appear legitimately, in the system prompt's `[Capsule]` block.
const CAPSULE_NAME: &str = "baseten-fake-proof";
const CAPSULE_VERSION: &str = "0.1.0";

/// The driver config most scenarios run with: replaying reasoning, by default.
const EFFORT_MAX: &[&str] = &["reasoning_effort: max"];

/// [`EFFORT_MAX`] with earlier turns' reasoning left out.
const EFFORT_MAX_NO_REPLAY: &[&str] = &["reasoning_effort: max", "replay_reasoning: false"];

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
                "driver-baseten",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for driver-baseten");
        assert!(status.success(), "cargo build of driver-baseten failed");
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

    /// The capsule manifest with [`EFFORT_MAX`] as its driver config.
    fn write_manifest(&self, endpoint: &str) {
        self.write_manifest_with_config(endpoint, EFFORT_MAX);
    }

    /// The capsule manifest: `zai-org/GLM-5.3` behind the endpoint with `config` as its
    /// `inference.driver.config` entries, and content tracing on so the session's request and
    /// response blobs are kept.
    fn write_manifest_with_config(&self, endpoint: &str, config: &[&str]) {
        let config: String = config
            .iter()
            .map(|entry| format!("      {entry}\n"))
            .collect();
        fs::write(
            self.manifest(),
            format!(
                "name: {CAPSULE_NAME}\nversion: {CAPSULE_VERSION}\nartifacts:\n\
                 \x20 - name: local/driver-baseten\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 inference:\n  transport: http\n  model: zai-org/GLM-5.3\n  max_tokens: 16384\n  \
                 driver:\n    artifact: local/driver-baseten\n    config:\n{config}\
                 trace:\n  capture: content\n",
                self.version
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
    staging.write_manifest("http://127.0.0.1:1/v1");

    let mut publish = mur(&staging.home);
    publish.arg("publish").arg(&zip);
    run_to_success(publish, "publishing driver-baseten");

    let mut install = mur(&staging.home);
    install
        .current_dir(staging.project.path())
        .arg("install")
        .arg(&zip);
    run_to_success(install, "installing driver-baseten");
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

/// The reasoning the fake streams on the tool-call turn, split across three deltas. Request 2
/// must carry it back byte for byte.
const STREAMED_REASONING: [&str; 3] = [
    "The user wants one probe call. ",
    "I'll call it with \"ls\" — ",
    "then report.",
];

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": "zai-org/GLM-5.3",
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn usage_chunk(prompt: u64, cached: u64, completion: u64, reasoning: u64) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": "zai-org/GLM-5.3",
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "prompt_tokens_details": {"cached_tokens": cached},
            "completion_tokens_details": {"reasoning_tokens": reasoning}
        }})
    )
}

/// Reasoning, then one `probe` tool call in two argument fragments, `finish_reason:
/// "tool_calls"`, a usage-only chunk and `[DONE]`.
fn reasoning_tool_call_stream() -> String {
    let head = json!({
        "index": 0, "id": "call_probe_1", "type": "function",
        "function": {"name": "probe", "arguments": "{\"cmd\":"}
    });
    let tail = json!({"index": 0, "function": {"arguments": "\"ls\"}"}});
    STREAMED_REASONING
        .iter()
        .map(|piece| {
            chat_chunk(
                json!({"role": "assistant", "reasoning_content": piece}),
                None,
            )
        })
        .collect::<String>()
        + &chat_chunk(json!({"tool_calls": [head]}), None)
        + &chat_chunk(json!({"tool_calls": [tail]}), None)
        + &chat_chunk(json!({}), Some("tool_calls"))
        + &usage_chunk(1200, 0, 40, 25)
        + "data: [DONE]\n\n"
}

fn text_stop_stream() -> String {
    chat_chunk(json!({"reasoning_content": "Probe answered."}), None)
        + &chat_chunk(json!({"content": "done after the tool"}), None)
        + &chat_chunk(json!({}), Some("stop"))
        + &usage_chunk(1300, 1152, 12, 4)
        + "data: [DONE]\n\n"
}

/// A complete turn whose stream stops after its finish reason, without `data: [DONE]`.
fn stream_without_done() -> String {
    chat_chunk(json!({"content": "The result is"}), None) + &chat_chunk(json!({}), Some("stop"))
}

const OVERLOADED: &str = r#"{"code":529,"message":"overloaded","type":"server_error"}"#;

/// Some text, then an `error` chunk, then the connection closes.
fn stream_with_error_chunk() -> String {
    chat_chunk(json!({"content": "partial"}), None)
        + &format!("data: {{\"error\":{OVERLOADED}}}\n\n")
}

#[derive(Clone, Copy)]
enum Script {
    /// Request 1 gets a reasoning plus tool-call turn; a request carrying a tool result gets text.
    RoundTrip,
    /// Every request gets [`stream_without_done`].
    NoDone,
    /// Every request gets [`stream_with_error_chunk`].
    ErrorChunk,
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

/// A loopback Chat Completions server that answers by `script` and records every request.
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
                recorded.lock().unwrap().push(request);
                let payload = match script {
                    Script::RoundTrip if follow_up => text_stop_stream(),
                    Script::RoundTrip => reasoning_tool_call_stream(),
                    Script::NoDone => stream_without_done(),
                    Script::ErrorChunk => stream_with_error_chunk(),
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

/// Copy the session records and the recorded requests into `DRIVER_BASETEN_KEEP`, when set.
fn keep(staging: &Staging, requests: &[Recorded]) {
    let Some(dir) = std::env::var_os("DRIVER_BASETEN_KEEP") else {
        return;
    };
    let dir = PathBuf::from(dir);
    fs::create_dir(&dir).expect("DRIVER_BASETEN_KEEP must name a directory that does not exist");
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

/// The `context_id` of the session's one task, read from its `task_start` trace line.
fn task_context_id(staging: &Staging) -> String {
    let traces = traces_under(&staging.records());
    assert_eq!(traces.len(), 1, "one session trace: {traces:?}");
    fs::read_to_string(&traces[0])
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "task_start")
        .and_then(|event| event["context_id"].as_str().map(str::to_string))
        .expect("the trace must hold a task_start line with a context_id")
}

fn assert_no_trap(text: &str) {
    assert!(
        !text.contains("E-RUN-007") && !text.contains("trapped"),
        "the guest must not trap:\n{text}"
    );
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// A reasoning plus tool-call turn, then a text turn: both requests carry the stamped body
/// fields and one hashed affinity header, the raw cache key goes nowhere, and request 2 replays
/// request 1's reasoning and tool call.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_tool_round_trip_replays_reasoning_and_keeps_one_session_affinity() {
    let staging = stage();
    let provider = Provider::start(Script::RoundTrip);
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    let requests = provider.requests();
    keep(&staging, &requests);

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(text.contains("status:  ok"), "{text}");
    assert_no_trap(&text);
    assert_eq!(requests.len(), 2, "one tool round trip:\n{text}");

    for (n, request) in requests.iter().enumerate() {
        eprintln!(
            "request {}: {} {}\n  x-session-affinity: {:?}\n  body: {}",
            n + 1,
            request.method,
            request.path,
            request.header("x-session-affinity"),
            request.body
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        let compact = request.body.replace([' ', '\n'], "");
        for stamped in [
            r#""reasoning_effort":"max""#,
            r#""stream":true"#,
            r#""stream_options":{"include_usage":true}"#,
        ] {
            assert!(
                compact.contains(stamped),
                "request {} must carry {stamped}: {}",
                n + 1,
                request.body
            );
        }
        let body = request.json();
        assert_eq!(body["model"], json!("zai-org/GLM-5.3"));
        assert_eq!(body["max_tokens"], json!(16384));
        assert_eq!(
            body["chat_template_args"],
            json!({"clear_thinking": false}),
            "request {} stamps the replaying setting: {}",
            n + 1,
            request.body
        );
        for forbidden in [
            "thinking",
            "clear_thinking",
            "tool_stream",
            "prompt_cache_key",
            "max_completion_tokens",
        ] {
            assert!(
                body.get(forbidden).is_none(),
                "request {} must not carry {forbidden}: {}",
                n + 1,
                request.body
            );
        }
    }

    let affinity: Vec<&str> = requests
        .iter()
        .map(|r| {
            r.header("x-session-affinity")
                .expect("every request carries x-session-affinity")
        })
        .collect();
    assert_eq!(
        affinity[0], affinity[1],
        "a task's turns share one affinity"
    );
    assert_eq!(affinity[0].len(), 64, "{}", affinity[0]);
    assert!(
        affinity[0]
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "lowercase hex: {}",
        affinity[0]
    );
    // The host's key for a task with a context id, rebuilt from the trace: the header is its
    // SHA-256, so the value Baseten routes on is derived from the key and nothing else.
    let raw_key = format!(
        "{CAPSULE_NAME}:{CAPSULE_VERSION}:{}",
        task_context_id(&staging)
    );
    let expected: String = Sha256::digest(raw_key.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(affinity[0], expected, "the SHA-256 of {raw_key}");

    let raw_key_prefix = format!("{CAPSULE_NAME}:{CAPSULE_VERSION}:");
    for (n, request) in requests.iter().enumerate() {
        assert!(
            !request.body.contains(&raw_key_prefix),
            "request {} body carries the raw cache key: {}",
            n + 1,
            request.body
        );
        for (name, value) in &request.headers {
            assert!(
                !value.contains(&raw_key_prefix),
                "request {} header {name} carries the raw cache key: {value}",
                n + 1
            );
        }
    }

    let follow_up = requests[1].json();
    let assistant = follow_up["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap_or_else(|| panic!("request 2 must carry the assistant turn: {follow_up}"));
    assert_eq!(
        assistant["reasoning_content"].as_str().map(str::as_bytes),
        Some(STREAMED_REASONING.concat().as_bytes()),
        "request 1's streamed reasoning goes back byte for byte: {assistant}"
    );
    assert_eq!(
        assistant["tool_calls"],
        json!([{"id": "call_probe_1", "type": "function",
                "function": {"name": "probe", "arguments": "{\"cmd\":\"ls\"}"}}])
    );
    assert_eq!(assistant["content"], Value::Null);
    assert!(
        follow_up["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_probe_1"),
        "request 2 carries the tool result: {follow_up}"
    );
}

/// The same round trip with `replay_reasoning: false`: request 2's assistant turn carries its
/// tool call but no reasoning, every request stamps `clear_thinking: true`, and the session
/// still completes.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn replay_reasoning_false_sends_no_reasoning_and_stamps_clear_thinking_true() {
    let staging = stage();
    let provider = Provider::start(Script::RoundTrip);
    staging.write_manifest_with_config(&provider.endpoint, EFFORT_MAX_NO_REPLAY);

    let output = run_session(&staging);
    let text = combined(&output);
    let requests = provider.requests();

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(text.contains("status:  ok"), "{text}");
    assert_no_trap(&text);
    assert_eq!(requests.len(), 2, "one tool round trip:\n{text}");

    for (n, request) in requests.iter().enumerate() {
        eprintln!("request {}: body: {}", n + 1, request.body);
        let body = request.json();
        assert_eq!(body["reasoning_effort"], json!("max"));
        assert_eq!(
            body["chat_template_args"],
            json!({"clear_thinking": true}),
            "request {} stamps the setting in force: {}",
            n + 1,
            request.body
        );
        assert!(
            !request.body.contains("reasoning_content"),
            "request {} carries no reasoning_content: {}",
            n + 1,
            request.body
        );
    }

    let follow_up = requests[1].json();
    let assistant = follow_up["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap_or_else(|| panic!("request 2 must carry the assistant turn: {follow_up}"));
    assert_eq!(
        assistant,
        &json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": "call_probe_1", "type": "function",
                            "function": {"name": "probe", "arguments": "{\"cmd\":\"ls\"}"}}]
        }),
        "the tool-call turn goes back without its reasoning"
    );
}

/// A stream that stops after its finish reason without `data: [DONE]` fails the task with the
/// driver's error; the guest does not trap.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_stream_without_done_fails_the_task_in_the_component() {
    let staging = stage();
    let provider = Provider::start(Script::NoDone);
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains("driver: Baseten stream ended before data: [DONE]"),
        "the task must end with the driver error:\n{text}"
    );
    let requests = provider.requests().len();
    eprintln!("requests the fake received: {requests}");
    assert!(requests > 0, "{text}");
}

/// A stream carrying an `error` chunk fails the task with the driver's error; the guest does
/// not trap.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn an_error_chunk_fails_the_task_in_the_component() {
    let staging = stage();
    let provider = Provider::start(Script::ErrorChunk);
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    eprintln!("--- mur run ---\n{text}");

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert_no_trap(&text);
    assert!(
        text.contains(&format!("driver: Baseten stream error: {OVERLOADED}")),
        "the task must end with the driver error:\n{text}"
    );
    let requests = provider.requests().len();
    eprintln!("requests the fake received: {requests}");
    assert!(requests > 0, "{text}");
}
