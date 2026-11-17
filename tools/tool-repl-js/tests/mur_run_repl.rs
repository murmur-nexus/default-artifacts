//! tool-repl-js in a real `mur run`, as a capsule's `code_mode.repl`.
//!
//! A fake Chat Completions provider on loopback scripts one `tool-repl-js` call per turn, then
//! ends. Each call's result is read as murmur hands it to the model, inside
//! `<untrusted-content source=tool:tool-repl-js>`, and the trace gives the code-mode records.
//! `driver-openai`, `tool-read` (the fixture tool the program calls through the bus) and
//! `tool-repl-js` are built out of this workspace and packed with `scripts/package-artifact.sh`
//! into a scratch `HOME` on every run.
//!
//! Needs a `mur` at WIT level 8 or later (`murmur:bus@0.1.0`), from `MUR_BIN`; a run without one
//! fails rather than skipping:
//!
//! ```sh
//! MUR_BIN=<level-8 mur> cargo test -p tool-repl-js --test mur_run_repl -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

const FENCE_OPEN: &str = "<untrusted-content source=tool:tool-repl-js>\n";
const FENCE_CLOSE: &str = "\n</untrusted-content>";

const REFUSAL: &str =
    "'tool-write' is not an operation this program may call; list() returns every operation it may call";

// ── the runtime under test ───────────────────────────────────────────────────

fn mur_binary() -> PathBuf {
    let path = PathBuf::from(std::env::var_os("MUR_BIN").expect(
        "set MUR_BIN to a mur whose `mur version` reports WIT level 8 or higher (murmur:bus@0.1.0)",
    ));
    let out = Command::new(&path).arg("version").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let level: u32 = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("WIT level "))
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        level >= 8,
        "MUR_BIN must report WIT level 8 or higher (murmur:bus@0.1.0):\n{text}"
    );
    path
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

fn scratch(prefix: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "tool-repl-js/{prefix}-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
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
    fs::read_dir(out_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .is_some_and(|f| f.to_string_lossy().starts_with(&format!("{name}-")))
        })
        .expect("the packed zip")
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

/// A `HOME` holding the store, and the project that installs the three artifacts.
struct Staging {
    home: PathBuf,
    project: PathBuf,
}

impl Staging {
    fn new() -> Self {
        let root = scratch("mur-run");
        let artifacts = root.join("artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        let staging = Staging {
            home: root.join("home"),
            project: root.join("project"),
        };
        fs::create_dir_all(&staging.home).unwrap();
        fs::create_dir_all(&staging.project).unwrap();
        // `mur install` installs into the project whose murmur.yaml it finds.
        fs::write(
            staging.project.join("murmur.yaml"),
            "name: staging\nversion: 0.1.0\n",
        )
        .unwrap();
        let zips = [
            pack("drivers/driver-openai", &artifacts),
            pack("tools/tool-read", &artifacts),
            pack("tools/tool-repl-js", &artifacts),
        ];
        for zip in &zips {
            let mut publish = staging.mur();
            publish.arg("publish").arg(zip);
            run_to_success(publish, &format!("publishing {}", zip.display()));
        }
        for zip in &zips {
            let mut install = staging.mur();
            install.arg("install").arg(zip);
            run_to_success(install, &format!("installing {}", zip.display()));
        }
        staging
    }

    fn mur(&self) -> Command {
        let mut command = Command::new(mur_binary());
        command
            .env("HOME", &self.home)
            .env("PATH", path_with_mur())
            .current_dir(&self.project);
        command
    }

    /// The capsule: driver-openai at `endpoint`, tool-read, and tool-repl-js as
    /// `code_mode.repl` with `repl_config` as its entry's extra lines.
    fn write_manifest(&self, name: &str, endpoint: &str, repl_config: &str) -> PathBuf {
        let path = self.project.join(format!("{name}.yaml"));
        fs::write(
            &path,
            format!(
                "name: {name}\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 \x20 - name: local/tool-read\n    version: {}\n    runtime: tool\n\
                 \x20 - name: local/tool-repl-js\n    version: {}\n    runtime: tool\n\
                 {repl_config}\
                 code_mode:\n  repl: local/tool-repl-js\n\
                 capabilities:\n  shell:\n    allow:\n      - echo\n\
                 trace:\n  capture: content\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                manifest_field("drivers/driver-openai", "version"),
                manifest_field("tools/tool-read", "version"),
                manifest_field("tools/tool-repl-js", "version"),
            ),
        )
        .unwrap();
        path
    }

    fn run(&self, manifest: &Path, workdir: &Path) -> Output {
        let mut command = self.mur();
        command.args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            "run the scripted programs",
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

fn fixture_workdir(staging: &Staging, name: &str) -> PathBuf {
    let workdir = staging.project.join(format!("work-{name}"));
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("note.txt"), "hello from the fixture\n").unwrap();
    fs::write(workdir.join("a.txt"), "alpha\n").unwrap();
    fs::write(workdir.join("b.txt"), "beta\n").unwrap();
    workdir
}

// ── the fake provider ────────────────────────────────────────────────────────

fn chat_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    )
}

fn tool_call_stream(turn: usize, code: &str) -> String {
    let call = json!({
        "index": 0, "id": format!("call_{turn}"), "type": "function",
        "function": {"name": "tool-repl-js", "arguments": json!({"code": code}).to_string()}
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
    fn start(script: Vec<&'static str>) -> Self {
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
                    Some(code) => tool_call_stream(turn + 1, code),
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

/// The tool's own text inside murmur's fence, byte for byte.
fn unfenced(content: &str) -> String {
    let inside = content
        .strip_prefix(FENCE_OPEN)
        .unwrap_or_else(|| panic!("an unfenced tool result: {content}"));
    inside
        .strip_suffix(FENCE_CLOSE)
        .unwrap_or_else(|| panic!("an unclosed fence: {content}"))
        .to_string()
}

/// One `mur run` of `script`; each call's text as the model received it, by turn.
fn run_script(
    staging: &Staging,
    name: &str,
    repl_config: &str,
    script: Vec<&'static str>,
) -> (Vec<String>, Vec<Value>) {
    let calls = script.len();
    let provider = Provider::start(script);
    let manifest = staging.write_manifest(name, &provider.endpoint, repl_config);
    let workdir = fixture_workdir(staging, name);
    let output = staging.run(&manifest, &workdir);
    let text = combined(&output);
    assert!(output.status.success(), "mur run {name} failed:\n{text}");
    let results = tool_results(&provider.last_request());
    assert_eq!(results.len(), calls, "{name}: one result per call:\n{text}");
    let seen = (1..=calls)
        .map(|turn| {
            let id = format!("call_{turn}");
            let content = &results
                .iter()
                .find(|(call, _)| *call == id)
                .unwrap_or_else(|| panic!("{name}: no result for {id}"))
                .1;
            unfenced(content)
        })
        .collect();
    (seen, staging.trace(&workdir))
}

fn kind(line: &Value) -> &str {
    line["event_type"].as_str().unwrap_or_default()
}

#[test]
#[ignore = "launches the real mur binary; needs MUR_BIN at WIT level 8"]
fn code_mode_runs_every_global_against_a_real_mur() {
    let staging = Staging::new();
    let script = vec![
        r#"globalThis.n = 41; let kept = "yes"; print("set")"#,
        r#"[n + 1, kept]"#,
        r#"tools().map(t => t.name)"#,
        r#"typeof tools().find(t => t.name === "tool-read").inputSchema"#,
        r#"const r = call("tool-read", {path: "note.txt"}); [r.status, r.data.includes("hello from the fixture"), r.data.startsWith("<untrusted-content")]"#,
        r#"try { call("tool-write", {path: "x", content: "y"}) } catch (e) { [e instanceof Error, e.kind, e.message] }"#,
        r#"callAll([["tool-read", {path: "a.txt"}], ["tool-read", {path: "b.txt"}], ["tool-write", {}]]).map(r => r instanceof Error ? r.kind : r.data.includes("alpha") ? "a" : r.data.includes("beta") ? "b" : "?")"#,
        r#"const o = run("echo", ["hi"]); [o.exitCode, o.stdout, o.timedOut]"#,
        r#"try { run("cat", ["note.txt"]) } catch (e) { [e.kind, e.rule] }"#,
        r#"print("x".repeat(2000))"#,
        r#"print("before"); function f() { throw new Error("boom") } f()"#,
        r#"n"#,
    ];
    let (seen, trace) = run_script(
        &staging,
        "repl",
        "    config:\n      output_limit_bytes: 512\n",
        script,
    );
    for (i, text) in seen.iter().enumerate() {
        println!("--- call {} ---\n{text}", i + 1);
    }

    // 1–2: state lasts across calls.
    assert_eq!(seen[0], "set\n");
    assert_eq!(seen[1], r#"=> [42,"yes"]"#);
    // 3–4: tools() lists the capsule's operations, not the REPL itself.
    let names: Value = serde_json::from_str(seen[2].strip_prefix("=> ").unwrap()).unwrap();
    let names: Vec<&str> = names
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(names.contains(&"tool-read"), "{names:?}");
    assert!(names.contains(&"echo"), "{names:?}");
    assert!(!names.contains(&"tool-repl-js"), "{names:?}");
    assert_eq!(seen[3], r#"=> "object""#);
    // 5–6: call returns the envelope, inside tool-read's fence; an ungranted name throws.
    assert_eq!(seen[4], r#"=> ["passed",true,true]"#);
    assert_eq!(seen[5], format!(r#"=> [true,"refused","{REFUSAL}"]"#));
    // 7: callAll keeps order, with the refusal in place.
    assert_eq!(seen[6], r#"=> ["a","b","refused"]"#);
    // 8–9: run of an allowlisted program, and the refusal of one that is not.
    assert_eq!(seen[7], r#"=> [0,"hi\n",false]"#);
    assert_eq!(seen[8], r#"=> ["refused","capabilities.shell.allow"]"#);
    // 10: the cut at output_limit_bytes.
    assert_eq!(
        seen[9],
        format!(
            "{}\n[output cut: 2001 bytes, the first 512 shown]",
            "x".repeat(512)
        )
    );
    // 11: an exception, its stack, then what was printed before it.
    assert!(
        seen[10].starts_with("Uncaught Error: boom\n"),
        "{}",
        seen[10]
    );
    assert!(
        seen[10].lines().any(|l| l.contains("at f ")),
        "a stack line naming f: {}",
        seen[10]
    );
    assert!(seen[10].ends_with("before\n"), "{}", seen[10]);
    // 12: the throw kept the context.
    assert_eq!(seen[11], "=> 41");

    // One instance served every call.
    let starts: Vec<&Value> = trace
        .iter()
        .filter(|l| kind(l) == "code_mode_instance_start")
        .collect();
    assert_eq!(starts.len(), 1, "{starts:#?}");
    assert_eq!(starts[0]["tool"], "tool-repl-js", "{}", starts[0]);
    assert_eq!(starts[0]["instance"], 1, "{}", starts[0]);

    // Each bus call to tool-read is recorded under the REPL call that made it.
    let repl_call_id = |turn: usize| -> String {
        trace
            .iter()
            .find(|l| {
                kind(l) == "tool_call"
                    && l["tool_name"] == "tool-repl-js"
                    && l["tool_call_id"] == format!("call_{turn}")
            })
            .unwrap_or_else(|| panic!("no tool_call for call_{turn}"))["tool_call_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let bus_reads: Vec<&Value> = trace
        .iter()
        .filter(|l| {
            kind(l) == "tool_call" && l["tool_name"] == "tool-read" && l["origin"] == "code_mode"
        })
        .collect();
    assert_eq!(bus_reads.len(), 3, "{bus_reads:#?}");
    let under = |turn: usize| {
        let id = repl_call_id(turn);
        bus_reads.iter().filter(|l| l["repl_call_id"] == id).count()
    };
    assert_eq!(under(5), 1, "{bus_reads:#?}");
    assert_eq!(under(7), 2, "{bus_reads:#?}");

    // Call 11 returned status failed, which the trace records as an error.
    let call_11 = trace
        .iter()
        .find(|l| {
            kind(l) == "tool_call"
                && l["tool_name"] == "tool-repl-js"
                && l["tool_call_id"] == "call_11"
        })
        .expect("the tool_call of call 11");
    assert_eq!(call_11["status"], "error", "{call_11}");
    let call_12 = trace
        .iter()
        .find(|l| {
            kind(l) == "tool_call"
                && l["tool_name"] == "tool-repl-js"
                && l["tool_call_id"] == "call_12"
        })
        .expect("the tool_call of call 12");
    assert_eq!(call_12["status"], "ok", "{call_12}");
}

#[test]
#[ignore = "launches the real mur binary; needs MUR_BIN at WIT level 8"]
fn without_config_nothing_is_cut() {
    let staging = Staging::new();
    let (seen, _trace) = run_script(&staging, "uncut", "", vec![r#"print("x".repeat(2000))"#]);
    assert_eq!(seen[0], format!("{}\n", "x".repeat(2000)));
}
