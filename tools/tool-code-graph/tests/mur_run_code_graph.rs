//! tool-code-graph under a real `mur run`.
//!
//! `tests/wasm_component.rs` proves the tool against preopens the test makes itself. Only a
//! launch shows that murmur hands the component the work folder and a `host_paths` folder the
//! way the tool expects, and that a database written in one call of a session is updated by the
//! next after the sources change on the host in between:
//!
//! * (a) The work folder holds `repo/`, the Rust fixture. Call 1 indexes it (`repo_path: "repo"`,
//!   2 changed files). Before answering, the fake provider edits `repo/src/util.rs` on the host.
//!   Call 2 re-indexes (1 changed file), and call 3, `impact_analysis` over the edited range,
//!   returns the new caller. After the session the database holds the edited rows and no lock
//!   or journal is left.
//! * (b) A `host_paths` entry with `access: read-write` lends a folder holding a repository; an
//!   absolute `repo_path` indexes it and the database appears on the host.
//!
//! The harness launches the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. `driver-openai` and `tool-code-graph` are built out of
//! this workspace and packed with `scripts/package-artifact.sh` on every run; `mur` comes from
//! `MUR_BIN` or `PATH`, and a run that finds neither fails rather than skipping. Scratch trees
//! live under the crate's git-ignored `.test-scratch/`, because `mur run` refuses a `host_paths`
//! entry under `/tmp`.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p tool-code-graph --test mur_run_code_graph --
//! --ignored --nocapture` runs it.

#![cfg(unix)]

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

use common::write_repo;
use serde_json::{json, Value};

const TOOL: &str = "tool-code-graph";
const HELPER: &str = "rust://fixture_crate/util#helper(->i64)";
const USE_HELPER: &str = "rust://fixture_crate/util#use_helper(->i64)";
const EDITED_UTIL: &str = "/// An unrelated helper in another file.\npub fn helper() -> i64 {\n    99\n}\n\n/// Calls the helper.\npub fn use_helper() -> i64 {\n    helper()\n}\n";

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

/// The provider's answer to one request: a call of the tool with these arguments, or text.
enum Turn {
    Call(Value),
    Text,
}

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn stream_for(turn: &Turn, call_number: usize) -> String {
    match turn {
        Turn::Call(arguments) => {
            let call = json!({
                "index": 0, "id": format!("call_{call_number}"), "type": "function",
                "function": {"name": TOOL, "arguments": arguments.to_string()}
            });
            chat_chunk(json!({"role": "assistant", "content": ""}), None)
                + &chat_chunk(json!({"tool_calls": [call]}), None)
                + &chat_chunk(json!({}), Some("tool_calls"))
                + "data: [DONE]\n\n"
        }
        Turn::Text => {
            chat_chunk(json!({"content": "done"}), None)
                + &chat_chunk(json!({}), Some("stop"))
                + "data: [DONE]\n\n"
        }
    }
}

type Script = Box<dyn FnMut(&[(String, String)]) -> Turn + Send>;

/// A loopback Chat Completions server. Each request is answered by `script`, given the
/// `(tool_call_id, content)` of every tool result the request carries; an assertion failing in
/// the script is recorded and reported by [`Provider::failure`]. Every request body is kept.
struct Provider {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl Provider {
    fn start(mut script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let failure = Arc::new(Mutex::new(None));
        let (recorded, failed) = (Arc::clone(&requests), Arc::clone(&failure));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(body) = read_request_body(&mut stream) else {
                    continue;
                };
                let results = tool_results(&body);
                recorded.lock().unwrap().push(body);
                let turn = if failed.lock().unwrap().is_some() {
                    Turn::Text
                } else {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        script(&results)
                    })) {
                        Ok(turn) => turn,
                        Err(panic) => {
                            let message = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_else(|| "the script panicked".to_string());
                            *failed.lock().unwrap() = Some(message);
                            Turn::Text
                        }
                    }
                };
                let payload = stream_for(&turn, results.len() + 1);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint,
            requests,
            failure,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
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
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
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

/// The tool's `data` in a tool result as the provider receives it: murmur fences the text as
/// `<untrusted-content source=tool:tool-code-graph>` … `</untrusted-content>`, and `data` is
/// the first JSON object inside.
fn result_data(content: &str) -> Value {
    let open = format!("<untrusted-content source=tool:{TOOL}>");
    let inner = content
        .split_once(&open)
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.rsplit_once("</untrusted-content>"))
        .map(|(inner, _)| inner)
        .unwrap_or_else(|| panic!("the tool result is not fenced as {open}: {content}"));
    let start = inner
        .find('{')
        .unwrap_or_else(|| panic!("no JSON data in the tool result: {content}"));
    serde_json::Deserializer::from_str(&inner[start..])
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_else(|| panic!("the tool result's data does not parse: {content}"))
}

/// The tool's `data` in the tool result for `call_<n>`.
fn data_of(results: &[(String, String)], n: usize) -> Value {
    let id = format!("call_{n}");
    let (_, content) = results
        .iter()
        .find(|(call, _)| *call == id)
        .unwrap_or_else(|| panic!("no tool result for {id} in {results:?}"));
    result_data(content)
}

// ── staging ──────────────────────────────────────────────────────────────────

/// One scenario's scratch tree under the crate's `.test-scratch/`: a `HOME`, a capsule project
/// folder, the packed zips, and the work folder. Removed on drop.
struct Staging {
    root: PathBuf,
    driver_version: String,
    tool_version: String,
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Staging {
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn workdir(&self) -> PathBuf {
        self.root.join("work")
    }

    fn manifest(&self) -> PathBuf {
        self.project().join("murmur.yaml")
    }

    fn mur(&self) -> Command {
        let mut command = Command::new(mur_binary());
        command.env("HOME", self.home());
        command
    }

    /// The capsule manifest. `MODEL_ROUTES` sends `chat-latest` to the Chat Completions surface.
    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: code-graph-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/{TOOL}\n    version: {}\n    runtime: tool\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version, self.tool_version
            ),
        )
        .unwrap();
    }

    /// Add `host_paths:` to the scratch `~/.murmur/config.yaml`, keeping what staging wrote.
    fn lend_host_path(&self, dir: &Path, access: &str) {
        let config = self.home().join(".murmur/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let existing = fs::read_to_string(&config).unwrap_or_default();
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

    /// Run one task to completion against `provider` and return its output after asserting the
    /// session succeeded and the provider's script raised nothing.
    fn run(&self, provider: &Provider) -> Output {
        self.write_manifest(&provider.endpoint);
        let output = self
            .mur()
            .current_dir(self.project())
            .args([
                "run",
                "--manifest",
                self.manifest().to_str().unwrap(),
                "--task",
                "index the repository",
                "--workdir",
                self.workdir().to_str().unwrap(),
            ])
            .output()
            .expect("spawning `mur run`");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        println!("{text}");
        for (n, body) in provider.requests().iter().enumerate() {
            for (id, content) in tool_results(body) {
                println!("request {}: {id}: {content}", n + 1);
            }
        }
        if let Some(failure) = provider.failure() {
            panic!("the provider's script failed: {failure}");
        }
        assert!(output.status.success(), "the session must succeed:\n{text}");
        output
    }
}

fn stage(tag: &str) -> Staging {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".test-scratch")
        .join(format!("mur_run_code_graph_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for dir in ["home", "project", "artifacts", "work"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    let artifacts = root.join("artifacts");
    let (driver_version, driver_zip) = pack("drivers/driver-openai", &artifacts);
    let (tool_version, tool_zip) = pack(&format!("tools/{TOOL}"), &artifacts);
    let staging = Staging {
        root,
        driver_version,
        tool_version,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");
    for (zip, what) in [(&driver_zip, "driver-openai"), (&tool_zip, TOOL)] {
        let mut publish = staging.mur();
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {what}"));
        let mut install = staging.mur();
        install
            .current_dir(staging.project())
            .arg("install")
            .arg(zip);
        run_to_success(install, &format!("installing {what}"));
    }
    staging
}

fn murmur_dir_entries(repo: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(repo.join(".murmur"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn no_lock_or_journal(repo: &Path) {
    let entries = murmur_dir_entries(repo);
    assert!(
        entries
            .iter()
            .all(|name| !name.starts_with("code-graph.db-") && !name.ends_with(".lock")),
        "no journal or lock beside the database: {entries:?}"
    );
}

fn symbol_ids(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT symbol_id FROM symbols ORDER BY symbol_id")
        .unwrap();
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    ids
}

// ── the scenarios ────────────────────────────────────────────────────────────

/// A repository in the work folder is indexed, edited on the host between calls, re-indexed,
/// and its impact read, within one session.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_repository_edited_between_calls_is_reindexed_and_its_impact_read() {
    let staging = stage("work_folder");
    let repo = staging.workdir().join("repo");
    fs::create_dir_all(&repo).unwrap();
    write_repo("rust", &repo);
    let index = json!({"operation": "index_repository", "repo_path": "repo"});
    let impact = json!({"operation": "impact_analysis", "repo_path": "repo",
                        "file": "src/util.rs", "start_line": 2, "end_line": 4});
    let (script_index, script_impact, util) =
        (index.clone(), impact.clone(), repo.join("src/util.rs"));
    let provider = Provider::start(Box::new(move |results| match results.len() {
        0 => Turn::Call(script_index.clone()),
        1 => {
            let data = data_of(results, 1);
            assert_eq!(data["changed_files"], 2, "{data}");
            assert_eq!(data["repo_path"], "repo", "{data}");
            fs::write(&util, EDITED_UTIL).unwrap();
            Turn::Call(script_index.clone())
        }
        2 => {
            let data = data_of(results, 2);
            assert_eq!(data["changed_files"], 1, "only util.rs changed: {data}");
            assert_eq!(data["unchanged_files"], 1, "{data}");
            Turn::Call(script_impact.clone())
        }
        3 => {
            let data = data_of(results, 3);
            assert_eq!(data["roots"][0]["symbol_id"], HELPER, "{data}");
            assert_eq!(data["callers"][0]["symbol_id"], USE_HELPER, "{data}");
            assert_eq!(data["callers"].as_array().unwrap().len(), 1, "{data}");
            let dp = data["data_path"].as_str().unwrap();
            assert!(dp.starts_with("repo/.murmur/impacts/impact-"), "{dp}");
            Turn::Text
        }
        n => panic!("unexpected request with {n} tool results"),
    }));
    staging.run(&provider);

    assert_eq!(provider.requests().len(), 4, "three calls, then text");
    let ids = symbol_ids(&repo.join(".murmur/code-graph.db"));
    assert!(
        ids.contains(&USE_HELPER.to_string()),
        "edited rows: {ids:?}"
    );
    assert!(ids.contains(&HELPER.to_string()), "{ids:?}");
    no_lock_or_journal(&repo);
}

/// A repository in a `read-write` host path, named by an absolute `repo_path`, is indexed onto
/// the host.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_repository_in_a_read_write_host_path_is_indexed() {
    let staging = stage("host_path");
    let lent = staging.root.join("lent");
    let repo = lent.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write_repo("rust", &repo);
    staging.lend_host_path(&lent, "read-write");
    let index = json!({"operation": "index_repository", "repo_path": repo.display().to_string()});
    let (script_index, expected) = (index.clone(), repo.display().to_string());
    let provider = Provider::start(Box::new(move |results| match results.len() {
        0 => Turn::Call(script_index.clone()),
        1 => {
            let data = data_of(results, 1);
            assert_eq!(data["changed_files"], 2, "{data}");
            assert_eq!(data["repo_path"], expected.as_str(), "{data}");
            Turn::Text
        }
        n => panic!("unexpected request with {n} tool results"),
    }));
    staging.run(&provider);

    assert_eq!(provider.requests().len(), 2, "one call, then text");
    assert!(repo.join(".murmur/code-graph.db").is_file());
    assert!(symbol_ids(&repo.join(".murmur/code-graph.db")).contains(&HELPER.to_string()));
    no_lock_or_journal(&repo);
    assert!(
        fs::read_dir(staging.workdir()).unwrap().next().is_none(),
        "the work folder stays empty"
    );
}
