//! `capabilities.filesystem.read_only` against this tool, proved under a real `mur run`.
//!
//! `tests/manifest_declares_destination.rs` holds the manifest to two destination locations,
//! `write.path` and `edit.path`. Only a launch shows that murmur lowers them as declared: a read
//! of a path under a `read_only` entry runs, an `edit` or `write` of one is refused before
//! dispatch naming the declared location, and an `edit` outside the entry applies.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. `tool-editor` and `driver-openai` are built out of this
//! workspace on every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that finds neither
//! fails rather than skipping.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p tool-editor --test mur_run_read_only -- --ignored`
//! runs it.

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

use serde_json::{json, Value};
use tempfile::TempDir;

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
    editor_version: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn workdir(&self) -> PathBuf {
        self.project.path().join("work")
    }

    /// The capsule manifest: `src` is read-only. `MODEL_ROUTES` sends `chat-latest` to the
    /// Chat Completions surface.
    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: read-only-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/tool-editor\n    version: {}\n    runtime: tool\n\
                 capabilities:\n  filesystem:\n    read_only:\n      - src\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version, self.editor_version
            ),
        )
        .unwrap();
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let (editor_version, editor_zip) = pack("tools/tool-editor", artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        editor_version,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");

    for (zip, what) in [(&driver_zip, "driver-openai"), (&editor_zip, "tool-editor")] {
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
            "apply the changes in src/spec.txt to notes.txt",
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

/// The tool calls the fake model makes, one per turn, in order; the turn after the last one
/// answers with text.
fn scripted_calls() -> Vec<Value> {
    vec![
        json!({ "read": { "path": "src/spec.txt" } }),
        json!({ "edit": { "path": "src/spec.txt", "edits": [{ "old": "a", "new": "b" }] } }),
        json!({ "write": { "path": "src/new.txt", "content": "x" } }),
        json!({ "edit": { "path": "notes.txt", "edits": [
            { "old": "colour", "new": "color" },
            { "old": "draft", "new": "final" },
        ] } }),
    ]
}

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

/// One assistant turn holding a single `tool-editor` call with id `call_<turn>`.
fn tool_call_stream(turn: usize, arguments: &Value) -> String {
    let call = json!({
        "index": 0, "id": format!("call_{turn}"), "type": "function",
        "function": {"name": "tool-editor", "arguments": arguments.to_string()}
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

// ── the scenario ─────────────────────────────────────────────────────────────

const SPEC: &str = "alpha spec: replace colour with color, draft with final\n";
const NOTES: &str = "the colour of the draft\n";

/// A read under `src` runs; an `edit` and a `write` under it are refused naming `edit.path` and
/// `write.path`; an `edit` outside it applies.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn read_only_refuses_edit_and_write_by_declared_location_and_allows_the_read() {
    let staging = stage();
    let workdir = staging.workdir();
    fs::create_dir_all(workdir.join("src")).unwrap();
    fs::write(workdir.join("src/spec.txt"), SPEC).unwrap();
    fs::write(workdir.join("notes.txt"), NOTES).unwrap();

    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");

    let results = tool_results(&provider.last_request());
    let result = |id: &str| -> &str {
        results
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, content)| content.as_str())
            .unwrap_or_else(|| panic!("no tool result for {id} in {results:?}"))
    };
    for (id, content) in &results {
        println!("{id}: {content}");
    }

    assert!(
        result("call_1").contains("alpha spec: replace colour with color"),
        "the read under src returns the file: {}",
        result("call_1")
    );
    for (id, location) in [("call_2", "edit.path"), ("call_3", "write.path")] {
        let refusal = result(id);
        assert!(
            refusal.contains("Refused by the capsule manifest")
                && refusal.contains(&format!(
                    "destination '{location}' declared by the tool's input schema"
                )),
            "{id} must be refused naming {location}: {refusal}"
        );
    }
    assert!(
        !result("call_4").contains("Refused"),
        "the edit outside src runs: {}",
        result("call_4")
    );

    assert_eq!(
        fs::read(workdir.join("src/spec.txt")).unwrap(),
        SPEC.as_bytes(),
        "the refused edit leaves src/spec.txt byte-identical"
    );
    assert!(
        !workdir.join("src/new.txt").exists(),
        "the refused write creates nothing"
    );
    assert_eq!(
        fs::read_to_string(workdir.join("notes.txt")).unwrap(),
        "the color of the final\n",
        "the edit outside src applied both pairs"
    );
}
