//! `capabilities.filesystem.read_only` against the four file tools, proved under a real `mur run`.
//!
//! `tests/manifests.rs` holds the manifests to two destination locations, `tool-write`'s and
//! `tool-edit`'s `path`, and to `murmur-destinations: []` on `tool-read` and `tool-find`. Only a
//! launch shows that murmur lowers them as declared: a read and a find under a `read_only` entry
//! run, an edit or a write of a path under it is refused before dispatch naming the declared
//! location, and an edit and a write outside it apply.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. The four tools and `driver-openai` are built out of
//! this workspace on every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that finds
//! neither fails rather than skipping. The last request's `tools` array is written to the test's
//! temp dir and measured, which is the request's tool array token count on the loopback.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p murmur-file-tools --test mur_run_read_only --
//! --ignored --nocapture` runs it.

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
    /// `(tool, version)` for each of the four file tools.
    tool_versions: Vec<(Tool, String)>,
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
        let tools: String = self
            .tool_versions
            .iter()
            .map(|(tool, version)| {
                format!(
                    "  - name: local/{}\n    version: {version}\n    runtime: tool\n",
                    tool.name()
                )
            })
            .collect();
        fs::write(
            self.manifest(),
            format!(
                "name: read-only-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 {tools}\
                 capabilities:\n  filesystem:\n    read_only:\n      - src\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version
            ),
        )
        .unwrap();
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let mut zips = vec![(driver_zip, "driver-openai")];
    let mut tool_versions = Vec::new();
    for tool in Tool::ALL {
        let (version, zip) = pack(&format!("tools/{}", tool.name()), artifacts.path());
        tool_versions.push((tool, version));
        zips.push((zip, tool.name()));
    }
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        tool_versions,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");

    for (zip, what) in &zips {
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

/// The tool calls the fake model makes, one per turn, in order, each as `(tool, arguments)`; the
/// turn after the last one answers with text.
fn scripted_calls() -> Vec<(Tool, Value)> {
    vec![
        (Tool::Read, json!({ "path": "src/spec.txt" })),
        (Tool::Find, json!({ "pattern": "colour", "dir": "src" })),
        (
            Tool::Edit,
            json!({ "path": "src/spec.txt", "edits": [{ "old": "alpha", "new": "beta" }] }),
        ),
        (
            Tool::Write,
            json!({ "path": "src/new.txt", "content": "x" }),
        ),
        (
            Tool::Edit,
            json!({ "path": "notes.txt", "edits": [
                { "old": "colour", "new": "color" },
                { "old": "draft", "new": "final" },
            ] }),
        ),
        (
            Tool::Write,
            json!({ "path": "out/new.txt", "content": "written\n" }),
        ),
    ]
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

/// The `(chars, cl100k tokens)` of `text`.
fn measure(bpe: &tiktoken_rs::CoreBPE, text: &str) -> (usize, usize) {
    (
        text.chars().count(),
        bpe.encode_with_special_tokens(text).len(),
    )
}

/// The request's `tools` array, written to `dir/tools.json`, then each file tool entry measured
/// as murmur counts a definition (trimmed description plus compact parameters), and as the whole
/// serialized entry, wrapper keys included.
fn report_tool_array(request: &str, dir: &Path) {
    let body: Value = serde_json::from_str(request).expect("the request body is JSON");
    let tools = body["tools"].as_array().expect("the request carries tools");
    let path = dir.join("tools.json");
    fs::write(&path, serde_json::to_string(&body["tools"]).unwrap()).unwrap();
    println!("tools array written to {}", path.display());

    let bpe = tiktoken_rs::cl100k_base().expect("the cl100k_base encoding loads");
    let (mut def_total, mut entry_total) = ((0, 0), (0, 0));
    for tool in Tool::ALL {
        let entry = tools
            .iter()
            .find(|entry| entry["function"]["name"] == tool.name())
            .unwrap_or_else(|| panic!("no {} entry in the request's tools", tool.name()));
        let function = &entry["function"];
        let description = function["description"].as_str().unwrap_or("").trim();
        let parameters = function["parameters"].to_string();
        let (d, p) = (measure(&bpe, description), measure(&bpe, &parameters));
        let whole = measure(&bpe, &entry.to_string());
        println!(
            "{:<11} definition {:>4} chars {:>3} tokens | entry {:>4} chars {:>3} tokens",
            tool.name(),
            d.0 + p.0,
            d.1 + p.1,
            whole.0,
            whole.1
        );
        def_total = (def_total.0 + d.0 + p.0, def_total.1 + d.1 + p.1);
        entry_total = (entry_total.0 + whole.0, entry_total.1 + whole.1);
    }
    println!(
        "four tools  definition {:>4} chars {:>3} tokens | entry {:>4} chars {:>3} tokens",
        def_total.0, def_total.1, entry_total.0, entry_total.1
    );
}

/// A read and a find under `src` run; an edit and a write under it are refused naming the
/// declared `path`; an edit and a write outside it apply.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn read_only_refuses_edit_and_write_by_declared_location_and_allows_the_reads() {
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
    assert!(
        !text.contains("W-SEC-018"),
        "no file tool is flagged as an undeclared writer:\n{text}"
    );

    let last = provider.last_request();
    let results = tool_results(&last);
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
    report_tool_array(&last, staging.project.path());

    assert!(
        result("call_1").contains("alpha spec: replace colour with color"),
        "the read under src returns the file: {}",
        result("call_1")
    );
    assert!(
        result("call_2").contains("spec.txt") && !result("call_2").contains("Refused"),
        "the find under src returns its match: {}",
        result("call_2")
    );
    for id in ["call_3", "call_4"] {
        let refusal = result(id);
        assert!(
            refusal.contains("Refused by the capsule manifest")
                && refusal.contains("destination 'path' declared by the tool's input schema"),
            "{id} must be refused naming path: {refusal}"
        );
    }
    for id in ["call_5", "call_6"] {
        assert!(
            !result(id).contains("Refused"),
            "{id} outside src runs: {}",
            result(id)
        );
    }

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
    assert_eq!(
        fs::read_to_string(workdir.join("out/new.txt")).unwrap(),
        "written\n",
        "the write outside src created its directory and file"
    );
}
