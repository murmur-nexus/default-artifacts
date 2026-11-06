//! A computer tool, proved to reach GPT as OpenAI's own `computer` tool, with each
//! `computer_call` run as an ordered group of tool calls and answered by one
//! `computer_call_output`, under a real `mur run`.
//!
//! The capsule runs driver-openai on `gpt-5.6-luna` with `fake-computer`, the
//! `murmur-fake-computer` test component (`libs/murmur-fake-computer`) built for wasm32-wasip2
//! and packed with `provider_tool: {kind: computer, display_width: 1440, display_height: 900}`.
//! It logs each call it gets to `computer-calls.jsonl` in the work folder and attaches the
//! 1440×900 `shot.png` the test writes there. The capsule runs `capabilities.containment: sealed`, which a provider tool
//! requires. A loopback fake OpenAI Responses API records each request and answers with scripted
//! `computer_call` items, then a final text.
//!
//! The safety-check scenarios run the capsule twice: under `mur run --task` with a 2 s
//! `lifecycle.input_timeout_secs`, where nobody answers murmur's question, and served (`lifecycle: {task_acceptance: queue, after_task: sleep}`), where the test
//! answers murmur's question over the door with JSON-RPC `SendMessage` and reads the task with
//! `GetTask`, as murmur's own `crates/murmur-cli/tests/provider_tool.rs` does.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch:
//! `MUR_BIN=… cargo test -p driver-openai --test mur_run_computer_tool -- --ignored`. `mur` must
//! be murmur `a9ea4726` or later, which reads `provider_tool:` and asks safety checks.
//!
//! Unix only: it spawns a host `mur` and reads Unix permission bits.
#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, Once},
    thread,
    time::{Duration, Instant},
};

use murmur_driver_image::fixtures::{png, screenshot_like};
use serde_json::{json, Value};
use tempfile::TempDir;

const MODEL: &str = "gpt-5.6-luna";
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

/// Build `crate_name` for wasm32-wasip2, once per test binary per `once`.
fn build_component(once: &'static Once, crate_name: &str) {
    once.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                crate_name,
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .unwrap_or_else(|err| panic!("failed to spawn `cargo build` for {crate_name}: {err}"));
        assert!(status.success(), "cargo build of {crate_name} failed");
    });
}

fn build_driver() {
    static BUILD: Once = Once::new();
    build_component(&BUILD, "driver-openai");
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
    let crate_path = workspace_root().join("drivers/driver-openai");
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let version = manifest_field(&manifest, "version");
    let wasm = target_dir().join("wasm32-wasip2/release/driver_openai.wasm");
    (version, package(&crate_path, out_dir, &wasm))
}

const FAKE_COMPUTER_MANIFEST: &str = "name: fake-computer\nversion: 0.1.0\nruntime: tool\n\
implementation: wasm\ndescription: Stands in for tool-computer on a 1440x900 display.\n\
requires_files:\n  - murmur_fake_computer.wasm\n\
input_schema: '{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\"},\
\"x\":{\"type\":\"integer\"},\"y\":{\"type\":\"integer\"},\"text\":{\"type\":\"string\"}},\
\"required\":[\"action\"]}'\n";

const PROVIDER_TOOL_BLOCK: &str =
    "provider_tool:\n  kind: computer\n  display_width: 1440\n  display_height: 900\n";

/// Build the `murmur-fake-computer` component and pack it as the `fake-computer` tool artifact,
/// declaring the computer `provider_tool:` block when `provider_tool`.
fn pack_fake_computer(scratch: &Path, out_dir: &Path, provider_tool: bool) -> PathBuf {
    static BUILD: Once = Once::new();
    build_component(&BUILD, "murmur-fake-computer");
    let wasm = target_dir().join("wasm32-wasip2/release/murmur_fake_computer.wasm");
    let dir = scratch.join("fake-computer");
    fs::create_dir_all(&dir).unwrap();
    let mut manifest = FAKE_COMPUTER_MANIFEST.to_string();
    if provider_tool {
        manifest.push_str(PROVIDER_TOOL_BLOCK);
    }
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
    package(&dir, out_dir, &wasm)
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

fn completed(output: Value) -> Value {
    json!({"id": "resp_1", "object": "response", "status": "completed", "model": MODEL,
           "output": output, "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}})
}

fn final_text() -> Value {
    completed(
        json!([{"type": "message", "id": "msg_9", "role": "assistant", "status": "completed",
                      "content": [{"type": "output_text", "text": "Done."}]}]),
    )
}

fn computer_call(call_id: &str, actions: Value, checks: Value) -> Value {
    completed(
        json!([{"type": "computer_call", "id": format!("cu_{call_id}"), "call_id": call_id,
                      "actions": actions, "pending_safety_checks": checks, "status": "completed"}]),
    )
}

fn click_and_type() -> Value {
    json!([{"type": "click", "button": "left", "x": 100, "y": 200}, {"type": "type", "text": "hi"}])
}

fn checks() -> Value {
    json!([{"id": "sc_1", "code": "malicious_instructions", "message": "The page asks for a card number."}])
}

/// The first answer is `first`; every later one ends the turn.
fn script(first: Value) -> Answer {
    Box::new(move |index, _| {
        if index == 0 {
            first.clone()
        } else {
            final_text()
        }
    })
}

// ── the capsule ──────────────────────────────────────────────────────────────

struct Capsule {
    home: TempDir,
    project: TempDir,
    _scratch: TempDir,
    _artifacts: TempDir,
    provider: Provider,
    manifest: PathBuf,
    workdir: PathBuf,
}

/// Install the driver and fake-computer into a sealed capsule against a fake OpenAI answering
/// with `answer`; `extra` is appended to the manifest.
fn capsule(provider_tool: bool, answer: Answer, extra: &str) -> Capsule {
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
             \x20 - name: local/driver-openai\n    version: {version}\n    runtime: driver\n    \
             gateway:\n      endpoint: {endpoint}/v1\n      api_key: test-key\n\
             \x20 - name: local/{TOOL}\n    version: 0.1.0\n    runtime: tool\n\
             capabilities:\n  containment: sealed\n\
             inference:\n  transport: http\n  model: {MODEL}\n  max_tokens: 4096\n  \
             driver:\n    artifact: local/driver-openai\n\
             trace:\n  capture: content\n{extra}"
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
    Capsule {
        home,
        project,
        _scratch: scratch,
        _artifacts: artifacts,
        provider,
        manifest,
        workdir,
    }
}

fn check_launch(output: &str) {
    assert!(
        !output.contains("E-RUN-007") && !output.contains("trapped"),
        "the guest must not trap:\n{output}"
    );
    if output.contains("E-CAP-") {
        panic!("sealed containment could not be reached on this host:\n{output}");
    }
}

impl Capsule {
    /// `mur run --task` once; returns its output after asserting the run succeeded.
    fn run_task(&self) -> String {
        let (text, success) = self.run_task_to_end();
        assert!(success, "the run must succeed:\n{text}");
        self.assert_sealed();
        text
    }

    /// `mur run --task` once; returns its output and whether it exited 0.
    fn run_task_to_end(&self) -> (String, bool) {
        let task = self.project.path().join("task.md");
        fs::write(&task, "Click the search box and type hi.\n").unwrap();
        let output = mur(&self.home)
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                task.to_str().unwrap(),
            ])
            .arg("--workdir")
            .arg(&self.workdir)
            .stdin(Stdio::null())
            .output()
            .expect("spawning `mur run`");
        let text = format!(
            "{}{}\nexit: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            output.status
        );
        check_launch(&text);
        eprintln!("{text}");
        (text, output.status.success())
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.workdir.join("computer-calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Assert the trace records the capsule running `sealed`.
    fn assert_sealed(&self) {
        let achieved: Vec<Value> = trace_events(self.home.path())
            .into_iter()
            .filter_map(|event| event.get("containment_achieved").cloned())
            .collect();
        eprintln!("containment_achieved in the trace: {achieved:?}");
        assert!(
            !achieved.is_empty() && achieved.iter().all(|value| value == "sealed"),
            "the capsule must run sealed, and the trace says {achieved:?}"
        );
    }

    /// Every `safety_check` trace event under the scratch HOME.
    fn safety_events(&self) -> Vec<Value> {
        trace_events(self.home.path())
            .into_iter()
            .filter(|event| event["event_type"] == "safety_check")
            .collect()
    }
}

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

fn input_items(request: &Recorded, kind: &str) -> Vec<Value> {
    request.body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == kind)
        .cloned()
        .collect()
}

fn tools(request: &Recorded) -> Vec<Value> {
    request.body["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

// ── scenarios ────────────────────────────────────────────────────────────────

#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_computer_call_runs_as_an_ordered_group_answered_by_one_output() {
    let capsule = capsule(
        true,
        script(computer_call("call_1", click_and_type(), json!([]))),
        "",
    );
    capsule.run_task();
    let requests = capsule.provider.requests();
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert!(
        tools(first).contains(&json!({"type": "computer"})),
        "request 1 offers no computer tool: {}",
        first.body["tools"]
    );
    assert!(!tools(first).iter().any(|tool| tool["name"] == TOOL));
    assert_eq!(
        first.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        first.header("openai-beta"),
        None,
        "the computer tool needs no beta header"
    );
    eprintln!("request 1 tools: {}", first.body["tools"]);

    eprintln!("computer-calls.jsonl: {:?}", capsule.calls());
    assert_eq!(
        capsule.calls(),
        vec![
            json!({"action": "left_click", "x": 100, "y": 200}),
            json!({"action": "type", "text": "hi"}),
        ]
    );

    let second = &requests[1];
    let outputs = input_items(second, "computer_call_output");
    assert_eq!(
        outputs.len(),
        1,
        "one output for the call: {}",
        second.body["input"]
    );
    assert_eq!(outputs[0]["call_id"], "call_1");
    assert_eq!(outputs[0]["output"]["type"], "computer_screenshot");
    assert!(outputs[0]["output"]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    let call = &input_items(second, "computer_call")[0];
    eprintln!("request 2 computer_call: {call}");
    assert_eq!(call["id"], "cu_call_1");
    assert_eq!(call["actions"], click_and_type());
    assert!(input_items(second, "function_call_output").is_empty());
}

/// Under `mur run --task` the task is in scope and its door is open, so murmur asks and waits;
/// with `lifecycle.input_timeout_secs: 2` and nobody answering, the wait times out and the call is
/// not run. Nothing about the check ever goes back to OpenAI.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_safety_check_nobody_answers_does_not_run_the_call() {
    let capsule = capsule(
        true,
        script(computer_call("call_s", click_and_type(), checks())),
        "lifecycle:\n  input_timeout_secs: 2\n",
    );
    let (output, success) = capsule.run_task_to_end();
    eprintln!("mur run --task exited 0: {success}");
    capsule.assert_sealed();
    assert!(
        capsule.calls().is_empty(),
        "nothing ran: {:?}",
        capsule.calls()
    );
    let events = capsule.safety_events();
    eprintln!("safety_check events: {events:?}");
    assert!(!events.is_empty(), "murmur asked about the call:\n{output}");
    for event in &events {
        assert_eq!(event["outcome"], "timed_out");
    }
    for request in capsule.provider.requests() {
        assert!(!request
            .body
            .to_string()
            .contains("acknowledged_safety_checks"));
    }
}

/// The door of a served capsule: its URL from `mur run`'s output, and the child to stop.
struct Served {
    child: std::process::Child,
    url: String,
    session: String,
}

impl Served {
    fn launch(capsule: &Capsule) -> Self {
        let mut child = mur(&capsule.home)
            .current_dir(capsule.project.path())
            .args(["run", "--manifest", capsule.manifest.to_str().unwrap()])
            .arg("--workdir")
            .arg(&capsule.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning `mur run`");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx_err = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                eprintln!("[mur stdout] {line}");
                let _ = tx.send(line);
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[mur stderr] {line}");
                let _ = tx_err.send(line);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        let (mut url, mut session) = (None, None);
        let mut seen = String::new();
        while url.is_none() || session.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("the served capsule printed no door URL:\n{seen}"));
            seen.push_str(&line);
            seen.push('\n');
            check_launch(&seen);
            if let Some(found) = line.strip_prefix("murmur: url ") {
                url = Some(found.trim().to_string());
            }
            if let Some(found) = line.strip_prefix("session: ") {
                session = Some(found.trim().to_string());
            }
        }
        Self {
            child,
            url: url.unwrap(),
            session: session.unwrap(),
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        http_post_json(&self.url, &body.to_string())
    }

    fn send(&self, context: &str, task_id: Option<&str>, text: &str) -> Value {
        let mut message = json!({"messageId": format!("msg-{context}-{text}"), "contextId": context,
                                 "role": "ROLE_USER", "parts": [{"text": text}]});
        if let Some(task_id) = task_id {
            message["taskId"] = json!(task_id);
        }
        self.rpc(
            "SendMessage",
            json!({"message": message, "configuration": {"returnImmediately": true}}),
        )
    }

    fn start(&self, context: &str) -> String {
        let answer = self.send(context, None, "Click the search box and type hi.");
        answer["result"]["task"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("a task id: {answer}"))
            .to_string()
    }

    fn poll(&self, task_id: &str, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let task = self.rpc("GetTask", json!({"id": task_id}));
            if task["result"]["status"]["state"] == state {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "no {state} for {task_id}: {task}"
            );
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn stop(mut self, capsule: &Capsule) {
        let output = mur(&capsule.home)
            .current_dir(capsule.project.path())
            .args(["stop", &self.session])
            .output();
        eprintln!("mur stop {}: {:?}", self.session, output.map(|o| o.status));
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http_post_json(url: &str, body: &str) -> Value {
    let addr = url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let mut stream = TcpStream::connect(&addr).expect("connect to the door");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nA2A-Version: \
         1.0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    serde_json::from_str(body).unwrap_or_else(|_| json!({"_raw": body}))
}

/// Served: a person says no, and the call never runs; then yes, and it runs. Nothing about the
/// checks goes back to OpenAI either way: its `computer` tool refuses both
/// `pending_safety_checks` and `acknowledged_safety_checks` in its input.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn served_a_person_declines_then_acknowledges_a_safety_check() {
    // Requests 0 and 2 start a task: each is answered with a flagged computer call. Requests 1
    // and 3 carry the call's output: each is answered with a final text.
    let answer: Answer = Box::new(|index, _| match index {
        0 => computer_call(
            "call_no",
            json!([{"type": "click", "button": "left", "x": 10, "y": 20}]),
            checks(),
        ),
        2 => computer_call(
            "call_yes",
            json!([{"type": "click", "button": "left", "x": 30, "y": 40}]),
            checks(),
        ),
        _ => final_text(),
    });
    let capsule = capsule(
        true,
        answer,
        "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n",
    );
    let served = Served::launch(&capsule);
    eprintln!("door: {} session: {}", served.url, served.session);

    let declined = served.start("ctx-no");
    let waiting = served.poll(&declined, "TASK_STATE_INPUT_REQUIRED");
    eprintln!("GetTask while waiting: {waiting}");
    assert!(capsule.calls().is_empty(), "nothing ran yet");
    served.send("ctx-no", Some(&declined), "no");
    served.poll(&declined, "TASK_STATE_COMPLETED");
    assert!(capsule.calls().is_empty(), "a declined call never runs");
    let requests = capsule.provider.requests();
    assert_eq!(requests.len(), 2);
    let after_no = &requests[1];
    assert!(!after_no
        .body
        .to_string()
        .contains("acknowledged_safety_checks"));
    assert!(after_no
        .body
        .to_string()
        .contains("did not acknowledge the safety check"));
    eprintln!(
        "request after no: {}",
        after_no.body["input"].as_array().unwrap().last().unwrap()
    );

    let acknowledged = served.start("ctx-yes");
    served.poll(&acknowledged, "TASK_STATE_INPUT_REQUIRED");
    served.send("ctx-yes", Some(&acknowledged), "yes");
    served.poll(&acknowledged, "TASK_STATE_COMPLETED");
    eprintln!("computer-calls.jsonl: {:?}", capsule.calls());
    assert_eq!(
        capsule.calls(),
        vec![json!({"action": "left_click", "x": 30, "y": 40})]
    );
    let requests = capsule.provider.requests();
    assert_eq!(requests.len(), 4);
    let after_yes = &requests[3];
    let outputs = input_items(after_yes, "computer_call_output");
    let output = outputs
        .iter()
        .find(|output| output["call_id"] == "call_yes")
        .unwrap_or_else(|| panic!("no output for call_yes: {}", after_yes.body["input"]));
    assert!(output.get("acknowledged_safety_checks").is_none());
    assert!(output["output"]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    eprintln!(
        "request after yes, output keys: {:?}",
        output.as_object().unwrap().keys().collect::<Vec<_>>()
    );

    let outcomes: Vec<Value> = capsule
        .safety_events()
        .iter()
        .map(|event| event["outcome"].clone())
        .collect();
    eprintln!("safety_check outcomes: {outcomes:?}");
    assert_eq!(outcomes, vec![json!("declined"), json!("acknowledged")]);
    capsule.assert_sealed();
    served.stop(&capsule);
}

/// Without `provider_tool:` the same tool goes out as an ordinary function.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn without_provider_tool_the_tool_is_an_ordinary_function() {
    let first = completed(
        json!([{"type": "function_call", "id": "fc_1", "call_id": "call_f",
        "name": TOOL, "arguments": "{\"action\":\"left_click\",\"x\":5,\"y\":6}", "status": "completed"}]),
    );
    let capsule = capsule(false, script(first), "");
    capsule.run_task();
    let requests = capsule.provider.requests();
    let entry = tools(&requests[0])
        .into_iter()
        .find(|tool| tool["name"] == TOOL)
        .unwrap_or_else(|| panic!("no {TOOL} function: {}", requests[0].body["tools"]));
    eprintln!("request 1 entry: {entry}");
    assert_eq!(entry["type"], "function");
    assert_eq!(entry["strict"], false);
    assert!(!tools(&requests[0]).contains(&json!({"type": "computer"})));
    assert_eq!(
        capsule.calls(),
        vec![json!({"action": "left_click", "x": 5, "y": 6})]
    );
    assert_eq!(input_items(&requests[1], "function_call_output").len(), 1);
}
