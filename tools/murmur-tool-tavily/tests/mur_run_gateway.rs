//! The tool and the runtime's credential gateway, proved against each other under a real
//! `mur run`.
//!
//! `tavily_ops.rs` and `wasm_component.rs` prove the tool sends no key. Only a launch proves
//! the other half: that the runtime attaches the operator's key to the tool's one request,
//! that the key reaches the upstream exactly once, and that it appears nowhere the tool, the
//! model or the session could see it — the workdir, the trace, or the launch's own output.
//!
//! Every test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, so the key is stored with `mur config set -g credentials.<NAME>` exactly as an
//! operator would, and never touches the developer's own config. The tool and the Anthropic
//! driver are built out of this workspace on every run; `mur` comes from `MUR_BIN` or `PATH`,
//! and a run that finds neither fails rather than skipping.
//!
//! They are `#[ignore]`d: a launch needs a `mur` carrying the per-entry credential gateway,
//! which the default `cargo test --workspace` has no reason to have.
//! `MUR_BIN=… cargo test -p murmur-tool-tavily --test mur_run_gateway -- --ignored` runs them.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex, Once},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const TOOL_CRATE: &str = "tools/murmur-tool-tavily";
const DRIVER_CRATE: &str = "drivers/murmur-driver-anthropic";
const TOOL_NAME: &str = "murmur-tool-tavily";
const CREDENTIAL_NAME: &str = "TAVILY_PROBE_KEY";
const FAKE_TITLE: &str = "Fake Tavily title for the gateway probe";

// ── the runtime under test ───────────────────────────────────────────────────

/// The `mur` binary these tests launch: `MUR_BIN` when set, otherwise the first executable
/// `mur` on `PATH`. Panics when neither resolves: a suite that reports success having
/// launched nothing proves nothing.
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
                 runtime as a subprocess, so a missing binary is a failure rather than a skip. \
                 Build one with `cargo build -p murmur-cli` in a murmur checkout."
            )
        })
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A `mur` invocation with the scratch `HOME` in place. `NEXUS_API_KEY` is removed so a
/// developer's own key cannot turn a local registry lookup into a remote one, and the probe
/// credential is removed so it can only come from the scratch config.
fn mur(home: &TempDir) -> Command {
    let mut command = Command::new(mur_binary());
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove(CREDENTIAL_NAME);
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

fn build_components() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                "murmur-tool-tavily",
                "-p",
                "murmur-driver-anthropic",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for the wasm components");
        assert!(
            status.success(),
            "cargo build of the wasm components failed"
        );
    });
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

struct Artifact {
    name: String,
    version: String,
    zip: PathBuf,
}

/// Pack a crate's own `murmur.yaml` and component with `zip -j`, as `build.yml` releases them,
/// so the launch reads the real `inference_auth:` block and `input_schema`.
fn pack(out_dir: &Path, crate_dir: &str) -> Artifact {
    build_components();
    let crate_path = workspace_root().join(crate_dir);
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
    let status = Command::new("zip")
        .current_dir(&crate_path)
        .arg("-jq")
        .arg(&zip)
        .arg("murmur.yaml")
        .arg(&wasm)
        .status()
        .expect("failed to spawn `zip`");
    assert!(status.success(), "packing {name}-{version}.mur.zip failed");
    Artifact { name, version, zip }
}

// ── staging ──────────────────────────────────────────────────────────────────

struct Staging {
    home: TempDir,
    project: TempDir,
    tool: Artifact,
    driver: Artifact,
    marker: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    /// Write the capsule manifest. The driver's entry carries its own gateway to the scripted
    /// LLM; the tool's carries one to the fake Tavily when `tavily` is `Some`, and none
    /// otherwise. The tool's direct egress is `allow: []` either way, so the gateway is its
    /// only way out.
    fn write_manifest(&self, llm: &str, tavily: Option<&str>) {
        let tool_gateway = tavily
            .map(|endpoint| {
                format!(
                    "    gateway:\n      endpoint: {endpoint}\n      api_key: ${{{CREDENTIAL_NAME}}}\n"
                )
            })
            .unwrap_or_default();
        let (tool, driver) = (&self.tool, &self.driver);
        fs::write(
            self.manifest(),
            format!(
                "name: tavily-gateway-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: {}\n    version: {}\n    runtime: driver\n    gateway:\n      \
                 endpoint: {llm}\n      api_key: test-key\n\
                 \x20 - name: {}\n    version: {}\n    runtime: tool\n{tool_gateway}    \
                 capabilities:\n      network:\n        allow: []\n\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {}\n",
                driver.name, driver.version, tool.name, tool.version, driver.name
            ),
        )
        .unwrap();
    }
}

fn random_marker() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("tvly-probe-{:x}{:x}", nanos, std::process::id())
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let tool = pack(artifacts.path(), TOOL_CRATE);
    let driver = pack(artifacts.path(), DRIVER_CRATE);
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        tool,
        driver,
        marker: random_marker(),
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1", Some("http://127.0.0.1:1"));

    for artifact in [&staging.driver, &staging.tool] {
        let mut publish = mur(&staging.home);
        publish.arg("publish").arg(&artifact.zip);
        run_to_success(publish, &format!("publishing {}", artifact.name));

        let mut install = mur(&staging.home);
        install
            .current_dir(staging.project.path())
            .arg("install")
            .arg(&artifact.zip);
        run_to_success(install, &format!("installing {}", artifact.name));
    }

    let mut set = mur(&staging.home);
    set.args([
        "config",
        "set",
        "-g",
        &format!("credentials.{CREDENTIAL_NAME}"),
        &staging.marker,
    ]);
    run_to_success(set, "storing the probe credential");
    staging
}

/// Run one task to completion; return its output and the session workdir it reported.
fn run_session(staging: &Staging) -> (Output, PathBuf) {
    let mut command = mur(&staging.home);
    command.args([
        "run",
        "--manifest",
        staging.manifest().to_str().unwrap(),
        "--task",
        "search the web for probe",
        "--verbose",
    ]);
    let output = run_to_success(command, "the session");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let marker = "workdir: ";
    let start = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("no '{marker}' in stdout:\n{stdout}"));
    let workdir = PathBuf::from(
        stdout[start + marker.len()..]
            .lines()
            .next()
            .unwrap_or_default()
            .trim(),
    );
    (output, workdir)
}

// ── the scripted LLM ─────────────────────────────────────────────────────────

fn tool_use_response(tool_id: &str, input: Value) -> String {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_id, "name": TOOL_NAME, "input": input}],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn_response() -> String {
    json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// One request a fake upstream received, headers lowercased.
#[derive(Debug, Clone)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// A loopback HTTP server answering request `n` with `responses[n]` (the last one repeats)
/// and recording every request it was sent.
struct Upstream {
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Upstream {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                recorded.lock().unwrap().push(request);
                let body = &responses[index.min(responses.len() - 1)];
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { endpoint, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .map(|r| serde_json::from_str(&r.body).unwrap_or(Value::Null))
            .collect()
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
    let request_line = lines.next()?.to_string();
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
        request_line,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

// ── reading the tool result back ─────────────────────────────────────────────

const FENCE_CLOSE: &str = "</untrusted-content>";

/// What the untrusted fence wrapped, requiring that there was one naming `source`. Only the
/// final closer is stripped: the runtime neutralises any inner one.
fn unfence(text: &str, source: &str) -> String {
    let expected_open = format!("<untrusted-content source={source}>");
    let (open, body) = text
        .split_once('\n')
        .unwrap_or_else(|| panic!("a tool result must arrive fenced; got:\n{text}"));
    assert_eq!(
        open, expected_open,
        "the fence must name this tool; got:\n{text}"
    );
    body.strip_suffix(&format!("\n{FENCE_CLOSE}"))
        .unwrap_or_else(|| panic!("a fenced result must end at `{FENCE_CLOSE}`; got:\n{text}"))
        .to_string()
}

fn find_tool_result(requests: &[Value], tool_id: &str) -> Option<Value> {
    requests.iter().find_map(|request| {
        request
            .get("messages")?
            .as_array()?
            .iter()
            .find_map(|message| {
                (message.get("role")?.as_str()? == "user").then_some(())?;
                message
                    .get("content")?
                    .as_array()?
                    .iter()
                    .find_map(|block| {
                        (block.get("type")?.as_str()? == "tool_result"
                            && block.get("tool_use_id")?.as_str()? == tool_id)
                            .then(|| block.clone())
                    })
            })
    })
}

fn extract_result_text(tool_result: &Value) -> String {
    if let Some(text) = tool_result.get("content").and_then(Value::as_str) {
        return text.to_string();
    }
    tool_result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_string)
            })
        })
        .unwrap_or_default()
}

fn tool_result_text(llm: &Upstream, tool_id: &str) -> String {
    let block = find_tool_result(&llm.bodies(), tool_id)
        .unwrap_or_else(|| panic!("no tool_result posted for {tool_id}"));
    unfence(&extract_result_text(&block), &format!("tool:{TOOL_NAME}"))
}

/// Every regular file under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn fake_tavily_response() -> String {
    json!({
        "query": "probe",
        "results": [{"title": FAKE_TITLE, "url": "https://example.org/probe", "content": "probe content"}],
        "usage": {"credits": 1},
        "request_id": "probe-request"
    })
    .to_string()
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// The operator's key reaches Tavily exactly once, attached by the runtime, and nowhere else.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn the_runtime_attaches_the_key_the_tool_never_holds() {
    let staging = stage();
    let tavily = Upstream::start(vec![fake_tavily_response()]);
    let llm = Upstream::start(vec![
        tool_use_response("toolu_probe", json!({"query": "probe"})),
        end_turn_response(),
    ]);
    staging.write_manifest(&llm.endpoint, Some(&tavily.endpoint));

    let (output, workdir) = run_session(&staging);

    let searches = tavily.requests();
    assert_eq!(
        searches.len(),
        1,
        "exactly one request reached Tavily: {searches:?}"
    );
    assert!(
        searches[0].request_line.starts_with("POST /search "),
        "{}",
        searches[0].request_line
    );
    let authorization: Vec<&str> = searches[0]
        .headers
        .iter()
        .filter(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(
        authorization,
        [format!("Bearer {}", staging.marker)],
        "exactly one, the runtime's"
    );
    println!(
        "authorization headers at the fake Tavily: {}",
        authorization.len()
    );

    let text = tool_result_text(&llm, "toolu_probe");
    assert!(text.contains(&format!("[1] {FAKE_TITLE}\n")), "{text}");

    let marker = staging.marker.as_bytes();
    let stdout = &output.stdout;
    let stderr = &output.stderr;
    assert!(
        !contains(stdout, marker),
        "the key must not appear on stdout"
    );
    assert!(
        !contains(stderr, marker),
        "the key must not appear on stderr"
    );
    assert!(
        workdir.is_dir(),
        "{} must be the session workdir",
        workdir.display()
    );

    // The only file allowed to hold the key is the operator's own global config.
    let config_file = staging.home.path().join(".murmur/config.yaml");
    let mut traces = 0;
    let mut scanned = 0;
    for root in [
        staging.project.path(),
        staging.home.path(),
        workdir.as_path(),
    ] {
        for file in files_under(root) {
            if file == config_file {
                continue;
            }
            scanned += 1;
            traces += usize::from(file.file_name().is_some_and(|name| name == "trace.jsonl"));
            let bytes = fs::read(&file).unwrap_or_default();
            assert!(
                !contains(&bytes, marker),
                "the key leaked into {}",
                file.display()
            );
        }
    }
    assert!(
        traces >= 1,
        "the scan must include the session trace ({scanned} files scanned)"
    );
    assert!(
        contains(&fs::read(&config_file).unwrap(), marker),
        "non-vacuity: the key is in the operator's config, where the scan skips it"
    );
    println!("marker grep: 0 hits in {scanned} files ({traces} trace.jsonl), stdout and stderr");

    let stderr = String::from_utf8_lossy(stderr);
    let warning = stderr
        .lines()
        .find(|line| line.contains("W-SEC-030"))
        .unwrap_or_else(|| panic!("no W-SEC-030 on stderr:\n{stderr}"));
    assert!(warning.contains(TOOL_NAME), "{warning}");
}

/// Without `gateway:` the tool has nowhere to send the search and says so, and nothing is sent.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn without_a_gateway_the_tool_fails_closed() {
    let staging = stage();
    let tavily = Upstream::start(vec![fake_tavily_response()]);
    let llm = Upstream::start(vec![
        tool_use_response("toolu_nogw", json!({"query": "probe"})),
        end_turn_response(),
    ]);
    staging.write_manifest(&llm.endpoint, None);

    let (output, _workdir) = run_session(&staging);

    let text = tool_result_text(&llm, "toolu_nogw");
    let data: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(data["error_kind"], json!("gateway_missing"), "{data}");
    assert!(tavily.requests().is_empty(), "nothing may reach Tavily");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("W-SEC-030"),
        "no gateway, so nothing unmetered to warn about"
    );
}
