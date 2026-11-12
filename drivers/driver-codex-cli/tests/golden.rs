//! The recordings of real `codex` output in `tests/fixtures/`, read into the exact event list
//! each one produces.
//!
//! - `0.154.0/` and `0.167.2/` were recorded with exactly the plan `launch` builds, against the
//!   mock provider and a mock bridge named `murmur`. Their `.exit` and `.stderr.txt` are what
//!   [`classify_exit`] reads for a recording that printed no terminal line.
//! - `tarball-0.167.2/` are byte-identical copies from the grok/codex recordings tarball, made
//!   with the hardened 0.167.2 flag set and `developer_instructions`, under the server name
//!   `murmur_bridge`.
//!
//! `tests/fixtures/README.md` maps every file to its source.

use driver_codex_cli::{
    classify_exit, parse_context_for, parse_line, parse_lines, Bridge, Event, ExitStatus,
    FailureKind, ParseContext, ParseState, RetryInfo, SessionInfo, SessionMode, TokenUsage,
    ToolCallInfo, ToolCallStart, ToolResultInfo, TurnFailure,
};
use serde_json::Value;

/// The MCP server name the recordings made for this driver used: murmur's own.
const RECORDED_SERVER: &str = "murmur";

/// The MCP server name the tarball's recordings used.
const TARBALL_SERVER: &str = "murmur_bridge";

/// How a recording was launched.
#[derive(Clone, Copy)]
struct Recording {
    dir: &'static str,
    name: &'static str,
    server: &'static str,
    mode: SessionMode,
    /// For a recording with a bridged call: the index of the call's `item.completed` line, so a
    /// batch boundary falls between the call's start and its result.
    tool_split: Option<usize>,
}

const fn new(dir: &'static str, name: &'static str, server: &'static str) -> Recording {
    Recording {
        dir,
        name,
        server,
        mode: SessionMode::New,
        tool_split: None,
    }
}

const fn resumed(dir: &'static str, name: &'static str, server: &'static str) -> Recording {
    Recording {
        mode: SessionMode::Resume,
        ..new(dir, name, server)
    }
}

const fn tool(
    dir: &'static str,
    name: &'static str,
    server: &'static str,
    split: usize,
) -> Recording {
    Recording {
        tool_split: Some(split),
        ..new(dir, name, server)
    }
}

const V154: &str = "0.154.0";
const V167: &str = "0.167.2";
const TARBALL: &str = "tarball-0.167.2";

/// Every recording, with how it was launched. The 0.167.2 files print one line more than the
/// 0.154.0 ones before a call (the model-metadata `error` item), so their split is one later.
const RECORDINGS: [Recording; 40] = [
    new(V154, "01-text-new-session", RECORDED_SERVER),
    resumed(V154, "02-resume-same-session", RECORDED_SERVER),
    resumed(V154, "03-resume-again", RECORDED_SERVER),
    resumed(V154, "04-resume-missing-session", RECORDED_SERVER),
    tool(V154, "05-bridge-tool-call", RECORDED_SERVER, 3),
    tool(V154, "06-bridge-tool-image", RECORDED_SERVER, 3),
    tool(V154, "07-bridge-tool-error", RECORDED_SERVER, 3),
    new(V154, "08-thinking", RECORDED_SERVER),
    new(V154, "09-auth-401", RECORDED_SERVER),
    new(V154, "10-quota-429", RECORDED_SERVER),
    new(V154, "11-interrupt-sigint", RECORDED_SERVER),
    new(V154, "12-apply-patch-refused", RECORDED_SERVER),
    new(V154, "13-ask-user-refused", RECORDED_SERVER),
    new(V154, "14-code-mode-model", RECORDED_SERVER),
    new(
        V154,
        "15-auth-401-unauthenticated-real-endpoint",
        RECORDED_SERVER,
    ),
    new(V167, "01-text-new-session", RECORDED_SERVER),
    resumed(V167, "02-resume-same-session", RECORDED_SERVER),
    resumed(V167, "03-resume-again", RECORDED_SERVER),
    resumed(V167, "04-resume-missing-session", RECORDED_SERVER),
    tool(V167, "05-bridge-tool-call", RECORDED_SERVER, 4),
    tool(V167, "06-bridge-tool-image", RECORDED_SERVER, 4),
    tool(V167, "07-bridge-tool-error", RECORDED_SERVER, 4),
    new(V167, "08-thinking", RECORDED_SERVER),
    new(V167, "09-auth-401", RECORDED_SERVER),
    new(V167, "10-quota-429", RECORDED_SERVER),
    new(V167, "11-interrupt-sigint", RECORDED_SERVER),
    new(V167, "12-apply-patch-refused", RECORDED_SERVER),
    new(V167, "13-ask-user-refused", RECORDED_SERVER),
    new(V167, "14-code-mode-model", RECORDED_SERVER),
    new(
        V167,
        "15-auth-401-unauthenticated-real-endpoint",
        RECORDED_SERVER,
    ),
    new(TARBALL, "c12-new", TARBALL_SERVER),
    resumed(TARBALL, "c13-resume", TARBALL_SERVER),
    resumed(TARBALL, "c16-resume2", TARBALL_SERVER),
    tool(TARBALL, "c17-tool-new", TARBALL_SERVER, 4),
    tool(TARBALL, "c07-ro-bridge", TARBALL_SERVER, 4),
    new(TARBALL, "c15-resume-missing", TARBALL_SERVER),
    new(TARBALL, "c18-sigint", TARBALL_SERVER),
    new(TARBALL, "c03-gpt-5.6-terra", TARBALL_SERVER),
    new(TARBALL, "c08-ask", TARBALL_SERVER),
    new(TARBALL, "c06-patch-readonly", TARBALL_SERVER),
];

/// The recordings that were interrupted with SIGINT.
const INTERRUPTED: [&str; 2] = ["11-interrupt-sigint", "c18-sigint"];

// ── reading a recording ──────────────────────────────────────────────────────

fn recording(dir: &str, name: &str) -> Recording {
    *RECORDINGS
        .iter()
        .find(|recording| recording.dir == dir && recording.name == name)
        .unwrap_or_else(|| panic!("{dir}/{name} is not in RECORDINGS"))
}

fn fixture(recording: &Recording, extension: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/{}/{}.{extension}",
        env!("CARGO_MANIFEST_DIR"),
        recording.dir,
        recording.name
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn recorded_lines(recording: &Recording) -> Vec<String> {
    fixture(recording, "stdout.jsonl")
        .lines()
        .map(str::to_string)
        .collect()
}

fn context_of(recording: &Recording) -> ParseContext {
    let bridge = Bridge {
        server_name: recording.server.to_string(),
        url: "http://127.0.0.1:1/mcp".to_string(),
        bearer_token: "unused".to_string(),
        tool_names: Vec::new(),
    };
    parse_context_for(Some(&bridge), recording.mode)
}

/// The events a recording produces, having first shown that the partition into batches does
/// not change them. Three partitions, each with its own fresh state: one batch, one line per
/// batch, and — for a recording with a bridged call — a boundary between the call's start and
/// its result.
fn events_of(dir: &str, name: &str) -> Vec<Event> {
    let recording = recording(dir, name);
    let lines = recorded_lines(&recording);
    let context = context_of(&recording);

    let whole = parse_lines(&lines, &context, &mut ParseState::new());

    let mut one_by_one = ParseState::new();
    let one_at_a_time: Vec<Event> = lines
        .iter()
        .flat_map(|line| parse_line(line, &context, &mut one_by_one))
        .collect();
    assert_eq!(one_at_a_time, whole, "{dir}/{name}: one line per batch");

    match recording.tool_split {
        Some(split) => {
            assert_split_is_mid_call(&recording, &lines, split);
            let (head, tail) = lines.split_at(split);
            let mut across = ParseState::new();
            let mut split_in_two = parse_lines(head, &context, &mut across);
            split_in_two.extend(parse_lines(tail, &context, &mut across));
            assert_eq!(split_in_two, whole, "{dir}/{name}: split inside the call");
        }
        None => assert!(
            !lines
                .iter()
                .any(|line| line.contains(r#""type":"mcp_tool_call""#)),
            "{dir}/{name} has a bridged call, so it needs a tool split"
        ),
    }
    whole
}

/// The boundary really does fall between a call's `item.started` and its `item.completed`.
fn assert_split_is_mid_call(recording: &Recording, lines: &[String], split: usize) {
    let item = |line: &str| -> (String, String) {
        let value: Value = serde_json::from_str(line).unwrap_or_default();
        (
            value["type"].as_str().unwrap_or_default().to_string(),
            value["item"]["type"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    };
    let name = recording.name;
    assert!(
        split > 0 && split < lines.len(),
        "{name}: split inside the recording"
    );
    assert_eq!(
        item(&lines[split - 1]),
        ("item.started".to_string(), "mcp_tool_call".to_string()),
        "{name}: line {} must start the call",
        split - 1
    );
    assert_eq!(
        item(&lines[split]),
        ("item.completed".to_string(), "mcp_tool_call".to_string()),
        "{name}: line {split} must complete the call"
    );
}

/// What `classify_exit` answers for a recording, built from its `.exit` and `.stderr.txt`.
fn exit_event(dir: &str, name: &str) -> Event {
    let recording = recording(dir, name);
    let code: i32 = fixture(&recording, "exit")
        .trim()
        .parse()
        .expect("exit code");
    classify_exit(ExitStatus {
        code: Some(code),
        signal: None,
        stderr_tail: fixture(&recording, "stderr.txt"),
        interrupted: INTERRUPTED.contains(&name),
        saw_terminal: false,
    })
}

// ── expected events, spelled out ─────────────────────────────────────────────

fn session(id: &str) -> Event {
    Event::SessionStarted(SessionInfo {
        id: id.to_string(),
        auth: "unknown".to_string(),
        model: None,
    })
}

fn text(text: &str) -> Event {
    Event::Text(text.to_string())
}

fn thinking(text: &str) -> Event {
    Event::Thinking(text.to_string())
}

fn note(text: &str) -> Event {
    Event::Note(text.to_string())
}

fn end(result: &str) -> Event {
    Event::TurnEnd(result.to_string())
}

fn failed(kind: FailureKind, message: &str) -> Event {
    Event::TurnFailed(TurnFailure {
        kind,
        message: message.to_string(),
    })
}

fn retry(attempt: u32, reason: &str) -> Event {
    Event::Retry(RetryInfo {
        attempt,
        reason: reason.to_string(),
    })
}

fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64, thinking: u64) -> Event {
    Event::Usage(TokenUsage {
        input: Some(input),
        output: Some(output),
        cache_read: Some(cache_read),
        cache_creation: Some(cache_creation),
        thinking: Some(thinking),
    })
}

/// The note a resumed turn writes in place of `usage`, for the thread total it was handed.
fn running_total(input: u64, cached: u64, output: u64, reasoning: u64) -> Event {
    note(&format!(
        "codex reports a resumed thread's running total, not this turn's spend, so no usage is reported: {{\"cache_write_input_tokens\":0,\"cached_input_tokens\":{cached},\"input_tokens\":{input},\"output_tokens\":{output},\"reasoning_output_tokens\":{reasoning}}}"
    ))
}

/// `tool-call-started`, `tool-call` and `tool-result` for one bridged call with `{"text":"hi"}`.
fn call(id: &str, name: &str, output: &str, is_error: bool) -> Vec<Event> {
    vec![
        Event::ToolCallStarted(ToolCallStart {
            id: id.to_string(),
            name: name.to_string(),
        }),
        Event::ToolCall(ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input: r#"{"text":"hi"}"#.to_string(),
        }),
        Event::ToolResult(ToolResultInfo {
            id: id.to_string(),
            output: output.to_string(),
            is_error,
        }),
    ]
}

/// The `error` item every 0.167.2 mock recording prints for the mock's model name.
const METADATA_NOTE: &str = "codex error item: Model metadata for `gpt-5.4` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.";

const OK_REPLY: &str = "MOCKREPLY ok";
const RECALLED: &str = "MOCKREPLY saw_secret=True";
const AFTER_TOOL: &str = "MOCKREPLY tool result received";
const NO_TOOL: &str = "NO_MATCHING_TOOL tools=";
const THOUGHT: &str = "Thinking about it.";
const QUOTA_MESSAGE: &str = "exceeded retry limit, last status: 429 Too Many Requests";
const MISSING_ROLLOUT: &str = "codex exited with code 1: Error: thread/resume: thread/resume failed: no rollout found for thread id 01a1cd99-0000-7000-8000-000000000000 (code -32600)";
const CANCELED: &str = "codex was interrupted before it reported a result";

/// What the image tool's result reads as: the bridge's text item, then the image's line.
const IMAGE_OUTPUT: &str =
    "PICTURE-TOOL-RAN\n[image attached: picture.png, image/png, 692 bytes]\n[image: image/png, 692 bytes]";

fn mock_401(port: u16) -> String {
    format!(
        "unexpected status 401 Unauthorized: mock 401, url: http://127.0.0.1:{port}/v1/responses"
    )
}

/// The real endpoint's 401 over WebSocket, for one `cf-ray`.
fn wss_401(ray: &str) -> String {
    format!("unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: wss://api.openai.com/v1/responses, cf-ray: {ray}-BOG")
}

/// The real endpoint's 401 over HTTPS, for one `cf-ray` and request id.
fn https_401(ray: &str, request: &str) -> String {
    format!("unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, cf-ray: {ray}-BOG, request id: req_{request}")
}

/// The events of a recording against the real endpoint with no credential: four WebSocket
/// retries, the fallback to HTTPS, five HTTPS retries, then the final error and the failure.
fn unauthenticated(
    opening: Vec<Event>,
    wss: [&str; 4],
    fallback: &str,
    https: [(&str, &str); 5],
    last: (&str, &str),
) -> Vec<Event> {
    let mut events = opening;
    for (attempt, ray) in (2..).zip(wss) {
        events.push(retry(attempt, &wss_401(ray)));
    }
    events.push(note(&format!(
        "codex error item: Falling back from WebSockets to HTTPS transport. {}",
        wss_401(fallback)
    )));
    for (attempt, (ray, request)) in (1..).zip(https) {
        events.push(retry(attempt, &https_401(ray, request)));
    }
    let last = https_401(last.0, last.1);
    events.push(note(&format!("codex error: {last}")));
    events.push(failed(FailureKind::Auth, &last));
    events
}

fn concat(parts: Vec<Vec<Event>>) -> Vec<Event> {
    parts.into_iter().flatten().collect()
}

// ── 0.154.0 ──────────────────────────────────────────────────────────────────

const T154_THREAD: &str = "01a1ce96-e74c-77f3-8131-5288914e0e3b";

#[test]
fn golden_154_01_text_new_session() {
    assert_eq!(
        events_of(V154, "01-text-new-session"),
        vec![
            session(T154_THREAD),
            text(OK_REPLY),
            usage(100, 15, 20, 0, 5),
            end(OK_REPLY)
        ]
    );
}

#[test]
fn golden_154_02_resume_same_session() {
    assert_eq!(
        events_of(V154, "02-resume-same-session"),
        vec![
            session(T154_THREAD),
            text(RECALLED),
            running_total(240, 40, 30, 10),
            end(RECALLED)
        ]
    );
}

#[test]
fn golden_154_03_resume_again() {
    assert_eq!(
        events_of(V154, "03-resume-again"),
        vec![
            session(T154_THREAD),
            text(RECALLED),
            running_total(360, 60, 45, 15),
            end(RECALLED)
        ]
    );
}

#[test]
fn golden_154_04_resume_missing_session() {
    assert_eq!(events_of(V154, "04-resume-missing-session"), vec![]);
    assert_eq!(
        exit_event(V154, "04-resume-missing-session"),
        failed(FailureKind::HarnessError, MISSING_ROLLOUT)
    );
}

#[test]
fn golden_154_05_bridge_tool_call() {
    // 0.154.0 defers MCP tools behind its client-side `tool_search`, so the turn took three
    // requests and the total counts three.
    assert_eq!(
        events_of(V154, "05-bridge-tool-call"),
        concat(vec![
            vec![session("01a1ce97-039d-75c2-a1ab-156c6359df75")],
            call("item_0", "echo_tool", "echoed: hi", false),
            vec![text(AFTER_TOOL), usage(300, 45, 60, 0, 15), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_154_06_bridge_tool_image() {
    assert_eq!(
        events_of(V154, "06-bridge-tool-image"),
        concat(vec![
            vec![session("01a1ce97-0d28-7c60-a160-99a55add01ae")],
            call("item_0", "picture_tool", IMAGE_OUTPUT, false),
            vec![text(AFTER_TOOL), usage(300, 45, 60, 0, 15), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_154_07_bridge_tool_error() {
    assert_eq!(
        events_of(V154, "07-bridge-tool-error"),
        concat(vec![
            vec![session("01a1ce97-13c1-71e1-8685-e5e3fcbb1df3")],
            call("item_0", "fail_tool", "fail_tool refused: hi", true),
            vec![text(AFTER_TOOL), usage(300, 45, 60, 0, 15), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_154_08_thinking() {
    assert_eq!(
        events_of(V154, "08-thinking"),
        vec![
            session("01a1ce97-196a-7652-9c1f-6845fd927cdc"),
            thinking(THOUGHT),
            text(OK_REPLY),
            usage(100, 15, 20, 0, 5),
            end(OK_REPLY),
        ]
    );
}

#[test]
fn golden_154_09_auth_401() {
    let reason = mock_401(18110);
    let mut expected = vec![session("01a1ce97-1ef6-7672-b52c-cf764de4b748")];
    expected.extend((1..=5).map(|attempt| retry(attempt, &reason)));
    expected.push(note(&format!("codex error: {reason}")));
    expected.push(failed(FailureKind::Auth, &reason));
    assert_eq!(events_of(V154, "09-auth-401"), expected);
}

#[test]
fn golden_154_10_quota_429() {
    assert_eq!(
        events_of(V154, "10-quota-429"),
        vec![
            session("01a1ce97-43c6-76b0-8e4b-e42ae5a533d8"),
            note(&format!("codex error: {QUOTA_MESSAGE}")),
            failed(FailureKind::Quota, QUOTA_MESSAGE),
        ]
    );
}

#[test]
fn golden_154_11_interrupt_sigint() {
    assert_eq!(
        events_of(V154, "11-interrupt-sigint"),
        vec![session("01a1ce97-5262-7f82-9056-28526c23d45e")]
    );
    assert_eq!(
        exit_event(V154, "11-interrupt-sigint"),
        failed(FailureKind::Canceled, CANCELED)
    );
}

#[test]
fn golden_154_12_apply_patch_refused() {
    assert_eq!(
        events_of(V154, "12-apply-patch-refused"),
        vec![
            session("01a1ce97-62f1-76e2-a66f-9d3482c53948"),
            text(AFTER_TOOL),
            usage(200, 30, 40, 0, 10),
            end(AFTER_TOOL),
        ]
    );
}

#[test]
fn golden_154_13_ask_user_refused() {
    assert_eq!(
        events_of(V154, "13-ask-user-refused"),
        vec![
            session("01a1ce97-753f-7a10-b31a-a02553b3bae9"),
            text(AFTER_TOOL),
            usage(200, 30, 40, 0, 10),
            end(AFTER_TOOL),
        ]
    );
}

#[test]
fn golden_154_14_code_mode_model() {
    assert_eq!(
        events_of(V154, "14-code-mode-model"),
        vec![
            session("01a1ce97-81a4-79d2-b4b5-ebe6deb04351"),
            text(NO_TOOL),
            usage(100, 15, 20, 0, 5),
            end(NO_TOOL),
        ]
    );
}

#[test]
fn golden_154_15_auth_401_unauthenticated_real_endpoint() {
    assert_eq!(
        events_of(V154, "15-auth-401-unauthenticated-real-endpoint"),
        unauthenticated(
            vec![session("01a1ce97-8d4c-7613-8e19-942dec5a2801")],
            [
                "a593b6f83b327453",
                "a593b6fd5a4cb61c",
                "a593b705cf779426",
                "a593b7126c8192c2"
            ],
            "a593b7276e919be9",
            [
                ("a593b72a2d0f5f8c", "46c3d303016b4441a9e67476283b6b26"),
                ("a593b72f486ce336", "a45f70de025645cc87d0013558013f89"),
                ("a593b735cf550c14", "4b22314500274b4598f926e45d276273"),
                ("a593b73dcd51f70f", "3ce24468acba4f98b28876ef283c2498"),
                ("a593b74a9f5ab226", "9ad45d8c65aa47ec8e00765ed8352ea3"),
            ],
            ("a593b7612b3dfacb", "2b09412f75de46c4b2830ff9f54c4f1a"),
        )
    );
}

// ── 0.167.2 ──────────────────────────────────────────────────────────────────

const T167_THREAD: &str = "01a1ce96-ea62-7a80-8bbb-ce74b0ae4706";

/// `session-started` and the model-metadata note every 0.167.2 mock recording opens with.
fn opening_167(id: &str) -> Vec<Event> {
    vec![session(id), note(METADATA_NOTE)]
}

/// The note the code mode model's `error` item becomes: the release binary ships without the
/// `codex-code-mode-host` executable beside it.
const CODE_MODE_NOTE: &str = "codex error item: Code Mode is unavailable because failed to spawn code-mode host /space/scratch/a6975964/codex-0.167.2/bin/codex-code-mode-host: host executable was not found. Code mode will fail closed; enable `features.code_mode_host` and install `codex-code-mode-host`.";

#[test]
fn golden_167_01_text_new_session() {
    assert_eq!(
        events_of(V167, "01-text-new-session"),
        concat(vec![
            opening_167(T167_THREAD),
            vec![text(OK_REPLY), usage(100, 15, 20, 0, 5), end(OK_REPLY)],
        ])
    );
}

#[test]
fn golden_167_02_resume_same_session() {
    assert_eq!(
        events_of(V167, "02-resume-same-session"),
        concat(vec![
            opening_167(T167_THREAD),
            vec![
                text(RECALLED),
                running_total(240, 40, 30, 10),
                end(RECALLED)
            ],
        ])
    );
}

#[test]
fn golden_167_03_resume_again() {
    assert_eq!(
        events_of(V167, "03-resume-again"),
        concat(vec![
            opening_167(T167_THREAD),
            vec![
                text(RECALLED),
                running_total(360, 60, 45, 15),
                end(RECALLED)
            ],
        ])
    );
}

#[test]
fn golden_167_04_resume_missing_session() {
    assert_eq!(events_of(V167, "04-resume-missing-session"), vec![]);
    assert_eq!(
        exit_event(V167, "04-resume-missing-session"),
        failed(FailureKind::HarnessError, MISSING_ROLLOUT)
    );
}

#[test]
fn golden_167_05_bridge_tool_call() {
    assert_eq!(
        events_of(V167, "05-bridge-tool-call"),
        concat(vec![
            opening_167("01a1ce97-09e4-71b3-a435-cbe77ea68f0f"),
            call("item_1", "echo_tool", "echoed: hi", false),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_167_06_bridge_tool_image() {
    assert_eq!(
        events_of(V167, "06-bridge-tool-image"),
        concat(vec![
            opening_167("01a1ce97-11dc-7b51-964d-e41fa6c52154"),
            call("item_1", "picture_tool", IMAGE_OUTPUT, false),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_167_07_bridge_tool_error() {
    assert_eq!(
        events_of(V167, "07-bridge-tool-error"),
        concat(vec![
            opening_167("01a1ce97-17a9-7232-828c-bbce2d38f2ec"),
            call("item_1", "fail_tool", "fail_tool refused: hi", true),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_167_08_thinking() {
    assert_eq!(
        events_of(V167, "08-thinking"),
        concat(vec![
            opening_167("01a1ce97-2356-73c2-b99a-359bdd2c7963"),
            vec![
                thinking(THOUGHT),
                text(OK_REPLY),
                usage(100, 15, 20, 0, 5),
                end(OK_REPLY)
            ],
        ])
    );
}

#[test]
fn golden_167_09_auth_401() {
    let reason = mock_401(18140);
    let mut expected = opening_167("01a1ce97-2a29-7062-b458-92f14374fe05");
    expected.extend((1..=5).map(|attempt| retry(attempt, &reason)));
    expected.push(note(&format!("codex error: {reason}")));
    expected.push(failed(FailureKind::Auth, &reason));
    assert_eq!(events_of(V167, "09-auth-401"), expected);
}

#[test]
fn golden_167_10_quota_429() {
    assert_eq!(
        events_of(V167, "10-quota-429"),
        concat(vec![
            opening_167("01a1ce97-51ae-7960-b2ac-8d8792744a38"),
            vec![
                note(&format!("codex error: {QUOTA_MESSAGE}")),
                failed(FailureKind::Quota, QUOTA_MESSAGE),
            ],
        ])
    );
}

#[test]
fn golden_167_11_interrupt_sigint() {
    assert_eq!(
        events_of(V167, "11-interrupt-sigint"),
        opening_167("01a1ce97-5a90-7fa1-96f9-859a52fcce8d")
    );
    assert_eq!(
        exit_event(V167, "11-interrupt-sigint"),
        failed(FailureKind::Canceled, CANCELED)
    );
}

#[test]
fn golden_167_12_apply_patch_refused() {
    assert_eq!(
        events_of(V167, "12-apply-patch-refused"),
        concat(vec![
            opening_167("01a1ce97-6cc2-73a1-a813-beaf7c9b2d72"),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_167_13_ask_user_refused() {
    assert_eq!(
        events_of(V167, "13-ask-user-refused"),
        concat(vec![
            opening_167("01a1ce97-7cf1-74d0-ab00-de3af0646ee9"),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_167_14_code_mode_model() {
    assert_eq!(
        events_of(V167, "14-code-mode-model"),
        vec![
            session("01a1ce97-878e-72d0-9f25-854538e9187f"),
            note(CODE_MODE_NOTE),
            text(NO_TOOL),
            usage(100, 15, 20, 0, 5),
            end(NO_TOOL),
        ]
    );
}

#[test]
fn golden_167_15_auth_401_unauthenticated_real_endpoint() {
    assert_eq!(
        events_of(V167, "15-auth-401-unauthenticated-real-endpoint"),
        unauthenticated(
            vec![
                session("01a1ce97-94ec-76a1-b2f4-e62d6149652f"),
                note(CODE_MODE_NOTE)
            ],
            [
                "a593b6fed9369224",
                "a593b703dbcd3490",
                "a593b70aef023490",
                "a593b716ef9257f5"
            ],
            "a593b72c2cde07d9",
            [
                ("a593b72ed926810e", "05082afd51c04c3587ad01dcd9878634"),
                ("a593b733e8250927", "33a0e8efabc34a7eadb776905b3ab1c2"),
                ("a593b73a49483eeb", "9f4128faf63f46e1abbb6210b6a37707"),
                ("a593b742d844facb", "6c5ccd0aa58e40e48e0b4f30db2e1189"),
                ("a593b74faf52a259", "44dfbed6dd5848c48e5ea863a309238f"),
            ],
            ("a593b7660a243eeb", "6347d83ea13b4b379ed067ad1aeed2ba"),
        )
    );
}

// ── the tarball's 0.167.2 recordings ─────────────────────────────────────────

const C12_THREAD: &str = "01a1cd9a-a4b0-79d0-ae59-160f5df00b3d";

#[test]
fn golden_c12_new() {
    assert_eq!(
        events_of(TARBALL, "c12-new"),
        vec![
            session(C12_THREAD),
            note(METADATA_NOTE),
            text(OK_REPLY),
            usage(100, 15, 20, 0, 5),
            end(OK_REPLY),
        ]
    );
}

#[test]
fn golden_c13_resume() {
    assert_eq!(
        events_of(TARBALL, "c13-resume"),
        vec![
            session(C12_THREAD),
            note(METADATA_NOTE),
            text(RECALLED),
            running_total(240, 40, 30, 10),
            end(RECALLED),
        ]
    );
}

#[test]
fn golden_c16_resume2() {
    assert_eq!(
        events_of(TARBALL, "c16-resume2"),
        vec![
            session(C12_THREAD),
            note(METADATA_NOTE),
            text(RECALLED),
            running_total(480, 80, 60, 20),
            end(RECALLED),
        ]
    );
}

#[test]
fn golden_c17_tool_new() {
    assert_eq!(
        events_of(TARBALL, "c17-tool-new"),
        concat(vec![
            vec![
                session("01a1cd9a-cc9b-7732-830d-9a3859dcdfee"),
                note(METADATA_NOTE)
            ],
            call("item_1", "echo_tool", "echoed: hi", false),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_c07_ro_bridge() {
    assert_eq!(
        events_of(TARBALL, "c07-ro-bridge"),
        concat(vec![
            vec![
                session("01a1cd99-dee1-7c33-b871-d8ea841f6fae"),
                note(METADATA_NOTE)
            ],
            call(
                "item_1",
                "echo_tool",
                "MCP tool call requires approval, but approval policy is never",
                true
            ),
            vec![text(AFTER_TOOL), usage(200, 30, 40, 0, 10), end(AFTER_TOOL)],
        ])
    );
}

#[test]
fn golden_c15_resume_missing() {
    assert_eq!(events_of(TARBALL, "c15-resume-missing"), vec![]);
    assert_eq!(
        exit_event(TARBALL, "c15-resume-missing"),
        failed(FailureKind::HarnessError, MISSING_ROLLOUT)
    );
}

#[test]
fn golden_c18_sigint() {
    assert_eq!(
        events_of(TARBALL, "c18-sigint"),
        vec![
            session("01a1cd9b-0167-7693-ab92-400271fd1cd0"),
            note(METADATA_NOTE)
        ]
    );
    assert_eq!(
        exit_event(TARBALL, "c18-sigint"),
        failed(FailureKind::Canceled, CANCELED)
    );
}

#[test]
fn golden_c03_gpt_5_6_terra() {
    assert_eq!(
        events_of(TARBALL, "c03-gpt-5.6-terra"),
        vec![
            session("01a1cd98-51ae-7873-bcad-a241c761ad9b"),
            text(NO_TOOL),
            usage(100, 15, 20, 0, 5),
            end(NO_TOOL),
        ]
    );
}

#[test]
fn golden_c08_ask() {
    assert_eq!(
        events_of(TARBALL, "c08-ask"),
        vec![
            session("01a1cd99-dfde-7462-b92c-de6dfc394768"),
            note(METADATA_NOTE),
            text(AFTER_TOOL),
            usage(200, 30, 40, 0, 10),
            end(AFTER_TOOL),
        ]
    );
}

#[test]
fn golden_c06_patch_readonly() {
    assert_eq!(
        events_of(TARBALL, "c06-patch-readonly"),
        vec![
            session("01a1cd99-dde6-77a3-b91a-17de1aba0e0b"),
            text(AFTER_TOOL),
            usage(200, 30, 40, 0, 10),
            end(AFTER_TOOL),
        ]
    );
}

// ── across recordings ────────────────────────────────────────────────────────

fn all_events() -> Vec<(Recording, Vec<Event>)> {
    RECORDINGS
        .iter()
        .map(|recording| (*recording, events_of(recording.dir, recording.name)))
        .collect()
}

fn has_line_of_type(recording: &Recording, kinds: &[&str]) -> bool {
    recorded_lines(recording).iter().any(|line| {
        let value: Value = serde_json::from_str(line).unwrap_or_default();
        kinds.contains(&value["type"].as_str().unwrap_or_default())
    })
}

/// How many notes each recording produces. Every one comes from Codex's own `error` lines and
/// items, or from a resumed turn's running total.
fn expected_notes(recording: &Recording) -> usize {
    let metadata = usize::from(recording.dir != V154);
    match recording.name {
        "04-resume-missing-session" | "c15-resume-missing" => 0,
        "02-resume-same-session" | "03-resume-again" | "c13-resume" | "c16-resume2" => metadata + 1,
        "09-auth-401" | "10-quota-429" => metadata + 1,
        "14-code-mode-model" => metadata,
        // The code mode note on 0.167.2, the HTTPS fallback item and the final `error` line.
        "15-auth-401-unauthenticated-real-endpoint" => metadata + 2,
        "c03-gpt-5.6-terra" | "c06-patch-readonly" => 0,
        _ => metadata,
    }
}

/// The openings a note can have, one per source in the parse table.
const NOTE_SOURCES: [&str; 3] = [
    "codex error item: ",
    "codex error: ",
    "codex reports a resumed thread's running total, not this turn's spend, so no usage is reported: ",
];

#[test]
fn every_recordings_notes_are_counted_and_come_only_from_the_table() {
    for (recording, events) in all_events() {
        let notes: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                Event::Note(note) => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(
            notes.len(),
            expected_notes(&recording),
            "{}/{}: {notes:?}",
            recording.dir,
            recording.name
        );
        for note in notes {
            assert!(
                NOTE_SOURCES.iter().any(|source| note.starts_with(source)),
                "{}/{}: {note}",
                recording.dir,
                recording.name
            );
        }
    }
}

#[test]
fn a_refused_built_in_tool_leaves_no_note() {
    for (recording, events) in all_events() {
        if [
            "12-apply-patch-refused",
            "13-ask-user-refused",
            "c06-patch-readonly",
            "c08-ask",
        ]
        .contains(&recording.name)
        {
            assert!(
                events.iter().all(|event| match event {
                    Event::Note(note) => note == METADATA_NOTE,
                    _ => true,
                }),
                "{}/{}",
                recording.dir,
                recording.name
            );
        }
    }
}

#[test]
fn a_terminal_event_ends_every_recording_that_printed_a_terminal_line() {
    for (recording, events) in all_events() {
        if has_line_of_type(&recording, &["turn.completed", "turn.failed"]) {
            assert!(
                matches!(
                    events.last(),
                    Some(Event::TurnEnd(_) | Event::TurnFailed(_))
                ),
                "{}/{}",
                recording.dir,
                recording.name
            );
        } else {
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TurnEnd(_) | Event::TurnFailed(_))),
                "{}/{}",
                recording.dir,
                recording.name
            );
        }
    }
}

#[test]
fn no_failed_turn_ends_successfully() {
    for (recording, events) in all_events() {
        if has_line_of_type(&recording, &["turn.failed"]) {
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TurnEnd(_))),
                "{}/{}",
                recording.dir,
                recording.name
            );
        }
    }
}

#[test]
fn no_resumed_recording_reports_usage() {
    for (recording, events) in all_events() {
        if recording.mode == SessionMode::Resume {
            assert!(
                !events.iter().any(|event| matches!(event, Event::Usage(_))),
                "{}/{}",
                recording.dir,
                recording.name
            );
        }
    }
}

#[test]
fn every_session_keeps_the_id_codex_minted() {
    for (recording, events) in all_events() {
        for event in &events {
            if let Event::SessionStarted(info) = event {
                assert!(!info.id.is_empty(), "{}/{}", recording.dir, recording.name);
            }
        }
    }
    // The resumed recordings report the thread their first turn started.
    for (dir, first, later) in [
        (
            V154,
            "01-text-new-session",
            ["02-resume-same-session", "03-resume-again"],
        ),
        (
            V167,
            "01-text-new-session",
            ["02-resume-same-session", "03-resume-again"],
        ),
        (TARBALL, "c12-new", ["c13-resume", "c16-resume2"]),
    ] {
        let id = |name: &str| {
            events_of(dir, name)
                .into_iter()
                .find_map(|event| match event {
                    Event::SessionStarted(info) => Some(info.id),
                    _ => None,
                })
        };
        for name in later {
            assert_eq!(id(name), id(first), "{dir}/{name}");
        }
    }
}

#[test]
fn the_recorded_image_reaches_the_feed_as_its_line_and_never_as_its_bytes() {
    for dir in [V154, V167] {
        let recording = recording(dir, "06-bridge-tool-image");
        let data = recorded_lines(&recording)
            .iter()
            .find_map(|line| {
                let value: Value = serde_json::from_str(line).ok()?;
                value["item"]["result"]["content"]
                    .as_array()?
                    .iter()
                    .find(|item| item["type"] == "image")
                    .and_then(|item| item["data"].as_str())
                    .map(str::to_string)
            })
            .expect("the recording carries the image's base64");
        let feed = format!("{:?}", events_of(dir, "06-bridge-tool-image"));
        assert!(feed.contains("[image: image/png, 692 bytes]"), "{dir}");
        for start in 0..=data.len() - 16 {
            assert!(
                !feed.contains(&data[start..start + 16]),
                "{dir}: the feed carries base64 at {start}"
            );
        }
    }
}

#[test]
fn every_numbered_reconnect_is_a_retry_and_nothing_else_is() {
    for (recording, events) in all_events() {
        let reconnects = recorded_lines(&recording)
            .iter()
            .filter(|line| line.contains(r#""message":"Reconnecting... "#))
            .count();
        let retries = events
            .iter()
            .filter(|event| matches!(event, Event::Retry(_)))
            .count();
        assert_eq!(retries, reconnects, "{}/{}", recording.dir, recording.name);
    }
}
