//! The tool and the runtime's credential gateway, proved against each other under a real
//! `mur run`.
//!
//! `hf_ops.rs` and `wasm_component.rs` prove the tool sends no token. Only a launch proves the
//! other half: that the runtime attaches the operator's token to every request the tool sends
//! to the gateway (the Hub), that requests the tool sends directly — to the CDN a download is
//! redirected to, and to the datasets server — carry none, and that the token appears nowhere
//! the tool, the model or the session could see it.
//!
//! Every test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, so the probe token is stored with `mur config set -g credentials.<NAME>` exactly
//! as an operator would. Three loopback fakes stand in for the Hub (behind the gateway), the
//! CDN and the datasets server; a fourth scripts the model through `driver-anthropic`. `mur`
//! comes from `MUR_BIN` or `PATH`, and a run that finds neither fails rather than skipping.
//!
//! They are `#[ignore]`d: `MUR_BIN=… cargo test -p tool-huggingface --test mur_run_gateway --
//! --ignored` runs them.

use std::{
    collections::BTreeSet,
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
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const TOOL_CRATE: &str = "tools/tool-huggingface";
const DRIVER_CRATE: &str = "drivers/driver-anthropic";
const TOOL_NAME: &str = "tool-huggingface";
const CREDENTIAL_NAME: &str = "HF_GATEWAY_PROBE_TOKEN";

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

/// A `mur` invocation with the scratch `HOME` in place. The probe credential is removed so it
/// can only come from the scratch config.
fn mur(home: &TempDir) -> Command {
    let mut command = Command::new(mur_binary());
    command.env("HOME", home.path()).env_remove(CREDENTIAL_NAME);
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
                "tool-huggingface",
                "-p",
                "driver-anthropic",
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

impl Artifact {
    /// How the capsule names it: an artifact installed from a `.mur.zip` file is `local/<name>`.
    fn reference(&self) -> String {
        format!("local/{}", self.name)
    }
}

/// Pack a crate's own `murmur.yaml` and component with `scripts/package-artifact.sh`, the
/// `mur build` packing the release runs, so the launch reads the real `upstream_auth:` block and
/// `input_schema`.
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
    /// LLM. The tool's entry carries `gateway` as given, the datasets server at `datasets`
    /// in its `config:`, and `allow` as both its own and the capsule-wide network allow-list,
    /// so the CDN and datasets fakes are reachable directly and nothing else is.
    fn write_manifest(&self, llm: &str, gateway: &ToolGateway, datasets: &str, allow: &[String]) {
        let tool_gateway = match gateway {
            ToolGateway::Keyed(endpoint) => format!(
                "    gateway:\n      endpoint: {endpoint}\n      api_key: ${{{CREDENTIAL_NAME}}}\n"
            ),
            ToolGateway::Keyless(endpoint) => {
                format!("    gateway:\n      endpoint: {endpoint}\n      keyless: true\n")
            }
            ToolGateway::None => String::new(),
        };
        let allow_list = if allow.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                allow
                    .iter()
                    .map(|entry| format!("\"{entry}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let (tool, driver) = (&self.tool, &self.driver);
        fs::write(
            self.manifest(),
            format!(
                "name: huggingface-gateway-proof\nversion: 0.1.0\n\
                 capabilities:\n  network:\n    allow: {allow_list}\n\
                 artifacts:\n\
                 \x20 - name: {}\n    version: {}\n    runtime: driver\n    gateway:\n      \
                 endpoint: {llm}\n      api_key: test-key\n\
                 \x20 - name: {}\n    version: {}\n    runtime: tool\n{tool_gateway}    \
                 config:\n      config_version: 1\n      datasets_server_endpoint: {datasets}\n    \
                 capabilities:\n      network:\n        allow: {allow_list}\n\
                 inference:\n  transport: http\n  model: test-model\n  max_tokens: 1024\n  driver:\n    artifact: {}\n",
                driver.reference(),
                driver.version,
                tool.reference(),
                tool.version,
                driver.reference()
            ),
        )
        .unwrap();
    }
}

/// How the tool's entry binds the Hub.
enum ToolGateway {
    Keyed(String),
    Keyless(String),
    None,
}

fn random_marker() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("hf-probe-{:x}{:x}", nanos, std::process::id())
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
    staging.write_manifest(
        "http://127.0.0.1:1",
        &ToolGateway::Keyed("http://127.0.0.1:1".into()),
        "http://127.0.0.1:1",
        &[],
    );

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

/// One finished `mur run`, and the directories its `--verbose` startup lines named.
struct Session {
    output: Output,
    workdir: PathBuf,
    /// The session's records directory, holding its `trace.jsonl`.
    records: PathBuf,
}

/// The path a `--verbose` startup line `<label><path>` names, if stdout has that line.
fn startup_path(stdout: &str, label: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .map(|path| PathBuf::from(path.trim()))
}

/// Run one task to completion; return its output and the session workdir and records
/// directory it reported.
fn run_session(staging: &Staging) -> Session {
    let mut command = mur(&staging.home);
    command.args([
        "run",
        "--manifest",
        staging.manifest().to_str().unwrap(),
        "--task",
        "fetch from Hugging Face",
        "--verbose",
    ]);
    let output = run_to_success(command, "the session");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let workdir = startup_path(&stdout, "workdir: ")
        .unwrap_or_else(|| panic!("no 'workdir: ' line in stdout:\n{stdout}"));
    let records = startup_path(&stdout, "records: ").unwrap_or_else(|| {
        panic!(
            "no 'records: ' line in stdout. These tests need a `mur` that prints the session's \
             records directory under --verbose: murmur #312 or later.\n{stdout}"
        )
    });
    Session {
        output,
        workdir,
        records,
    }
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

// ── the Hub, CDN and datasets-server fakes ───────────────────────────────────

type Handler = dyn Fn(&Recorded) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync;

/// A loopback HTTP server answering each request through `handler` and recording it. A `HEAD`
/// gets the headers only.
struct Server {
    endpoint: String,
    authority: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    fn start(handler: Arc<Handler>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                recorded.lock().unwrap().push(request.clone());
                let (status, headers, body) = handler(&request);
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nconnection: close\r\ncontent-length: {}\r\n",
                    body.len()
                );
                for (name, value) in headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                if !request.request_line.starts_with("HEAD ") {
                    let _ = stream.write_all(&body);
                }
            }
        });
        Self {
            endpoint: format!("http://{authority}"),
            authority,
            requests,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

fn target(request: &Recorded) -> &str {
    request.request_line.split(' ').nth(1).unwrap_or_default()
}

const REPO_FILE_PATH: &str = "/org/model/resolve/main/weights.bin";

fn blob() -> Vec<u8> {
    (0..50_000u32).map(|i| (i % 253) as u8).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The CDN: serves the blob to any GET.
fn cdn() -> Server {
    let blob = blob();
    Server::start(Arc::new(move |_: &Recorded| {
        (200, Vec::new(), blob.clone())
    }))
}

/// The Hub: a `302` to the CDN for the file, repo metadata, and a card.
fn hub(cdn: &Server) -> Server {
    let location = format!("{}/blob?X-Amz-Signature=probe", cdn.endpoint);
    let blob = blob();
    let size = blob.len().to_string();
    let etag = format!("\"{}\"", sha256_hex(&blob));
    Server::start(Arc::new(move |request: &Recorded| {
        let target = target(request);
        if target == REPO_FILE_PATH {
            return (
                302,
                vec![
                    ("location".into(), location.clone()),
                    ("x-linked-size".into(), size.clone()),
                    ("x-linked-etag".into(), etag.clone()),
                ],
                Vec::new(),
            );
        }
        if target.starts_with("/api/models/org/model") {
            let meta = json!({"id":"org/model","private":false,"gated":false,"tags":["license:mit"],
                "siblings":[{"rfilename":"weights.bin","size":50000}]});
            return (
                200,
                vec![("content-type".into(), "application/json".into())],
                meta.to_string().into_bytes(),
            );
        }
        if target == "/org/model/resolve/main/README.md" {
            return (200, Vec::new(), b"# Probe model card".to_vec());
        }
        (
            404,
            vec![("x-error-code".into(), "EntryNotFound".into())],
            Vec::new(),
        )
    }))
}

/// The datasets server: one split and one row.
fn datasets() -> Server {
    Server::start(Arc::new(|request: &Recorded| {
        let target = target(request);
        let body = if target.starts_with("/splits?") {
            json!({"splits":[{"dataset":"org/data","config":"default","split":"train"}],"pending":[],"failed":[]})
        } else if target.starts_with("/rows?") {
            json!({"features":[{"feature_idx":0,"name":"text","type":{"dtype":"string","_type":"Value"}}],
                "rows":[{"row_idx":0,"row":{"text":"probe row"},"truncated_cells":[]}],"num_rows_total":1})
        } else {
            return (404, Vec::new(), b"{\"error\":\"Not found.\"}".to_vec());
        };
        (
            200,
            vec![("content-type".into(), "application/json".into())],
            body.to_string().into_bytes(),
        )
    }))
}

fn authorization(request: &Recorded) -> Vec<&str> {
    request
        .headers
        .iter()
        .filter(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str())
        .collect()
}

/// The tool's `session_start.gateways[]` entry in the session trace.
fn tool_gateway_entry(records: &Path) -> Option<Value> {
    let trace = fs::read_to_string(records.join("trace.jsonl")).ok()?;
    trace.lines().find_map(|line| {
        let event: Value = serde_json::from_str(line).ok()?;
        let find = |value: &Value| -> Option<Value> {
            value
                .get("gateways")?
                .as_array()?
                .iter()
                .find(|entry| {
                    entry
                        .get("artifact")
                        .and_then(Value::as_str)
                        .is_some_and(|a| a.contains(TOOL_NAME))
                })
                .cloned()
        };
        find(&event).or_else(|| event.get("data").and_then(find))
    })
}

const DOWNLOAD: &str = "toolu_download";
const PEEK: &str = "toolu_peek";
const REPO: &str = "toolu_repo";

fn download_input() -> Value {
    json!({"op":"download","repo":"org/model","repo_file":"weights.bin","dest":"dl/weights.bin"})
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// The token reaches the Hub on every gateway request, attached once by the runtime, and never
/// the CDN, the datasets server, or any file or output of the session.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn the_runtime_attaches_the_token_to_hub_requests_only() {
    let staging = stage();
    let cdn = cdn();
    let hub = hub(&cdn);
    let datasets = datasets();
    let llm = Upstream::start(vec![
        tool_use_response(DOWNLOAD, download_input()),
        tool_use_response(PEEK, json!({"op":"dataset","repo":"org/data","limit":1})),
        tool_use_response(REPO, json!({"op":"repo","repo":"org/model"})),
        end_turn_response(),
    ]);
    let allow = vec![cdn.authority.clone(), datasets.authority.clone()];
    staging.write_manifest(
        &llm.endpoint,
        &ToolGateway::Keyed(hub.endpoint.clone()),
        &datasets.endpoint,
        &allow,
    );

    let Session {
        output,
        workdir,
        records,
    } = run_session(&staging);

    let download = tool_result_text(&llm, DOWNLOAD);
    let digest = sha256_hex(&blob());
    assert!(
        download.contains(&format!("sha256: {digest}")),
        "{download}"
    );
    assert!(
        download.contains(&format!("→ {} direct (no credential)", cdn.authority)),
        "{download}"
    );
    let written = fs::read(workdir.join("dl/weights.bin")).expect("the download landed");
    assert_eq!(sha256_hex(&written), digest);
    let peek = tool_result_text(&llm, PEEK);
    assert!(peek.contains("[row 0] {\"text\":\"probe row\"}"), "{peek}");
    let repo = tool_result_text(&llm, REPO);
    assert!(repo.contains("card:\n# Probe model card"), "{repo}");
    println!("download result:\n{download}\n\npeek result:\n{peek}\n");

    let hub_requests = hub.requests();
    assert!(
        hub_requests.len() >= 4,
        "HEAD, GET, metadata, card: {hub_requests:?}"
    );
    let bearer = format!("Bearer {}", staging.marker);
    for request in &hub_requests {
        assert_eq!(
            authorization(request),
            [bearer.as_str()],
            "exactly one, the runtime's, on {}",
            request.request_line
        );
    }
    println!(
        "hub fake: {} gateway requests, each with exactly one authorization header",
        hub_requests.len()
    );
    let marker = staging.marker.as_bytes();
    for (name, server) in [("cdn", &cdn), ("datasets", &datasets)] {
        let requests = server.requests();
        assert!(!requests.is_empty(), "the {name} fake was reached");
        for request in &requests {
            assert!(authorization(request).is_empty(), "{name}: {request:?}");
            assert!(
                !contains(request.request_line.as_bytes(), marker),
                "{name}: {request:?}"
            );
            for (header, value) in &request.headers {
                assert!(
                    !contains(value.as_bytes(), marker),
                    "{name}: probe in {header}"
                );
            }
        }
        println!(
            "{name} fake: {} direct requests, headers {:?}",
            requests.len(),
            requests[0]
                .headers
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
        );
    }

    let gateway = tool_gateway_entry(&records).expect("session_start.gateways names the tool");
    assert_eq!(gateway["credential_source"], json!("config"), "{gateway}");

    assert!(
        !contains(&output.stdout, marker),
        "the token must not appear on stdout"
    );
    assert!(
        !contains(&output.stderr, marker),
        "the token must not appear on stderr"
    );
    let config_file = staging.home.path().join(".murmur/config.yaml");
    let files: BTreeSet<PathBuf> = [
        staging.project.path(),
        staging.home.path(),
        workdir.as_path(),
        records.as_path(),
    ]
    .into_iter()
    .flat_map(files_under)
    .filter(|file| *file != config_file)
    .collect();
    for file in &files {
        let bytes = fs::read(file).unwrap_or_default();
        assert!(
            !contains(&bytes, marker),
            "the token leaked into {}",
            file.display()
        );
    }
    assert!(files.contains(&records.join("trace.jsonl")));
    assert!(
        contains(&fs::read(&config_file).unwrap(), marker),
        "non-vacuity: the token is in the operator's config, where the scan skips it"
    );
    println!(
        "marker grep: 0 hits in {} files, stdout and stderr",
        files.len()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let warning = stderr
        .lines()
        .find(|line| line.contains("W-SEC-030"))
        .unwrap_or_else(|| panic!("no W-SEC-030 on stderr:\n{stderr}"));
    assert!(warning.contains(TOOL_NAME), "{warning}");
}

/// Without `gateway:` every op is `gateway_missing`, the dataset peek included, and nothing is
/// sent anywhere.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn without_a_gateway_every_op_fails_closed() {
    let staging = stage();
    let cdn = cdn();
    let hub = hub(&cdn);
    let datasets = datasets();
    let llm = Upstream::start(vec![
        tool_use_response(DOWNLOAD, download_input()),
        tool_use_response(PEEK, json!({"op":"dataset","repo":"org/data"})),
        end_turn_response(),
    ]);
    let allow = vec![cdn.authority.clone(), datasets.authority.clone()];
    staging.write_manifest(
        &llm.endpoint,
        &ToolGateway::None,
        &datasets.endpoint,
        &allow,
    );

    let Session { workdir, .. } = run_session(&staging);

    for id in [DOWNLOAD, PEEK] {
        let text = tool_result_text(&llm, id);
        let data: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        assert_eq!(data["error_kind"], json!("gateway_missing"), "{data}");
        println!("{id}: {text}");
    }
    assert!(hub.requests().is_empty(), "nothing may reach the Hub");
    assert!(cdn.requests().is_empty(), "nothing may reach the CDN");
    assert!(
        datasets.requests().is_empty(),
        "nothing may reach the datasets server"
    );
    assert!(!workdir.join("dl/weights.bin").exists());
}

/// With `keyless: true` the runtime attaches nothing, and public reads still work.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_keyless_gateway_attaches_no_token() {
    let staging = stage();
    let cdn = cdn();
    let hub = hub(&cdn);
    let datasets = datasets();
    let llm = Upstream::start(vec![
        tool_use_response(DOWNLOAD, download_input()),
        end_turn_response(),
    ]);
    let allow = vec![cdn.authority.clone(), datasets.authority.clone()];
    staging.write_manifest(
        &llm.endpoint,
        &ToolGateway::Keyless(hub.endpoint.clone()),
        &datasets.endpoint,
        &allow,
    );

    let Session { records, .. } = run_session(&staging);

    let download = tool_result_text(&llm, DOWNLOAD);
    assert!(
        download.contains(&format!("sha256: {}", sha256_hex(&blob()))),
        "{download}"
    );
    let hub_requests = hub.requests();
    assert!(!hub_requests.is_empty());
    for request in hub_requests.iter().chain(cdn.requests().iter()) {
        assert!(authorization(request).is_empty(), "{request:?}");
    }
    let gateway = tool_gateway_entry(&records).expect("session_start.gateways names the tool");
    assert_eq!(gateway["credential_source"], json!("keyless"), "{gateway}");
}
