//! The Chat Completions tool call limit, proved in the component under a real `mur run`.
//!
//! The unit tests in `src/lib.rs` drive `process_openai_sse_line` and the native body parser,
//! neither of which owns a WASI resource. Only a launch shows that a stream naming an index
//! past `murmur_driver_stream::MAX_SLOTS_PER_TURN` ends the turn with the driver error rather
//! than trapping the guest while it releases the response body, and that the last index under
//! the limit still completes a tool round trip.
//!
//! Each test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake provider. The driver is built out of this workspace on
//! every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that finds neither fails rather
//! than skipping.
//!
//! They are `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=… cargo test -p driver-openai --test mur_run_tool_call_limit -- --ignored` runs
//! them.

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

const DRIVER_CRATE: &str = "drivers/driver-openai";

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
                "driver-openai",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for driver-openai");
        assert!(status.success(), "cargo build of driver-openai failed");
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

    /// The capsule manifest. `gpt-4.1` is not a gpt-5 model, so the driver takes the Chat
    /// Completions surface.
    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: tool-call-limit-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 inference:\n  transport: http\n  model: gpt-4.1\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.version
            ),
        )
        .unwrap();
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
    run_to_success(publish, "publishing driver-openai");

    let mut install = mur(&staging.home);
    install
        .current_dir(staging.project.path())
        .arg("install")
        .arg(&zip);
    run_to_success(install, "installing driver-openai");
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

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

/// One assistant turn holding a single tool call at `index`.
fn tool_call_stream(index: u64) -> String {
    let call = json!({
        "index": index, "id": "call_x", "type": "function",
        "function": {"name": "probe", "arguments": "{\"cmd\":\"ls\"}"}
    });
    chat_chunk(json!({"role": "assistant", "content": ""}), None)
        + &chat_chunk(json!({"tool_calls": [call]}), None)
        + &chat_chunk(json!({}), Some("tool_calls"))
        + "data: [DONE]\n\n"
}

fn text_stream() -> String {
    chat_chunk(json!({"content": "done after the tool"}), None)
        + &chat_chunk(json!({}), Some("stop"))
        + "data: [DONE]\n\n"
}

/// A loopback Chat Completions server: it answers a request carrying a tool result with text,
/// and any other request with one tool call at `index`. It counts the requests it was sent.
struct Provider {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Provider {
    fn start(index: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(body) = read_request_body(&mut stream) else {
                    continue;
                };
                let follow_up = body.replace(' ', "").contains("\"role\":\"tool\"");
                recorded.lock().unwrap().push(body);
                let payload = if follow_up {
                    text_stream()
                } else {
                    tool_call_stream(index)
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

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
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

// ── scenarios ────────────────────────────────────────────────────────────────

/// A tool call at index 2^32 ends the turn with the driver's error; the guest does not trap.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_tool_call_index_past_the_limit_fails_the_turn_in_the_component() {
    let staging = stage();
    let provider = Provider::start(4_294_967_296);
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);

    assert!(!output.status.success(), "the session must fail:\n{text}");
    assert!(
        !text.contains("trapped"),
        "the guest must not trap:\n{text}"
    );
    assert!(text.contains("status:  failed"), "{text}");
    assert!(
        text.contains(
            r#"{"error":"driver: tool call index 4294967296 is past the limit of 1024 per turn","stop_reason":"error"}"#
        ),
        "the turn must end with the driver error:\n{text}"
    );
    assert_eq!(provider.request_count(), 1, "{text}");
}

/// A tool call at index 1023 completes a tool round trip.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_tool_call_at_the_last_index_under_the_limit_completes_in_the_component() {
    let staging = stage();
    let provider = Provider::start(1023);
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);

    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(text.contains("status:  ok"), "{text}");
    assert_eq!(
        provider.request_count(),
        2,
        "the tool result must go back to the provider:\n{text}"
    );
}
