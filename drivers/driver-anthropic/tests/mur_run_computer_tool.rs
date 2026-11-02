//! A computer tool, proved to reach Claude as Anthropic's own computer tool and to run with
//! Claude's coordinates scaled back to its display, under a real `mur run`.
//!
//! The capsule runs driver-anthropic with `fake-computer`, a native tool compiled with `rustc`
//! from `libs/murmur-driver-computer/test-support/fake_computer.rs` and packed with
//! `provider_tool: {kind: computer, display_width: 1440, display_height: 900}`. It logs each call
//! it gets to `computer-calls.jsonl` in the work folder and attaches the 1440×900 `shot.png` the
//! test writes there. The capsule runs `capabilities.containment: sealed`, which a provider tool
//! requires. A loopback fake Anthropic records each request's body and headers and answers with
//! scripted computer calls, then a final text.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch:
//! `MUR_BIN=… cargo test -p driver-anthropic --test mur_run_computer_tool -- --ignored`. `mur`
//! must be murmur `a9ea4726` or later, which reads `provider_tool:` and serves WIT level 3.
//!
//! Unix only: it spawns a host `mur` and reads Unix permission bits.
#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, Once},
    thread,
    time::Duration,
};

use murmur_driver_image::fixtures::{png, screenshot_like};
use murmur_driver_image::{decode_base64, dimensions};
use serde_json::{json, Value};
use tempfile::TempDir;

const TOOL: &str = "fake-computer";

// ── the runtime under test ───────────────────────────────────────────────────

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
        .unwrap_or_else(|| panic!("no `mur` binary: set MUR_BIN to one, or put it on PATH"))
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
                "driver-anthropic",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for driver-anthropic");
        assert!(status.success(), "cargo build of driver-anthropic failed");
    });
}

fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

fn package(artifact_dir: &Path, out_dir: &Path, payload: &Path) -> PathBuf {
    let output = Command::new("bash")
        .arg(workspace_root().join("scripts/package-artifact.sh"))
        .arg(artifact_dir)
        .arg(out_dir)
        .arg("--payload")
        .arg(payload)
        .env("MUR_BIN", mur_binary())
        .stderr(Stdio::inherit())
        .output()
        .expect("failed to spawn scripts/package-artifact.sh");
    assert!(
        output.status.success(),
        "packing {} failed",
        artifact_dir.display()
    );
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

/// Pack the driver; returns its version and zip.
fn pack_driver(out_dir: &Path) -> (String, PathBuf) {
    build_driver();
    let crate_path = workspace_root().join("drivers/driver-anthropic");
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let version = manifest_field(&manifest, "version");
    let wasm = target_dir().join("wasm32-wasip2/release/driver_anthropic.wasm");
    (version, package(&crate_path, out_dir, &wasm))
}

const FAKE_COMPUTER_MANIFEST: &str = "name: fake-computer\nversion: 0.1.0\nruntime: tool\n\
implementation: native\ndescription: Stands in for tool-computer on a 1440x900 display.\n\
requires_files:\n  - bin/fake-computer\n\
input_schema: '{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\"},\
\"x\":{\"type\":\"integer\"},\"y\":{\"type\":\"integer\"},\"text\":{\"type\":\"string\"}},\
\"required\":[\"action\"]}'\n";

const PROVIDER_TOOL_BLOCK: &str =
    "provider_tool:\n  kind: computer\n  display_width: 1440\n  display_height: 900\n";

/// Compile `fake_computer.rs` with `rustc` and pack it as a native tool artifact, declaring the
/// computer `provider_tool:` block when `provider_tool`.
fn pack_fake_computer(scratch: &Path, out_dir: &Path, provider_tool: bool) -> PathBuf {
    let binary = scratch.join("fake-computer-bin");
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-o"])
        .arg(&binary)
        .arg(workspace_root().join("libs/murmur-driver-computer/test-support/fake_computer.rs"))
        .status()
        .expect("failed to spawn rustc");
    assert!(status.success(), "rustc fake_computer.rs failed");
    let dir = scratch.join("fake-computer");
    fs::create_dir_all(&dir).unwrap();
    let mut manifest = FAKE_COMPUTER_MANIFEST.to_string();
    if provider_tool {
        manifest.push_str(PROVIDER_TOOL_BLOCK);
    }
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
    package(&dir, out_dir, &binary)
}

// ── the fake provider ────────────────────────────────────────────────────────

fn response(body: &Value) -> String {
    let payload = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{payload}",
        payload.len()
    )
}

/// One recorded request: its lower-cased header block and its body.
#[derive(Clone)]
struct Recorded {
    headers: String,
    body: Value,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
    }
}

type Answer = Box<dyn Fn(usize, &Value) -> Value + Send>;

struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Provider {
    fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some((headers, body)) = read_request(&mut stream) else {
                    continue;
                };
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let mut all = recorded.lock().unwrap();
                let reply = response(&answer(all.len(), &body));
                all.push(Recorded { headers, body });
                drop(all);
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Self { port, requests }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 65536];
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
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((head, body))
}

fn message(model: &str, content: Value, stop_reason: &str) -> Value {
    json!({"id": "msg_1", "type": "message", "role": "assistant", "model": model,
           "content": content, "stop_reason": stop_reason,
           "usage": {"input_tokens": 10, "output_tokens": 5}})
}

/// The first answer is `calls`; every later one ends the turn.
fn script(model: &'static str, calls: Value) -> Answer {
    Box::new(move |index, _| {
        if index == 0 {
            message(model, calls.clone(), "tool_use")
        } else {
            message(
                model,
                json!([{"type": "text", "text": "Done."}]),
                "end_turn",
            )
        }
    })
}

// ── the capsule ──────────────────────────────────────────────────────────────

struct Run {
    requests: Vec<Recorded>,
    calls: Vec<Value>,
    output: String,
}

/// Install the driver and fake-computer into a sealed capsule on `model`, run one task against a
/// fake Anthropic answering with `answer`, and return what the provider and the tool saw.
fn run_capsule(model: &str, provider_tool: bool, answer: Answer) -> Run {
    let scratch = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let (version, driver_zip) = pack_driver(artifacts.path());
    let tool_zip = pack_fake_computer(scratch.path(), artifacts.path(), provider_tool);
    let provider = Provider::start(answer);
    let endpoint = provider.endpoint();

    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: computer-proof\nversion: 0.1.0\nartifacts:\n\
             \x20 - name: local/driver-anthropic\n    version: {version}\n    runtime: driver\n    \
             gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
             \x20 - name: local/{TOOL}\n    version: 0.1.0\n    runtime: tool\n\
             capabilities:\n  containment: sealed\n\
             inference:\n  transport: http\n  model: {model}\n  max_tokens: 4096\n  \
             driver:\n    artifact: local/driver-anthropic\n\
             trace:\n  capture: content\n"
        ),
    )
    .unwrap();
    for zip in [&driver_zip, &tool_zip] {
        let mut publish = mur(&home);
        publish.arg("publish").arg(zip);
        run_to_success(publish, "publishing");
        let mut install = mur(&home);
        install.current_dir(project.path()).arg("install").arg(zip);
        run_to_success(install, "installing");
    }

    let workdir = project.path().join("work");
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("shot.png"), png(&screenshot_like(1440, 900))).unwrap();
    let task = project.path().join("task.md");
    fs::write(&task, "Click the OK button.\n").unwrap();

    let output = mur(&home)
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            task.to_str().unwrap(),
        ])
        .arg("--workdir")
        .arg(&workdir)
        .stdin(Stdio::null())
        .output()
        .expect("spawning `mur run`");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ) + &format!("\nexit: {}", output.status);
    assert!(
        !output.contains("E-RUN-007") && !output.contains("trapped"),
        "the guest must not trap:\n{output}"
    );
    if output.contains("E-CAP-") {
        panic!(
            "sealed containment could not be reached on this host, so the run failed:\n{output}"
        );
    }
    assert!(
        output.contains("exit: exit status: 0"),
        "the run must succeed:\n{output}"
    );
    eprintln!("{output}");

    let achieved: Vec<Value> = trace_events(home.path())
        .into_iter()
        .filter_map(|event| event.get("containment_achieved").cloned())
        .collect();
    eprintln!("containment_achieved in the trace: {achieved:?}");
    assert!(
        !achieved.is_empty() && achieved.iter().all(|value| value == "sealed"),
        "the capsule must run sealed, and the trace says {achieved:?}"
    );

    let calls = fs::read_to_string(workdir.join("computer-calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Run {
        requests: provider.requests(),
        calls,
        output,
    }
}

/// Every event of every `trace.jsonl` under `dir`.
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
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
            );
        }
    }
    events
}

fn tools(request: &Recorded) -> Vec<Value> {
    request.body["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The blocks of the request's last message.
fn last_message(request: &Recorded) -> Vec<Value> {
    request.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone()
}

/// The blocks of every message after the request's last assistant message: murmur answers each
/// call of a turn with its own tool message, so the results arrive as consecutive user messages,
/// which Anthropic reads as one turn.
fn after_last_assistant(request: &Recorded) -> Vec<Value> {
    let messages = request.body["messages"].as_array().unwrap();
    let last = messages
        .iter()
        .rposition(|message| message["role"] == "assistant")
        .unwrap();
    messages[last + 1..]
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap().clone())
        .collect()
}

/// The blocks of the request's last assistant message.
fn last_assistant(request: &Recorded) -> Vec<Value> {
    request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant")
        .unwrap()["content"]
        .as_array()
        .unwrap()
        .clone()
}

fn image_size(block: &Value) -> (u32, u32) {
    let source = &block["source"];
    let bytes = decode_base64(source["data"].as_str().unwrap()).unwrap();
    dimensions(source["media_type"].as_str().unwrap(), &bytes).unwrap()
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// claude-sonnet-4-6 reads images at the standard tier, which sends a 1440×900 screenshot at
/// 1389×868: Claude's point (694, 434) is the display's (719, 450).
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn sonnet_4_6_gets_computer_20251124_and_its_click_is_scaled_to_the_display() {
    let run = run_capsule(
        "claude-sonnet-4-6",
        true,
        script(
            "claude-sonnet-4-6",
            json!([{"type": "tool_use", "id": "toolu_c1", "name": "computer",
                    "input": {"action": "left_click", "coordinate": [694, 434]}}]),
        ),
    );
    assert_eq!(run.requests.len(), 2, "{}", run.output);

    let first = &run.requests[0];
    let entry = tools(first)
        .into_iter()
        .find(|tool| tool["type"] == "computer_20251124")
        .unwrap_or_else(|| {
            panic!(
                "request 1 offers no computer_20251124: {}",
                first.body["tools"]
            )
        });
    eprintln!("request 1 computer entry: {entry}");
    assert_eq!(entry["name"], "computer");
    assert_eq!(entry["display_width_px"], 1389);
    assert_eq!(entry["display_height_px"], 868);
    assert_eq!(entry["enable_zoom"], true);
    assert_eq!(
        first.header("anthropic-beta").as_deref(),
        Some("computer-use-2025-11-24")
    );
    assert!(!tools(first).iter().any(|tool| tool["name"] == TOOL));

    eprintln!("computer-calls.jsonl: {:?}", run.calls);
    assert_eq!(
        run.calls,
        vec![json!({"action": "left_click", "x": 719, "y": 450})]
    );

    let second = &run.requests[1];
    let call = last_assistant(second)
        .into_iter()
        .find(|block| block["type"] == "tool_use")
        .unwrap();
    eprintln!("request 2 tool_use: {call}");
    assert_eq!(
        call,
        json!({"type": "tool_use", "id": "toolu_c1", "name": "computer",
               "input": {"action": "left_click", "coordinate": [694, 434]}})
    );
    let result = last_message(second)
        .into_iter()
        .find(|block| block["type"] == "tool_result")
        .unwrap();
    assert!(result.get("toolset_name").is_none());
    let content = result["content"].as_array().unwrap();
    // murmur fences every tool result's text; the rewrite keeps the fence around the cursor.
    assert_eq!(
        content[0],
        json!({"type": "text",
               "text": "<untrusted-content source=tool:fake-computer>\ncursor: 694,434\n</untrusted-content>"})
    );
    assert_eq!(image_size(&content[1]), (1389, 868));
    eprintln!(
        "request 2 tool_result: text {:?}, image {:?}",
        content[0]["text"],
        image_size(&content[1])
    );
    assert_eq!(
        second.header("anthropic-beta").as_deref(),
        Some("computer-use-2025-11-24")
    );
}

/// claude-sonnet-5-5 takes the toolset: a two-action batch runs the tool twice, in order, and
/// both results echo `toolset_name`.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn sonnet_5_5_runs_a_toolset_batch_in_order() {
    let run = run_capsule(
        "claude-sonnet-5-5",
        true,
        script(
            "claude-sonnet-5-5",
            json!([
                {"type": "tool_use", "id": "toolu_b1", "name": "left_click",
                 "toolset_name": "computer", "input": {"coordinate": [100, 200]}},
                {"type": "tool_use", "id": "toolu_b2", "name": "type",
                 "toolset_name": "computer", "input": {"text": "hi"}}
            ]),
        ),
    );
    assert_eq!(run.requests.len(), 2, "{}", run.output);
    let first = &run.requests[0];
    assert!(tools(first)
        .iter()
        .any(|tool| tool == &json!({"type": "computer_toolset_20260801"})
            || (tool["type"] == "computer_toolset_20260801" && tool.get("name").is_none())));
    assert_eq!(first.header("anthropic-beta"), None);

    eprintln!("computer-calls.jsonl: {:?}", run.calls);
    assert_eq!(
        run.calls,
        vec![
            json!({"action": "left_click", "x": 100, "y": 200}),
            json!({"action": "type", "text": "hi"}),
        ]
    );

    let second = &run.requests[1];
    let calls: Vec<Value> = last_assistant(second)
        .into_iter()
        .filter(|block| block["type"] == "tool_use")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["name"], "left_click");
    assert_eq!(calls[0]["toolset_name"], "computer");
    assert_eq!(calls[0]["input"], json!({"coordinate": [100, 200]}));
    assert_eq!(calls[1]["name"], "type");
    let results: Vec<Value> = after_last_assistant(second)
        .into_iter()
        .filter(|block| block["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 2, "one tool_result per call");
    assert_eq!(results[0]["tool_use_id"], "toolu_b1");
    assert_eq!(results[1]["tool_use_id"], "toolu_b2");
    for result in &results {
        eprintln!(
            "request 2 tool_result {}: toolset_name {}",
            result["tool_use_id"], result["toolset_name"]
        );
        assert_eq!(result["toolset_name"], "computer");
    }
}

/// Without `provider_tool:` the same tool goes out as an ordinary function, with no beta.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn without_provider_tool_the_tool_is_an_ordinary_function() {
    let run = run_capsule(
        "claude-sonnet-4-6",
        false,
        script(
            "claude-sonnet-4-6",
            json!([{"type": "tool_use", "id": "toolu_f1", "name": TOOL,
                    "input": {"action": "left_click", "x": 5, "y": 6}}]),
        ),
    );
    let first = &run.requests[0];
    let entry = tools(first)
        .into_iter()
        .find(|tool| tool["name"] == TOOL)
        .unwrap_or_else(|| {
            panic!(
                "request 1 offers no {TOOL} function: {}",
                first.body["tools"]
            )
        });
    eprintln!("request 1 entry: {entry}");
    assert!(entry.get("input_schema").is_some());
    assert!(entry.get("type").is_none());
    assert_eq!(first.header("anthropic-beta"), None);
    assert_eq!(
        run.calls,
        vec![json!({"action": "left_click", "x": 5, "y": 6})]
    );
    let result = last_message(&run.requests[1])
        .into_iter()
        .find(|block| block["type"] == "tool_result")
        .unwrap();
    // Function mode: the tool's text as murmur fenced it, nothing rewritten.
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\nleft_click at 5,6\ncursor: 5,6\nscreenshot: "));
}
