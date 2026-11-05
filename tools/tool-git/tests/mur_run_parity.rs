//! tool-git in a real `mur run`, as WASM and as the native 0.6.0 build, at `containment: scoped`
//! and `containment: sealed`.
//!
//! A fake Chat Completions provider on loopback scripts [`common::sequence`], one tool call per
//! turn, against a fresh [`common::fixture`] work folder (`--workdir`). It captures each tool
//! result as murmur hands it to the model — `data`, or `summary` when there is none, inside
//! `<untrusted-content source=tool:tool-git>` — and the trace gives each call's status. Both are
//! normalised as `tests/parity.rs` normalises, and per tier the WASM results must equal the
//! native results call for call, and both must equal the recorded native fixture. The WASM run's
//! trace holds one `process_start`/`process_exit` pair (`kind: run`, `tool: tool-git`) per git
//! command; the native run's holds none.
//!
//! `decision_point_sees_tool_git_commands` checks, at `scoped`, that a `hook-command-guard` rule
//! refusing `git push --force` refuses `push` with `force: true`, and records what a
//! `capabilities.filesystem.read_only` grant refuses.
//!
//! Needs a `mur` at WIT level 4 or later (`MUR_BIN`) and the native 0.6.0 zip
//! (`TOOL_GIT_NATIVE_ZIP`; recipe in `tests/fixtures/README.md`):
//!
//! ```sh
//! MUR_BIN=<level-4 mur> TOOL_GIT_NATIVE_ZIP=<tool-git-0.6.0-…mur.zip> \
//!   cargo test -p tool-git --test mur_run_parity -- --ignored --nocapture
//! ```

#![cfg(unix)]

mod common;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use common::{normalise::Normaliser, sequence};
use serde_json::{json, Value};

const FENCE_OPEN: &str = "<untrusted-content source=tool:tool-git>";
const FENCE_CLOSE: &str = "</untrusted-content>";

// ── the runtime under test ───────────────────────────────────────────────────

fn mur_binary() -> PathBuf {
    let path = PathBuf::from(std::env::var_os("MUR_BIN").expect(
        "set MUR_BIN to a mur whose `mur version` reports WIT level 4 or higher (murmur:process)",
    ));
    let out = Command::new(&path).arg("version").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let level: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("WIT level "))
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        level >= 4,
        "MUR_BIN must report WIT level 4 or higher:\n{text}"
    );
    path
}

fn native_zip() -> PathBuf {
    let zip = PathBuf::from(
        std::env::var_os("TOOL_GIT_NATIVE_ZIP")
            .expect("set TOOL_GIT_NATIVE_ZIP to the native tool-git 0.6.0 zip"),
    );
    assert!(zip.is_file(), "no zip at {}", zip.display());
    zip
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn path_with_mur() -> String {
    format!(
        "{}:{}",
        mur_binary().parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    )
}

fn manifest_field(crate_dir: &str, key: &str) -> String {
    let manifest =
        fs::read_to_string(workspace_root().join(crate_dir).join("murmur.yaml")).unwrap();
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no `{key}:` in {crate_dir}/murmur.yaml"))
}

/// Build the wasm artifact at `crate_dir` and pack it with `scripts/package-artifact.sh`.
fn pack(crate_dir: &str, out_dir: &Path) -> PathBuf {
    let name = manifest_field(crate_dir, "name");
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
        .unwrap();
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
            .unwrap_or("")
            .trim_start_matches("Built artifact: ")
            .split(" (")
            .next()
            .unwrap_or(""),
    );
    let zip = if zip.is_file() {
        zip
    } else {
        fs::read_dir(out_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().contains(&format!("{name}-")))
            .expect("the packed zip")
    };
    zip
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_to_success(mut command: Command, what: &str) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{what} failed:\n{}",
        combined(&output)
    );
}

// ── staging ──────────────────────────────────────────────────────────────────

/// A `HOME` holding the store, and one project per tool-git build: a project's `murmur.lock`
/// resolves each artifact name to one version, so the two builds cannot share one.
struct Staging {
    home: PathBuf,
    root: PathBuf,
    driver_version: String,
}

const WASM_VERSION: &str = "0.7.0";
const NATIVE_VERSION: &str = "0.6.0";

impl Staging {
    /// The project that installs tool-git at `version`.
    fn project(&self, version: &str) -> PathBuf {
        self.root.join(format!("project-{version}"))
    }

    fn mur(&self, version: &str) -> Command {
        let mut command = Command::new(mur_binary());
        command
            .env("HOME", &self.home)
            .env("PATH", path_with_mur())
            .current_dir(self.project(version));
        command
    }

    /// A capsule using driver-openai at `endpoint` and tool-git at `version`, plus `extra`
    /// artifact entries; `capabilities` is the YAML under `capabilities:`.
    fn write_manifest(
        &self,
        name: &str,
        endpoint: &str,
        version: &str,
        extra: &str,
        capabilities: &str,
    ) -> PathBuf {
        let path = self.project(version).join(format!("{name}.yaml"));
        fs::write(
            &path,
            format!(
                "name: {name}\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/tool-git\n    version: {version}\n    runtime: tool\n\
                 {extra}\
                 capabilities:\n{capabilities}\
                 trace:\n  capture: content\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version
            ),
        )
        .unwrap();
        path
    }

    fn run(&self, version: &str, manifest: &Path, workdir: &Path) -> Output {
        let mut command = self.mur(version);
        command.args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            "run the scripted git operations",
            "--workdir",
            workdir.to_str().unwrap(),
        ]);
        command.output().expect("spawning `mur run`")
    }

    /// The trace lines of the sessions run with `workdir`.
    fn trace(&self, workdir: &Path) -> Vec<Value> {
        let mut lines = Vec::new();
        let mut stack = vec![self.home.join(".murmur")];
        let tag = workdir.file_name().unwrap().to_string_lossy().into_owned();
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().is_some_and(|n| n == "trace.jsonl")
                    && p.to_string_lossy().contains(&tag)
                {
                    let text = fs::read_to_string(&p).unwrap_or_default();
                    lines.extend(text.lines().filter_map(|l| serde_json::from_str(l).ok()));
                }
            }
        }
        lines
    }
}

fn stage(extra_zips: &[PathBuf]) -> Staging {
    let root = common::scratch("mur-run");
    let artifacts = root.join("artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    let staging = Staging {
        home: root.join("home"),
        root: root.clone(),
        driver_version: manifest_field("drivers/driver-openai", "version"),
    };
    fs::create_dir_all(&staging.home).unwrap();
    assert_eq!(manifest_field("tools/tool-git", "version"), WASM_VERSION);
    let driver = pack("drivers/driver-openai", &artifacts);
    let builds = [
        (WASM_VERSION, pack("tools/tool-git", &artifacts)),
        (NATIVE_VERSION, native_zip()),
    ];
    for (version, _) in &builds {
        let project = staging.project(version);
        fs::create_dir_all(&project).unwrap();
        // `mur install` installs into the project whose murmur.yaml it finds.
        fs::write(
            project.join("murmur.yaml"),
            "name: staging\nversion: 0.1.0\n",
        )
        .unwrap();
    }
    let mut every_zip = vec![driver.clone(), builds[0].1.clone(), builds[1].1.clone()];
    every_zip.extend(extra_zips.iter().cloned());
    for zip in &every_zip {
        let mut publish = staging.mur(WASM_VERSION);
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {}", zip.display()));
    }
    for (version, tool) in &builds {
        let project = staging.project(version);
        let mut zips = vec![driver.clone(), tool.clone()];
        zips.extend(extra_zips.iter().cloned());
        for zip in &zips {
            let mut install = staging.mur(version);
            install.arg("install").arg(zip);
            run_to_success(install, &format!("installing {}", zip.display()));
        }
        let lock = fs::read_to_string(project.join("murmur.lock")).unwrap();
        assert!(
            lock.contains(&format!(
                "name: local/tool-git\n  resolved_version: {version}"
            )),
            "project-{version} resolves tool-git to {version}:\n{lock}"
        );
    }
    staging
}

/// What `capabilities.containment: <tier>` grants git: `git` in `shell.allow`, its
/// remote-helper directory as an `interpreter_runtime` entry, and `sh`, through which git starts
/// `git-upload-pack` and `git-receive-pack` for a remote that is a local path: without it
/// `fetch`, `pull`, `push` and `clone` of `remote.git` fail on both tiers, for both builds, with
/// `cannot exec 'git-upload-pack …': Permission denied`.
fn git_capabilities(tier: &str) -> String {
    format!(
        "  containment: {tier}\n  shell:\n    allow:\n      - git\n      - sh\n    interpreter_runtime:\n      \
         - binary: git\n        dirs:\n          - path: /usr/lib/git-core\n            list_dir: false\n"
    )
}

// ── the fake provider ────────────────────────────────────────────────────────

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn tool_call_stream(turn: usize, arguments: &Value) -> String {
    let call = json!({
        "index": 0, "id": format!("call_{turn}"), "type": "function",
        "function": {"name": "tool-git", "arguments": arguments.to_string()}
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
/// call, or text once the script is spent.
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
        Provider { endpoint, requests }
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

/// `(tool_call_id, content)` of every tool message in a request body.
fn tool_results(body: &str) -> Vec<(String, String)> {
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            let content = match &m["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                other => other.to_string(),
            };
            (
                m["tool_call_id"].as_str().unwrap_or_default().to_string(),
                content,
            )
        })
        .collect()
}

fn read_request_body(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
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

/// The text inside the fence of one tool result.
fn unfenced(content: &str) -> String {
    let start = content
        .find(FENCE_OPEN)
        .unwrap_or_else(|| panic!("an unfenced tool result: {content}"));
    let inside = &content[start + FENCE_OPEN.len()..];
    let end = inside.rfind(FENCE_CLOSE).unwrap_or(inside.len());
    inside[..end].trim_matches('\n').to_string()
}

/// What the model saw for one call, as JSON: `{"data": <value>}` when the text parses as a
/// JSON object (the result's `data`), else `{"summary": <text>}`.
fn seen(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => json!({ "data": v }),
        _ => json!({ "summary": text }),
    }
}

/// What the model would see for a recorded fixture entry.
fn seen_from_fixture(entry: &Value) -> Value {
    if entry["data"].is_null() {
        json!({ "summary": entry["summary"] })
    } else {
        json!({ "data": entry["data"] })
    }
}

fn work_names(work: &Path) -> Vec<String> {
    let mut names = vec![work.display().to_string()];
    let canonical = fs::canonicalize(work).unwrap().display().to_string();
    if !names.contains(&canonical) {
        names.push(canonical);
    }
    names
}

/// A fixture entry's status as the trace's `tool_call` line spells it: `ok` for `passed`,
/// `error` otherwise.
fn trace_status(entry: &Value) -> &'static str {
    if entry["status"] == "passed" {
        "ok"
    } else {
        "error"
    }
}

fn trace_kind(line: &Value) -> &str {
    line["event_type"].as_str().unwrap_or_default()
}

struct RunResult {
    /// Per call: `{"status", "seen"}`, normalised.
    calls: Vec<Value>,
    process_starts: usize,
    process_exits: usize,
}

/// One `mur run` of the whole sequence with tool-git at `version`, at `tier`.
fn run_sequence(staging: &Staging, tier: &str, version: &str) -> RunResult {
    let steps = sequence::steps();
    let provider = Provider::start(steps.iter().map(|s| s.data.clone()).collect());
    let name = format!("parity-{tier}-{version}");
    let manifest = staging.write_manifest(
        &name,
        &provider.endpoint,
        version,
        "",
        &git_capabilities(tier),
    );
    let workdir = staging.project(version).join(format!("work-{name}"));
    fs::create_dir_all(&workdir).unwrap();
    common::fixture::build(&workdir);
    let output = staging.run(version, &manifest, &workdir);
    let text = combined(&output);
    assert!(output.status.success(), "mur run {name} failed:\n{text}");

    let results = tool_results(&provider.last_request());
    assert_eq!(
        results.len(),
        steps.len(),
        "{name}: one result per call:\n{text}"
    );
    let trace = staging.trace(&workdir);
    let statuses: Vec<String> = trace
        .iter()
        .filter(|l| trace_kind(l) == "tool_call" && l["tool_name"] == "tool-git")
        .map(|l| l["status"].as_str().unwrap_or("?").to_string())
        .collect();
    let mut raw = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let id = format!("call_{}", i + 1);
        let content = &results
            .iter()
            .find(|(call, _)| *call == id)
            .unwrap_or_else(|| panic!("{name}: no result for {id}"))
            .1;
        raw.push(json!({
            "step": step.name,
            "status": statuses.get(i).cloned().unwrap_or_else(|| "?".into()),
            "seen": seen(&unfenced(content)),
        }));
    }
    let count = |kind: &str| {
        trace
            .iter()
            .filter(|l| trace_kind(l) == kind && l["tool"] == "tool-git" && l["kind"] == "run")
            .count()
    };
    RunResult {
        calls: Normaliser::new(&work_names(&workdir)).run(&raw),
        process_starts: count("process_start"),
        process_exits: count("process_exit"),
    }
}

fn fixture() -> Vec<Value> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-0.6.0-results.json");
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    doc["results"].as_array().unwrap().clone()
}

#[test]
#[ignore = "launches the real mur binary; needs MUR_BIN and TOOL_GIT_NATIVE_ZIP"]
fn wasm_and_native_agree_on_scoped_and_sealed() {
    let staging = stage(&[]);
    let fixture = fixture();
    let git_commands: usize = fixture
        .iter()
        .map(|e| e["argv"].as_array().unwrap().len())
        .sum();
    let mut failures = Vec::new();
    for tier in ["scoped", "sealed"] {
        let wasm = run_sequence(&staging, tier, WASM_VERSION);
        let native = run_sequence(&staging, tier, NATIVE_VERSION);
        println!("--- {tier} ---");
        for ((w, n), f) in wasm.calls.iter().zip(&native.calls).zip(&fixture) {
            let want = seen_from_fixture(f);
            let line = format!(
                "{:<18} wasm {:<6} native {:<6} fixture {:<6} {}",
                f["step"].as_str().unwrap(),
                w["status"].as_str().unwrap(),
                n["status"].as_str().unwrap(),
                trace_status(f),
                if w["seen"] == n["seen"] && w["seen"] == want {
                    "same"
                } else {
                    "DIFFERENT"
                }
            );
            println!("{line}");
            if w["seen"] != n["seen"] || w["seen"] != want || w["status"] != n["status"] {
                failures.push(format!(
                    "{tier} {}:\n  wasm:    {} {}\n  native:  {} {}\n  fixture: {} {}",
                    f["step"], w["status"], w["seen"], n["status"], n["seen"], f["status"], want
                ));
            }
            if n["status"] != trace_status(f) {
                failures.push(format!(
                    "{tier} {}: native status {} vs fixture {}",
                    f["step"],
                    n["status"],
                    trace_status(f)
                ));
            }
        }
        println!(
            "{tier}: wasm trace process_start {} process_exit {} (git commands in the fixture: \
             {git_commands}); native trace process_start {} process_exit {}",
            wasm.process_starts, wasm.process_exits, native.process_starts, native.process_exits
        );
        if wasm.process_starts != git_commands || wasm.process_exits != git_commands {
            failures.push(format!("{tier}: wasm process_start/process_exit count"));
        }
        if native.process_starts != 0 || native.process_exits != 0 {
            failures.push(format!("{tier}: native run wrote process lines"));
        }
    }
    assert!(failures.is_empty(), "differences:\n{}", failures.join("\n"));
}

// ── the decision point ───────────────────────────────────────────────────────

/// Run `calls` with tool-git at `version` at `scoped`; returns each result's fenced text and the
/// trace.
fn run_calls(
    staging: &Staging,
    name: &str,
    version: &str,
    extra: &str,
    capabilities: &str,
    calls: Vec<Value>,
) -> (Vec<String>, Vec<Value>, PathBuf) {
    let n = calls.len();
    let provider = Provider::start(calls);
    let manifest = staging.write_manifest(name, &provider.endpoint, version, extra, capabilities);
    let workdir = staging.project(version).join(format!("work-{name}"));
    fs::create_dir_all(&workdir).unwrap();
    common::fixture::build(&workdir);
    let output = staging.run(version, &manifest, &workdir);
    let text = combined(&output);
    assert!(output.status.success(), "mur run {name} failed:\n{text}");
    let results = tool_results(&provider.last_request());
    assert_eq!(results.len(), n, "{name}:\n{text}");
    // A call the runtime refused before dispatch carries the runtime's own text, unfenced.
    let texts = (1..=n)
        .map(|i| {
            let id = format!("call_{i}");
            let content = &results.iter().find(|(c, _)| *c == id).unwrap().1;
            if content.contains(FENCE_OPEN) {
                unfenced(content)
            } else {
                format!("[runtime] {content}")
            }
        })
        .collect();
    (texts, staging.trace(&workdir), workdir)
}

fn hook_zip() -> PathBuf {
    let out = common::scratch("hook-zip");
    pack("hooks/hook-command-guard", &out)
}

#[test]
#[ignore = "launches the real mur binary; needs MUR_BIN and TOOL_GIT_NATIVE_ZIP"]
fn decision_point_sees_tool_git_commands() {
    let staging = stage(&[hook_zip()]);
    let guard = format!(
        "\x20 - name: local/hook-command-guard\n    version: {}\n    runtime: hook\n    config:\n      \
         rules:\n        - program: git\n          any_flag: [\"--force\"]\n          reason: no force push\n",
        manifest_field("hooks/hook-command-guard", "version")
    );
    for version in [WASM_VERSION, NATIVE_VERSION] {
        let (texts, trace, workdir) = run_calls(
            &staging,
            &format!("guard-{version}"),
            version,
            &guard,
            &git_capabilities("scoped"),
            vec![
                json!({"operation": "push", "repo": "repo", "force": true}),
                json!({"operation": "push", "repo": "repo"}),
            ],
        );
        let denied: Vec<&Value> = trace
            .iter()
            .filter(|l| trace_kind(l) == "call_denied")
            .collect();
        println!("--- guard, tool-git {version} ---");
        println!("push force: {}", texts[0]);
        println!("push:       {}", texts[1]);
        for d in &denied {
            println!("call_denied: {d}");
        }
        let remote_main = common::git_ok(&[
            "-C",
            workdir.join("remote.git").to_str().unwrap(),
            "rev-parse",
            "main",
        ]);
        let repo_main = common::git_ok(&[
            "-C",
            workdir.join("repo").to_str().unwrap(),
            "rev-parse",
            "main",
        ]);
        println!(
            "remote main {} repo main {}",
            remote_main.trim(),
            repo_main.trim()
        );
        // The fixture is deterministic, so a fresh one shows the remote as the run found it.
        let fresh = common::scratch("fixture-fresh");
        common::fixture::build(&fresh);
        let remote_before = common::git_ok(&[
            "-C",
            fresh.join("remote.git").to_str().unwrap(),
            "rev-parse",
            "main",
        ]);
        if version == WASM_VERSION {
            assert_eq!(
                texts[0],
                "tool-git: murmur refused to run git: on-shell: policy hook 'hook-command-guard' \
                 refused: rules[0]: no force push; matched `git -C repo push --porcelain --force \
                 origin`"
            );
            assert_eq!(denied.len(), 1, "one call_denied line");
            assert_eq!(remote_main, remote_before, "remote.git is unchanged");
        } else {
            // The native build runs git itself, out of the decision point's sight.
            assert!(denied.is_empty());
            assert_eq!(
                remote_main.trim(),
                repo_main.trim(),
                "the native force push landed"
            );
        }
    }

    // A read_only grant on repo/src.
    let mut caps = git_capabilities("scoped");
    caps.push_str("  filesystem:\n    read_only:\n      - repo/src\n");
    let calls = vec![
        json!({"operation": "status", "repo": "repo"}),
        json!({"operation": "log", "repo": "repo", "n": 1}),
        json!({"operation": "restore", "repo": "repo", "dest_paths": ["src/a.rs"]}),
        json!({"operation": "restore", "repo": "repo", "dest_paths": ["repo/src/lib.rs"]}),
        json!({"operation": "add", "repo": "repo", "paths": ["NEW.md"]}),
        json!({"operation": "commit", "repo": "repo", "message": "ro"}),
        json!({"operation": "checkout", "repo": "repo", "ref": "feature"}),
        json!({"operation": "switch", "repo": "repo", "branch": "main"}),
        json!({"operation": "reset", "repo": "repo", "mode": "hard", "ref": "HEAD"}),
        json!({"operation": "worktree", "repo": "repo", "subcommand": "add",
               "dest": "repo/src/wt", "branch": "topic"}),
        json!({"operation": "clone", "url": "remote.git", "dest": "repo/src/c"}),
    ];
    for version in [WASM_VERSION, NATIVE_VERSION] {
        let (texts, trace, _) = run_calls(
            &staging,
            &format!("readonly-{version}"),
            version,
            "",
            &caps,
            calls.clone(),
        );
        println!("--- read_only [repo/src], tool-git {version} ---");
        for (call, text) in calls.iter().zip(&texts) {
            let short: String = text.chars().take(200).collect();
            println!("{} → {}", call, short.replace('\n', "\\n"));
        }
        let refused: Vec<&Value> = trace
            .iter()
            .filter(|l| {
                matches!(
                    trace_kind(l),
                    "process_refused" | "call_denied" | "call_refused"
                )
            })
            .collect();
        for r in refused {
            println!("trace: {r}");
        }
        let runtime_refused =
            |i: usize| texts[i].starts_with("[runtime] Refused by the capsule manifest: ");
        for i in [0, 1, 4, 5, 6, 7, 8] {
            assert!(
                texts[i].starts_with('{'),
                "{} passes: {}",
                calls[i],
                texts[i]
            );
        }
        // `dest_paths` are checked relative to the work folder: `src/a.rs` is not under
        // `repo/src`, and git, run in `repo`, finds no such path.
        assert!(!runtime_refused(2) && texts[2].starts_with("git restore failed: "));
        for i in [3, 9, 10] {
            assert!(runtime_refused(i), "{} is refused: {}", calls[i], texts[i]);
        }
    }
}
