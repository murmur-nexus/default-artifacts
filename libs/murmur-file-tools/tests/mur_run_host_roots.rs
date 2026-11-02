//! Host roots — the host folders murmur hands an unscoped WASM tool besides the work folder —
//! proved under a real `mur run`.
//!
//! `tests/wasm_components.rs` proves the resolution against preopens the test makes itself. Only
//! a launch shows that murmur hands the roots as the tools expect: the whole host, read and
//! write, at `murmur-host` under `mur run --unconfined`, and each `host_paths` entry of
//! `~/.murmur/config.yaml` at `murmur-host<path>`, with its access, in a contained session. It
//! also shows that `capabilities.filesystem.read_only` still refuses, before dispatch, a write
//! naming the work folder's absolute host path.
//!
//! The harness is `tests/mur_run_read_only.rs`'s: the real `mur` binary as a subprocess with
//! `HOME` pointed at a scratch directory, against a loopback fake Chat Completions provider that
//! scripts the model's tool calls and records every request body. The four tools and
//! `driver-openai` are built out of this workspace on every run; `mur` comes from `MUR_BIN` or
//! `PATH`, and a run that finds neither fails rather than skipping. It needs a `mur` at murmur
//! `e86e0b42` or later, the first that hands a WASM tool its host roots.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p murmur-file-tools --test mur_run_host_roots --
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

    /// The work folder, created, by its canonical absolute path.
    fn workdir(&self) -> PathBuf {
        let workdir = self.project.path().join("work");
        fs::create_dir_all(&workdir).unwrap();
        fs::canonicalize(workdir).unwrap()
    }

    /// The scratch `HOME`, by its canonical absolute path.
    fn home(&self) -> PathBuf {
        fs::canonicalize(self.home.path()).unwrap()
    }

    /// The capsule manifest, with `read_only` entries when given. `MODEL_ROUTES` sends
    /// `chat-latest` to the Chat Completions surface.
    fn write_manifest(&self, endpoint: &str, read_only: &[&str]) {
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
        let capabilities = if read_only.is_empty() {
            String::new()
        } else {
            let entries: String = read_only
                .iter()
                .map(|entry| format!("      - {entry}\n"))
                .collect();
            format!("capabilities:\n  filesystem:\n    read_only:\n{entries}")
        };
        fs::write(
            self.manifest(),
            format!(
                "name: host-roots-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 {tools}\
                 {capabilities}\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version
            ),
        )
        .unwrap();
    }

    /// Add `host_paths:` to the scratch `~/.murmur/config.yaml`, keeping what staging wrote.
    fn lend_host_path(&self, dir: &Path, access: &str) {
        let config = self.home.path().join(".murmur/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let existing = fs::read_to_string(&config).unwrap_or_default();
        assert!(
            !existing.contains("host_paths"),
            "the scratch config already lends host paths:\n{existing}"
        );
        fs::write(
            &config,
            format!(
                "{existing}{}host_paths:\n  - path: {}\n    access: {access}\n",
                if existing.is_empty() || existing.ends_with('\n') {
                    ""
                } else {
                    "\n"
                },
                dir.display()
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
    staging.write_manifest("http://127.0.0.1:1/v1", &[]);

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

/// Run one task to completion, with `extra` flags before `--task`, and return its output,
/// whatever its exit status.
fn run_session(staging: &Staging, extra: &[&str]) -> Output {
    mur(&staging.home)
        .current_dir(staging.project.path())
        .arg("run")
        .args(extra)
        .args([
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "use the host folders",
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
    /// A provider making the calls `script` holds, one per turn, in order, each as
    /// `(tool, arguments)`; the turn after the last one answers with text.
    fn start(script: Vec<(Tool, Value)>) -> Self {
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

// ── the scenarios ────────────────────────────────────────────────────────────

/// Runs `script` in `staging` with `extra` flags and returns each call's tool result, by call
/// number from 1, after asserting the session succeeded.
fn run_script(
    staging: &Staging,
    read_only: &[&str],
    extra: &[&str],
    script: Vec<(Tool, Value)>,
) -> Vec<String> {
    let calls = script.len();
    let provider = Provider::start(script);
    staging.write_manifest(&provider.endpoint, read_only);
    let output = run_session(staging, extra);
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");

    let results = tool_results(&provider.last_request());
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    (1..=calls)
        .map(|n| {
            let id = format!("call_{n}");
            results
                .iter()
                .find(|(call, _)| *call == id)
                .map(|(_, content)| content.clone())
                .unwrap_or_else(|| panic!("no tool result for {id} in {results:?}"))
        })
        .collect()
}

/// A scratch directory outside every temp folder and the work folder, for a `host_paths`
/// entry: `mur run` refuses one under `/tmp`, where `tempfile` may put its directories.
fn lendable_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".test-scratch")
        .join(format!("mur_run_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

/// Under `--unconfined` the tools hold the whole host: a read of a file in the scratch `HOME`
/// and a write outside the work folder, both by absolute path, work, and `read_only` still
/// refuses a write naming the work folder's absolute host path before dispatch.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn unconfined_tools_reach_the_whole_host_and_read_only_still_holds() {
    let staging = stage();
    let home = staging.home();
    let workdir = staging.workdir();
    fs::create_dir_all(workdir.join("protected")).unwrap();
    let secret = home.join("host-notes.txt");
    fs::write(&secret, "token-from-the-host-home\n").unwrap();
    let outside = staging.project.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let written = fs::canonicalize(&outside).unwrap().join("made/by-tool.txt");
    let protected = workdir.join("protected/x.txt");

    let results = run_script(
        &staging,
        &["protected"],
        &["--unconfined"],
        vec![
            (Tool::Read, json!({ "path": secret })),
            (
                Tool::Write,
                json!({ "path": written, "content": "written on the host\n" }),
            ),
            (Tool::Write, json!({ "path": protected, "content": "x" })),
        ],
    );

    assert!(
        results[0].contains("token-from-the-host-home"),
        "the read in HOME returns the file: {}",
        results[0]
    );
    assert!(
        !results[1].contains("Refused") && !results[1].contains("absolute_path"),
        "the write outside the work folder runs: {}",
        results[1]
    );
    assert_eq!(
        fs::read_to_string(&written).unwrap(),
        "written on the host\n",
        "the write by absolute path created the host file and its folder"
    );
    assert!(
        results[2].contains("Refused by the capsule manifest")
            && results[2].contains("destination 'path' declared by the tool's input schema"),
        "the write naming <workdir>/protected absolutely is refused before dispatch: {}",
        results[2]
    );
    assert!(!protected.exists(), "the refused write creates nothing");
}

/// In a contained session with one `read` host path, the tools hold that folder alone,
/// read-only: a read inside it works, a write inside it is refused naming it read-only, and a
/// read of another absolute path is refused listing it.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn contained_tools_reach_a_read_host_path_only() {
    let staging = stage();
    let lent = lendable_dir("host_roots_lent");
    fs::write(lent.join("lent.txt"), "lent-folder-content\n").unwrap();
    staging.lend_host_path(&lent, "read");
    let elsewhere = staging.home().join("elsewhere.txt");
    fs::write(&elsewhere, "not lent\n").unwrap();
    let blocked = lent.join("new.txt");

    let results = run_script(
        &staging,
        &[],
        &[],
        vec![
            (Tool::Read, json!({ "path": lent.join("lent.txt") })),
            (Tool::Write, json!({ "path": blocked, "content": "x" })),
            (Tool::Read, json!({ "path": elsewhere })),
        ],
    );

    let lent_path = lent.display().to_string();
    assert!(
        results[0].contains("lent-folder-content"),
        "the read in the host path returns the file: {}",
        results[0]
    );
    assert!(
        results[1].contains(&format!(
            "'path' '{}' is in {lent_path}, which this session opens read-only",
            blocked.display()
        )),
        "the write in the read host path is refused naming it: {}",
        results[1]
    );
    assert!(!blocked.exists(), "the refused write creates nothing");
    assert!(
        results[2].contains(&format!(
            "'path' is outside every host folder tool-read may open: {lent_path} (read); got '{}'",
            elsewhere.display()
        )),
        "the read elsewhere is refused listing the host path: {}",
        results[2]
    );
    assert!(!results[2].contains("not lent"), "{}", results[2]);
    let _ = fs::remove_dir_all(&lent);
}
