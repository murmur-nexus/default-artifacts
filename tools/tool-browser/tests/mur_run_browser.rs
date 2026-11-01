//! `tool-browser` under a real `mur run`: the grant, the browser start inside the capsule's
//! containment, and the fence murmur puts around the result.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. `driver-openai` (wasm32-wasip2) and `tool-browser`
//! (native) are built out of this workspace on every run; `mur` comes from `MUR_BIN` or `PATH`,
//! and a run that finds neither fails rather than skipping. So does a host with no browser.
//!
//! The page is a file in the work folder rather than a loopback server: a native tool runs in
//! the session's network namespace, whose loopback is not the host's, so a server this test
//! started on the host's `127.0.0.1` is out of the browser's reach.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p tool-browser --test mur_run_browser -- --ignored
//! --nocapture` runs it.

#![cfg(target_os = "linux")]

mod common;

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

/// `capabilities.resources` as the README gives them.
const RESOURCES: &str = "  resources:\n    memory_bytes: 274877906944\n    cgroup_pids_max: 1024\n    cgroup_memory_bytes: 4294967296\n";

const NO_GRANT_LINE: &str = "tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow";

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

fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

/// Build and pack the artifact at `crate_dir`; returns its version and zip.
fn pack(crate_dir: &str, native: bool, out_dir: &Path) -> (String, PathBuf) {
    let crate_path = workspace_root().join(crate_dir);
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let name = manifest_field(&manifest, "name");
    let version = manifest_field(&manifest, "version");
    let mut build = Command::new(env!("CARGO"));
    build
        .current_dir(workspace_root())
        .args(["build", "-p", &name, "--release"]);
    if !native {
        build.args(["--target", "wasm32-wasip2"]);
    }
    assert!(
        build.status().unwrap().success(),
        "cargo build of {name} failed"
    );
    let payload = if native {
        target_dir().join("release").join(&name)
    } else {
        target_dir()
            .join("wasm32-wasip2/release")
            .join(format!("{}.wasm", name.replace('-', "_")))
    };
    assert!(
        payload.exists(),
        "{} not found after a successful build",
        payload.display()
    );
    let package = |dir: &Path| {
        Command::new("bash")
            .arg(workspace_root().join("scripts/package-artifact.sh"))
            .arg(dir)
            .arg(out_dir)
            .arg("--payload")
            .arg(&payload)
            .env("MUR_BIN", mur_binary())
            .output()
            .expect("failed to spawn scripts/package-artifact.sh")
    };
    let mut output = package(&crate_path);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() && stderr.contains("E-BLD-005") && stderr.contains(".vision") {
        // A `mur` older than the driver's `models.<id>.vision` key refuses to pack it. The key
        // only says which models read images, which this test does not use, so pack a copy
        // without it rather than require a newer `mur`.
        println!("note: this mur predates `models.<id>.vision`; packing {name} without it");
        let copy = out_dir.join("unvisioned").join(&name);
        fs::create_dir_all(&copy).unwrap();
        let stripped: String = manifest
            .lines()
            .filter(|l| !l.trim_start().starts_with("vision:"))
            .map(|l| format!("{l}\n"))
            .collect();
        fs::write(copy.join("murmur.yaml"), stripped).unwrap();
        output = package(&copy);
    }
    assert!(
        output.status.success(),
        "packing {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
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

/// A released `driver-openai` zip, for a `mur` older than the workspace driver's WIT: such a
/// `mur` refuses the workspace build at launch with `E-RUN-007` ("missing export").
fn driver_from_zip(zip: PathBuf) -> (String, PathBuf) {
    assert!(
        zip.is_file(),
        "TOOL_BROWSER_DRIVER_ZIP names {}, which is not a file",
        zip.display()
    );
    let name = zip.file_name().unwrap().to_string_lossy().into_owned();
    let version = name
        .strip_prefix("driver-openai-")
        .and_then(|v| v.strip_suffix(".mur.zip"))
        .unwrap_or_else(|| {
            panic!("TOOL_BROWSER_DRIVER_ZIP must name a driver-openai-<x.y.z>.mur.zip")
        })
        .to_string();
    println!("note: using the driver in {}", zip.display());
    (version, zip)
}

// ── staging ──────────────────────────────────────────────────────────────────

struct Staging {
    home: TempDir,
    project: TempDir,
    driver_version: String,
    tool_version: String,
    /// The `PATH` `mur run` starts with: the test browser's directory first.
    path: String,
    /// The name the capsule grants, which resolves to the test browser on `path`.
    browser_name: String,
    _artifacts: TempDir,
    _bin: TempDir,
}

impl Staging {
    fn manifest(&self, name: &str) -> PathBuf {
        self.project.path().join(format!("{name}.yaml"))
    }

    fn workdir(&self, name: &str) -> PathBuf {
        self.project.path().join(format!("work-{name}"))
    }

    fn mur(&self) -> Command {
        let mut command = Command::new(mur_binary());
        command
            .env("HOME", self.home.path())
            .env("PATH", &self.path);
        command
    }

    /// A capsule manifest; `capabilities` is the YAML under `capabilities:`.
    fn write_manifest(&self, name: &str, endpoint: &str, capabilities: &str) {
        fs::write(
            self.manifest(name),
            format!(
                "name: {name}\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/tool-browser\n    version: {}\n    runtime: tool\n\
                 capabilities:\n{capabilities}\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version, self.tool_version
            ),
        )
        .unwrap();
    }

    fn run(&self, name: &str, extra: &[&str]) -> Output {
        let mut command = self.mur();
        command.current_dir(self.project.path()).args([
            "run",
            "--manifest",
            self.manifest(name).to_str().unwrap(),
            "--task",
            "open page.html and report its console errors",
            "--workdir",
            self.workdir(name).to_str().unwrap(),
        ]);
        command.args(extra);
        command.output().expect("spawning `mur run`")
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = match std::env::var_os("TOOL_BROWSER_DRIVER_ZIP") {
        Some(zip) => driver_from_zip(PathBuf::from(zip)),
        None => pack("drivers/driver-openai", false, artifacts.path()),
    };
    let (tool_version, tool_zip) = pack("tools/tool-browser", true, artifacts.path());

    // The capsule's PATH puts the test browser first under the name it is found by.
    let browser = common::test_browser();
    let bin = tempfile::tempdir().unwrap();
    let browser_name = common::name_on_path(&browser).unwrap_or_else(|| {
        std::os::unix::fs::symlink(&browser, bin.path().join("chromium")).unwrap();
        "chromium".to_string()
    });
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        tool_version,
        path,
        browser_name,
        _artifacts: artifacts,
        _bin: bin,
    };
    // `mur install` installs into the project whose murmur.yaml it finds.
    staging.write_manifest("murmur", "http://127.0.0.1:1/v1", "");
    for (zip, what) in [(&driver_zip, "driver-openai"), (&tool_zip, "tool-browser")] {
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

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ── the fake provider ────────────────────────────────────────────────────────

fn scripted_calls() -> Vec<Value> {
    vec![
        json!({"action": "open", "url": "page.html"}),
        json!({"action": "console"}),
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
        "function": {"name": "tool-browser", "arguments": arguments.to_string()}
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

/// A loopback Chat Completions server. A request carrying N tool results gets the N-th scripted
/// call of `script`, or text once it is spent.
struct Provider {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Provider {
    fn start(script: Vec<Value>) -> Self {
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

fn result_of<'a>(results: &'a [(String, String)], id: &str) -> &'a str {
    results
        .iter()
        .find(|(call, _)| call == id)
        .map(|(_, content)| content.as_str())
        .unwrap_or_else(|| panic!("no tool result for {id} in {results:?}"))
}

// ── the scenario ─────────────────────────────────────────────────────────────

const FENCE_OPEN: &str = "<untrusted-content source=tool:tool-browser>";

/// A granted capsule opens a work-folder page inside its containment, reads `boom-load` from
/// the console through the fence, and saves a screenshot; a capsule without the grant gets the
/// one no-grant line, fenced.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn granted_capsule_reads_console_through_the_fence_and_ungranted_gets_one_line() {
    let staging = stage();
    // The grants the README gives for a browser binary on the Full and Sealed tiers: its own
    // directory (resources, ICU data), /proc, and the fonts.
    let browser_dir = common::test_browser()
        .canonicalize()
        .unwrap()
        .parent()
        .unwrap()
        .display()
        .to_string();
    let dirs: String = [
        browser_dir.as_str(),
        "/proc",
        "/etc/fonts",
        "/usr/share/fonts",
    ]
    .iter()
    .map(|d| format!("          - path: {d}\n            list_dir: true\n"))
    .collect();
    let granted = format!(
        "  shell:\n    allow:\n      - {name}\n    interpreter_runtime:\n      - binary: {name}\n        dirs:\n{dirs}{RESOURCES}",
        name = staging.browser_name
    );
    let provider = Provider::start(scripted_calls());
    staging.write_manifest("granted", &provider.endpoint, &granted);
    let workdir = staging.workdir("granted");
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("page.html"), common::FIXTURE_PAGE).unwrap();

    let scope = staging.run("granted", &["--explain-scope"]);
    let scope_text = combined(&scope);
    let tier = scope_text
        .lines()
        .find(|l| l.to_ascii_lowercase().contains("achieved"))
        .unwrap_or("(no achieved line)")
        .trim()
        .to_string();
    println!("--- mur run --explain-scope ---\n{scope_text}");
    println!("achieved tier: {tier}");

    let output = staging.run("granted", &[]);
    let text = combined(&output);
    println!("--- granted run ---\n{text}");
    assert!(
        !text.contains("missing export murmur:tool/run"),
        "this mur is older than the workspace driver's WIT: set TOOL_BROWSER_DRIVER_ZIP to a \
         driver-openai zip built for it (e.g. ~/.murmur/artifacts/murmur/driver-openai/<v>/...)"
    );
    let results = tool_results(&provider.last_request());
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    assert!(
        output.status.success(),
        "the granted session must succeed:\n{text}"
    );
    let opened = result_of(&results, "call_1");
    assert!(
        opened.contains("page: Fixture Page"),
        "open returns the page: {opened}"
    );
    let console = result_of(&results, "call_2");
    assert!(
        console.contains(FENCE_OPEN),
        "the console result is fenced: {console}"
    );
    let inside = &console[console.find(FENCE_OPEN).unwrap() + FENCE_OPEN.len()..];
    assert!(
        inside.contains("[error] boom-load"),
        "the fence wraps the boom-load console error: {console}"
    );
    let shot = result_of(&results, "call_3");
    assert!(
        shot.contains("saved .tool-browser/"),
        "the screenshot is saved: {shot}"
    );
    assert!(
        fs::read_dir(workdir.join(".tool-browser"))
            .unwrap()
            .flatten()
            .any(|session| {
                fs::read_dir(session.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|f| f.file_name().to_string_lossy().ends_with(".png"))
            }),
        "a screenshot PNG is in the work folder"
    );

    // Without the grant: exactly the one line, fenced, and nothing in the work folder.
    let provider = Provider::start(vec![json!({"action": "open", "url": "page.html"})]);
    staging.write_manifest("ungranted", &provider.endpoint, RESOURCES);
    let workdir = staging.workdir("ungranted");
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("page.html"), common::FIXTURE_PAGE).unwrap();
    let output = staging.run("ungranted", &[]);
    let text = combined(&output);
    println!("--- ungranted run ---\n{text}");
    let results = tool_results(&provider.last_request());
    let refused = result_of(&results, "call_1");
    println!("call_1: {refused}");
    assert!(
        refused.contains(FENCE_OPEN),
        "the refusal is fenced: {refused}"
    );
    let inside = &refused[refused.find(FENCE_OPEN).unwrap() + FENCE_OPEN.len()..];
    let inside = inside
        .split("</untrusted-content>")
        .next()
        .unwrap_or("")
        .trim();
    assert!(
        inside.ends_with(NO_GRANT_LINE) && inside.matches(NO_GRANT_LINE).count() >= 1,
        "the fence holds the no-grant line: {refused}"
    );
    assert!(
        !workdir.join(".tool-browser").exists(),
        "nothing is created without the grant"
    );
}
