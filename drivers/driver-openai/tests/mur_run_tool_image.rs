//! A tool's image, proved to reach the provider on both OpenAI surfaces, sized to the model,
//! under a real `mur run`.
//!
//! The capsule runs driver-openai with the workspace's `tool-read`, built for wasm32-wasip2 and
//! packed as a release packs it. A loopback fake OpenAI answers the first request with a call to the tool for a 3000×2000 PNG in the work folder, and the second
//! with a final text. On `chat-latest` (Chat Completions) the second request must carry the image
//! in a user message right after the tool message, at 1920×1280; on `gpt-5.6-luna` (Responses), in
//! the call's `function_call_output` array at its original 3000×2000. The guest must never trap.
//! Only a launch runs `mod wasm_driver`, where the decode, resize and re-encode happen inside the
//! component under the runtime's memory cap.
//!
//! `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch:
//! `MUR_BIN=… cargo test -p driver-openai --test mur_run_tool_image -- --ignored`. `mur` must
//! serve WIT level 3 and read `vision` (murmur `a9ea4726` or later).
//!
//! Unix only: it spawns a host `mur` and reads Unix permission bits.
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
    time::Duration,
};

use murmur_driver_image::{decode_base64, dimensions};
use serde_json::{json, Value};
use tempfile::TempDir;

const IMAGE: &str = "shot.png";

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
        .unwrap_or_else(|| panic!("no `mur` binary: set MUR_BIN to one, or put it on PATH"))
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

/// Build `crate_name` for wasm32-wasip2, once per test binary per `once`.
fn build_component(once: &'static Once, crate_name: &str) {
    once.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                crate_name,
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .unwrap_or_else(|err| panic!("failed to spawn `cargo build` for {crate_name}: {err}"));
        assert!(status.success(), "cargo build of {crate_name} failed");
    });
}

fn build_driver() {
    static BUILD: Once = Once::new();
    build_component(&BUILD, "driver-openai");
}

fn manifest_field(manifest: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    manifest
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no top-level `{key}:` in murmur.yaml"))
}

fn package(artifact_dir: &Path, out_dir: &Path, payload: &Path) -> PathBuf {
    let output = Command::new("bash")
        .arg(workspace_root().join("scripts/package-artifact.sh"))
        .arg(artifact_dir)
        .arg(out_dir)
        .arg("--payload")
        .arg(payload)
        .env("MUR_BIN", mur_binary())
        .stderr(Stdio::inherit())
        .output()
        .expect("failed to spawn scripts/package-artifact.sh");
    assert!(
        output.status.success(),
        "packing {} failed",
        artifact_dir.display()
    );
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

/// Pack the driver; returns its version and zip.
fn pack_driver(out_dir: &Path) -> (String, PathBuf) {
    build_driver();
    let crate_path = workspace_root().join("drivers/driver-openai");
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let version = manifest_field(&manifest, "version");
    let wasm = target_dir().join("wasm32-wasip2/release/driver_openai.wasm");
    (version, package(&crate_path, out_dir, &wasm))
}

/// Pack the workspace's `tool-read`, which attaches an image file it is asked to read; returns
/// its version and zip.
fn pack_tool_read(out_dir: &Path) -> (String, PathBuf) {
    static BUILD: Once = Once::new();
    build_component(&BUILD, "tool-read");
    let crate_path = workspace_root().join("tools/tool-read");
    let manifest = fs::read_to_string(crate_path.join("murmur.yaml")).unwrap();
    let version = manifest_field(&manifest, "version");
    let wasm = target_dir().join("wasm32-wasip2/release/tool_read.wasm");
    (version, package(&crate_path, out_dir, &wasm))
}

/// A 3000×2000 PNG of flat bands and one dark block: small enough on disk for the runtime's
/// default 5 MiB attachment cap.
fn write_image(path: &Path) {
    let image = image::RgbImage::from_fn(3000, 2000, |x, y| {
        if (1000..2000).contains(&x) && (800..1200).contains(&y) {
            image::Rgb([20, 20, 30])
        } else {
            image::Rgb([240, 240 - (y / 100) as u8, 230])
        }
    });
    image.save(path).unwrap();
}

// ── the fake provider ────────────────────────────────────────────────────────

fn response(body: &Value) -> String {
    let payload = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{payload}",
        payload.len()
    )
}

/// The name the request offers tool-read under, on either surface.
fn read_tool(request: &Value) -> String {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().or(tool["function"]["name"].as_str()))
        .find(|name| name.contains("read"))
        .unwrap_or_else(|| panic!("the request offers no tool-read: {}", request["tools"]))
        .to_string()
}

/// The first answer calls tool-read on the image; every later one ends the turn. A body with
/// `messages` is Chat Completions; one with `input` is Responses.
fn answer(index: usize, request: &Value) -> String {
    let chat = request.get("messages").is_some();
    let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "input_tokens": 10, "output_tokens": 5});
    let body = match (chat, index) {
        (true, 0) => json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_img1", "type": "function", "function": {
                    "name": read_tool(request),
                    "arguments": json!({"path": IMAGE}).to_string()
                }}]
            }}],
            "usage": usage
        }),
        (true, _) => json!({
            "id": "chatcmpl-2", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "I see the image."}}],
            "usage": usage
        }),
        (false, 0) => json!({
            "id": "resp_1", "object": "response", "status": "completed",
            "output": [{"type": "function_call", "id": "fc_1", "call_id": "call_img1",
                "name": read_tool(request), "arguments": json!({"path": IMAGE}).to_string(),
                "status": "completed"}],
            "usage": usage
        }),
        (false, _) => json!({
            "id": "resp_2", "object": "response", "status": "completed",
            "output": [{"type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "I see the image."}]}],
            "usage": usage
        }),
    };
    response(&body)
}

struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
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
                let Some(body) = read_body(&mut stream) else {
                    continue;
                };
                let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let mut all = recorded.lock().unwrap();
                let reply = answer(all.len(), &request);
                all.push(request);
                drop(all);
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Self { port, requests }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 65536];
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
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(body)
}

// ── scenario ─────────────────────────────────────────────────────────────────

/// Every file under `dir` whose text holds `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if fs::read(&path)
            .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        {
            found.push(path);
        }
    }
    found
}

fn data_url_size(url: &str) -> (String, (u32, u32), usize) {
    let (media_type, data) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .unwrap_or_else(|| panic!("not a base64 data URL: {}", &url[..url.len().min(80)]));
    let bytes = decode_base64(data).unwrap();
    let size = dimensions(media_type, &bytes).unwrap();
    (media_type.to_string(), size, bytes.len())
}

/// Run the capsule on `model`; returns the two requests the fake received and the scratch HOME.
fn run_model(model: &str) -> (Vec<Value>, TempDir) {
    let artifacts = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let (version, driver_zip) = pack_driver(artifacts.path());
    let (tool_version, tool_zip) = pack_tool_read(artifacts.path());
    let provider = Provider::start();

    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: tool-image-proof\nversion: 0.1.0\nartifacts:\n\
             \x20 - name: local/driver-openai\n    version: {version}\n    runtime: driver\n    \
             gateway:\n      endpoint: {}/v1\n      api_key: test-key\n\
             \x20 - name: local/tool-read\n    version: {tool_version}\n    runtime: tool\n\
             inference:\n  transport: http\n  model: {model}\n  max_tokens: 4000\n  \
             driver:\n    artifact: local/driver-openai\n\
             trace:\n  capture: content\n",
            provider.endpoint()
        ),
    )
    .unwrap();
    for zip in [&driver_zip, &tool_zip] {
        let mut publish = mur(&home);
        publish.arg("publish").arg(zip);
        run_to_success(publish, "publishing");
        let mut install = mur(&home);
        install.current_dir(project.path()).arg("install").arg(zip);
        run_to_success(install, "installing");
    }

    let workdir = project.path().join("work");
    fs::create_dir_all(&workdir).unwrap();
    write_image(&workdir.join(IMAGE));
    let task = project.path().join("task.md");
    fs::write(
        &task,
        format!("Call tool-read on {IMAGE} and tell me what it shows.\n"),
    )
    .unwrap();

    let output = mur(&home)
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            task.to_str().unwrap(),
        ])
        .arg("--workdir")
        .arg(&workdir)
        .stdin(Stdio::null())
        .output()
        .expect("spawning `mur run`");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !text.contains("E-RUN-007") && !text.contains("trapped"),
        "{model}: the guest must not trap:\n{text}"
    );
    assert!(
        output.status.success(),
        "{model}: the run must succeed:\n{text}"
    );
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "{model}: a tool call, then the final answer:\n{text}"
    );
    (requests, home)
}

fn assert_report_stored(home: &TempDir, placement: &str, sent_width: u32) {
    let stored = files_containing(home.path(), &format!("\"placement\":\"{placement}\""));
    assert!(
        !stored.is_empty(),
        "no stored response carries the images report"
    );
    let report = fs::read_to_string(&stored[0]).unwrap();
    eprintln!("images report stored in {}", stored[0].display());
    assert!(
        report.contains(&format!("\"sent_width\":{sent_width}")),
        "{report}"
    );
}

#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn chat_completions_sends_a_tool_image_in_a_user_message_after_the_tool() {
    let (requests, home) = run_model("chat-latest");
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_index = messages
        .iter()
        .position(|m| m["role"] == "tool" && m["tool_call_id"] == "call_img1")
        .unwrap_or_else(|| panic!("no tool message for the call: {}", requests[1]["messages"]));
    assert!(
        messages[tool_index]["content"].is_string(),
        "a tool message carries text only"
    );
    let follow = &messages[tool_index + 1];
    assert_eq!(
        follow["role"], "user",
        "the image follows the tool message: {follow}"
    );
    let parts = follow["content"].as_array().unwrap();
    assert_eq!(
        parts[0]["text"],
        "Image attached to the result of tool call call_img1:"
    );
    let (media_type, size, bytes) = data_url_size(parts[1]["image_url"]["url"].as_str().unwrap());
    eprintln!(
        "model: chat-latest; tool message at {tool_index}, user message after it; image {media_type} {}x{} ({bytes} bytes)",
        size.0, size.1
    );
    assert_eq!((media_type.as_str(), size), ("image/png", (1920, 1280)));
    assert_report_stored(&home, "user_after_tool", 1920);
}

#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn responses_sends_a_tool_image_in_the_function_call_output() {
    let (requests, home) = run_model("gpt-5.6-luna");
    let output = requests[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call_img1")
        .unwrap_or_else(|| {
            panic!(
                "no function_call_output for the call: {}",
                requests[1]["input"]
            )
        })
        .clone();
    let parts = output["output"]
        .as_array()
        .unwrap_or_else(|| panic!("output is not an array: {output}"));
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[1]["type"], "input_image");
    let (media_type, size, bytes) = data_url_size(parts[1]["image_url"].as_str().unwrap());
    eprintln!(
        "model: gpt-5.6-luna; function_call_output parts: {:?}; image {media_type} {}x{} ({bytes} bytes)",
        parts.iter().map(|p| p["type"].as_str().unwrap_or("?")).collect::<Vec<_>>(),
        size.0,
        size.1
    );
    assert_eq!((media_type.as_str(), size), ("image/png", (3000, 2000)));
    assert_report_stored(&home, "function_call_output", 3000);
}
