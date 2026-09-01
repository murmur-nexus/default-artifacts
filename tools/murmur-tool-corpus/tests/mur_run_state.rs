//! The corpus and the runtime's durable-state grant, proved against each other under a real
//! `mur run`.
//!
//! `store_ops.rs` covers the operations against a directory the test itself created, and
//! `wasm_component.rs` covers the compiled component against a Wasmtime context the test itself
//! built. Neither can see whether the runtime agrees with any of it. Four things have to agree for
//! a capsule to reach the corpus, and only a launch exercises all four at once:
//!
//! 1. the preopen name — the runtime mounts the store at `state`, which is [`STATE_DIR`];
//! 2. the store name `capabilities.state: {}` defaults to, which is the capsule's name and not the
//!    artifact's;
//! 3. how the operator's configuration reaches the tool — the `config:` block on this artifact's
//!    entry in the capsule manifest, lowered by the runtime into the guest environment on this
//!    artifact's grant alone, which is what puts it out of the agent's reach;
//! 4. what a missing grant produces — `state_unavailable`, rather than a corpus quietly written
//!    into the session workdir.
//!
//! Every test here drives the real `mur` binary as a subprocess with `HOME` pointed at a temporary
//! directory. The store's host path is resolved from the launching process's own `HOME`, so a test
//! that launched in-process would either write into the developer's real `~/.murmur/state/` or
//! have to mutate `HOME` and race every other test in the binary.
//!
//! Both components are built out of this workspace on every run: the corpus under test, and the
//! Anthropic driver every launch needs to reach the scripted inference endpoint. `mur` is not
//! built here — it comes from `MUR_BIN` or from `PATH`, and a run that can find it in neither
//! fails rather than skipping, so a green run of this file always means these tests ran.
//!
//! They are `#[ignore]`d: a launch needs a `mur` the default `cargo test --workspace` has no
//! reason to have. `cargo test -p murmur-tool-corpus --test mur_run_state -- --ignored` runs them,
//! which is what the `corpus-state` CI job does.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, Once, OnceLock},
    thread,
};

use murmur_tool_corpus::{store::CORPUS_FILE, STATE_DIR};
use serde_json::{json, Value};
use tempfile::TempDir;

/// Crate directories, relative to the workspace root, of the two artifacts a launch needs.
const TOOL_CRATE: &str = "tools/murmur-tool-corpus";
const DRIVER_CRATE: &str = "drivers/murmur-driver-anthropic";

/// The capsule and store names Scenario A and the non-vacuity check share. They are the same
/// staging in both, distinguished only by whether the grant is declared.
const PROOF_CAPSULE: &str = "corpus-proof-capsule";
const PROOF_STORE: &str = "corpus-proof";

/// The `state:` mapping that names [`PROOF_STORE`], spliced in after the tool entry's `state:` key
/// and so indented to the block that key opens.
const PROOF_STATE_YAML: &str = "\n        store: corpus-proof";

/// The operator configuration every scenario that expects a working corpus declares on the tool's
/// entry in the capsule manifest, exactly as an operator would.
///
/// Compact JSON rather than a YAML block because JSON is YAML: spliced into the manifest it is a
/// flow mapping, and the runtime lowers it back to the same bytes it arrived as.
///
/// One type, `note`, whose derived three-letter id prefix (`not`) collides with none of the
/// reserved runtime prefixes, so no `prefix_map` override is needed. The `read_recent` and
/// `search` blocks are absent — the corpus supplies its own caps for both, and omitting them keeps
/// this the minimal config that parses.
const OPERATOR_CONFIG: &str = r#"{"config_version":1,"types":{"note":{"schema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}}}}"#;

// ── the runtime under test ───────────────────────────────────────────────────

/// The `mur` binary these tests launch: `MUR_BIN` when set, otherwise the first executable `mur`
/// on `PATH`.
///
/// Panics when neither resolves. A skip would be worse than useless here: this file exists to
/// prove a join between two repositories, and a suite that reports success having launched nothing
/// says the join holds when nothing checked it.
fn mur_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os("MUR_BIN") {
        let path = PathBuf::from(explicit);
        assert!(
            is_executable(&path),
            "MUR_BIN names {}, which is not an executable file",
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
                 runtime as a subprocess, so a missing binary is a failure rather than a skip — \
                 a suite that reports success having launched nothing proves nothing. Build one \
                 with `cargo build -p murmur-cli` in a murmur checkout."
            )
        })
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A `mur` invocation with the scratch `HOME` in place.
///
/// `NEXUS_API_KEY` is removed so a developer's own key cannot turn a local registry lookup into a
/// remote one.
fn mur(home: &TempDir) -> Command {
    let mut command = Command::new(mur_binary());
    command.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    command
}

/// Run a `mur` invocation to completion and return its stdout, panicking with both streams when it
/// fails.
fn run_to_success(mut command: Command, what: &str) -> String {
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
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ── building and packing this workspace's artifacts ──────────────────────────

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

/// Build both components once per test binary, whatever order the tests run in.
///
/// The build runs even when the files are already there, because the point is to test what this
/// checkout currently describes; cargo no-ops when they are fresh. One invocation for both, since
/// the two share every dependency they have.
fn build_components() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                "murmur-tool-corpus",
                "-p",
                "murmur-driver-anthropic",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for the wasm components");
        assert!(status.success(), "cargo build of the wasm components failed");
    });
}

/// The value of a top-level scalar key in an artifact's `murmur.yaml`.
///
/// A line scan rather than a parse: this workspace has no YAML dependency, and the two fields read
/// here — `name` and `version` — are top-level scalars in both manifests. An indented line is
/// skipped, so a `version:` nested under `artifacts:` cannot be mistaken for the artifact's own.
fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml:\n{manifest}"))
}

/// One artifact of this workspace, packed as `mur publish` and `mur install` take it.
struct Artifact {
    name: String,
    version: String,
    zip: PathBuf,
}

/// Pack a crate's own `murmur.yaml` and its compiled component into a `.mur.zip`.
///
/// The manifest is copied byte for byte from the checkout rather than synthesised, so a launch
/// exercises the real `input_schema`, the real `description` the model is shown, and — for the
/// corpus — the real bundled `capabilities:` block, which Scenarios B and C are what show grants
/// nothing on its own.
///
/// `zip -j` is the same packing `.github/workflows/build.yml` releases with, so what is published
/// here has the layout a released artifact has.
fn pack(out_dir: &Path, crate_dir: &str) -> Artifact {
    build_components();

    let crate_path = workspace_root().join(crate_dir);
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml"))
        .unwrap_or_else(|err| panic!("reading {crate_dir}/murmur.yaml: {err}"));
    let name = manifest_field(&manifest, "name");
    // The version comes from the manifest inside the archive, because that is where `mur publish`
    // reads it: a literal here would name what the capsule asks for but not what the registry
    // holds, and the two would agree only until this repository's next release.
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
        .expect("failed to spawn `zip`; these tests pack artifacts the way build.yml does");
    assert!(status.success(), "packing {name}-{version}.mur.zip failed");

    Artifact { name, version, zip }
}

// ── staging ──────────────────────────────────────────────────────────────────

/// A scratch `HOME`, a project directory, and both artifacts reachable from the two places a
/// `mur run` launch consults.
struct Staging {
    home: TempDir,
    project: TempDir,
    tool: Artifact,
    driver: Artifact,
    /// Where the packed `.mur.zip`s live. Held only to keep the directory alive.
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    /// The host directory `capabilities.state` opens for a given store name.
    fn store_dir(&self, store: &str) -> PathBuf {
        self.home.path().join(".murmur/state").join(store)
    }

    fn state_root(&self) -> PathBuf {
        self.home.path().join(".murmur/state")
    }

    /// Non-empty lines in a store's corpus file.
    fn corpus_lines(&self, store: &str) -> Vec<String> {
        let path = self.store_dir(store).join(CORPUS_FILE);
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Write the capsule manifest, pointing inference at `endpoint` and allowing it on the
    /// network. Called again before every launch, because the endpoint a scripted server binds is
    /// not known until it has bound one.
    fn write_manifest(
        &self,
        capsule: &str,
        endpoint: &str,
        state_yaml: Option<&str>,
        tool_config: Option<&str>,
    ) {
        let capabilities = state_yaml
            .map(|yaml| format!("    capabilities:\n      state:{yaml}\n"))
            .unwrap_or_default();
        let config = tool_config
            .map(|json| format!("    config: {json}\n"))
            .unwrap_or_default();
        let (tool, driver) = (&self.tool, &self.driver);

        fs::write(
            self.manifest(),
            format!(
                "name: {capsule}\nversion: 0.1.0\nartifacts:\n  - name: {}\n    version: {}\n    \
                 runtime: driver\n  - name: {}\n    version: {}\n    runtime: \
                 tool\n{capabilities}{config}capabilities:\n  network:\n    allow:\n      - \
                 {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: \
                 test-model\n  api_key: test-key\n  driver:\n    artifact: {}\n",
                driver.name, driver.version, tool.name, tool.version, driver.name
            ),
        )
        .unwrap();
    }
}

/// Stage a capsule that installs the corpus alongside the inference driver.
///
/// `state_yaml` is spliced in after the tool entry's `state:` key, so `None` produces an entry with
/// no `capabilities:` block at all — an absent block, which is the default-deny baseline, rather
/// than an empty one. `tool_config` likewise: `None` leaves the entry with no `config:` key, which
/// is what the tool reports as `config_missing`.
///
/// Both artifacts are published into the scratch `HOME`'s registry *and* installed into the project
/// store: the first is what artifact resolution reads, the second is what the launch resolves
/// against, and installing into only one produces `E-RUN-008` at launch.
fn stage(capsule: &str, state_yaml: Option<&str>, tool_config: Option<&str>) -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let tool = pack(artifacts.path(), TOOL_CRATE);
    let driver = pack(artifacts.path(), DRIVER_CRATE);

    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        tool,
        driver,
        _artifacts: artifacts,
    };

    // A placeholder endpoint, so `mur install` can find the project root before any server is
    // bound; every launch rewrites the manifest with the endpoint it will actually talk to.
    staging.write_manifest(capsule, "http://127.0.0.1:1", state_yaml, tool_config);

    for artifact in [&staging.driver, &staging.tool] {
        let mut publish = mur(&staging.home);
        publish.arg("publish").arg(&artifact.zip);
        run_to_success(publish, &format!("publishing {}", artifact.name));

        let mut install = Command::new(mur_binary());
        install
            .current_dir(staging.project.path())
            .arg("install")
            .arg(&artifact.zip);
        run_to_success(install, &format!("installing {}", artifact.name));
    }

    staging
}

// ── driving one session ──────────────────────────────────────────────────────

/// Run one task to completion and return the session workdir it reported.
///
/// No `--workdir`, so every invocation gets a fresh `<manifest_dir>/workdir/<session_id>` — which
/// is what makes "a second session, a different directory" free rather than something the test has
/// to arrange.
fn run_session(staging: &Staging, task: &str) -> PathBuf {
    let mut command = mur(&staging.home);
    command.args([
        "run",
        "--manifest",
        staging.manifest().to_str().unwrap(),
        "--task",
        task,
        "--verbose",
    ]);
    let stdout = run_to_success(command, "the session");

    let marker = "workdir: ";
    let start = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("missing '{marker}' in stdout:\n{stdout}"));
    PathBuf::from(
        stdout[start + marker.len()..]
            .lines()
            .next()
            .unwrap_or_default()
            .trim(),
    )
}

/// `mur run --explain-scope --json`, which resolves the capability scope and creates nothing.
fn explain_scope_json(staging: &Staging) -> Value {
    let mut command = mur(&staging.home);
    command.args([
        "run",
        "--manifest",
        staging.manifest().to_str().unwrap(),
        "--json",
        "--explain-scope",
    ]);
    let stdout = run_to_success(command, "--explain-scope");
    serde_json::from_str(&stdout).expect("--explain-scope --json emits one JSON object")
}

fn tool_use_response(tool_id: &str, input: Value) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": tool_id,
            "name": "murmur-tool-corpus",
            "input": input,
        }],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn_response(text: &str) -> String {
    json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// One scripted call's response envelope, read back out of the `tool_result` block the runtime
/// posted on the following request.
///
/// The runtime sends the tool's `data` (falling back to its `summary`), and the corpus puts its
/// whole `{ok, operation, …}` envelope in `data`, so this parses back to exactly what the tool
/// returned.
fn corpus_response(requests: &[Value], tool_id: &str) -> Value {
    let block = find_tool_result(requests, tool_id)
        .unwrap_or_else(|| panic!("no tool_result posted for {tool_id}"));
    let text = unfence(&extract_result_text(&block), corpus_fence_source());
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{tool_id} returned text that is not JSON ({err}): {text}"))
}

/// The closing marker of the runtime's untrusted fence, in full.
///
/// The runtime wraps every tool result it hands the model between
/// `<untrusted-content source=tool:…>` and this, so what a driver serialises into a `tool_result`
/// block is the fence, not the tool's own bytes. Mirrors `FENCE_CLOSE` in murmur's
/// `crates/capsule-runtime/src/fence.rs`.
const FENCE_CLOSE: &str = "</untrusted-content>";

/// The `source=` name the runtime gives this tool's results.
///
/// Read from the same `murmur.yaml` [`pack`] packs rather than written out here, for the reason
/// the version is: a literal would name what this test expects and not what the artifact is
/// called, and the two would agree only until the artifact was renamed.
fn corpus_fence_source() -> &'static str {
    static SOURCE: OnceLock<String> = OnceLock::new();
    SOURCE.get_or_init(|| {
        let path = workspace_root().join(TOOL_CRATE).join("murmur.yaml");
        let manifest = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        format!("tool:{}", manifest_field(&manifest, "name"))
    })
}

/// Return what the untrusted fence wrapped, requiring that there was one.
///
/// The fence is asserted, not tolerated. Were it stripped only when present, this suite would go
/// on passing against a runtime that had stopped fencing tool output altogether: the JSON inside
/// would still parse, every assertion below would still hold, and the regression would ship with
/// four green tests over it. A missing opening marker, or one naming another source, fails here.
///
/// Only the *final* closing marker is stripped. The runtime rewrites any closer found inside the
/// content to `<!MURMUR-NEUTRALISED!/untrusted-content>`, so a fenced block closes exactly once,
/// at its own last marker — but matching the first closer instead would truncate the body of a
/// hostile payload that spelled one, and pass a half-record to `serde_json` as if that were what
/// the tool returned.
fn unfence(text: &str, expected_source: &str) -> String {
    let expected_open = format!("<untrusted-content source={expected_source}>");
    let Some((open, body)) = text.split_once('\n') else {
        panic!(
            "a tool result must arrive wrapped in the runtime's untrusted fence, opening with \
             `{expected_open}` on its own line; got a single line:\n{text}"
        )
    };
    assert_eq!(
        open, expected_open,
        "a tool result must open with the untrusted fence naming this tool; got:\n{text}"
    );
    body.strip_suffix(&format!("\n{FENCE_CLOSE}"))
        .unwrap_or_else(|| {
            panic!("a fenced tool result must end at `{FENCE_CLOSE}`; got:\n{text}")
        })
        .to_string()
}

/// The `tool_result` block a scripted server saw posted back for one `tool_use` id.
fn find_tool_result(requests: &[Value], tool_id: &str) -> Option<Value> {
    for request in requests {
        for message in request.get("messages")?.as_array()? {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_id)
                {
                    return Some(block.clone());
                }
            }
        }
    }
    None
}

/// A `tool_result` block's text. The Anthropic driver writes it as either a plain string or an
/// array of `{type: "text", text: …}` blocks, so both shapes have to be read here.
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

// ── the scripted inference endpoint ──────────────────────────────────────────

/// An HTTP server that answers each request with the next scripted response and keeps every
/// request it was sent, which is where the tool results are read back from.
struct ScriptedServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ScriptedServer {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);

        let join = thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request_body = read_http_request_body(&mut stream).unwrap_or_default();
                let parsed = serde_json::from_str::<Value>(&request_body)
                    .unwrap_or_else(|_| json!({"_raw": request_body}));
                requests_for_thread.lock().unwrap().push(parsed);

                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        Self { endpoint, requests, join: Some(join) }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        // Drop the handle without joining. A session that made fewer requests than were scripted
        // leaves the thread blocked in `accept`, and joining it would hang the test runner; the
        // detached thread goes away with the process.
        let _ = self.join.take();
    }
}

fn read_http_request_body(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(String::new());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index;
            break;
        }
    }

    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let mut parts = line.splitn(2, ':');
            let key = parts.next()?.trim().to_ascii_lowercase();
            let value = parts.next()?.trim();
            (key == "content-length")
                .then(|| value.parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(String::from_utf8_lossy(&body[..content_length]).to_string())
}

// ── assertions shared by the scenarios ───────────────────────────────────────

/// Nothing the corpus writes may appear anywhere under a session workdir, at any depth.
///
/// The corpus's core safety property is that it refuses rather than falling back to the workdir,
/// and a fallback would be invisible to every other assertion here: the store would work, and it
/// would be one the agent can rewrite at will. A recursive walk is the only check that sees it.
fn assert_workdir_holds_no_corpus(workdir: &Path) {
    assert!(
        workdir.is_dir(),
        "{} must be a session workdir",
        workdir.display()
    );
    let mut pending = vec![workdir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let file_type = entry.file_type().unwrap();

            assert!(
                !(file_type.is_dir() && name == STATE_DIR),
                "a '{STATE_DIR}' directory must never appear in a session workdir: {}",
                entry.path().display()
            );
            assert!(
                name != CORPUS_FILE,
                "the corpus must never write into a session workdir: {}",
                entry.path().display()
            );

            if file_type.is_dir() {
                pending.push(entry.path());
            }
        }
    }
}

/// Both directories a store needs are owner-only: the store itself, and the root above it, since a
/// readable root leaks the names of every capsule's store.
fn assert_store_is_private(staging: &Staging, store: &str) {
    for dir in [staging.state_root(), staging.store_dir(store)] {
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} must be 0700, got {mode:04o}", dir.display());
    }
}

// ── A — durability across sessions ───────────────────────────────────────────

/// Two `mur run` invocations, two session workdirs, one store: the second session searches back,
/// resolves and verifies records the first one appended and never saw again.
///
/// This is what shows the two halves agree on the preopen name. The corpus writes to the relative
/// guest path `state/corpus.jsonl` and the runtime mounts the store at the preopen `state`; if
/// those two literals disagreed, the first session's append would either fail or land in the
/// workdir, and the second session — a different directory entirely — would find nothing.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn corpus_records_survive_into_a_second_session() {
    let staging = stage(PROOF_CAPSULE, Some(PROOF_STATE_YAML), Some(OPERATOR_CONFIG));

    // Session one: two appends the session never reads back.
    let first_server = ScriptedServer::start(vec![
        tool_use_response(
            "call-append-1",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "a kestrel hunts over the estuary at dawn"},
            }),
        ),
        tool_use_response(
            "call-append-2",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "a heron waits in the estuary shallows"},
            }),
        ),
        end_turn_response("recorded both notes"),
    ]);
    staging.write_manifest(
        PROOF_CAPSULE,
        &first_server.endpoint,
        Some(PROOF_STATE_YAML),
        Some(OPERATOR_CONFIG),
    );
    let first_workdir = run_session(&staging, "Record two notes.");

    let first_requests = first_server.requests();
    let mut ids = Vec::new();
    for tool_id in ["call-append-1", "call-append-2"] {
        let response = corpus_response(&first_requests, tool_id);
        assert_eq!(response["ok"], json!(true), "{tool_id}: {response}");
        let id = response["id"].as_str().unwrap_or_default().to_string();
        assert!(!id.is_empty(), "{tool_id} must mint an id: {response}");
        ids.push(id);
    }
    assert_eq!(staging.corpus_lines(PROOF_STORE).len(), 2);

    // Session two: a second process, a second workdir, the same store.
    let second_server = ScriptedServer::start(vec![
        tool_use_response(
            "call-search",
            json!({"operation": "search", "query": "estuary"}),
        ),
        tool_use_response("call-get", json!({"operation": "get", "id": ids[0]})),
        tool_use_response("call-verify", json!({"operation": "verify"})),
        end_turn_response("found them"),
    ]);
    staging.write_manifest(
        PROOF_CAPSULE,
        &second_server.endpoint,
        Some(PROOF_STATE_YAML),
        Some(OPERATOR_CONFIG),
    );
    let second_workdir = run_session(&staging, "Find the notes.");

    assert_ne!(
        first_workdir, second_workdir,
        "each launch must get its own session workdir, or durability is not what was proved"
    );

    let second_requests = second_server.requests();
    let search = corpus_response(&second_requests, "call-search");
    assert_eq!(search["ok"], json!(true), "{search}");
    let hits = search["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("search must return hits: {search}"));
    let hit_ids: BTreeSet<&str> = hits.iter().filter_map(|hit| hit["id"].as_str()).collect();
    for id in &ids {
        assert!(
            hit_ids.contains(id.as_str()),
            "search must name {id}, got {hit_ids:?}"
        );
    }

    // A hit is not a record: it carries the matching text as an `excerpt`, and the body is what
    // `get` is for. A hit that carried a `body` would be the corpus spending the agent's context
    // on records it has not asked for yet.
    for hit in hits {
        assert!(
            hit.get("body").is_none(),
            "a search hit must not carry the record body: {hit}"
        );
        assert!(
            hit["excerpt"].as_str().is_some_and(|text| !text.is_empty()),
            "every hit must carry a non-empty excerpt: {hit}"
        );
    }
    let first_hit = hits
        .iter()
        .find(|hit| hit["id"].as_str() == Some(ids[0].as_str()))
        .unwrap_or_else(|| panic!("{} must be among the hits: {search}", ids[0]));
    assert_eq!(
        first_hit["excerpt"],
        json!("a kestrel hunts over the estuary at dawn"),
        "the excerpt must be the text that matched: {first_hit}"
    );

    let got = corpus_response(&second_requests, "call-get");
    assert_eq!(got["ok"], json!(true), "{got}");
    assert_eq!(got["record"]["id"], json!(ids[0]));
    assert_eq!(
        got["record"]["body"]["text"],
        json!("a kestrel hunts over the estuary at dawn"),
        "the body must be the one session one appended: {got}"
    );

    // `verify` reads the file the appends actually left behind, so it is the tool's own account of
    // the store the two sessions shared, and it needs the grant but not the configuration.
    let verified = corpus_response(&second_requests, "call-verify");
    assert_eq!(verified["ok"], json!(true), "{verified}");
    assert_eq!(verified["lines"], json!(2), "{verified}");
    assert_eq!(verified["records"], json!(2), "{verified}");
    assert_eq!(verified["bad_line_count"], json!(0), "{verified}");
    assert_eq!(verified["bad_lines"], json!([]), "{verified}");

    // Reading is not writing: two sessions in, the corpus is still the two lines session one left.
    assert_eq!(staging.corpus_lines(PROOF_STORE).len(), 2);
    assert_store_is_private(&staging, PROOF_STORE);
    for workdir in [&first_workdir, &second_workdir] {
        assert_workdir_holds_no_corpus(workdir);
    }
}

/// Scenario A with the `state:` block deleted and nothing else changed: the first append refuses
/// and the store it would have written to never comes into existence.
///
/// This is what makes Scenario A's pass attributable. Everything there — the config block, the
/// store name, both sessions — is held fixed, so a durability that survived the grant's removal
/// would be a durability coming from somewhere else, and Scenario A would be asserting nothing
/// about `capabilities.state` at all.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn the_durability_proof_fails_without_the_state_grant() {
    let staging = stage(PROOF_CAPSULE, None, Some(OPERATOR_CONFIG));

    let server = ScriptedServer::start(vec![
        tool_use_response(
            "call-append-1",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "a kestrel hunts over the estuary at dawn"},
            }),
        ),
        end_turn_response("the append refused"),
    ]);
    staging.write_manifest(PROOF_CAPSULE, &server.endpoint, None, Some(OPERATOR_CONFIG));
    let workdir = run_session(&staging, "Record two notes.");

    let response = corpus_response(&server.requests(), "call-append-1");
    assert_eq!(response["ok"], json!(false), "{response}");
    assert_eq!(
        response["error_kind"],
        json!("state_unavailable"),
        "the append Scenario A depends on must refuse for the missing grant: {response}"
    );
    assert!(
        !staging.store_dir(PROOF_STORE).exists(),
        "the store Scenario A reads back from must not exist without the grant"
    );
    assert_workdir_holds_no_corpus(&workdir);
}

// ── B — the default store name ───────────────────────────────────────────────

/// `capabilities.state: {}` with no `store:` lands in the *capsule's* directory, not the
/// artifact's.
///
/// The distinction is the whole reason the store name is read from the operator's manifest entry:
/// were it the artifact's name, every capsule that installed the corpus from a registry would land
/// in one shared `murmur-tool-corpus` directory and read each other's records with no grant on
/// either side.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn an_undeclared_store_name_defaults_to_the_capsule_name() {
    let capsule = "corpus-default-capsule";
    let staging = stage(capsule, Some(" {}"), Some(OPERATOR_CONFIG));

    let server = ScriptedServer::start(vec![
        tool_use_response(
            "call-append",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "the default store is the capsule's own name"},
            }),
        ),
        end_turn_response("recorded"),
    ]);
    staging.write_manifest(capsule, &server.endpoint, Some(" {}"), Some(OPERATOR_CONFIG));
    run_session(&staging, "Record one note.");

    let response = corpus_response(&server.requests(), "call-append");
    assert_eq!(response["ok"], json!(true), "{response}");
    assert_eq!(staging.corpus_lines(capsule).len(), 1);
    assert!(
        !staging.store_dir(&staging.tool.name).exists(),
        "the default store name is the capsule's, never the artifact's"
    );

    // The same resolution reaches the diagnostic, so an operator can see where their records will
    // land without launching anything.
    assert_eq!(
        explain_scope_json(&staging)["state_stores"],
        json!([{
            "artifact": staging.tool.name,
            "store": capsule,
            "host_path": staging.store_dir(capsule).display().to_string(),
        }])
    );
}

// ── C — refusal without the grant ────────────────────────────────────────────

/// With no `capabilities:` block on the tool entry, every corpus operation refuses by name and
/// nothing is written anywhere.
///
/// The corpus's own bundled `murmur.yaml` declares `capabilities: state: {}`, and this is where
/// that declaration is shown to grant nothing: the grant comes from the operator's manifest entry
/// or it does not come at all. `verify` is in the list because it is the one operation that runs
/// without the configuration — it still does not run without the grant.
///
/// The workdir half of the assertion is the load-bearing one. Without the grant the guest path
/// `state/` resolves inside the workdir preopen, so a corpus that created its own directory would
/// work perfectly and be worthless — a store the agent can rewrite at will.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn every_operation_refuses_without_the_state_grant() {
    let capsule = "corpus-ungranted-capsule";
    let staging = stage(capsule, None, None);

    // No operator configuration and no state root: a missing grant must be reported as a missing
    // grant, not as a missing configuration, so neither is put in place to be found.
    let calls = [
        (
            "call-append",
            json!({"operation": "append", "type": "note", "body": {"text": "unreachable"}}),
        ),
        ("call-get", json!({"operation": "get", "id": "not-1"})),
        (
            "call-read-recent",
            json!({"operation": "read_recent", "type": "note"}),
        ),
        (
            "call-search",
            json!({"operation": "search", "query": "estuary"}),
        ),
        ("call-verify", json!({"operation": "verify"})),
    ];
    let mut responses: Vec<String> = calls
        .iter()
        .map(|(id, input)| tool_use_response(id, input.clone()))
        .collect();
    responses.push(end_turn_response("every call refused"));

    let server = ScriptedServer::start(responses);
    staging.write_manifest(capsule, &server.endpoint, None, None);

    // The tool refuses; it neither traps nor fails the session.
    let workdir = run_session(&staging, "Try every operation.");

    let requests = server.requests();
    for (tool_id, _) in &calls {
        let response = corpus_response(&requests, tool_id);
        assert_eq!(response["ok"], json!(false), "{tool_id}: {response}");
        assert_eq!(
            response["error_kind"],
            json!("state_unavailable"),
            "{tool_id} must refuse for the missing grant, not for anything found beyond it: \
             {response}"
        );
        let message = response["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("capabilities.state"),
            "{tool_id} must name the declaration the operator has to add: {message}"
        );
    }

    assert!(
        !staging.state_root().exists(),
        "default-deny must bring no part of the state tree into existence"
    );
    assert_workdir_holds_no_corpus(&workdir);
}
