//! `tool-browser` under a real `mur run`: the grant, Chrome started as a session process inside
//! the capsule's containment on the `scoped` and `sealed` tiers, the fence murmur puts around
//! every result, the screenshot reaching the model, an `https` page, and nothing left running
//! after the session.
//!
//! The capsule grants `chromium` and names no host path: what Chrome needs from the host comes
//! from the `programs:` block of tool-browser's own `murmur.yaml`, which murmur grants because the
//! capsule pins the tool with `mur install` (an `operator` lock origin) and grants the name. The
//! session's `session_start.effective_grants.declared_needs` must show it. A second staging packs
//! a copy of the tool whose `programs:` withholds NSS's modules, and its first `https` page kills
//! Chrome.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. `driver-openai` and `tool-browser` (both
//! wasm32-wasip2) are built out of this workspace and packed by `scripts/package-artifact.sh`
//! on every run. `mur` comes from `MUR_BIN` and must report WIT level 4 or higher, the first
//! with `murmur:process`, and must read `programs.<program>.download` (murmur `b0d4c42e` or
//! later); a run without one fails rather than skipping.
//!
//! The main test runs one copy of `chromium` after another:
//!
//! | Phase | `PATH` | The copy that runs |
//! |---|---|---|
//! | pinned | the inherited `PATH` without any directory holding a `chromium` | the `chrome-headless-shell` `mur install` fetched for `programs.chromium.download.linux-x86_64` |
//! | host | `TOOL_BROWSER_TEST_CHROME`, linked as `chromium`, first | that binary |
//! | store removed | as for pinned, with the store entry deleted | none: the call answers the not-installed line |
//!
//! The pinned copy is about 128 MB from `storage.googleapis.com`, fetched once per run into
//! `<scratch HOME>/.murmur/programs/<sha256>/`, where murmur grants it though it lies beneath the
//! home folder. When `storage.googleapis.com:443` does not answer within 5 s, the test prints one
//! line saying the pinned phases are skipped and why, accepts `mur install` failing with
//! `E-REG-013`, and runs only the host phase and the ungranted session.
//!
//! The host browser is `TOOL_BROWSER_TEST_CHROME`, which must be `chrome-headless-shell`: full
//! Chrome and Chromium cannot start on the scoped and sealed tiers. The test links it as
//! `chromium` in its own directory (the sealed tier's composed root binds only the resolved
//! binary's directory) and puts that directory first on `PATH`. It must lie outside the scratch
//! `HOME` the test gives `mur`, because murmur grants no declared need of a host program beneath
//! the home folder.
//!
//! The fixture page is a file in the work folder rather than a loopback server: Chrome runs in
//! the session's network namespace, whose loopback is not the host's. The `https` page is
//! `https://example.com/`; when the test process cannot reach it, the `https` steps are skipped
//! with one line saying why.
//!
//! The granted capsules take their capabilities from the README's working manifest, copied as is
//! but for the tier, so the run proves that manifest. On `scoped`, the NSS-withheld copy answers
//! its `https` page with the line a model gets: Chrome's exit during `Page.navigate` and the NSS
//! hint. That copy pins no download, so it runs the host browser and fetches nothing.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=<mur at murmur b0d4c42e or later> TOOL_BROWSER_TEST_CHROME=<chrome-headless-shell>
//! cargo test -p tool-browser --test mur_run_browser -- --ignored --nocapture` runs it.

#![cfg(target_os = "linux")]

mod common;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Duration,
};

use serde_json::{json, Value};
use tempfile::TempDir;

/// `capabilities.resources` as the README gives them.
const RESOURCES: &str = "  resources:\n    memory_bytes: 274877906944\n    cgroup_pids_max: 1024\n    cgroup_memory_bytes: 4294967296\n";

const NO_GRANT_LINE: &str = "tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow";

/// The answer when `chromium` is granted and neither the host nor the program store has a copy.
const NOT_INSTALLED_LINE: &str = "tool-browser: capabilities.shell.allow grants chromium, but no chromium is on PATH and no pinned copy is in place; run mur install in the capsule's project to fetch the chrome-headless-shell tool-browser pins for chromium, or install Chrome or Chromium on this host; mur doctor names what is missing";

/// The host that serves the pinned `chrome-headless-shell`.
const DOWNLOAD_HOST: &str = "storage.googleapis.com";

/// Where the pinned zip unpacks the file `bin/chromium` links to, inside a store entry.
const PINNED_RUN: &str = "files/chrome-headless-shell-linux64/chrome-headless-shell";

/// The `https` page every `https` step opens, and the host `network.allow` grants.
const HTTPS_PAGE: &str = "https://example.com/";

/// The `programs:` needs the NSS-withheld copy drops: NSS's modules and their `.chk` files.
const NSS_NEEDS: [&str; 11] = [
    "library: libsoftokn3.so",
    "file: libsoftokn3.chk",
    "library: libfreebl3.so",
    "file: libfreebl3.chk",
    "library: libfreeblpriv3.so",
    "file: libfreeblpriv3.chk",
    "library: libnssdbm3.so",
    "file: libnssdbm3.chk",
    "library: libnssckbi.so",
    "library: libnss3.so",
    "library: libnssutil3.so",
];

/// Both tests start Chrome and check that no browser is left afterwards, which is a host-wide
/// look, so they take turns.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── the runtime under test ───────────────────────────────────────────────────

fn mur_binary() -> PathBuf {
    let path = PathBuf::from(std::env::var_os("MUR_BIN").unwrap_or_else(|| {
        panic!(
            "set MUR_BIN to a mur at murmur b0d4c42e or later, whose `mur version` reports WIT \
             level 4 or higher. This test \
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
        "MUR_BIN must report WIT level 4 or higher (murmur:process), at murmur b0d4c42e or later \
         (programs.<program>.download); it reports:\n{text}"
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

// ── building and packing ─────────────────────────────────────────────────────

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

/// The `PATH` with `mur`'s directory first, so `scripts/package-artifact.sh` packs with it.
fn path_with_mur() -> String {
    format!(
        "{}:{}",
        mur_binary().parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    )
}

/// Build the wasm artifact whose `murmur.yaml` is in `crate_dir` (relative to the workspace, or
/// absolute) and pack it; returns its version and zip.
fn pack(crate_dir: &Path, out_dir: &Path) -> (String, PathBuf) {
    let crate_path = workspace_root().join(crate_dir);
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let name = manifest_field(&manifest, "name");
    let version = manifest_field(&manifest, "version");
    let built = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .args([
            "build",
            "-p",
            &name,
            "--release",
            "--target",
            "wasm32-wasip2",
        ])
        .status()
        .unwrap();
    assert!(built.success(), "cargo build of {name} failed");
    let output = Command::new("bash")
        .current_dir(workspace_root())
        .arg(workspace_root().join("scripts/package-artifact.sh"))
        .arg(crate_dir)
        .arg(out_dir)
        .env("MUR_BIN", mur_binary())
        .env("PATH", path_with_mur())
        .output()
        .expect("failed to spawn scripts/package-artifact.sh");
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

// ── staging ──────────────────────────────────────────────────────────────────

/// The headless shell, linked as `chromium` in its own directory; returns that directory.
fn browser_dir() -> PathBuf {
    let explicit = std::env::var_os("TOOL_BROWSER_TEST_CHROME").unwrap_or_else(|| {
        panic!("set TOOL_BROWSER_TEST_CHROME to a chrome-headless-shell binary")
    });
    let binary = PathBuf::from(explicit).canonicalize().unwrap();
    assert!(
        binary
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains("headless-shell")),
        "TOOL_BROWSER_TEST_CHROME must be chrome-headless-shell: full Chrome and Chromium cannot \
         start on the scoped and sealed tiers ({})",
        binary.display()
    );
    let dir = binary.parent().unwrap().to_path_buf();
    let link = dir.join("chromium");
    if !link.exists() {
        std::os::unix::fs::symlink(binary.file_name().unwrap(), &link).unwrap_or_else(|e| {
            panic!(
                "cannot link {} as chromium: {e}; link it by hand",
                binary.display()
            )
        });
    }
    dir
}

struct Staging {
    home: TempDir,
    project: TempDir,
    driver_version: String,
    tool_version: String,
    /// The `PATH` `mur run` starts with: the browser's directory first.
    path: String,
    browser_dir: PathBuf,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self, name: &str) -> PathBuf {
        self.project.path().join(format!("{name}.yaml"))
    }

    fn workdir(&self, name: &str) -> PathBuf {
        self.project.path().join(format!("work-{name}"))
    }

    fn mur(&self) -> Command {
        self.mur_on(&self.path)
    }

    /// `mur` with the scratch `HOME` and `path` as its `PATH`.
    fn mur_on(&self, path: &str) -> Command {
        let mut command = Command::new(mur_binary());
        command.env("HOME", self.home.path()).env("PATH", path);
        command
    }

    /// `mur <args>` in the project, on `path`.
    fn mur_in_project(&self, path: &str, args: &[&str]) -> Output {
        let mut command = self.mur_on(path);
        command.current_dir(self.project.path()).args(args);
        command.output().expect("spawning `mur`")
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
                 trace:\n  capture: content\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version, self.tool_version
            ),
        )
        .unwrap();
    }

    fn run(&self, name: &str, extra: &[&str]) -> Output {
        self.run_on(&self.path, name, extra)
    }

    fn run_on(&self, path: &str, name: &str, extra: &[&str]) -> Output {
        let mut command = self.mur_on(path);
        command.current_dir(self.project.path()).args([
            "run",
            "--manifest",
            self.manifest(name).to_str().unwrap(),
            "--task",
            "open page.html, read its console and take a screenshot",
            "--workdir",
            self.workdir(name).to_str().unwrap(),
        ]);
        command.args(extra);
        command.output().expect("spawning `mur run`")
    }

    /// The trace lines of every session this staging ran with `workdir`.
    fn trace(&self, workdir: &Path) -> Vec<Value> {
        let mut lines = Vec::new();
        let mut stack = vec![self.home.path().join(".murmur")];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().is_some_and(|n| n == "trace.jsonl") {
                    let text = fs::read_to_string(&p).unwrap_or_default();
                    let parsed: Vec<Value> = text
                        .lines()
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    if text.contains(&*workdir.to_string_lossy())
                        || p.to_string_lossy()
                            .contains(&*workdir.file_name().unwrap().to_string_lossy())
                    {
                        lines.extend(parsed);
                    }
                }
            }
        }
        lines
    }
}

/// A scratch `HOME` and project with `driver-openai` and the tool in `tool_dir` installed with
/// `mur install`, so the tool's lock origin is `operator`. Returns the staging and the output of
/// the tool's `mur install`, which the caller judges: it fetches the tool's pinned downloads.
fn stage(tool_dir: &Path) -> (Staging, Output) {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack(Path::new("drivers/driver-openai"), artifacts.path());
    let (tool_version, tool_zip) = pack(tool_dir, artifacts.path());
    let browser_dir = browser_dir();
    let path = format!(
        "{}:{}",
        browser_dir.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        tool_version,
        path,
        browser_dir,
        _artifacts: artifacts,
    };
    // `mur install` installs into the project whose murmur.yaml it finds.
    staging.write_manifest("murmur", "http://127.0.0.1:1/v1", "");
    for (zip, what) in [(&driver_zip, "driver-openai"), (&tool_zip, "tool-browser")] {
        let mut publish = staging.mur();
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {what}"));
    }
    let mut install = staging.mur();
    install
        .current_dir(staging.project.path())
        .arg("install")
        .arg(&driver_zip);
    run_to_success(install, "installing driver-openai");
    let tool_install =
        staging.mur_in_project(&staging.path, &["install", tool_zip.to_str().unwrap()]);
    (staging, tool_install)
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ── the fake provider ────────────────────────────────────────────────────────

/// open → console → screenshot, then the `https` page when `https` is set.
fn scripted_calls(https: bool) -> Vec<Value> {
    let mut calls = vec![
        json!({"action": "open", "url": "page.html"}),
        json!({"action": "console"}),
        json!({"action": "screenshot"}),
    ];
    if https {
        calls.push(json!({"action": "open", "url": HTTPS_PAGE}));
    }
    calls
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

const README_HEADING: &str = "### A working manifest on `scoped` and `sealed`";
const HTTPS_PAGE_LINE: &str = "page: Example Domain — https://example.com/";

/// The README's working manifest for `tier`: the lines under its `capabilities:`, with only the
/// `containment:` value replaced. It grants `chromium`, the `https` host and the resources, and
/// names no host path: tool-browser's `programs:` carries the rest.
fn granted_capabilities(tier: &str) -> String {
    let yaml = common::readme_yaml(README_HEADING);
    let block: Vec<&str> = yaml
        .lines()
        .skip_while(|l| *l != "capabilities:")
        .skip(1)
        .take_while(|l| l.is_empty() || l.starts_with(' '))
        .collect();
    assert!(
        !block.is_empty(),
        "the README's working manifest has a capabilities: block: {yaml}"
    );
    let mut containment = 0;
    let caps: String = block
        .iter()
        .map(|l| {
            if l.trim_start().starts_with("containment:") {
                containment += 1;
                format!("  containment: {tier}\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    assert_eq!(
        containment, 1,
        "the README's manifest has one containment: line"
    );
    caps
}

/// The trace's `event` lines (`process_start`, `process_exit`) for the process `browser`.
fn browser_events<'t>(trace: &'t [Value], event: &str) -> Vec<&'t Value> {
    trace
        .iter()
        .filter(|l| l["event_type"] == event && l["name"] == "browser")
        .collect()
}

/// Every `browser.log` in the work folder, joined.
fn browser_logs(workdir: &Path) -> String {
    fs::read_dir(workdir.join(".tool-browser"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("browser.log")).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `effective_grants.declared_needs` entry of `local/tool-browser` for `chromium` in the
/// session's `session_start` line.
fn declared_needs_entry(trace: &[Value]) -> Value {
    let start = trace
        .iter()
        .find(|l| l["event_type"] == "session_start")
        .expect("a session_start line");
    let needs = start["effective_grants"]["declared_needs"]
        .as_array()
        .unwrap_or_else(|| panic!("session_start.effective_grants.declared_needs: {start}"));
    needs
        .iter()
        .find(|e| e["artifact"] == "local/tool-browser" && e["program"] == "chromium")
        .cloned()
        .unwrap_or_else(|| {
            panic!("no declared_needs entry for local/tool-browser (chromium): {needs:?}")
        })
}

/// The paths of the granted need labelled `need` in a `declared_needs` entry.
fn granted_paths(entry: &Value, need: &str) -> Vec<String> {
    entry["granted"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|g| g["need"] == need)
        .unwrap_or_else(|| panic!("no granted `{need}` in {entry}"))["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Print the tail of every `browser.log`; with `TOOL_BROWSER_KEEP_LOGS=<dir>`, copy each there.
fn print_browser_logs(workdir: &Path) {
    let keep = std::env::var_os("TOOL_BROWSER_KEEP_LOGS").map(PathBuf::from);
    for e in fs::read_dir(workdir.join(".tool-browser"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Some(keep) = &keep {
            let _ = fs::create_dir_all(keep);
            let _ = fs::copy(
                e.path().join("browser.log"),
                keep.join(format!(
                    "{}-{}.log",
                    workdir.file_name().unwrap().to_string_lossy(),
                    e.file_name().to_string_lossy()
                )),
            );
        }
        if let Ok(log) = fs::read_to_string(e.path().join("browser.log")) {
            let tail: Vec<&str> = log.lines().rev().take(60).collect();
            println!(
                "--- {}/browser.log (last {} lines) ---",
                e.path().display(),
                tail.len()
            );
            for line in tail.into_iter().rev() {
                println!("{line}");
            }
        }
    }
}

/// The `chromium` a granted session expects to run.
enum Chromium<'a> {
    /// The program store's copy of `programs.chromium.download.linux-x86_64`.
    Pinned(&'a Pin),
    /// `TOOL_BROWSER_TEST_CHROME`, linked as `chromium` first on `PATH`.
    Host,
}

impl Chromium<'_> {
    fn label(&self) -> &'static str {
        match self {
            Chromium::Pinned(_) => "pinned",
            Chromium::Host => "host",
        }
    }
}

/// tool-browser's `programs.chromium.download.linux-x86_64`, and its store entry once installed.
struct Pin {
    url: String,
    sha256: String,
    /// The Chrome for Testing build in `url`.
    build: String,
    /// `<scratch HOME>/.murmur/programs/<sha256>`, canonical.
    entry: PathBuf,
}

impl Pin {
    /// The file `bin/chromium` links to.
    fn run_file(&self) -> PathBuf {
        self.entry.join(PINNED_RUN)
    }
}

/// `programs.chromium.download.linux-x86_64`'s `url`, `sha256` and build, from tool-browser's
/// `murmur.yaml`.
fn pinned_download() -> (String, String, String) {
    let manifest =
        fs::read_to_string(workspace_root().join("tools/tool-browser/murmur.yaml")).unwrap();
    let doc: serde_norway::Value = serde_norway::from_str(&manifest).unwrap();
    let entry = &doc["programs"]["chromium"]["download"]["linux-x86_64"];
    let url = entry["url"].as_str().expect("a linux-x86_64 url").to_string();
    let sha256 = entry["sha256"]
        .as_str()
        .expect("a linux-x86_64 sha256")
        .to_string();
    let build = url
        .strip_prefix("https://storage.googleapis.com/chrome-for-testing-public/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_else(|| panic!("no Chrome for Testing build in {url}"))
        .to_string();
    (url, sha256, build)
}

/// The one store entry under `<home>/.murmur/programs/`: the directory named by 64 hex digits.
fn store_entry(home: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = fs::read_dir(home.join(".murmur/programs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "one store entry under {}/.murmur/programs: {entries:?}",
        home.display()
    );
    entries[0].canonicalize().unwrap()
}

/// The inherited `PATH` without any directory that holds an executable `chromium`.
fn path_without_chromium() -> String {
    let inherited = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let kept: Vec<&str> = inherited
        .split(':')
        .filter(|dir| !dir.is_empty() && !is_executable(&Path::new(dir).join("chromium")))
        .collect();
    let path = kept.join(":");
    for dir in path.split(':') {
        assert!(
            !Path::new(dir).join("chromium").exists(),
            "chromium resolves in {dir}, on the pinned phase's PATH"
        );
    }
    path
}

/// The `--explain-scope` line under `pinned programs:` for `local/tool-browser (chromium)`,
/// without its `- `.
fn pinned_programs_line(scope_text: &str) -> String {
    scope_text
        .lines()
        .map(|l| l.trim().trim_start_matches("- "))
        .find(|l| {
            ["pinned ", "host ", "missing "]
                .iter()
                .any(|source| l.starts_with(&format!("local/tool-browser (chromium): {source}")))
        })
        .unwrap_or_else(|| {
            panic!("no pinned programs: line for local/tool-browser (chromium):\n{scope_text}")
        })
        .to_string()
}

/// The `effective_grants.pinned_programs` entry of `local/tool-browser` for `chromium` in the
/// session's `session_start` line.
fn pinned_programs_entry(trace: &[Value]) -> Value {
    let start = trace
        .iter()
        .find(|l| l["event_type"] == "session_start")
        .expect("a session_start line");
    let pinned = start["effective_grants"]["pinned_programs"]
        .as_array()
        .unwrap_or_else(|| panic!("session_start.effective_grants.pinned_programs: {start}"));
    pinned
        .iter()
        .find(|e| e["artifact"] == "local/tool-browser" && e["program"] == "chromium")
        .cloned()
        .unwrap_or_else(|| {
            panic!("no pinned_programs entry for local/tool-browser (chromium): {pinned:?}")
        })
}

/// `mur doctor` in the project, with `capabilities` in its `murmur.yaml`, on `path`.
fn doctor(staging: &Staging, path: &str, capabilities: &str) -> String {
    staging.write_manifest("murmur", "http://127.0.0.1:1/v1", capabilities);
    combined(&staging.mur_in_project(path, &["doctor"]))
}

/// One granted session on `tier`, on `path`, with `copy` as the `chromium` that runs: open →
/// console → screenshot, all inside the fence, the image in the next request, then the `https`
/// page when `https` is set; which copy runs in `--explain-scope` and in `session_start`,
/// Chrome's declared needs resolved against that copy, its GPU process up without `/proc` named,
/// and the browser a session process murmur kills at the session's end.
fn granted_session(staging: &Staging, tier: &str, https: bool, path: &str, copy: &Chromium) {
    let name = format!("{}-{tier}", copy.label());
    let provider = Provider::start(scripted_calls(https));
    staging.write_manifest(&name, &provider.endpoint, &granted_capabilities(tier));
    let workdir = staging.workdir(&name);
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("page.html"), common::FIXTURE_PAGE).unwrap();

    let scope = staging.run_on(path, &name, &["--explain-scope"]);
    let scope_text = combined(&scope);
    let achieved: Vec<&str> = scope_text
        .lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("achieved") || l.contains("tier")
        })
        .collect();
    println!("--- {tier}: mur run --explain-scope (tier lines) ---");
    for line in &achieved {
        println!("{line}");
    }
    let declared: Vec<&str> = scope_text
        .lines()
        .filter(|l| l.contains("local/tool-browser (chromium):") && l.contains(" -> "))
        .collect();
    println!("--- {tier}: mur run --explain-scope (declared needs) ---");
    for line in &declared {
        println!("{line}");
    }
    assert!(
        declared.iter().any(|l| l.contains("program_folder . -> ")),
        "--explain-scope lists tool-browser's declared needs on {tier}:\n{scope_text}"
    );
    assert!(
        !declared.iter().any(|l| l.contains("-> missing")),
        "no declared need is missing on {tier}:\n{scope_text}"
    );
    let pinned_line = pinned_programs_line(&scope_text);
    println!("--- {tier}: mur run --explain-scope (pinned programs) ---\n- {pinned_line}");
    let want_folder = match copy {
        Chromium::Pinned(pin) => {
            assert_eq!(
                pinned_line,
                format!(
                    "local/tool-browser (chromium): pinned {} (sha256 {}, from {})",
                    pin.run_file().display(),
                    pin.sha256,
                    pin.url
                ),
                "the store's copy runs on {tier}"
            );
            let folder = pin.run_file().parent().unwrap().to_path_buf();
            assert!(
                declared.iter().any(|l| l
                    .contains(&format!("program_folder . -> {}", folder.display()))),
                "program_folder . resolves into the store on {tier}:\n{scope_text}"
            );
            folder
        }
        Chromium::Host => {
            let host = pinned_line
                .strip_prefix("local/tool-browser (chromium): host ")
                .unwrap_or_else(|| panic!("the host's chromium runs on {tier}: {pinned_line}"));
            let want = PathBuf::from(std::env::var_os("TOOL_BROWSER_TEST_CHROME").unwrap())
                .canonicalize()
                .unwrap();
            assert_eq!(
                Path::new(host).canonicalize().unwrap(),
                want,
                "the host copy is TOOL_BROWSER_TEST_CHROME on {tier}"
            );
            staging.browser_dir.canonicalize().unwrap()
        }
    };

    let output = staging.run_on(path, &name, &[]);
    let text = combined(&output);
    println!("--- {tier}: granted run ---\n{text}");
    let results = tool_results(&provider.last_request());
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    print_browser_logs(&workdir);
    assert!(
        output.status.success(),
        "the granted session on {tier} must succeed:\n{text}"
    );
    let opened = result_of(&results, "call_1");
    assert!(opened.contains(FENCE_OPEN), "open is fenced: {opened}");
    assert!(
        opened.contains("page: Fixture Page — file://"),
        "open returns the page on {tier}: {opened}"
    );
    assert!(
        opened.contains("browser: chromium ")
            && opened.contains(", local, headless; Chrome sandbox "),
        "open carries the banner: {opened}"
    );
    if let Chromium::Pinned(pin) = copy {
        assert!(
            opened.contains(&format!("browser: chromium {}, local, headless", pin.build)),
            "the banner names the pinned build {}: {opened}",
            pin.build
        );
    }
    let console = result_of(&results, "call_2");
    assert!(console.contains(FENCE_OPEN), "console is fenced: {console}");
    let inside = &console[console.find(FENCE_OPEN).unwrap() + FENCE_OPEN.len()..];
    assert!(
        inside.contains("[error] boom-load"),
        "the fence wraps the boom-load console error: {console}"
    );
    let shot = result_of(&results, "call_3");
    assert!(
        shot.contains("saved .tool-browser/") && shot.contains("(1280x800, "),
        "the screenshot is saved: {shot}"
    );
    let last = provider.last_request();
    assert!(
        last.contains("data:image/png;base64,"),
        "the request after the screenshot carries the image"
    );
    if https {
        let page = result_of(&results, "call_4");
        assert!(
            page.contains(FENCE_OPEN),
            "the https page is fenced: {page}"
        );
        assert!(
            page.lines().any(|l| l == HTTPS_PAGE_LINE),
            "the https page opens on {tier}: {page}"
        );
    }
    let logs = browser_logs(&workdir);
    assert!(
        !logs.contains("GPU process launch failed: error_code=1002"),
        "the GPU process starts on {tier} without /proc named in the capsule"
    );

    let trace = staging.trace(&workdir);
    let entry = declared_needs_entry(&trace);
    println!("--- {tier}: session_start.effective_grants.declared_needs (tool-browser, chromium) ---\n{entry}");
    let folder = granted_paths(&entry, "program_folder .");
    assert_eq!(
        folder
            .iter()
            .map(|p| PathBuf::from(p.trim_end_matches(" (list_dir)")))
            .collect::<Vec<_>>(),
        vec![want_folder],
        "program_folder . is the running copy's own folder on {tier}"
    );
    let pinned = pinned_programs_entry(&trace);
    println!("--- {tier}: session_start.effective_grants.pinned_programs (tool-browser, chromium) ---\n{pinned}");
    assert_eq!(pinned["platform"], "linux-x86_64", "{pinned}");
    assert_eq!(
        pinned["source"],
        copy.label(),
        "the trace records which copy ran on {tier}: {pinned}"
    );
    assert!(
        granted_paths(&entry, "library libsoftokn3.so")
            .iter()
            .any(|p| p.contains("libsoftokn3.so")),
        "libsoftokn3.so is granted on {tier}: {entry}"
    );
    assert_eq!(
        entry["missing"],
        json!([]),
        "no declared need is missing on {tier}: {entry}"
    );
    let starts = browser_events(&trace, "process_start");
    // Two when Chrome's own sandbox could not start and it ran again with --no-sandbox.
    assert!(
        (1..=2).contains(&starts.len()),
        "a process_start for browser on {tier}: {starts:?}"
    );
    for start in &starts {
        println!("process_start: {start}");
    }
    let exits = browser_events(&trace, "process_exit");
    // Only the start that could not use Chrome's own sandbox ends before the session: no browser
    // died during a call, the https page's included.
    for exit in exits.iter().filter(|l| l["reason"] != "session_end") {
        let start = starts
            .iter()
            .find(|s| s["process_id"] == exit["process_id"])
            .unwrap_or_else(|| panic!("a start for {exit}"));
        assert!(
            !start.to_string().contains("--no-sandbox"),
            "only the sandboxed first start exits early on {tier}: {exit}"
        );
    }
    assert!(
        exits.iter().any(|l| l["reason"] == "session_end"),
        "process_exit with reason session_end on {tier}: {exits:?}"
    );
    for exit in &exits {
        println!("process_exit: {exit}");
    }
    assert_eq!(
        exits
            .iter()
            .filter(|l| l["reason"] == "session_end")
            .count(),
        1,
        "exactly one browser ran until the session's end on {tier}"
    );
    let refused: Vec<&Value> = trace
        .iter()
        .filter(|l| l["event_type"] == "process_refused")
        .collect();
    assert!(refused.is_empty(), "no process_refused: {refused:?}");
    let attachments: Vec<&Value> = trace
        .iter()
        .filter(|l| l.to_string().contains("screenshot-1.png"))
        .collect();
    println!("trace lines naming screenshot-1.png: {}", attachments.len());

    let left =
        murmur_process_test_host::pids_mentioning(&format!("{}/.tool-browser/", workdir.display()));
    let rel_left = murmur_process_test_host::pids_mentioning("--user-data-dir=.tool-browser/");
    assert!(
        left.is_empty() && rel_left.is_empty(),
        "no process names .tool-browser/ after mur run exits on {tier}: {left:?} {rel_left:?}"
    );
}

/// The capsule in the README, granting only `chromium`: `mur install` fetches the pinned
/// `chrome-headless-shell` once, which runs on `scoped` and `sealed` when no `chromium` is on
/// `PATH`; a `chromium` on `PATH` runs instead; with the store entry gone the call answers the
/// not-installed line and `mur doctor` warns; and a capsule without the grant gets the one
/// no-grant line, fenced.
#[test]
#[ignore = "launches the real mur binary and fetches about 128 MB; run with --ignored"]
fn pinned_copy_runs_without_a_browser_on_path_and_host_chromium_wins() {
    let _turn = one_at_a_time();
    let https = match common::reachable("example.com") {
        Ok(()) => true,
        Err(why) => {
            println!("skipping the https step: {why}");
            false
        }
    };
    let online = match common::reachable(DOWNLOAD_HOST) {
        Ok(()) => true,
        Err(why) => {
            println!(
                "skipping the pinned phases (install fetch, pinned copy, store removed): {why}"
            );
            false
        }
    };
    let (url, sha256, build) = pinned_download();
    let (staging, install) = stage(Path::new("tools/tool-browser"));
    let installed = combined(&install);
    println!("--- mur install <tool-browser zip> ---\n{installed}");

    // Phase 1: the install fetches chromium's download for this platform, and nothing else.
    let pin = if online {
        assert!(
            install.status.success(),
            "mur install of tool-browser succeeds:\n{installed}"
        );
        let fetched: Vec<&str> = installed
            .lines()
            .filter(|l| l.trim_start().starts_with("fetched "))
            .collect();
        assert_eq!(
            fetched.len(),
            1,
            "mur install fetches one program; a mur before murmur b0d4c42e fetches none:\n{installed}"
        );
        assert!(
            fetched[0].contains("fetched chromium for linux-x86_64 (local/tool-browser): ")
                && fetched[0].ends_with("sha256 verified"),
            "the fetch line names chromium: {}",
            fetched[0]
        );
        let again = staging.mur_in_project(&staging.path, &["install"]);
        let again_text = combined(&again);
        println!("--- mur install (again) ---\n{again_text}");
        assert!(again.status.success(), "the second mur install succeeds");
        assert!(
            again_text.contains("chromium for linux-x86_64 (local/tool-browser): already in the store"),
            "the second mur install fetches nothing:\n{again_text}"
        );
        assert!(!again_text.contains("fetched "), "{again_text}");

        let entry = store_entry(staging.home.path());
        assert_eq!(
            entry.file_name().unwrap().to_string_lossy(),
            sha256,
            "the store entry is named by the pinned sha256"
        );
        let pin = Pin {
            url,
            sha256,
            build,
            entry,
        };
        assert_eq!(
            pin.entry.join("bin/chromium").canonicalize().unwrap(),
            pin.run_file().canonicalize().unwrap(),
            "bin/chromium links to the pinned run file"
        );
        println!("store entry: {}", pin.entry.display());
        Some(pin)
    } else {
        assert!(
            install.status.success() || installed.contains("E-REG-013"),
            "offline, mur install fails only on the fetch (E-REG-013):\n{installed}"
        );
        None
    };

    // Phase 2: no chromium on PATH, so the store's copy runs.
    let bare_path = path_without_chromium();
    if let Some(pin) = &pin {
        for tier in ["scoped", "sealed"] {
            granted_session(&staging, tier, https, &bare_path, &Chromium::Pinned(pin));
        }
        let report = doctor(&staging, &bare_path, &granted_capabilities("scoped"));
        println!("--- mur doctor (pinned) ---\n{report}");
        assert!(report.contains("Pinned programs:"), "{report}");
        let line = report
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("\u{2713}  local/tool-browser (chromium): pinned copy present; runs "))
            .unwrap_or_else(|| panic!("a present, running pinned copy in mur doctor:\n{report}"));
        assert!(line.ends_with(" (pinned)"), "the pinned copy runs: {line}");
        for code in ["W-RUN-011", "W-RUN-012"] {
            assert!(!report.contains(code), "no {code} in mur doctor:\n{report}");
        }
    }

    // Phase 3: the host's chromium comes first on PATH, and it runs.
    for tier in ["scoped", "sealed"] {
        granted_session(&staging, tier, https, &staging.path, &Chromium::Host);
    }

    // Phase 4: the store entry removed, nothing runs and the call says how to get a copy.
    if let Some(pin) = &pin {
        assert!(
            pin.entry.starts_with(staging.home.path().canonicalize().unwrap()),
            "the store entry lies in the scratch HOME: {}",
            pin.entry.display()
        );
        fs::remove_dir_all(&pin.entry).unwrap();
        let name = "store-removed";
        let provider = Provider::start(vec![json!({"action": "open", "url": "page.html"})]);
        staging.write_manifest(name, &provider.endpoint, &granted_capabilities("scoped"));
        let workdir = staging.workdir(name);
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("page.html"), common::FIXTURE_PAGE).unwrap();
        let output = staging.run_on(&bare_path, name, &[]);
        println!("--- store removed: run ---\n{}", combined(&output));
        let results = tool_results(&provider.last_request());
        let answer = result_of(&results, "call_1");
        println!("call_1: {answer}");
        assert_eq!(
            fenced(answer),
            NOT_INSTALLED_LINE,
            "the fence holds exactly the not-installed line"
        );
        let browsers: Vec<PathBuf> = fs::read_dir(workdir.join(".tool-browser"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(browsers.is_empty(), "no browser folder: {browsers:?}");

        let report = doctor(&staging, &bare_path, &granted_capabilities("scoped"));
        println!("--- mur doctor (store removed) ---\n{report}");
        assert!(
            report.contains(
                "warning[W-RUN-012]: local/tool-browser pins chromium for linux-x86_64, and the store holds no copy; run mur install"
            ),
            "mur doctor warns that the store holds no copy:\n{report}"
        );
    }

    // Phase 5: without the grant, exactly the one line, fenced, and nothing in the work folder.
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
    assert_eq!(
        fenced(refused),
        NO_GRANT_LINE,
        "the fence holds exactly the no-grant line"
    );
    assert!(
        !workdir.join(".tool-browser").exists(),
        "nothing is created without the grant"
    );
}

/// The text inside the tool-browser fence of a tool result, trimmed.
fn fenced(result: &str) -> &str {
    let start = result
        .find(FENCE_OPEN)
        .unwrap_or_else(|| panic!("the result is fenced: {result}"));
    result[start + FENCE_OPEN.len()..]
        .split("</untrusted-content>")
        .next()
        .unwrap_or("")
        .trim()
}

/// tool-browser's `murmur.yaml` with the needs in [`NSS_NEEDS`] dropped from `programs:`.
fn without_nss_needs(manifest: &str) -> String {
    let mut out = String::new();
    let mut dropping = false;
    for line in manifest.lines() {
        let item = line.trim_start().strip_prefix("- ");
        if let Some(item) = item {
            dropping = NSS_NEEDS.contains(&item);
        } else if !line.starts_with("        ") {
            dropping = false;
        }
        if !dropping {
            out.push_str(line);
            out.push('\n');
        }
    }
    for need in NSS_NEEDS {
        assert!(!out.contains(need), "the copy still declares `{need}`");
    }
    assert!(
        out.contains("- library: libnspr4.so"),
        "the copy keeps the needs after NSS's"
    );
    out
}

/// `manifest` without `programs.chromium.download`, so installing it fetches nothing.
fn without_download(manifest: &str) -> String {
    let mut out = String::new();
    let mut dropping = false;
    for line in manifest.lines() {
        if line == "    download:" {
            dropping = true;
        } else if dropping && !line.starts_with("      ") {
            dropping = false;
        }
        if !dropping {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(
        !out.contains("    download:\n") && !out.contains("sha256:"),
        "the copy pins no download"
    );
    assert!(out.contains("  chromium-browser:\n"), "the copy keeps programs:");
    out
}

/// A copy of tool-browser whose `programs:` withholds NSS's modules and pins no download, same
/// name and version, in its own `HOME` and project: the host's Chrome starts and renders a file,
/// and the first `https` page kills it in NSS's start-up check.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn https_page_kills_chrome_when_the_nss_needs_are_withheld() {
    let _turn = one_at_a_time();
    if let Err(why) = common::reachable("example.com") {
        println!("skipping the NSS-withheld https run: {why}");
        return;
    }
    let copy = tempfile::tempdir().unwrap();
    let tool_dir = copy.path().join("tool-browser");
    fs::create_dir_all(&tool_dir).unwrap();
    let manifest =
        fs::read_to_string(workspace_root().join("tools/tool-browser/murmur.yaml")).unwrap();
    fs::write(
        tool_dir.join("murmur.yaml"),
        without_download(&without_nss_needs(&manifest)),
    )
    .unwrap();
    let (staging, install) = stage(&tool_dir);
    assert!(
        install.status.success(),
        "installing the NSS-withheld copy:\n{}",
        combined(&install)
    );

    let name = "nss-withheld";
    let provider = Provider::start(vec![json!({"action": "open", "url": HTTPS_PAGE})]);
    staging.write_manifest(name, &provider.endpoint, &granted_capabilities("scoped"));
    let workdir = staging.workdir(name);
    fs::create_dir_all(&workdir).unwrap();

    let output = staging.run(name, &[]);
    let text = combined(&output);
    println!("--- NSS withheld: run ---\n{text}");
    let results = tool_results(&provider.last_request());
    let opened = result_of(&results, "call_1");
    println!("call_1: {opened}");
    print_browser_logs(&workdir);
    let inside = &opened[opened.find(FENCE_OPEN).expect("fenced") + FENCE_OPEN.len()..];
    let line = inside
        .split("</untrusted-content>")
        .next()
        .unwrap_or("")
        .trim();
    let line = line.lines().last().unwrap_or("");
    assert!(
        line.starts_with("tool-browser: chromium exited with 133 during Page.navigate:"),
        "Chrome goes during the navigation to the https page: {line}"
    );
    assert!(
        line.contains("FATAL:crypto/nss_util.cc") && line.contains("nss_error="),
        "the line carries NSS's FATAL line: {line}"
    );
    assert!(
        line.ends_with(common::NSS_HINT),
        "the line ends with the NSS hint: {line}"
    );

    let trace = staging.trace(&workdir);
    let entry = declared_needs_entry(&trace);
    assert!(
        !entry.to_string().contains("libsoftokn3"),
        "the copy declares no NSS module: {entry}"
    );
    let starts = browser_events(&trace, "process_start");
    let last = starts.last().expect("a process_start for browser");
    let exit = browser_events(&trace, "process_exit")
        .into_iter()
        .find(|l| l["process_id"] == last["process_id"])
        .unwrap_or_else(|| panic!("a process_exit for {last}"));
    println!("process_exit: {exit}");
    assert_eq!(exit["exit_code"], 133, "Chrome dies with SIGTRAP: {exit}");
    assert_eq!(exit["reason"], "exited", "Chrome died on its own: {exit}");
    let logs = browser_logs(&workdir);
    assert!(
        logs.contains("FATAL:crypto/nss_util.cc") && logs.contains("nss_error="),
        "browser.log names NSS's failed start-up"
    );
}
