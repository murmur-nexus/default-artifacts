//! Host-target integration test for the compiled `tool-github` **wasm32-wasip2
//! component**, run through Wasmtime with `wasi:http` linked.
//!
//! Two things can only be proved here, not by the host unit/scenario tests against `ops::run`
//! directly: the component's `wasi:http` transport really does address only
//! `MURMUR_GATEWAY_ENDPOINT`, carrying the standard headers and no credential header of its
//! own making; and the gateway address is read out of the guest environment, its absence
//! failing closed with nothing sent.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_http::WasiHttpCtx;

mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput, ToolResult};
use bindings::Tool;

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

static BUILD: Once = Once::new();

/// Path to the compiled component, building it if absent. `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so the test cannot assume the artifact
/// already exists.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/tool_github.wasm");

    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-github",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(manifest.join("../.."))
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(status.success(), "cargo build of the wasm component failed");
    });

    assert!(
        wasm.exists(),
        "compiled component not found at {}",
        wasm.display()
    );
    wasm
}

fn run_component(gateway: Option<&str>, config: Option<&str>, data: &str) -> ToolResult {
    let workdir = tempfile::tempdir().unwrap();
    run_component_in_dir(workdir.path(), gateway, config, data)
}

/// Like [`run_component`], but preopens `dir` itself as the guest's `.` rather than a fresh
/// empty tempdir — so `push` reading a real repository's objects (loose and packed alike) goes
/// through actual WASI filesystem calls inside the compiled `wasm32-wasip2` component, not just
/// a host-native `cargo test`.
fn run_component_in_dir(
    dir: &Path,
    gateway: Option<&str>,
    config: Option<&str>,
    data: &str,
) -> ToolResult {
    let mut engine_config = Config::new();
    engine_config.wasm_component_model(true);
    let engine = Engine::new(&engine_config).expect("engine");
    let component = Component::from_file(&engine, component_path()).expect("load component");

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker).expect("add wasi:http");

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(dir, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen");
    if let Some(gateway) = gateway {
        builder.env(tool_github::GATEWAY_ENDPOINT_ENV, gateway);
    }
    if let Some(config) = config {
        builder.env(tool_github::ARTIFACT_CONFIG_ENV, config);
    }
    let mut store = Store::new(
        &engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
            http: WasiHttpCtx::new(),
        },
    );
    let tool = Tool::instantiate(&mut store, &component, &linker).expect("instantiate");
    let input = ToolInput {
        data: Some(data.to_string()),
        log_path: None,
    };
    tool.murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run")
}

/// [`run_component_in_dir`] on a spawned thread, failing once `deadline` passes: a guest that
/// writes a body over the host's two 1 MiB body chunks before dispatching the request never
/// returns, because nothing drains the body until the request is handed to the host.
fn run_component_within(
    dir: PathBuf,
    gateway: Option<String>,
    config: Option<String>,
    data: String,
    deadline: Duration,
) -> ToolResult {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = run_component_in_dir(&dir, gateway.as_deref(), config.as_deref(), &data);
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "the component was still running after {deadline:?}: the request body never left \
             the guest"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the component thread panicked; see its message above")
        }
    }
}

#[derive(Debug, Clone)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Value,
}

struct FakeGithub {
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeGithub {
    fn start(status_line: &'static str, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                if let Some(request) = read_request(&mut stream) {
                    recorded.lock().unwrap().push(request);
                }
                let response = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
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
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

const CONFIG: &str = r#"{"config_version":1,"repo":"acme/widgets"}"#;

#[test]
fn pr_view_is_one_get_carrying_the_standard_headers_and_no_credential() {
    let pr = json!({
        "number": 1, "state": "open", "mergeable": true, "title": "t", "body": "b",
        "user": {"login": "someone"},
        "head": {"sha": "a".repeat(40), "ref": "feature-x"},
        "base": {"ref": "main"},
    });
    // pr_view issues three GETs; the fake answers every request the same way, which is enough
    // for the header/credential assertions below (the reviews/check-runs bodies do not parse
    // as those shapes, so `checks`/`reviews` come back empty — irrelevant here).
    let fake = FakeGithub::start("HTTP/1.1 200 OK", pr.to_string());
    let result = run_component(
        Some(&fake.endpoint),
        Some(CONFIG),
        r#"{"op":"pr_view","number":1}"#,
    );

    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    let requests = fake.requests();
    assert!(
        requests[0]
            .request_line
            .starts_with("GET /repos/acme/widgets/pulls/1 "),
        "{}",
        requests[0].request_line
    );
    let names: Vec<&str> = requests[0]
        .headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(names.contains(&"user-agent"), "{names:?}");
    let user_agent = requests[0]
        .headers
        .iter()
        .find(|(n, _)| n == "user-agent")
        .map(|(_, v)| v.as_str());
    assert_eq!(user_agent, Some("tool-github"));
    assert!(names.contains(&"accept"), "{names:?}");
    assert!(names.contains(&"x-github-api-version"), "{names:?}");
    for forbidden in ["authorization", "x-api-key"] {
        assert!(
            !names.contains(&forbidden),
            "the tool must send no {forbidden} header: {names:?}"
        );
    }
}

#[test]
fn an_unset_or_empty_gateway_is_refused_with_nothing_sent() {
    let fake = FakeGithub::start("HTTP/1.1 200 OK", json!({}).to_string());
    for gateway in [None, Some("")] {
        let result = run_component(gateway, Some(CONFIG), r#"{"op":"pr_view","number":1}"#);
        assert!(matches!(result.status, Status::Error));
        let data: Value = serde_json::from_str(&result.data.unwrap()).unwrap();
        assert_eq!(data["error_kind"], json!("gateway_missing"), "{gateway:?}");
    }
    assert!(fake.requests().is_empty(), "nothing may reach the upstream");
}

fn created_pr() -> Value {
    json!({
        "number": 9, "state": "open", "mergeable": null, "title": "Add feature", "body": "b",
        "user": {"login": "someone"},
        "head": {"sha": "c".repeat(40), "ref": "feature-x"},
        "base": {"ref": "main"},
    })
}

#[test]
fn pr_create_with_no_footer_sends_the_body_untouched() {
    let fake = FakeGithub::start("HTTP/1.1 201 Created", created_pr().to_string());
    let input = r#"{"op":"pr_create","title":"Add feature","body":"does the thing","head":"feature-x","base":"main"}"#;
    let result = run_component(Some(&fake.endpoint), Some(CONFIG), input);
    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );

    let requests = fake.requests();
    assert!(
        requests[0]
            .request_line
            .starts_with("POST /repos/acme/widgets/pulls "),
        "{}",
        requests[0].request_line
    );
    assert_eq!(requests[0].body["body"], json!("does the thing"));
}

#[test]
fn pr_create_with_a_footer_line_sends_it_appended() {
    let fake = FakeGithub::start("HTTP/1.1 201 Created", created_pr().to_string());
    let config =
        r#"{"config_version":1,"repo":"acme/widgets","pr_footer":"Opened by our release bot"}"#;
    let input = r#"{"op":"pr_create","title":"Add feature","body":"does the thing","head":"feature-x","base":"main"}"#;
    let result = run_component(Some(&fake.endpoint), Some(config), input);
    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );

    let requests = fake.requests();
    assert_eq!(
        requests[0].body["body"],
        json!("does the thing\n\nOpened by our release bot")
    );
}

// ── the mandatory runtime spike: reading real packfiles inside the compiled wasm32-wasip2 ──
// ── component, through actual WASI filesystem calls under Wasmtime, not just `cargo test` ──

fn base64_decode(text: &str) -> Vec<u8> {
    const TABLE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let clean: Vec<u8> = text
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let values: Vec<u32> = chunk
            .iter()
            .map(|&b| TABLE.find(b as char).unwrap() as u32)
            .collect();
        let n = values
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, v)| acc | (v << (18 - 6 * i)));
        out.push((n >> 16) as u8);
        if values.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if values.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn hex_decode(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn object_sha(kind: &str, content: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let header = format!("{kind} {}\0", content.len());
    let mut store = header.into_bytes();
    store.extend_from_slice(content);
    let mut hasher = Sha1::new();
    hasher.update(&store);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn identity_tail(name: &str, email: &str, date: &str) -> String {
    let year: i64 = date[0..4].parse().unwrap();
    let month: i64 = date[5..7].parse().unwrap();
    let day: i64 = date[8..10].parse().unwrap();
    let hour: i64 = date[11..13].parse().unwrap();
    let minute: i64 = date[14..16].parse().unwrap();
    let second: i64 = date[17..19].parse().unwrap();
    let sign = &date[19..20];
    let oh: i64 = date[20..22].parse().unwrap();
    let om: i64 = date[23..25].parse().unwrap();
    let offset_minutes = if sign == "-" {
        -(oh * 60 + om)
    } else {
        oh * 60 + om
    };
    let days = days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    let seconds = local_seconds - offset_minutes * 60;
    format!("{name} <{email}> {seconds} {sign}{oh:02}{om:02}")
}

/// A real (TCP, threaded) fake GitHub that recomputes git object hashes for every blob/tree/
/// commit it is asked to create, so a passing push through it proves the compiled component
/// really did read and correctly reconstruct the fixture's packed and loose objects — not that
/// some canned sha was echoed back.
struct EchoGithub {
    endpoint: String,
    refs: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

impl EchoGithub {
    fn start(default_branch: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let refs = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let refs_thread = Arc::clone(&refs);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let (status_line, body) = handle(&request, default_branch, &refs_thread);
                let response = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { endpoint, refs }
    }
}

fn handle(
    request: &Recorded,
    default_branch: &str,
    refs: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) -> (&'static str, String) {
    let mut parts = request.request_line.split(' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let body = &request.body;

    if path.ends_with("/git/blobs") {
        let content = base64_decode(body["content"].as_str().unwrap());
        return (
            "HTTP/1.1 201 Created",
            json!({"sha": object_sha("blob", &content)}).to_string(),
        );
    }
    if path.ends_with("/git/trees") {
        let mut content = Vec::new();
        for entry in body["tree"].as_array().unwrap() {
            // Like GitHub: only six-digit modes are accepted, and a directory is stored as `40000`.
            let mode = match entry["mode"].as_str().unwrap() {
                "040000" => "40000",
                mode @ ("100644" | "100755" | "120000" | "160000") => mode,
                _ => {
                    return (
                        "HTTP/1.1 422 Unprocessable Entity",
                        json!({"message": "Must supply a valid tree.mode"}).to_string(),
                    )
                }
            };
            let name = entry["path"].as_str().unwrap();
            let sha = entry["sha"].as_str().unwrap();
            content.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            content.extend_from_slice(&hex_decode(sha));
        }
        return (
            "HTTP/1.1 201 Created",
            json!({"sha": object_sha("tree", &content)}).to_string(),
        );
    }
    if path.ends_with("/git/commits") {
        let mut header = format!("tree {}\n", body["tree"].as_str().unwrap());
        for parent in body["parents"].as_array().unwrap() {
            header.push_str(&format!("parent {}\n", parent.as_str().unwrap()));
        }
        let author = &body["author"];
        let committer = &body["committer"];
        header.push_str(&format!(
            "author {}\n",
            identity_tail(
                author["name"].as_str().unwrap(),
                author["email"].as_str().unwrap(),
                author["date"].as_str().unwrap()
            )
        ));
        header.push_str(&format!(
            "committer {}\n",
            identity_tail(
                committer["name"].as_str().unwrap(),
                committer["email"].as_str().unwrap(),
                committer["date"].as_str().unwrap()
            )
        ));
        let content = format!("{header}\n{}", body["message"].as_str().unwrap());
        return (
            "HTTP/1.1 201 Created",
            json!({"sha": object_sha("commit", content.as_bytes())}).to_string(),
        );
    }
    if path.contains("/git/refs/heads/") && method == "GET" {
        let branch = path.rsplit('/').next().unwrap();
        return match refs.lock().unwrap().get(branch) {
            Some(sha) => (
                "HTTP/1.1 200 OK",
                json!({"object": {"sha": sha}}).to_string(),
            ),
            None => (
                "HTTP/1.1 404 Not Found",
                json!({"message": "not found"}).to_string(),
            ),
        };
    }
    if path.ends_with("/git/refs") && method == "POST" {
        let ref_name = body["ref"].as_str().unwrap();
        let branch = ref_name.strip_prefix("refs/heads/").unwrap();
        let sha = body["sha"].as_str().unwrap().to_string();
        refs.lock().unwrap().insert(branch.to_string(), sha.clone());
        return (
            "HTTP/1.1 201 Created",
            json!({"ref": ref_name, "object": {"sha": sha}}).to_string(),
        );
    }
    if path == "/repos/acme/widgets" {
        return (
            "HTTP/1.1 200 OK",
            json!({"default_branch": default_branch}).to_string(),
        );
    }
    (
        "HTTP/1.1 404 Not Found",
        json!({"message": format!("unhandled path {path}")}).to_string(),
    )
}

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The mandatory runtime spike this slice's design required before writing anything else:
/// `gix_pack::index::File::from_data`/`gix_pack::data::File::from_data`, fed bytes read with
/// `std::fs::read` (never `at(path)`, a no-op mmap stub on WASI), reading a commit, a tree and
/// an `OFS_DELTA`-compressed blob — for real, inside the compiled `wasm32-wasip2` component,
/// executing under Wasmtime with WASI filesystem access to a real cloned-and-repacked
/// repository. It passed: this test is that proof, kept as permanent regression coverage
/// rather than a throwaway.
#[test]
fn the_wasm_component_reads_a_real_packed_and_loose_repository() {
    let git_dir = fixture_dir("packed_clone");
    let fake = EchoGithub::start("main");
    let head_sha = "f310270529aa5bd40cc65dfba01b276bc296e087";
    let input = format!(r#"{{"op":"push","head_sha":"{head_sha}","remote_branch":"feature-x"}}"#);
    let result = run_component_in_dir(&git_dir, Some(&fake.endpoint), Some(CONFIG), &input);

    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    assert_eq!(
        fake.refs.lock().unwrap().get("feature-x"),
        Some(&head_sha.to_string()),
        "the fake independently recomputed the same sha for every object the guest uploaded, \
         proving the packed and loose objects were read and reconstructed correctly"
    );
}

/// The same proof again over `tests/fixtures/mixed_cross_pack`: loose objects, an intra-pack
/// `OFS_DELTA` blob, and a `REF_DELTA` blob whose base lives only in a *different* opened pack.
#[test]
fn the_wasm_component_resolves_a_cross_pack_ref_delta() {
    let git_dir = fixture_dir("mixed_cross_pack");
    let fake = EchoGithub::start("main");
    let head_sha = "9ff22a1025d8dc260a991c94d2680b14bd7e9608";
    let input = format!(r#"{{"op":"push","head_sha":"{head_sha}","remote_branch":"feature-x"}}"#);
    let result = run_component_in_dir(&git_dir, Some(&fake.endpoint), Some(CONFIG), &input);

    assert!(
        matches!(result.status, Status::Passed),
        "{:?}",
        result.summary
    );
    assert_eq!(
        fake.refs.lock().unwrap().get("feature-x"),
        Some(&head_sha.to_string())
    );
}

// ── fetch: rebuilt objects and the remote-tracking ref written through WASI ──────────────────

/// A fake GitHub answering each request by its path from a fixed table, for a guest whose
/// requests are known in advance.
struct RoutedGithub {
    endpoint: String,
}

impl RoutedGithub {
    fn start(routes: std::collections::HashMap<String, String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let path = request
                    .request_line
                    .split(' ')
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let (status_line, body) = match routes.get(&path) {
                    Some(body) => ("HTTP/1.1 200 OK", body.clone()),
                    None => (
                        "HTTP/1.1 404 Not Found",
                        json!({"message": format!("unhandled path {path}")}).to_string(),
                    ),
                };
                let response = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { endpoint }
    }
}

/// GitHub's read API over every object reachable from `tip` in `git_dir`: the branch ref, one
/// GraphQL history page, and each tree and (text) blob.
fn read_routes(git_dir: &std::path::Path, tip: &str) -> std::collections::HashMap<String, String> {
    use tool_github::git::date::to_iso8601;
    use tool_github::git::object::{parse_commit, parse_tree, ObjectKind};

    let store = tool_github::git::store::ObjectStore::open(git_dir).unwrap();
    let mut routes = std::collections::HashMap::new();
    routes.insert(
        "/repos/acme/widgets/git/refs/heads/main".to_string(),
        json!({"object": {"sha": tip}}).to_string(),
    );
    let mut history = Vec::new();
    let mut stack = vec![tip.to_string()];
    while let Some(sha) = stack.pop() {
        let raw = store.read(&sha).unwrap();
        match raw.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&raw.content).unwrap();
                history.push(json!({
                    "oid": sha,
                    "message": commit.message.strip_suffix('\n').unwrap(),
                    "tree": {"oid": commit.tree},
                    "parents": {"nodes": commit.parents.iter().map(|p| json!({"oid": p})).collect::<Vec<_>>()},
                    "author": {"name": commit.author.name, "email": commit.author.email, "date": to_iso8601(&commit.author)},
                    "committer": {"name": commit.committer.name, "email": commit.committer.email, "date": to_iso8601(&commit.committer)},
                    "signature": null,
                }));
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            ObjectKind::Tree => {
                let entries = parse_tree(&raw.content).unwrap();
                let listed: Vec<Value> = entries
                    .iter()
                    .map(|e| json!({"path": e.name, "mode": e.api_mode(), "type": e.api_type(), "sha": e.sha}))
                    .collect();
                routes.insert(
                    format!("/repos/acme/widgets/git/trees/{sha}"),
                    json!({"sha": sha, "tree": listed, "truncated": false}).to_string(),
                );
                stack.extend(entries.into_iter().map(|e| e.sha));
            }
            _ => {
                let content = String::from_utf8(raw.content).unwrap();
                routes.insert(
                    format!("/repos/acme/widgets/git/blobs/{sha}"),
                    json!({"sha": sha, "content": content, "encoding": "utf-8"}).to_string(),
                );
            }
        }
    }
    routes.insert(
        "/graphql".to_string(),
        json!({"data": {"repository": {"object": {"history": {"nodes": history}}}}}).to_string(),
    );
    routes
}

#[test]
fn the_wasm_component_fetches_a_branch_into_an_empty_repository() {
    let head_sha = "f310270529aa5bd40cc65dfba01b276bc296e087";
    let fake = RoutedGithub::start(read_routes(&fixture_dir("packed_clone"), head_sha));
    let target = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(target.path().join("objects")).unwrap();

    let result = run_component_in_dir(
        target.path(),
        Some(&fake.endpoint),
        Some(CONFIG),
        r#"{"op":"fetch","branch":"main"}"#,
    );
    assert!(
        matches!(result.status, Status::Passed),
        "{:?} {:?}",
        result.summary,
        result.data
    );

    assert_eq!(
        std::fs::read_to_string(target.path().join("refs/remotes/origin/main")).unwrap(),
        format!("{head_sha}\n")
    );
    let store = tool_github::git::store::ObjectStore::open(target.path()).unwrap();
    let source_dir = fixture_dir("packed_clone");
    let source = tool_github::git::store::ObjectStore::open(&source_dir).unwrap();
    let mut stack = vec![head_sha.to_string()];
    while let Some(sha) = stack.pop() {
        let fetched = store
            .read(&sha)
            .unwrap_or_else(|e| panic!("{sha} was not written: {e:?}"));
        assert_eq!(fetched.content, source.read(&sha).unwrap().content, "{sha}");
        match fetched.kind {
            tool_github::git::object::ObjectKind::Commit => {
                let commit = tool_github::git::object::parse_commit(&fetched.content).unwrap();
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            tool_github::git::object::ObjectKind::Tree => {
                stack.extend(
                    tool_github::git::object::parse_tree(&fetched.content)
                        .unwrap()
                        .into_iter()
                        .map(|e| e.sha),
                );
            }
            _ => {}
        }
    }
}

// ── request bodies past the host's body buffer ───────────────────────────────────────────────

/// How long a large-body call may take before it counts as the hang: the body is a few MiB on
/// loopback, so a healthy call finishes in seconds.
const LARGE_BODY_DEADLINE: Duration = Duration::from_secs(90);

/// `n` bytes that do not repeat over any short period, so neither git nor base64 shrinks them.
fn noise(n: usize) -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// `git <args>` in `dir`, isolated from the user's and the system's git config; panics unless
/// it succeeds and returns stdout, trimmed.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("spawning git");
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A 2.5 MiB file uploads as a blob whose base64 body is about 3.4 MiB, past both of the host's
/// 1 MiB body chunks. The fake recomputes every sha from what it received, so the ref it ends
/// up holding the head commit proves the blob arrived whole.
#[test]
fn a_push_whose_blob_body_passes_two_mib_completes() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("big.bin"), noise(5 << 19)).unwrap();
    git(repo.path(), &["add", "big.bin"]);
    git(
        repo.path(),
        &[
            "-c",
            "user.name=Large Body",
            "-c",
            "user.email=large-body@example.invalid",
            "commit",
            "-q",
            "-m",
            "add a file larger than two MiB",
        ],
    );
    let head_sha = git(repo.path(), &["rev-parse", "HEAD"]);

    let fake = EchoGithub::start("main");
    let result = run_component_within(
        repo.path().join(".git"),
        Some(fake.endpoint.clone()),
        Some(CONFIG.to_string()),
        format!(r#"{{"op":"push","head_sha":"{head_sha}","remote_branch":"big-blob"}}"#),
        LARGE_BODY_DEADLINE,
    );

    assert!(
        matches!(result.status, Status::Passed),
        "{:?} {:?}",
        result.summary,
        result.data
    );
    assert_eq!(
        fake.refs.lock().unwrap().get("big-blob"),
        Some(&head_sha),
        "the fake recomputed the same sha for every object the guest uploaded"
    );
}

/// A gateway address that refuses connections: the host drops the request, and the body
/// channel with it, without reading any of the body. A 3 MiB body keeps the guest blocked in
/// its write loop until then, so the write deterministically sees the stream closed, and the
/// result must report the host's refusal rather than a body-write error.
#[test]
fn a_request_refused_before_its_body_is_read_reports_the_refusal() {
    let refused = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let input = json!({
        "op": "pr_create",
        "title": "Add feature",
        "body": "a long pull request description ".repeat(3 << 15),
        "head": "feature-x",
        "base": "main",
    });
    let workdir = tempfile::tempdir().unwrap();
    let result = run_component_within(
        workdir.path().to_path_buf(),
        Some(refused),
        Some(CONFIG.to_string()),
        input.to_string(),
        LARGE_BODY_DEADLINE,
    );

    assert!(!matches!(result.status, Status::Passed), "{result:?}");
    let data: Value = serde_json::from_str(result.data.as_deref().unwrap()).unwrap();
    assert_eq!(data["error_kind"], json!("transport_error"), "{data}");
    let text = format!("{} {data}", result.summary.as_deref().unwrap_or_default());
    assert!(text.contains("ConnectionRefused"), "{text}");
    for body_write_error in [
        "check-write failed",
        "write failed",
        "flush failed",
        "failed to finalize request body",
    ] {
        assert!(
            !text.contains(body_write_error),
            "the refusal must not surface as a body-write error: {text}"
        );
    }
}
