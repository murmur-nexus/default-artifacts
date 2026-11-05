//! The network allow-list, proved under a real `mur run`: every request the component sends,
//! each redirect hop included, is checked by the runtime against the capsule's
//! `capabilities.network.allow`, narrowed by `capabilities.network` on the tool's entry.
//!
//! `tests/search_index.rs` runs the component under Wasmtime's own `wasi:http`, which admits
//! every request; only a launch shows the runtime refusing a hop and the tool naming the host.
//!
//! Two loopback servers stand for an index host and the host it redirects to: A answers
//! `/latest` with `302 Location: http://127.0.0.1:<B>/v1/artifacts-index.json`, and B serves the
//! test fixture. A loopback fake Chat Completions provider scripts one call to the tool, then a
//! final answer, and records every request body, so the tool result the model saw is read back
//! from the last one. The component and `driver-openai` are built and packed out of this
//! workspace on every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that finds neither
//! fails rather than skipping.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p tool-registry-search --test mur_run_network --
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

use serde_json::{json, Value};
use tempfile::TempDir;

const TOOL: &str = "tool-registry-search";

// ── mur ──────────────────────────────────────────────────────────────────────

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

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ── build and pack ───────────────────────────────────────────────────────────

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

// ── the fake provider ────────────────────────────────────────────────────────

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

/// A loopback Chat Completions server: the first request gets one call to this tool with
/// `arguments`, every later one a text answer. It records every request body.
struct Provider {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Provider {
    fn start(arguments: Value) -> Self {
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
                let first = tool_results(&body).is_empty();
                recorded.lock().unwrap().push(body);
                let payload = if first {
                    let call = json!({
                        "index": 0, "id": "call_1", "type": "function",
                        "function": {"name": TOOL, "arguments": arguments.to_string()}
                    });
                    chat_chunk(json!({"role": "assistant", "content": ""}), None)
                        + &chat_chunk(json!({"tool_calls": [call]}), None)
                        + &chat_chunk(json!({}), Some("tool_calls"))
                        + "data: [DONE]\n\n"
                } else {
                    chat_chunk(json!({"content": "done"}), None)
                        + &chat_chunk(json!({}), Some("stop"))
                        + "data: [DONE]\n\n"
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

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
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
            let id = message["tool_call_id"].as_str().unwrap_or_default();
            (id.to_string(), content)
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

// ── the index hosts ──────────────────────────────────────────────────────────

/// A loopback HTTP server answering every request with `respond(path)`, recording each path.
struct Host {
    port: u16,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Host {
    fn start(respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&paths);
        let respond = Arc::new(respond);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&request);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                seen.lock().unwrap().push(path.clone());
                let _ = stream.write_all(&respond(&path));
            }
        });
        Self { port, paths }
    }

    fn authority(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

fn http_response(status: &str, extra_headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// B, serving the fixture at `/v1/artifacts-index.json`, and A, redirecting `/latest` there.
fn index_hosts() -> (Host, Host) {
    let fixture = fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/artifacts-index.json"),
    )
    .unwrap();
    let b = Host::start(move |path| match path {
        "/v1/artifacts-index.json" => http_response("200 OK", "", &fixture),
        _ => http_response("404 Not Found", "", b""),
    });
    let location = format!("http://{}/v1/artifacts-index.json", b.authority());
    let a = Host::start(move |path| match path {
        "/latest" => http_response("302 Found", &format!("Location: {location}\r\n"), b""),
        _ => http_response("404 Not Found", "", b""),
    });
    (a, b)
}

// ── staging ──────────────────────────────────────────────────────────────────

struct Staging {
    home: TempDir,
    project: TempDir,
    driver_version: String,
    tool_version: String,
    _artifacts: TempDir,
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let (tool_version, tool_zip) = pack("tools/tool-registry-search", artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        tool_version,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1", &[], None);
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

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    /// The capsule manifest: `allow` as the capsule's `capabilities.network.allow`, and
    /// `entry_allow`, when given, as `capabilities.network.allow` on the tool's entry.
    fn write_manifest(&self, endpoint: &str, allow: &[String], entry_allow: Option<&[String]>) {
        let list = |items: &[String], indent: &str| -> String {
            if items.is_empty() {
                " []\n".to_string()
            } else {
                items
                    .iter()
                    .map(|item| format!("\n{indent}- \"{item}\""))
                    .collect::<String>()
                    + "\n"
            }
        };
        let entry_capabilities = match entry_allow {
            None => String::new(),
            Some(items) => format!(
                "    capabilities:\n      network:\n        allow:{}",
                list(items, "          ")
            ),
        };
        let capabilities = if allow.is_empty() {
            String::new()
        } else {
            format!(
                "capabilities:\n  network:\n    allow:{}",
                list(allow, "      ")
            )
        };
        fs::write(
            self.manifest(),
            format!(
                "name: registry-search-network-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/{TOOL}\n    version: {}\n    runtime: tool\n\
                 {entry_capabilities}\
                 {capabilities}\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version, self.tool_version
            ),
        )
        .unwrap();
    }

    /// One session in which the model calls the tool once with `arguments`; returns the tool
    /// result the model was sent, after asserting the session succeeded.
    fn run_one_call(
        &self,
        arguments: Value,
        allow: &[String],
        entry_allow: Option<&[String]>,
    ) -> String {
        let provider = Provider::start(arguments);
        self.write_manifest(&provider.endpoint, allow, entry_allow);
        println!(
            "--- manifest ---\n{}",
            fs::read_to_string(self.manifest()).unwrap()
        );
        let workdir = self.project.path().join("work");
        fs::create_dir_all(&workdir).unwrap();
        let output = mur(&self.home)
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest().to_str().unwrap(),
                "--task",
                "search the registry",
                "--workdir",
                workdir.to_str().unwrap(),
            ])
            .output()
            .expect("spawning `mur run`");
        let text = combined(&output);
        println!("{text}");
        assert!(output.status.success(), "the session must succeed:\n{text}");

        let requests = provider.requests();
        let results = tool_results(requests.last().expect("the provider was sent a request"));
        let (_, content) = results
            .iter()
            .find(|(id, _)| id == "call_1")
            .unwrap_or_else(|| panic!("no tool result for call_1 in {results:?}"));
        println!("--- tool result ---\n{content}");
        assert!(
            content.contains(&format!("<untrusted-content source=tool:{TOOL}")),
            "the runtime fences the tool result: {content}"
        );
        content.clone()
    }
}

/// The JSON inside the runtime's `<untrusted-content …>` fence: a passed call's `data`.
fn fenced_json(content: &str) -> Value {
    let inner = content
        .trim()
        .strip_prefix(&format!("<untrusted-content source=tool:{TOOL}>"))
        .and_then(|rest| rest.strip_suffix("</untrusted-content>"))
        .unwrap_or_else(|| panic!("not one fenced tool result: {content}"));
    serde_json::from_str(inner.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {inner}"))
}

fn http_rule(host: &Host) -> String {
    format!("http://{}", host.authority())
}

// ── the cases ────────────────────────────────────────────────────────────────

#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn every_hop_is_checked_against_the_allow_list_the_tool_runs_under() {
    let staging = stage();
    let (a, b) = index_hosts();
    let url = format!("http://{}/latest", a.authority());
    let arguments = json!({ "query": "tool-editor", "index": url });

    // (a) both hosts allowed capsule-wide: the redirect is followed and the index read.
    let content = staging.run_one_call(arguments.clone(), &[http_rule(&a), http_rule(&b)], None);
    let data = fenced_json(&content);
    assert_eq!(data["index"], url, "(a) {content}");
    assert_eq!(data["scope"], "murmur", "(a) {content}");
    assert_eq!(data["count"], 1, "(a) {content}");
    assert_eq!(
        data["results"][0]["name"], "murmur/tool-editor",
        "(a) {content}"
    );
    assert_eq!(
        data["results"][0]["versions"],
        json!([
            {"version": "0.7.0", "release": "v0.26.0", "built_at_level": 1, "platforms": ["any"]},
            {"version": "0.6.0", "release": "v0.25.0", "built_at_level": 1, "platforms": ["any"]},
        ]),
        "(a) {content}"
    );
    assert_eq!(a.paths(), ["/latest"]);
    assert_eq!(b.paths(), ["/v1/artifacts-index.json"]);

    // (b) only A allowed: the hop to B is denied by the runtime, and the tool names B.
    let content = staging.run_one_call(arguments.clone(), &[http_rule(&a)], None);
    for needle in [
        format!("failed to fetch {url}: "),
        format!("{} is not in the network allow-list", b.authority()),
        "capabilities.network.allow".to_string(),
    ] {
        assert!(content.contains(&needle), "(b) {needle:?} not in {content}");
    }
    assert!(!content.contains("murmur/tool-editor"), "(b) {content}");
    assert_eq!(a.paths(), ["/latest", "/latest"]);
    assert_eq!(
        b.paths(),
        ["/v1/artifacts-index.json"],
        "(b) B is never reached"
    );

    // (c) both allowed capsule-wide, `allow: []` on the entry: the first request is denied.
    let content = staging.run_one_call(arguments, &[http_rule(&a), http_rule(&b)], Some(&[]));
    for needle in [
        format!("failed to fetch {url}: "),
        format!("{} is not in the network allow-list", a.authority()),
        "capabilities.network.allow".to_string(),
    ] {
        assert!(content.contains(&needle), "(c) {needle:?} not in {content}");
    }
    assert_eq!(a.paths(), ["/latest", "/latest"], "(c) A is never reached");
}
