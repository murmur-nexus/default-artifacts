//! The built `tool-registry-search` binary, driven the way the runtime drives a native tool:
//! the `{"data": …}` envelope on stdin, one JSON line on stdout, and a chosen working directory.
//!
//! HTTP is served by `std`-only loopback stubs. The `https_*` tests reach github.com and are
//! `#[ignore]`d; `cargo test -p tool-registry-search --test search_index -- --ignored https_`
//! runs them.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const FIXTURE: &str = "tests/fixtures/artifacts-index.json";
const SCHEMA_1_FIXTURE: &str = "tests/fixtures/artifacts-index-schema1.json";
const SCHEMA_1_HINT: &str = "`mur index <dir> --repo <owner>/<repo> --release <tag>`";
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;

/// Keys that would make the tool a judge of fit or a source of install material.
const FORBIDDEN_KEYS: &[&str] = &[
    "fit",
    "newest_fitting",
    "unserved",
    "wit_level",
    "platform",
    "interfaces",
    "assets",
    "file",
    "sha256",
    "published_date",
];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Runs the tool with `data` as the envelope's `data`, in `cwd`, and returns its output line.
fn tool_in(cwd: &Path, data: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tool-registry-search"))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning tool-registry-search");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({ "data": data, "log_path": null })
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "tool exited {}", output.status);
    let stdout = String::from_utf8(output.stdout).unwrap();
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("tool printed non-JSON ({e}): {stdout}"))
}

fn tool(data: Value) -> Value {
    tool_in(&crate_dir(), data)
}

fn search_fixture(query: &str) -> Value {
    let out = tool(json!({ "query": query, "index": FIXTURE }));
    assert_eq!(out["ok"], true, "{out}");
    out
}

fn result_names(out: &Value) -> Vec<String> {
    out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

fn message(out: &Value) -> &str {
    out["message"].as_str().unwrap()
}

fn assert_error(out: &Value, kind: &str, needles: &[&str]) {
    assert_eq!(out["ok"], false, "{out}");
    assert_eq!(out["error_kind"], kind, "{out}");
    for needle in needles {
        assert!(message(out).contains(needle), "{needle:?} not in {out}");
    }
}

fn assert_no_forbidden_keys(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    !FORBIDDEN_KEYS.contains(&key.as_str()),
                    "output carries the forbidden key {key:?}"
                );
                assert_no_forbidden_keys(child);
            }
        }
        Value::Array(items) => items.iter().for_each(assert_no_forbidden_keys),
        _ => {}
    }
}

fn the_acceptance_result(out: &Value) {
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["scope"], "murmur");
    assert_eq!(out["count"], 1);
    assert_eq!(out["results"][0]["name"], "murmur/tool-editor");
    assert_eq!(
        out["results"][0]["versions"],
        json!([
            {"version": "0.7.0", "release": "v0.26.0", "built_at_level": 1, "platforms": ["any"]},
            {"version": "0.6.0", "release": "v0.25.0", "built_at_level": 1, "platforms": ["any"]},
        ])
    );
    assert_no_forbidden_keys(out);
}

// ── loopback HTTP stub ───────────────────────────────────────────────────────

/// Serves every connection to a fresh loopback port with the raw response `respond` gives
/// for the request path, for as long as the test process lives. Returns the base URL.
fn serve(respond: fn(&str) -> Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer(stream, respond));
        }
    });
    base
}

fn answer(mut stream: TcpStream, respond: fn(&str) -> Vec<u8>) {
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&request);
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    // The client may hang up mid-body (the size cap); that is not the stub's failure.
    let _ = stream.write_all(&respond(&path));
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

// ── 1. acceptance ────────────────────────────────────────────────────────────

#[test]
fn acceptance_tool_editor_lists_every_version_newest_first() {
    let out = search_fixture("tool-editor");
    the_acceptance_result(&out);
    assert_eq!(out["index"], FIXTURE);
    assert_eq!(out["data"]["results"], out["results"]);
    assert_eq!(message(&out), "1 result(s) for 'tool-editor'");
}

// ── 2. platforms ─────────────────────────────────────────────────────────────

#[test]
fn native_tool_lists_each_payload_platform() {
    let out = search_fixture("tool-git");
    assert_eq!(out["results"][0]["name"], "murmur/tool-git");
    assert_eq!(
        out["results"][0]["versions"],
        json!([{"version": "0.2.0", "release": "v0.25.0", "built_at_level": 1,
                "platforms": ["darwin-aarch64", "linux-x86_64"]}])
    );
}

#[test]
fn skill_payload_is_platform_independent() {
    let out = search_fixture("skill-create-manifest");
    assert_eq!(out["results"][0]["name"], "murmur/skill-create-manifest");
    assert_eq!(
        out["results"][0]["versions"][0]["platforms"],
        json!(["any"])
    );
}

// ── 3. ranking ───────────────────────────────────────────────────────────────

#[test]
fn full_bare_and_upper_case_names_rank_the_artifact_first() {
    for query in ["murmur/tool-editor", "tool-editor", "TOOL-EDITOR"] {
        let out = search_fixture(query);
        assert_eq!(result_names(&out)[0], "murmur/tool-editor", "query {query}");
    }
}

#[test]
fn limit_caps_the_results() {
    let out = tool(json!({ "query": "tool", "index": FIXTURE, "limit": 1 }));
    assert_eq!(out["count"], 1, "{out}");
    assert_eq!(out["results"].as_array().unwrap().len(), 1);
}

#[test]
fn description_only_match_is_returned() {
    // "messages" is in driver-anthropic's description, and in no name or tag.
    let out = search_fixture("Messages");
    assert_eq!(result_names(&out), ["murmur/driver-anthropic"]);
}

#[test]
fn tag_only_match_is_returned() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("artifacts-index.json");
    let index = json!({"schema_version": 2, "scope": "acme", "artifacts": [
        {"name": "tool-a", "runtime": "tool", "description": "Does a.", "tags": ["zebra"],
         "versions": [{"version": "0.1.0", "release": "v1", "interfaces": [],
                       "assets": [{"file": "tool-a-0.1.0.mur.zip", "sha256": "00"}]}]},
        {"name": "tool-b", "runtime": "tool", "tags": [], "versions": []},
    ]});
    fs::write(&path, index.to_string()).unwrap();
    let out = tool(json!({ "query": "zebra", "index": path }));
    assert_eq!(result_names(&out), ["acme/tool-a"], "{out}");
    assert_eq!(
        out["results"][0]["versions"][0]["built_at_level"],
        Value::Null
    );
}

#[test]
fn no_match_is_ok_with_zero_results() {
    let out = search_fixture("zzz");
    assert_eq!(out["count"], 0);
    assert_eq!(message(&out), "no results for 'zzz'");
}

// ── 4. schema refusals ───────────────────────────────────────────────────────

#[test]
fn schema_1_index_is_refused_with_the_regeneration_command() {
    let out = tool(json!({ "query": "git", "index": SCHEMA_1_FIXTURE }));
    assert_error(
        &out,
        "schema_error",
        &[SCHEMA_1_FIXTURE, "schema 1", SCHEMA_1_HINT],
    );
}

/// Writes the fixture's text with `from` replaced by `to` into a temp dir, returning its path.
fn edited_fixture(dir: &TempDir, fixture: &str, from: &str, to: &str) -> PathBuf {
    let text = fs::read_to_string(crate_dir().join(fixture)).unwrap();
    assert!(text.contains(from), "{fixture} has no {from:?}");
    let path = dir.path().join("artifacts-index.json");
    fs::write(&path, text.replacen(from, to, 1)).unwrap();
    path
}

#[test]
fn integer_schema_1_index_is_refused_the_same_way() {
    let dir = TempDir::new().unwrap();
    let path = edited_fixture(
        &dir,
        SCHEMA_1_FIXTURE,
        r#""schema_version": "1""#,
        r#""schema_version": 1"#,
    );
    let out = tool(json!({ "query": "git", "index": path }));
    assert_error(
        &out,
        "schema_error",
        &[path.to_str().unwrap(), "schema 1", SCHEMA_1_HINT],
    );
}

#[test]
fn other_schema_version_is_refused_naming_schema_2() {
    let dir = TempDir::new().unwrap();
    let path = edited_fixture(
        &dir,
        FIXTURE,
        r#""schema_version": 2"#,
        r#""schema_version": 3"#,
    );
    let out = tool(json!({ "query": "git", "index": path }));
    assert_error(&out, "schema_error", &["schema_version 3", "schema 2"]);
}

#[test]
fn missing_schema_version_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = edited_fixture(&dir, FIXTURE, r#""schema_version": 2,"#, "");
    let out = tool(json!({ "query": "git", "index": path }));
    assert_error(&out, "schema_error", &["no schema_version", "schema 2"]);
}

// ── 5. malformed ─────────────────────────────────────────────────────────────

#[test]
fn malformed_indexes_are_parse_errors_naming_the_source() {
    let dir = TempDir::new().unwrap();
    let cases = [
        ("not-json.json", "this is not json".to_string()),
        ("array.json", "[]".to_string()),
        (
            "no-versions.json",
            json!({"schema_version": 2, "scope": "murmur",
                   "artifacts": [{"name": "tool-x", "runtime": "tool", "tags": []}]})
            .to_string(),
        ),
    ];
    for (file, text) in cases {
        let path = dir.path().join(file);
        fs::write(&path, text).unwrap();
        let out = tool(json!({ "query": "x", "index": path }));
        assert_error(&out, "parse_error", &[path.to_str().unwrap()]);
        if file == "no-versions.json" {
            assert!(message(&out).contains("versions"), "{out}");
        }
    }
}

// ── 6. old inputs ────────────────────────────────────────────────────────────

#[test]
fn registry_input_is_refused_naming_index() {
    let out = tool(json!({ "query": "x", "registry": "local" }));
    assert_eq!(out["ok"], false);
    assert!(
        message(&out).contains("'registry' input was removed"),
        "{out}"
    );
    assert!(message(&out).contains("'index'"), "{out}");
}

#[test]
fn local_index_is_refused_naming_mur_search() {
    let out = tool(json!({ "query": "editor", "index": "local" }));
    assert_eq!(out["ok"], false);
    assert!(
        message(&out).contains("mur search editor --index local"),
        "{out}"
    );
}

#[test]
fn double_encoded_data_reads_the_fixture() {
    let inner = json!({ "query": "tool-editor", "index": FIXTURE }).to_string();
    let out = tool(Value::String(inner));
    the_acceptance_result(&out);
}

// ── 7. paths ─────────────────────────────────────────────────────────────────

#[test]
fn relative_path_resolves_against_the_working_directory() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::copy(crate_dir().join(FIXTURE), dir.path().join("sub/index.json")).unwrap();
    the_acceptance_result(&tool_in(
        dir.path(),
        json!({ "query": "tool-editor", "index": "sub/index.json" }),
    ));
    // The same relative path from another directory names no file.
    let out = tool(json!({ "query": "tool-editor", "index": "sub/index.json" }));
    assert_error(&out, "io_error", &["sub/index.json"]);
}

#[test]
fn absolute_path_is_read() {
    let path = crate_dir().join(FIXTURE);
    let out = tool_in(
        &std::env::temp_dir(),
        json!({ "query": "tool-editor", "index": path }),
    );
    the_acceptance_result(&out);
    assert_eq!(out["index"], path.to_str().unwrap());
}

#[test]
fn missing_file_is_an_io_error_naming_the_path() {
    let out = tool(json!({ "query": "x", "index": "no/such/artifacts-index.json" }));
    assert_error(
        &out,
        "io_error",
        &["failed to read no/such/artifacts-index.json"],
    );
}

// ── 8. loopback HTTP ─────────────────────────────────────────────────────────

#[test]
fn http_redirect_is_followed() {
    let base = serve(|path| match path {
        "/latest" => http_response("302 Found", "Location: /v1/artifacts-index.json\r\n", b""),
        "/v1/artifacts-index.json" => {
            let body = fs::read(crate_dir().join(FIXTURE)).unwrap();
            http_response("200 OK", "Content-Type: application/json\r\n", &body)
        }
        _ => http_response("404 Not Found", "", b""),
    });
    let url = format!("{base}/latest");
    let out = tool(json!({ "query": "tool-editor", "index": url }));
    the_acceptance_result(&out);
    assert_eq!(out["index"], url);
}

#[test]
fn http_404_is_a_fetch_error() {
    let base = serve(|_| http_response("404 Not Found", "", b"not here"));
    let url = format!("{base}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, "fetch_error", &["HTTP 404", &url]);
}

#[test]
fn oversized_body_is_a_fetch_error() {
    let base = serve(|_| http_response("200 OK", "", &vec![b' '; MAX_INDEX_BYTES + 1]));
    let url = format!("{base}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, "fetch_error", &[&url, "more than 16 MiB"]);
}

#[test]
fn non_json_body_is_a_parse_error() {
    let base = serve(|_| http_response("200 OK", "", b"<html>hello</html>"));
    let url = format!("{base}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    assert_error(&out, "parse_error", &[&url]);
}

// ── 9. the TLS backend ───────────────────────────────────────────────────────

/// An `https://` index makes the tool open a TLS session: the first bytes on the wire are a
/// handshake record. A build without a TLS backend sends nothing.
#[test]
fn https_index_starts_a_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stub = thread::spawn(move || -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(_) => return Vec::new(),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut first = Vec::new();
        let mut buf = [0u8; 512];
        while first.len() < 2 {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => first.extend_from_slice(&buf[..n]),
            }
        }
        first
    });

    let url = format!("https://127.0.0.1:{port}/artifacts-index.json");
    let out = tool(json!({ "query": "x", "index": url }));
    let first = stub.join().unwrap();

    assert!(
        first.starts_with(&[0x16, 0x03]),
        "expected a TLS handshake record, got {first:02x?}; tool said {out}"
    );
    assert_error(&out, "fetch_error", &[&url]);
    assert!(!message(&out).contains("no TLS backend"), "{out}");
}

// ── 10. real HTTPS ───────────────────────────────────────────────────────────

const V0_24_0_RAW_URL: &str =
    "https://raw.githubusercontent.com/murmur-nexus/default-artifacts/v0.24.0/artifacts-index.json";

#[test]
#[ignore = "needs network: fetches from github.com"]
fn https_schema_1_index_on_github_is_read_and_refused() {
    let out = tool(json!({ "query": "git", "index": V0_24_0_RAW_URL }));
    assert_error(
        &out,
        "schema_error",
        &[V0_24_0_RAW_URL, "schema 1", SCHEMA_1_HINT],
    );
}

#[test]
#[ignore = "needs network: fetches from github.com"]
fn https_default_index_is_fetched() {
    let out = tool(json!({ "query": "git" }));
    if out["ok"] == true {
        assert_eq!(out["scope"], "murmur", "{out}");
        assert_no_forbidden_keys(&out);
    } else {
        // Until a release publishes artifacts-index.json, the latest-release URL is a 404.
        assert_error(&out, "fetch_error", &["HTTP 404"]);
        for word in ["TLS", "certificate", "backend"] {
            assert!(!message(&out).contains(word), "{out}");
        }
    }
}
