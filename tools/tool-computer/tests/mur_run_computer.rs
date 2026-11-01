//! S12: `tool-computer` under a real `mur run` in a `sealed` capsule.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. `driver-openai` and `tool-computer` are built out of this
//! workspace and installed on every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that
//! finds neither fails rather than skipping. The `mur` must carry murmur `a9ea4726` (#381, the
//! `provider_tool:` block), or the `scoped` control is not refused.
//!
//! While the session runs, a monitor reads `/proc` for the tool's Xvfb and its cgroup, and prints
//! Xvfb's peak RSS and VmPeak and the scope's peak task count and memory.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=<mur with a9ea4726> cargo test -p tool-computer --test mur_run_computer -- --ignored
//! --nocapture` runs it.

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use serde_json::{json, Value};
use tempfile::TempDir;

const TOOL: &str = "tool-computer";
const LAUNCH: &str = r#"[xmessage, -print, -geometry, "+20+20", -buttons, "Press:0", hello]"#;
const HELD: &str = "tool-computer did not run left_click: this capsule's config lists left_click \
                    in confirm, so it needs a person's yes, and murmur gives a native tool no way \
                    to ask for one; ask the person to do this step, or to remove left_click from \
                    confirm";

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

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
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

fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

fn cargo_build(args: &[&str]) {
    let status = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .arg("build")
        .args(args)
        .status()
        .expect("spawning cargo build");
    assert!(status.success(), "cargo build {args:?} failed");
}

/// Builds the artifact at `crate_dir` and packs it with `scripts/package-artifact.sh`; returns its
/// version and zip.
fn pack(crate_dir: &str, out_dir: &Path) -> (String, PathBuf) {
    let crate_path = workspace_root().join(crate_dir);
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let name = manifest_field(&manifest, "name");
    let version = manifest_field(&manifest, "version");
    let native = manifest
        .lines()
        .any(|l| l.trim() == "implementation: native");
    let payload = if native {
        cargo_build(&["--release", "-p", &name]);
        target_dir().join("release").join(&name)
    } else {
        cargo_build(&["-p", &name, "--target", "wasm32-wasip2", "--release"]);
        target_dir()
            .join("wasm32-wasip2/release")
            .join(format!("{}.wasm", name.replace('-', "_")))
    };
    let output = Command::new("bash")
        .arg(workspace_root().join("scripts/package-artifact.sh"))
        .arg(&crate_path)
        .arg(out_dir)
        .arg("--payload")
        .arg(&payload)
        .env("MUR_BIN", mur_binary())
        .output()
        .expect("failed to spawn scripts/package-artifact.sh");
    assert!(
        output.status.success(),
        "packing {name} failed: {}",
        combined(&output)
    );
    let zip = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    assert!(zip.is_file(), "{} was not written", zip.display());
    (version, zip)
}

// ── staging ──────────────────────────────────────────────────────────────────

struct Staging {
    home: TempDir,
    project: TempDir,
    driver_version: String,
    tool_version: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn workdir(&self) -> PathBuf {
        self.project.path().join("work")
    }

    fn write_manifest(&self, endpoint: &str, containment: &str, confirm: bool) {
        let confirm = if confirm {
            "      confirm: [left_click]\n"
        } else {
            ""
        };
        fs::write(
            self.manifest(),
            format!(
                "name: computer-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {driver}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/{TOOL}\n    version: {tool}\n    runtime: tool\n    \
                 config:\n      launch:\n        - {LAUNCH}\n{confirm}\
                 capabilities:\n  containment: {containment}\n  shell:\n    allow: [Xvfb, xkbcomp, sh, xmessage]\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n\
                 trace:\n  capture: content\n",
                driver = self.driver_version,
                tool = self.tool_version,
            ),
        )
        .unwrap();
    }

    fn sessions(&self) -> Vec<PathBuf> {
        fs::read_dir(self.home.path().join(".murmur/sessions"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect()
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let (tool_version, tool_zip) = pack(&format!("tools/{TOOL}"), artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        tool_version,
        _artifacts: artifacts,
    };
    fs::create_dir_all(staging.workdir()).unwrap();
    staging.write_manifest("http://127.0.0.1:1/v1", "sealed", false);
    for (zip, what) in [(&driver_zip, "driver-openai"), (&tool_zip, TOOL)] {
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

fn run_session(staging: &Staging) -> Output {
    mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "press the button on the screen",
            "--workdir",
            staging.workdir().to_str().unwrap(),
        ])
        .output()
        .expect("spawning `mur run`")
}

// ── the fake provider ────────────────────────────────────────────────────────

fn scripted_calls() -> Vec<Value> {
    vec![
        json!({"action": "screenshot"}),
        json!({"action": "left_click", "x": 46, "y": 60}),
        json!({"action": "screenshot"}),
    ]
}

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn tool_call_stream(turn: usize, arguments: &Value) -> String {
    let call = json!({
        "index": 0, "id": format!("call_{turn}"), "type": "function",
        "function": {"name": TOOL, "arguments": arguments.to_string()}
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
                    Some(call) => tool_call_stream(turn + 1, call),
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

    fn last_request(&self) -> String {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the provider was sent at least one request")
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

// ── the monitor ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct Peaks {
    xvfb_pids: Vec<i32>,
    xvfb_vm_peak_kb: u64,
    xvfb_vm_hwm_kb: u64,
    cgroup: Option<String>,
    cgroup_pids_max_seen: u64,
    cgroup_pids_peak: Option<u64>,
    cgroup_memory_peak: Option<u64>,
}

fn status_kb(status: &str, key: &str) -> u64 {
    status
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn read_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Polls `/proc` every 50 ms for an Xvfb whose command line names `needle`, until `stop`.
fn monitor(needle: String, stop: Arc<AtomicBool>) -> thread::JoinHandle<Peaks> {
    thread::spawn(move || {
        let mut peaks = Peaks::default();
        while !stop.load(Ordering::Relaxed) {
            for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                let Ok(cmd) = fs::read(entry.path().join("cmdline")) else {
                    continue;
                };
                let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
                if !(cmd.contains("Xvfb") && cmd.contains("-displayfd") && cmd.contains(&needle)) {
                    continue;
                }
                if !peaks.xvfb_pids.contains(&pid) {
                    peaks.xvfb_pids.push(pid);
                }
                if let Ok(status) = fs::read_to_string(entry.path().join("status")) {
                    peaks.xvfb_vm_peak_kb =
                        peaks.xvfb_vm_peak_kb.max(status_kb(&status, "VmPeak:"));
                    peaks.xvfb_vm_hwm_kb = peaks.xvfb_vm_hwm_kb.max(status_kb(&status, "VmHWM:"));
                }
                if let Ok(cgroup) = fs::read_to_string(entry.path().join("cgroup")) {
                    if let Some(path) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) {
                        let dir = format!("/sys/fs/cgroup{path}");
                        peaks.cgroup = Some(dir.clone());
                        if let Some(n) = read_u64(&format!("{dir}/pids.current")) {
                            peaks.cgroup_pids_max_seen = peaks.cgroup_pids_max_seen.max(n);
                        }
                        if let Some(n) = read_u64(&format!("{dir}/pids.peak")) {
                            peaks.cgroup_pids_peak = Some(n);
                        }
                        if let Some(n) = read_u64(&format!("{dir}/memory.peak")) {
                            peaks.cgroup_memory_peak = Some(n);
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        peaks
    })
}

// ── trace ────────────────────────────────────────────────────────────────────

fn find_files(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_files(&path, name, out);
        } else if path.file_name().is_some_and(|n| n == name) {
            out.push(path);
        }
    }
}

/// Every `tool_call` record of the session's `trace.jsonl`.
fn trace_tool_calls(staging: &Staging) -> Vec<Value> {
    let mut traces = Vec::new();
    find_files(
        &staging.home.path().join(".murmur"),
        "trace.jsonl",
        &mut traces,
    );
    assert_eq!(traces.len(), 1, "one trace: {traces:?}");
    fs::read_to_string(&traces[0])
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["event_type"] == "tool_call")
        .collect()
}

fn explain_scope(staging: &Staging) -> String {
    let output = mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--explain-scope",
        ])
        .output()
        .expect("spawning mur run --explain-scope");
    combined(&output)
}

fn print_keeper_log(staging: &Staging) {
    let mut keeper_logs = Vec::new();
    find_files(
        &staging.workdir().join(".tool-computer"),
        "keeper.log",
        &mut keeper_logs,
    );
    for log in keeper_logs {
        println!(
            "--- {} ---\n{}",
            log.display(),
            fs::read_to_string(&log).unwrap_or_default()
        );
    }
}

// ── the scenarios ────────────────────────────────────────────────────────────

/// Screenshot, click the button, screenshot, in a sealed capsule.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn sealed_capsule_screenshots_clicks_and_records_both_images() {
    let staging = stage();
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint, "sealed", false);

    let scope = explain_scope(&staging);
    let tier: Vec<&str> = scope
        .lines()
        .filter(|l| l.to_ascii_lowercase().contains("achieved"))
        .collect();
    println!(
        "--- mur run --explain-scope (achieved) ---\n{}",
        tier.join("\n")
    );
    assert!(
        tier.iter().any(|l| l.contains("sealed")),
        "the achieved tier is sealed:\n{scope}"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let watcher = monitor(
        staging.workdir().to_string_lossy().into_owned(),
        Arc::clone(&stop),
    );
    let started = std::time::Instant::now();
    let output = run_session(&staging);
    let took = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let peaks = watcher.join().unwrap();
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");
    println!("session took {} ms", took.as_millis());
    println!("measured: {peaks:#?}");

    print_keeper_log(&staging);
    let last = provider.last_request();
    let body: Value = serde_json::from_str(&last).unwrap();
    let tools = body["tools"].as_array().expect("the request carries tools");
    let entry = tools
        .iter()
        .find(|t| t["function"]["name"] == TOOL)
        .unwrap_or_else(|| panic!("no {TOOL} entry in {tools:?}"));
    println!("tools entry: {entry}");

    let results = tool_results(&last);
    assert_eq!(results.len(), 3, "{results:#?}");
    for (id, content) in &results {
        println!("{id}: {content}");
        assert!(
            content.contains("<untrusted-content source=tool:tool-computer>"),
            "{id} is wrapped: {content}"
        );
    }
    assert!(results[0].1.contains("display: Xvfb 127.0.0.1:"));
    assert!(results[1].1.contains("cursor: 46,60"), "{}", results[1].1);

    let calls = trace_tool_calls(&staging);
    let shas: Vec<String> = calls
        .iter()
        .filter(|c| c["input"]["action"] == "screenshot")
        .filter_map(|c| c["attachments"][0]["sha256"].as_str().map(String::from))
        .collect();
    for call in &calls {
        println!("trace tool_call attachments: {}", call["attachments"]);
    }
    assert_eq!(
        shas.len(),
        2,
        "two screenshot calls carry attachments: {calls:#?}"
    );
    assert_ne!(shas[0], shas[1], "the screen changed between them");

    let mut logs = Vec::new();
    find_files(
        &staging.workdir().join(".tool-computer"),
        "launch-0-xmessage.log",
        &mut logs,
    );
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(
        log.contains("Press"),
        "xmessage printed its button: {log:?}"
    );
    assert!(
        staging.sessions().is_empty(),
        "the session directory is removed at the end"
    );
}

/// The same capsule at `scoped` is refused before a session exists.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn scoped_capsule_is_refused_before_a_session_exists() {
    let staging = stage();
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint, "scoped", false);
    let output = run_session(&staging);
    let text = combined(&output);
    println!("{text}");
    assert!(
        !output.status.success(),
        "a scoped capsule is refused:\n{text}"
    );
    assert!(
        text.contains(
            "error[E-CAP-003]: tool 'tool-computer' (provider_tool: computer) declares a provider \
             tool, which runs only under the 'sealed' containment floor"
        ),
        "{text}"
    );
    assert!(staging.sessions().is_empty(), "no session directory");
    assert!(
        !staging.workdir().join(".tool-computer").exists(),
        "no display started"
    );
    assert!(provider.requests.lock().unwrap().is_empty(), "no inference");
}

/// With `confirm: [left_click]` the click's tool message is the held line.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn confirm_listed_click_is_held_in_a_sealed_capsule() {
    let staging = stage();
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint, "sealed", true);
    let output = run_session(&staging);
    let text = combined(&output);
    assert!(output.status.success(), "the session must succeed:\n{text}");
    let results = tool_results(&provider.last_request());
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    let click = &results
        .iter()
        .find(|(id, _)| id == "call_2")
        .expect("a result for the click")
        .1;
    assert!(click.contains(HELD), "{click}");
    let mut logs = Vec::new();
    find_files(
        &staging.workdir().join(".tool-computer"),
        "launch-0-xmessage.log",
        &mut logs,
    );
    assert_eq!(
        fs::read_to_string(&logs[0]).unwrap(),
        "",
        "xmessage was never clicked"
    );
}
