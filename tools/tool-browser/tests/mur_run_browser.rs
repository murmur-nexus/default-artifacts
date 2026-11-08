//! `tool-browser` under a real `mur run`: the grant, Chrome started as a session process inside
//! the capsule's containment on the `scoped` and `sealed` tiers, the fence murmur puts around
//! every result, the screenshot reaching the model, and nothing left running after the session.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. `driver-openai` and `tool-browser` (both
//! wasm32-wasip2) are built out of this workspace and packed by `scripts/package-artifact.sh`
//! on every run. `mur` comes from `MUR_BIN` and must report WIT level 4 or higher, the first
//! with `murmur:process`; a run without one fails rather than skipping.
//!
//! The browser is `TOOL_BROWSER_TEST_CHROME`, which must be `chrome-headless-shell`: full Chrome
//! and Chromium cannot start on the scoped and sealed tiers. The test links it as `chromium` in
//! its own directory (the sealed tier's composed root binds only the resolved binary's
//! directory) and puts that directory first on `PATH`.
//!
//! The page is a file in the work folder rather than a loopback server: Chrome runs in the
//! session's network namespace, whose loopback is not the host's.
//!
//! The granted capsules take their capabilities from the README's working manifest, copied as is
//! but for the browser's directory and the tier, so the run proves that manifest. Where the host
//! reaches `example.com:443`, each also opens `https://example.com/`, and one more capsule on
//! `scoped` withholds the NSS files to show the line a model gets; elsewhere both print a skip
//! line.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=<level-4 mur> TOOL_BROWSER_TEST_CHROME=<chrome-headless-shell> cargo test -p
//! tool-browser --test mur_run_browser -- --ignored --nocapture` runs it.

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

/// Build the wasm artifact at `crate_dir` and pack it; returns its version and zip.
fn pack(crate_dir: &str, out_dir: &Path) -> (String, PathBuf) {
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
                 trace:\n  capture: content\n\
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

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let (tool_version, tool_zip) = pack("tools/tool-browser", artifacts.path());
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

/// The browser directory the README's working manifest names as its example.
const README_BROWSER_DIR: &str =
    "/home/you/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64";
const README_HEADING: &str = "### A working manifest on `scoped` and `sealed`";
const HTTPS_URL: &str = "https://example.com/";
const HTTPS_PAGE_LINE: &str = "page: Example Domain — https://example.com/";
const NSS_HINT: &str = "; Chrome loads NSS's modules with dlopen, so granting chromium does not grant them: add libsoftokn3.so, libsoftokn3.chk, libfreebl3.so, libfreebl3.chk, libfreeblpriv3.so, libfreeblpriv3.chk, libnssdbm3.so, libnssdbm3.chk, libnssckbi.so and libsqlite3.so.0, which libsoftokn3.so links, to the capabilities.shell.interpreter_runtime dirs of chromium, as tool-browser's README lists";

/// The README's working manifest for `tier`: the lines under its `capabilities:`, with only the
/// browser's directory and the `containment:` value replaced.
fn granted_capabilities(staging: &Staging, tier: &str) -> String {
    let readme =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let block: Vec<&str> = readme
        .lines()
        .skip_while(|l| *l != README_HEADING)
        .skip_while(|l| *l != "```yaml")
        .skip(1)
        .take_while(|l| *l != "```")
        .collect();
    assert_eq!(
        block.first(),
        Some(&"capabilities:"),
        "the README's working manifest starts with capabilities:"
    );
    let (mut containment, mut dir) = (0, 0);
    let caps: String = block[1..]
        .iter()
        .map(|l| {
            if l.trim_start().starts_with("containment:") {
                containment += 1;
                format!("  containment: {tier}\n")
            } else if l.contains(README_BROWSER_DIR) {
                dir += 1;
                format!(
                    "{}\n",
                    l.replace(
                        README_BROWSER_DIR,
                        &staging.browser_dir.display().to_string()
                    )
                )
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    assert_eq!(
        (containment, dir),
        (1, 1),
        "the README's manifest has one containment: line and names {README_BROWSER_DIR}"
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

/// One granted session on `tier`: open → console → screenshot, all inside the fence, the image
/// in the next request, the browser a session process murmur kills at the session's end.
fn granted_session(staging: &Staging, tier: &str, https: bool) {
    let name = format!("granted-{tier}");
    let mut calls = scripted_calls();
    if https {
        calls.push(json!({"action": "open", "url": HTTPS_URL}));
    }
    let provider = Provider::start(calls);
    staging.write_manifest(
        &name,
        &provider.endpoint,
        &granted_capabilities(staging, tier),
    );
    let workdir = staging.workdir(&name);
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("page.html"), common::FIXTURE_PAGE).unwrap();

    let scope = staging.run(&name, &["--explain-scope"]);
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

    let output = staging.run(&name, &[]);
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
        let opened = result_of(&results, "call_4");
        assert!(opened.contains(FENCE_OPEN), "open is fenced: {opened}");
        assert!(
            opened.lines().any(|l| l == HTTPS_PAGE_LINE),
            "the https page opens on {tier}: {opened}"
        );
    }

    let trace = staging.trace(&workdir);
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

/// The nine NSS files the README's manifest grants, each a module or its `.chk`.
const NSS_MODULES: [&str; 5] = [
    "libsoftokn3.",
    "libfreebl3.",
    "libfreeblpriv3.",
    "libnssdbm3.",
    "libnssckbi.so",
];

/// The README's manifest without its nine NSS lines, on `scoped`: the https page kills Chrome,
/// and the call names Chrome's exit, the navigation and its FATAL line, with the NSS hint.
fn nss_withheld_session(staging: &Staging) {
    let name = "nss-withheld";
    let provider = Provider::start(vec![json!({"action": "open", "url": HTTPS_URL})]);
    let caps: String = granted_capabilities(staging, "scoped")
        .lines()
        .filter(|l| !(l.contains("path: ") && NSS_MODULES.iter().any(|m| l.contains(m))))
        .map(|l| format!("{l}\n"))
        .collect();
    let granted = |file: &str| {
        caps.lines()
            .any(|l| l.contains("path: ") && l.contains(file))
    };
    assert!(
        !granted("libsoftokn3") && granted("libsqlite3.so.0"),
        "{caps}"
    );
    staging.write_manifest(name, &provider.endpoint, &caps);
    let workdir = staging.workdir(name);
    fs::create_dir_all(&workdir).unwrap();
    let output = staging.run(name, &[]);
    let text = combined(&output);
    println!("--- NSS withheld run ---\n{text}");
    let results = tool_results(&provider.last_request());
    print_browser_logs(&workdir);
    let died = result_of(&results, "call_1");
    println!("call_1: {died}");
    let inside = &died[died.find(FENCE_OPEN).expect("fenced") + FENCE_OPEN.len()..];
    let line = inside
        .split("</untrusted-content>")
        .next()
        .unwrap_or("")
        .trim();
    let line = line.lines().last().unwrap_or("");
    assert!(
        line.starts_with("tool-browser: chromium exited with 133 during Page.navigate:"),
        "{line}"
    );
    assert!(
        line.contains("FATAL:crypto/nss_util.cc") && line.contains("nss_error="),
        "{line}"
    );
    assert!(line.ends_with(NSS_HINT), "{line}");

    let trace = staging.trace(&workdir);
    let starts = browser_events(&trace, "process_start");
    let exits = browser_events(&trace, "process_exit");
    for event in starts.iter().chain(&exits) {
        println!("{event}");
    }
    let last = starts.last().expect("a process_start for browser");
    let died = exits
        .iter()
        .find(|e| e["process_id"] == last["process_id"])
        .unwrap_or_else(|| panic!("a process_exit for the browser that ran: {exits:?}"));
    assert_eq!(
        (died["exit_code"].as_i64(), died["reason"].as_str()),
        (Some(133), Some("exited")),
        "{died}"
    );
}

/// A granted capsule, the README's manifest, runs the browser on `scoped` and on `sealed` and
/// opens an https page where the host reaches one; without the NSS files that page kills Chrome
/// with a line naming why; a capsule without the grant gets the one no-grant line, fenced.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn browser_runs_on_scoped_and_sealed_and_ungranted_gets_one_line() {
    let staging = stage();
    let https = match common::reachable("example.com") {
        Ok(()) => true,
        Err(why) => {
            println!("skipped the https page and the NSS-withheld capsule: {why}");
            false
        }
    };
    for tier in ["scoped", "sealed"] {
        granted_session(&staging, tier, https);
    }
    if https {
        nss_withheld_session(&staging);
    }

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
    assert_eq!(
        inside, NO_GRANT_LINE,
        "the fence holds exactly the no-grant line"
    );
    assert!(
        !workdir.join(".tool-browser").exists(),
        "nothing is created without the grant"
    );
}
