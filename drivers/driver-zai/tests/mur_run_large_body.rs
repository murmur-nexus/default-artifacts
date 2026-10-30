//! A request body over 5 MiB, proved to leave driver-zai under a real `mur run`.
//!
//! The host buffers an outgoing request body in two 1 MiB chunks, and only the send that
//! `outgoing-handler.handle` starts drains them. A driver that writes the whole body before it
//! dispatches the request therefore blocks forever once its body passes 1 MiB, with no
//! connection open. Only a launch shows the order the guest really calls those imports in: the
//! unit tests never compile `mod wasm_driver`.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Z.ai that reads each request whole by its
//! `content-length` and answers HTTP 400 naming the byte count it received. The driver is built
//! out of this workspace on every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that
//! finds neither fails rather than skipping.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=… cargo test -p driver-zai --test mur_run_large_body -- --ignored` runs it.
//!
//! Unix only: it spawns a host `mur` and reads Unix permission bits, so a `wasm32-wasip2` build
//! of the test targets compiles it to nothing.
#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, Once},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

const DRIVER_CRATE: &str = "drivers/driver-zai";

/// The model the capsule names.
const MODEL: &str = "glm-5.3";

/// The path the driver posts to under the fake's endpoint.
const REQUEST_PATH: &str = "/v1/chat/completions";

/// The smallest task, and so the smallest request body, the test sends: 5 MiB.
const TASK_BYTES: usize = 5 * 1024 * 1024;

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

fn build_driver() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                "driver-zai",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("failed to spawn `cargo build` for driver-zai");
        assert!(status.success(), "cargo build of driver-zai failed");
    });
}

/// A top-level scalar in the driver's `murmur.yaml`, by line scan.
fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

/// Pack the driver with `scripts/package-artifact.sh`; returns its version and zip.
fn pack(out_dir: &Path) -> (String, PathBuf) {
    build_driver();
    let crate_path = workspace_root().join(DRIVER_CRATE);
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
        .stdout(Stdio::null())
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
    version: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    /// The capsule manifest: [`MODEL`] behind `endpoint`, with the fake's key.
    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: large-body-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-zai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 inference:\n  transport: http\n  model: {MODEL}\n  max_tokens: 16384\n  \
                 driver:\n    artifact: local/driver-zai\n",
                self.version
            ),
        )
        .unwrap();
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (version, zip) = pack(artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        version,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");

    let mut publish = mur(&staging.home);
    publish.arg("publish").arg(&zip);
    run_to_success(publish, "publishing driver-zai");

    let mut install = mur(&staging.home);
    install
        .current_dir(staging.project.path())
        .arg("install")
        .arg(&zip);
    run_to_success(install, "installing driver-zai");
    staging
}

/// How long the run may take before it counts as the hang: the body is a few MiB on loopback,
/// so a healthy run finishes in seconds.
const LARGE_BODY_DEADLINE: Duration = Duration::from_secs(180);

/// Run the task held in `task_file`, polling `mur run` every 250 ms. Once `deadline` passes,
/// kill it by its PID and panic: a guest that writes the whole body before dispatching never
/// opens a connection once the body outgrows the host's buffer, so the run never ends.
fn run_task_file_within(
    staging: &Staging,
    task_file: &Path,
    provider: &Provider,
    deadline: Duration,
) -> Output {
    let workdir = staging.project.path().join("work");
    fs::create_dir_all(&workdir).unwrap();
    let stdout_path = staging.project.path().join("mur-run.stdout");
    let stderr_path = staging.project.path().join("mur-run.stderr");
    let mut child = mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            task_file.to_str().unwrap(),
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout_path).unwrap())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawning `mur run`");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("polling `mur run`") {
            break status;
        }
        if started.elapsed() >= deadline {
            let pid = child.id();
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the request body never left the guest: `mur run` (pid {pid}) was still running \
                 after {deadline:?} and was killed; the fake recorded {} request(s)\n--- stdout \
                 ---\n{}\n--- stderr ---\n{}",
                provider.requests().len(),
                fs::read_to_string(&stdout_path).unwrap_or_default(),
                fs::read_to_string(&stderr_path).unwrap_or_default()
            );
        }
        thread::sleep(Duration::from_millis(250));
    };
    eprintln!("`mur run` ended in {:?}", started.elapsed());
    Output {
        status,
        stdout: fs::read(&stdout_path).unwrap(),
        stderr: fs::read(&stderr_path).unwrap(),
    }
}

/// Write a task of at least `min_bytes` of English prose into the staging project, opening with
/// `marker` on its own line. Plain ASCII prose, so JSON escaping does not inflate the body.
fn write_prose_task(staging: &Staging, marker: &str, min_bytes: usize) -> PathBuf {
    const SENTENCES: [&str; 8] = [
        "The river keeps its course past the old mill, and the miller counts the sacks again.",
        "A gardener walks the rows at dawn, pulling weeds before the sun climbs over the wall.",
        "Every evening the baker banks the oven and writes tomorrow's order on a slate.",
        "The ferry waits at the pier while a boy carries crates of apples down the ramp.",
        "Rain drums on the station roof as the last train of the night pulls slowly away.",
        "In the library a clerk files returned books and sweeps the dust from the shelves.",
        "The shepherd whistles twice, and the dog brings the flock down from the high field.",
        "A lighthouse keeper trims the wick, then logs the wind and the height of the tide.",
    ];
    let mut task = format!("{marker}\n");
    let mut paragraph = 0usize;
    while task.len() < min_bytes {
        paragraph += 1;
        task.push_str(&format!("Paragraph {paragraph}. "));
        for offset in 0..SENTENCES.len() {
            task.push_str(SENTENCES[(paragraph + offset) % SENTENCES.len()]);
            task.push(' ');
        }
        task.push('\n');
    }
    let path = staging.project.path().join("large-task.md");
    fs::write(&path, task).unwrap();
    path
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_trap(text: &str) {
    assert!(
        !text.contains("E-RUN-007") && !text.contains("trapped"),
        "the guest must not trap:\n{text}"
    );
}

// ── the fake provider ────────────────────────────────────────────────────────

/// One request as the fake received it, header names lowercased.
#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The fake's answer to a request whose body held `received` bytes. A 400, never a 429 or 5xx:
/// murmur retries those, and the test wants exactly one request.
fn bad_request(received: usize) -> String {
    let payload = format!(
        r#"{{"error":{{"message":"fake: received {received} bytes","type":"invalid_request_error"}}}}"#
    );
    format!(
        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{payload}",
        payload.len()
    )
}

/// A loopback server that reads every request whole, records it, and answers [`bad_request`].
struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let response = bad_request(request.body.len());
                recorded.lock().unwrap().push(request);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { port, requests }
    }

    /// The `gateway.endpoint` the capsule names for this fake.
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
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
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
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
        path,
        headers,
        body,
    })
}

// ── scenario ─────────────────────────────────────────────────────────────────

/// A task of 5 MiB makes a request body over 5 MiB, far more than the host's two 1 MiB body
/// chunks hold: the driver must dispatch the request before it writes the body, or the write
/// loop blocks forever with no connection open. The fake's 400 ends the run, carrying the byte
/// count it received back through the driver's error.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_five_mib_request_body_reaches_the_provider() {
    let staging = stage();
    let provider = Provider::start();
    staging.write_manifest(&provider.endpoint());
    let marker = format!("LARGE-BODY-MARKER-{TASK_BYTES}-{}", std::process::id());
    let task_file = write_prose_task(&staging, &marker, TASK_BYTES);

    let output = run_task_file_within(&staging, &task_file, &provider, LARGE_BODY_DEADLINE);
    let text = combined(&output);
    let requests = provider.requests();

    assert!(
        !output.status.success(),
        "the fake's 400 must fail the run:\n{text}"
    );
    assert_no_trap(&text);
    assert_eq!(requests.len(), 1, "one request, never retried:\n{text}");

    let request = &requests[0];
    assert_eq!(request.path, REQUEST_PATH);
    let content_length: usize = request
        .header("content-length")
        .expect("the request carries content-length")
        .parse()
        .expect("content-length is a number");
    eprintln!(
        "model: {MODEL}; path: {}; task bytes: {}; body bytes received: {}; content-length: \
         {content_length}; exit: {}",
        request.path,
        fs::metadata(&task_file).unwrap().len(),
        request.body.len(),
        output.status
    );
    assert_eq!(
        request.body.len(),
        content_length,
        "the fake must receive the whole body"
    );
    assert!(
        content_length >= TASK_BYTES,
        "body of {content_length} bytes is under {TASK_BYTES}"
    );
    let error = text
        .lines()
        .find(|line| line.contains("HTTP 400"))
        .unwrap_or_else(|| panic!("the run must report the fake's HTTP 400:\n{text}"));
    eprintln!("error: {error}");
    assert!(
        text.contains(&format!("fake: received {content_length} bytes")),
        "the driver's error must carry the fake's byte count:\n{text}"
    );
    let body: Value = serde_json::from_slice(&request.body).expect("the request body is JSON");
    assert!(
        body.to_string().contains(&marker),
        "the request body must carry the task's marker"
    );
}
