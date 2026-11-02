//! tool-read's image attachment, proved under a real `mur run` against driver-openai.
//!
//! A capsule with driver-openai on `chat-latest` and tool-read reads `logo.png`. The loopback
//! fake Chat Completions provider's first reply calls `tool-read` with `{"path":"logo.png"}`,
//! and its second ends the turn. The test checks that the second request carries the picture as
//! an `image_url` part whose `data:image/png;base64,` payload is `logo.png`'s bytes (a 64×48 PNG
//! fits `chat-latest`'s 2048 px / 2,500-patch rule, so driver-openai sends it untouched), that
//! the session trace's `tool_call` records the accepted attachment, and that no request says an
//! attachment `was not attached`.
//!
//! The harness is `tests/common`: the real `mur` binary as a subprocess with
//! `HOME` pointed at a scratch directory, the artifacts built out of this workspace on every run.
//! `mur` comes from `MUR_BIN` or `PATH`, and a run that finds none fails rather than skipping.
//! It must print `WIT level 3` from `mur version` and be murmur `a9ea4726` or later: an earlier
//! level-3 `mur` refuses to pack driver-openai, whose `models:` entries carry `vision`
//! (E-BLD-005). It is `#[ignore]`d because the default
//! `cargo test --workspace` has no `mur` to launch:
//!
//! ```text
//! MUR_BIN=<level-3 mur> cargo test -p murmur-file-tools --test mur_run_read_image -- \
//!     --ignored --nocapture
//! ```

#![cfg(unix)]

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use common::{combined, mur, mur_binary, pack, run_to_success, tool_results, Provider};
use murmur_file_tools::Tool;
use serde_json::{json, Value};
use tempfile::TempDir;

#[path = "../src/test_png.rs"]
mod test_png;

// ── staging ──────────────────────────────────────────────────────────────────

struct Staging {
    home: TempDir,
    project: TempDir,
    driver_version: String,
    read_version: String,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn workdir(&self) -> PathBuf {
        self.project.path().join("work")
    }

    /// The capsule manifest: driver-openai on `chat-latest`, which `MODEL_ROUTES` sends to the
    /// Chat Completions surface, and tool-read, with the trace capturing content.
    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: read-image-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/tool-read\n    version: {}\n    runtime: tool\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n\
                 trace:\n  capture: content\n",
                self.driver_version, self.read_version
            ),
        )
        .unwrap();
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let (read_version, read_zip) = pack("tools/tool-read", artifacts.path());
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        read_version,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");

    for (zip, what) in [(&driver_zip, "driver-openai"), (&read_zip, "tool-read")] {
        let mut publish = mur(&staging.home);
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {what}"));

        let mut install = mur(&staging.home);
        install
            .current_dir(staging.project.path())
            .arg("install")
            .arg(zip);
        run_to_success(install, &format!("installing {what}"));
    }
    staging
}

/// Run one task to completion and return its output, whatever its exit status.
fn run_session(staging: &Staging) -> Output {
    mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "What colour is logo.png? Answer in one word.",
            "--workdir",
            staging.workdir().to_str().unwrap(),
        ])
        .output()
        .expect("spawning `mur run`")
}

// ── the fake provider ────────────────────────────────────────────────────────

/// The tool calls the fake model makes, one per turn, in order, each as `(tool, arguments)`; the
/// turn after the last one answers with text.
fn scripted_calls() -> Vec<(Tool, Value)> {
    vec![(Tool::Read, json!({ "path": "logo.png" }))]
}

// ── the scenario ─────────────────────────────────────────────────────────────

/// Standard base64 with padding, decoded; `None` on any other byte.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&a| a == byte)? as u32;
        buffer = buffer << 6 | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Every `image_url` URL in the body's messages, in order.
fn image_urls(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .filter_map(|part| part["image_url"]["url"].as_str().map(str::to_string))
        .collect()
}

/// Every `trace.jsonl` line under `dir`, parsed.
fn trace_events(dir: &Path) -> Vec<Value> {
    let mut events = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return events;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            events.extend(trace_events(&path));
        } else if path.file_name().is_some_and(|name| name == "trace.jsonl") {
            events.extend(
                fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
            );
        }
    }
    events
}

#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn a_read_png_reaches_the_model_as_an_image() {
    let version = combined(&mur_version());
    println!("{version}");
    assert!(
        version.contains("WIT level 3"),
        "an attachment needs a mur at WIT level 3:\n{version}"
    );

    let staging = stage();
    let workdir = staging.workdir();
    fs::create_dir_all(&workdir).unwrap();
    let png = test_png::png(64, 48);
    fs::write(workdir.join("logo.png"), &png).unwrap();

    let provider = Provider::start(scripted_calls());
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one tool call, then the end of the turn");
    for request in &requests {
        assert!(
            !request.contains("was not attached"),
            "murmur refused the attachment: {request}"
        );
    }
    let second: Value = serde_json::from_str(&requests[1]).expect("the request body is JSON");
    let results = tool_results(&requests[1]);
    println!("tool results: {results:?}");
    // murmur fences every tool result as untrusted content before the driver sees it.
    assert_eq!(
        results,
        [(
            "call_1".to_string(),
            format!(
                "<untrusted-content source=tool:tool-read>\nimage/png, {} bytes, 64x48 \
                 pixels\n</untrusted-content>",
                png.len()
            )
        )]
    );
    let urls = image_urls(&second);
    assert_eq!(urls.len(), 1, "one image in the second request: {urls:?}");
    let payload = urls[0]
        .strip_prefix("data:image/png;base64,")
        .unwrap_or_else(|| panic!("a PNG data URL: {}", &urls[0]));
    assert_eq!(
        base64_decode(payload).expect("the payload is base64"),
        png,
        "a 64x48 PNG fits chat-latest's rule, so driver-openai sends logo.png's own bytes"
    );

    let events = trace_events(&staging.home.path().join(".murmur/records"));
    let call = events
        .iter()
        .find(|event| event["event_type"] == "tool_call")
        .unwrap_or_else(|| panic!("no tool_call event in the trace: {events:?}"));
    println!("tool_call attachments: {}", call["attachments"]);
    assert_eq!(call["attachments"][0]["media_type"], "image/png");
    assert_eq!(call["attachments"][0]["path"], "logo.png");
    assert_eq!(call["attachments"][0]["bytes"], png.len());
    assert!(call["attachments"][0]["sha256"].is_string(), "{call}");
    assert!(call["attachments"][0].get("refused").is_none(), "{call}");
}

fn mur_version() -> Output {
    Command::new(mur_binary())
        .arg("version")
        .output()
        .expect("spawning `mur version`")
}
