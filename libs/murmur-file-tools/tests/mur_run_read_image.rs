//! tool-read's image attachment, proved under a real `mur run` against driver-openai.
//!
//! A capsule with driver-openai on `chat-latest` and tool-read reads `logo.png`. The loopback
//! fake Chat Completions provider's first reply calls `tool-read` with `{"path":"logo.png"}`,
//! and its second ends the turn. The test checks that the second request carries the picture as
//! an `image_url` part whose `data:image/png;base64,` payload is `logo.png`'s bytes (a 64×48 PNG
//! fits `chat-latest`'s 2048 px / 2,500-patch rule, so driver-openai sends it untouched), that
//! the session trace's `tool_call` records the accepted attachment, and that no request says an
//! attachment `was not attached`.
//!
//! The harness is `tests/mur_run_read_only.rs`'s: the real `mur` binary as a subprocess with
//! `HOME` pointed at a scratch directory, the artifacts built out of this workspace on every run.
//! `mur` comes from `MUR_BIN` or `PATH`, and a run that finds none fails rather than skipping.
//! It must print `WIT level 3` from `mur version` and be murmur `a9ea4726` or later: an earlier
//! level-3 `mur` refuses to pack driver-openai, whose `models:` entries carry `vision`
//! (E-BLD-005). It is `#[ignore]`d because the default
//! `cargo test --workspace` has no `mur` to launch:
//!
//! ```text
//! MUR_BIN=<level-3 mur> cargo test -p murmur-file-tools --test mur_run_read_image -- \
//!     --ignored --nocapture
//! ```

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use murmur_file_tools::Tool;
use serde_json::{json, Value};
use tempfile::TempDir;

#[path = "../src/test_png.rs"]
mod test_png;

// ── the runtime under test ───────────────────────────────────────────────────

/// The `mur` binary this test launches: `MUR_BIN` when set, otherwise the first executable
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
                "no `mur` binary: set MUR_BIN to one, or put it on PATH. This test launches the \
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

/// A top-level scalar in an artifact's `murmur.yaml`, by line scan.
fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

/// Build the artifact at `crate_dir` (workspace-relative) for wasm32-wasip2 and pack it with
/// `scripts/package-artifact.sh`; returns its version and zip.
fn pack(crate_dir: &str, out_dir: &Path) -> (String, PathBuf) {
    let crate_path = workspace_root().join(crate_dir);
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let name = manifest_field(&manifest, "name");
    let version = manifest_field(&manifest, "version");

    let status = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .args([
            "build",
            "-p",
            &name,
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .status()
        .unwrap_or_else(|err| panic!("spawning `cargo build` for {name}: {err}"));
    assert!(status.success(), "cargo build of {name} failed");

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
    driver_version: String,
    read_version: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn workdir(&self) -> PathBuf {
        self.project.path().join("work")
    }

    /// The capsule manifest: driver-openai on `chat-latest`, which `MODEL_ROUTES` sends to the
    /// Chat Completions surface, and tool-read, with the trace capturing content.
    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: read-image-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/tool-read\n    version: {}\n    runtime: tool\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n\
                 trace:\n  capture: content\n",
                self.driver_version, self.read_version
            ),
        )
        .unwrap();
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let (read_version, read_zip) = pack("tools/tool-read", artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        read_version,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");

    for (zip, what) in [(&driver_zip, "driver-openai"), (&read_zip, "tool-read")] {
        let mut publish = mur(&staging.home);
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {what}"));

        let mut install = mur(&staging.home);
        install
            .current_dir(staging.project.path())
            .arg("install")
            .arg(zip);
        run_to_success(install, &format!("installing {what}"));
    }
    staging
}

/// Run one task to completion and return its output, whatever its exit status.
fn run_session(staging: &Staging) -> Output {
    mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "What colour is logo.png? Answer in one word.",
            "--workdir",
            staging.workdir().to_str().unwrap(),
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

/// The tool calls the fake model makes, one per turn, in order, each as `(tool, arguments)`; the
/// turn after the last one answers with text.
fn scripted_calls() -> Vec<(Tool, Value)> {
    vec![(Tool::Read, json!({ "path": "logo.png" }))]
}

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

/// One assistant turn holding a single call to `tool` with id `call_<turn>`.
fn tool_call_stream(turn: usize, tool: Tool, arguments: &Value) -> String {
    let call = json!({
        "index": 0, "id": format!("call_{turn}"), "type": "function",
        "function": {"name": tool.name(), "arguments": arguments.to_string()}
    });
    chat_chunk(json!({"role": "assistant", "content": ""}), None)
        + &chat_chunk(json!({"tool_calls": [call]}), None)
        + &chat_chunk(json!({}), Some("tool_calls"))
        + "data: [DONE]\n\n"
}

fn text_stream() -> String {
    chat_chunk(json!({"content": "done"}), None)
        + &chat_chunk(json!({}), Some("stop"))
        + "data: [DONE]\n\n"
}

/// A loopback Chat Completions server. A request carrying N tool results gets the N-th
/// scripted call, or text once the script is spent. It records every request body.
struct Provider {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            let script = scripted_calls();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(body) = read_request_body(&mut stream) else {
                    continue;
                };
                let turn = tool_results(&body).len();
                recorded.lock().unwrap().push(body);
                let payload = match script.get(turn) {
                    Some((tool, call)) => tool_call_stream(turn + 1, *tool, call),
                    None => text_stream(),
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

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// The `(tool_call_id, content)` of every `role: tool` message in a Chat Completions body.
fn tool_results(body: &str) -> Vec<(String, String)> {
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(|message| {
            let content = match &message["content"] {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            (
                message["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                content,
            )
        })
        .collect()
}

fn read_request_body(stream: &mut TcpStream) -> Option<String> {
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
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length: usize = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim() == "content-length")
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

// ── the scenario ─────────────────────────────────────────────────────────────

/// Standard base64 with padding, decoded; `None` on any other byte.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&a| a == byte)? as u32;
        buffer = buffer << 6 | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Every `image_url` URL in the body's messages, in order.
fn image_urls(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .filter_map(|part| part["image_url"]["url"].as_str().map(str::to_string))
        .collect()
}

/// Every `trace.jsonl` line under `dir`, parsed.
fn trace_events(dir: &Path) -> Vec<Value> {
    let mut events = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return events;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            events.extend(trace_events(&path));
        } else if path.file_name().is_some_and(|name| name == "trace.jsonl") {
            events.extend(
                fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
            );
        }
    }
    events
}

#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_read_png_reaches_the_model_as_an_image() {
    let version = combined(&mur_version());
    println!("{version}");
    assert!(
        version.contains("WIT level 3"),
        "an attachment needs a mur at WIT level 3:\n{version}"
    );

    let staging = stage();
    let workdir = staging.workdir();
    fs::create_dir_all(&workdir).unwrap();
    let png = test_png::png(64, 48);
    fs::write(workdir.join("logo.png"), &png).unwrap();

    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one tool call, then the end of the turn");
    for request in &requests {
        assert!(
            !request.contains("was not attached"),
            "murmur refused the attachment: {request}"
        );
    }
    let second: Value = serde_json::from_str(&requests[1]).expect("the request body is JSON");
    let results = tool_results(&requests[1]);
    println!("tool results: {results:?}");
    // murmur fences every tool result as untrusted content before the driver sees it.
    assert_eq!(
        results,
        [(
            "call_1".to_string(),
            format!(
                "<untrusted-content source=tool:tool-read>\nimage/png, {} bytes, 64x48 \
                 pixels\n</untrusted-content>",
                png.len()
            )
        )]
    );
    let urls = image_urls(&second);
    assert_eq!(urls.len(), 1, "one image in the second request: {urls:?}");
    let payload = urls[0]
        .strip_prefix("data:image/png;base64,")
        .unwrap_or_else(|| panic!("a PNG data URL: {}", &urls[0]));
    assert_eq!(
        base64_decode(payload).expect("the payload is base64"),
        png,
        "a 64x48 PNG fits chat-latest's rule, so driver-openai sends logo.png's own bytes"
    );

    let events = trace_events(&staging.home.path().join(".murmur/records"));
    let call = events
        .iter()
        .find(|event| event["event_type"] == "tool_call")
        .unwrap_or_else(|| panic!("no tool_call event in the trace: {events:?}"));
    println!("tool_call attachments: {}", call["attachments"]);
    assert_eq!(call["attachments"][0]["media_type"], "image/png");
    assert_eq!(call["attachments"][0]["path"], "logo.png");
    assert_eq!(call["attachments"][0]["bytes"], png.len());
    assert!(call["attachments"][0]["sha256"].is_string(), "{call}");
    assert!(call["attachments"][0].get("refused").is_none(), "{call}");
}

fn mur_version() -> Output {
    Command::new(mur_binary())
        .arg("version")
        .output()
        .expect("spawning `mur version`")
}
