//! S14–S16: `tool-computer` under a real `mur run`: Xvfb, the window manager and the launch
//! programs started as session processes inside the capsule's containment, the X protocol over
//! `murmur:process` `connect`, both screenshots reaching the model, and nothing left running after
//! the session.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls (screenshot, left_click at the xmessage button, screenshot) and records every request
//! body. `driver-openai` and `tool-computer` (both wasm32-wasip2) are built out of this workspace
//! and packed by `scripts/package-artifact.sh` on every run. `mur` comes from `MUR_BIN` and must
//! report WIT level 4 or higher, the first with `murmur:process`; a run without one fails rather
//! than skipping.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=<level-4 mur> cargo test -p tool-computer --test mur_run_computer -- --ignored
//! --nocapture` runs it.

#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const TOOL: &str = "tool-computer";
const LAUNCH: &str = r#"[xmessage, -print, -geometry, "+20+20", -buttons, "Press:0", hello]"#;
const HELD: &str = "tool-computer did not run left_click: this capsule's config lists left_click \
                    in confirm, so it needs a person's yes, and a tool cannot ask a person for \
                    one; ask the person to do this step, or to remove left_click from confirm";
const FENCE_OPEN: &str = "<untrusted-content source=tool:tool-computer>";

/// The leftover checks scan every process on the host for `.tool-computer/` and `Press:0`, so a
/// test that ends while another test's display still runs would count that display as its own.
/// Each test holds this lock for its whole run.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── the runtime under test ───────────────────────────────────────────────────

fn mur_binary() -> PathBuf {
    let path = PathBuf::from(std::env::var_os("MUR_BIN").unwrap_or_else(|| {
        panic!(
            "set MUR_BIN to a mur whose `mur version` reports WIT level 4 or higher. This test \
             launches the runtime as a subprocess, so a missing binary is a failure rather than a \
             skip."
        )
    }));
    assert!(
        is_executable(&path),
        "MUR_BIN names {}, which is not executable",
        path.display()
    );
    let version = Command::new(&path).arg("version").output().unwrap();
    let text = String::from_utf8_lossy(&version.stdout).into_owned();
    let level: u32 = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("WIT level "))
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        level >= 4,
        "MUR_BIN must report WIT level 4 or higher (murmur:process); it reports:\n{text}"
    );
    path
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
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

/// Builds the wasm artifact `name` and packs the artifact directory `dir` around it with
/// `scripts/package-artifact.sh`; returns its version and zip.
fn pack(dir: &Path, name: &str, out_dir: &Path) -> (String, PathBuf) {
    let manifest = fs::read_to_string(dir.join("murmur.yaml")).unwrap();
    let version = manifest_field(&manifest, "version");
    let built = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .args([
            "build",
            "-p",
            name,
            "--release",
            "--target",
            "wasm32-wasip2",
        ])
        .status()
        .unwrap();
    assert!(built.success(), "cargo build of {name} failed");
    let payload = target_dir()
        .join("wasm32-wasip2/release")
        .join(format!("{}.wasm", name.replace('-', "_")));
    let output = Command::new("bash")
        .current_dir(workspace_root())
        .arg(workspace_root().join("scripts/package-artifact.sh"))
        .arg(dir)
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
    let zip = PathBuf::from(
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .lines()
            .last()
            .unwrap_or(""),
    );
    assert!(zip.is_file(), "packing {name} wrote no zip");
    (version, zip)
}

/// A test-only copy of the artifact directory whose `murmur.yaml` has no `provider_tool:` block,
/// so murmur's sealed floor for provider tools does not apply. Never shipped.
fn copy_without_provider_tool(into: &Path) -> PathBuf {
    let dir = into.join("tool-computer");
    fs::create_dir_all(&dir).unwrap();
    let manifest =
        fs::read_to_string(workspace_root().join("tools/tool-computer/murmur.yaml")).unwrap();
    let mut out = String::new();
    let mut skipping = false;
    for line in manifest.lines() {
        if line.starts_with("provider_tool:") {
            skipping = true;
            continue;
        }
        if skipping && (line.starts_with(' ') || line.is_empty()) {
            continue;
        }
        skipping = false;
        out.push_str(line);
        out.push('\n');
    }
    assert!(!out.contains("provider_tool") && !out.contains("display_width"));
    fs::write(dir.join("murmur.yaml"), out).unwrap();
    dir
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
    fn mur(&self) -> Command {
        let mut command = Command::new(mur_binary());
        command.env("HOME", self.home.path());
        command
    }

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
        // On `scoped`, Landlock lets Xvfb read only its own binary and libraries; its keymap
        // compile (sh, then xkbcomp, which inherit Xvfb's domain) reads the XKB data.
        let runtime = if containment == "scoped" {
            "    interpreter_runtime:\n      - binary: Xvfb\n        dirs:\n          - path: /usr/share/X11/xkb\n            list_dir: true\n"
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
                 capabilities:\n  containment: {containment}\n  shell:\n    allow: [Xvfb, xkbcomp, sh, xmessage]\n{runtime}\
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

    fn run(&self) -> Output {
        self.mur()
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest().to_str().unwrap(),
                "--task",
                "press the button on the screen",
                "--workdir",
                self.workdir().to_str().unwrap(),
            ])
            .output()
            .expect("spawning `mur run`")
    }

    /// Every trace line this staging's sessions wrote.
    fn trace(&self) -> Vec<Value> {
        let mut traces = Vec::new();
        find_files(
            &self.home.path().join(".murmur"),
            "trace.jsonl",
            &mut traces,
        );
        traces
            .iter()
            .flat_map(|t| {
                fs::read_to_string(t)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Packs `driver-openai` and the tool (from `tool_dir`, the shipped directory by default),
/// publishes and installs both into a fresh `HOME` and project.
fn stage_from(tool_dir: Option<&Path>) -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack(
        &workspace_root().join("drivers/driver-openai"),
        "driver-openai",
        artifacts.path(),
    );
    let shipped = workspace_root().join("tools/tool-computer");
    let (tool_version, tool_zip) = pack(tool_dir.unwrap_or(&shipped), TOOL, artifacts.path());
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
        let mut publish = staging.mur();
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {what}"));
        let mut install = staging.mur();
        install
            .current_dir(staging.project.path())
            .arg("install")
            .arg(zip);
        run_to_success(install, &format!("installing {what}"));
    }
    staging
}

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

fn xmessage_log(staging: &Staging) -> String {
    let mut logs = Vec::new();
    find_files(
        &staging.workdir().join(".tool-computer"),
        "launch-0-xmessage.log",
        &mut logs,
    );
    assert_eq!(logs.len(), 1, "one xmessage log: {logs:?}");
    fs::read_to_string(&logs[0]).unwrap()
}

fn print_logs(staging: &Staging) {
    let mut logs = Vec::new();
    find_files(
        &staging.workdir().join(".tool-computer"),
        "display.log",
        &mut logs,
    );
    find_files(
        &staging.workdir().join(".tool-computer"),
        "state.json",
        &mut logs,
    );
    for log in logs {
        println!(
            "--- {} ---\n{}",
            log.display(),
            fs::read_to_string(&log).unwrap_or_default()
        );
    }
}

/// Live Xvfb or xmessage processes started by this run: their command lines name the run's
/// state folder (Xvfb's `-auth`) or its xmessage text.
fn leftovers() -> Vec<u64> {
    let mut left = murmur_process_test_host::pids_mentioning(".tool-computer/");
    left.retain(|pid| {
        fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| !String::from_utf8_lossy(&c).contains("cargo"))
    });
    left
}

fn xmessages_running() -> Vec<u64> {
    murmur_process_test_host::pids_mentioning("Press:0")
        .into_iter()
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).starts_with("xmessage"))
        })
        .collect()
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

// ── the scenarios ────────────────────────────────────────────────────────────

/// Screenshot, click the button, screenshot on `tier`, with every S14 assertion.
fn screenshot_click_screenshot(staging: &Staging, tier: &str) {
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint, tier, false);
    let xmessages_before = xmessages_running();

    let started = Instant::now();
    let output = staging.run();
    let took = started.elapsed();
    let text = combined(&output);
    println!("--- {tier}: mur run ---\n{text}");
    print_logs(staging);
    let requests = provider.requests.lock().unwrap().clone();
    let last = provider.last_request();
    let results = tool_results(&last);
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    assert!(
        output.status.success(),
        "the session must succeed on {tier}:\n{text}"
    );
    println!("{tier}: session took {} ms", took.as_millis());

    assert_eq!(results.len(), 3, "{results:#?}");
    for (id, content) in &results {
        assert!(content.contains(FENCE_OPEN), "{id} is fenced: {content}");
    }
    assert!(
        results[0].1.contains("display: Xvfb 127.0.0.1:")
            && results[0].1.contains("; launched xmessage; "),
        "{}",
        results[0].1
    );
    assert!(
        results[1].1.contains("left_click at 46,60"),
        "{}",
        results[1].1
    );
    assert!(results[1].1.contains("cursor: 46,60"), "{}", results[1].1);
    assert!(!results[2].1.contains("display: Xvfb"), "one banner");

    // Both screenshots reach the model: the request after each carries its image.
    let images = |body: &str| body.matches("data:image/png;base64,").count();
    assert_eq!(requests.len(), 4, "four inference requests");
    assert!(
        images(&requests[1]) >= 1,
        "the request after the first screenshot carries it"
    );
    assert!(
        images(&requests[3]) >= 3,
        "the last request carries all three screenshots ({})",
        images(&requests[3])
    );

    let trace = staging.trace();
    let calls: Vec<&Value> = trace
        .iter()
        .filter(|l| l["event_type"] == "tool_call")
        .collect();
    let shas: Vec<String> = calls
        .iter()
        .filter_map(|c| c["attachments"][0]["sha256"].as_str().map(String::from))
        .collect();
    assert_eq!(shas.len(), 3, "every call carries a screenshot: {calls:#?}");
    assert_ne!(
        shas[0], shas[2],
        "the screen changed between the screenshots"
    );
    for call in &calls {
        println!(
            "{tier}: tool_call {} duration_ms={} status={}",
            call["input"]["action"], call["duration_ms"], call["status"]
        );
    }

    for name in ["display", "launch-0"] {
        let starts: Vec<&Value> = trace
            .iter()
            .filter(|l| l["event_type"] == "process_start" && l["name"] == name)
            .collect();
        assert_eq!(
            starts.len(),
            1,
            "one process_start for {name} on {tier}: {starts:?}"
        );
        println!("{tier}: process_start: {}", starts[0]);
    }
    let exits: Vec<&Value> = trace
        .iter()
        .filter(|l| l["event_type"] == "process_exit")
        .collect();
    for exit in &exits {
        println!("{tier}: process_exit: {exit}");
    }
    assert!(
        exits
            .iter()
            .any(|l| l["name"] == "display" && l["reason"] == "session_end"),
        "process_exit for display with reason session_end on {tier}: {exits:?}"
    );
    let refused: Vec<&Value> = trace
        .iter()
        .filter(|l| l["event_type"] == "process_refused")
        .collect();
    assert!(
        refused.is_empty(),
        "no process_refused on {tier}: {refused:?}"
    );

    let log = xmessage_log(staging);
    assert!(
        log.contains("Press"),
        "xmessage printed its button on {tier}: {log:?}"
    );

    let left = leftovers();
    assert!(
        left.is_empty(),
        "nothing names .tool-computer/ after mur run on {tier}: {left:?}"
    );
    let xmessages: Vec<u64> = xmessages_running()
        .into_iter()
        .filter(|p| !xmessages_before.contains(p))
        .collect();
    assert!(
        xmessages.is_empty(),
        "no xmessage left on {tier}: {xmessages:?}"
    );
    assert!(
        staging.sessions().is_empty(),
        "the session directory is removed at the end"
    );
}

/// S14: the shipped artifact on `sealed`.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn s14_sealed_screenshots_clicks_and_returns_both_images() {
    let _serial = one_at_a_time();
    let staging = stage_from(None);
    screenshot_click_screenshot(&staging, "sealed");
}

/// S15: the shipped artifact on `scoped` is refused before a session, by murmur's provider-tool
/// floor; a test-only copy without `provider_tool:` runs the S14 flow on `scoped`.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn s15_scoped_refuses_the_provider_tool_and_runs_a_copy_without_it() {
    let _serial = one_at_a_time();
    let staging = stage_from(None);
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint, "scoped", false);
    let output = staging.run();
    let text = combined(&output);
    println!("--- scoped, shipped manifest ---\n{text}");
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

    let scratch = tempfile::tempdir().unwrap();
    let copy = copy_without_provider_tool(scratch.path());
    let staging = stage_from(Some(&copy));
    screenshot_click_screenshot(&staging, "scoped");
}

/// S16: with `confirm: [left_click]` the click's tool message is the held line.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn s16_a_confirm_listed_click_is_held_on_sealed() {
    let _serial = one_at_a_time();
    let staging = stage_from(None);
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint, "sealed", true);
    let output = staging.run();
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
    // A click makes xmessage print its button's name. The log also holds xmessage's stderr, where
    // a host may warn about fonts or locale, so it is not expected to be empty.
    let log = xmessage_log(&staging);
    assert!(
        !log.lines().any(|line| line == "Press"),
        "xmessage was never clicked: {log:?}"
    );
    assert!(leftovers().is_empty(), "nothing left running");
}
