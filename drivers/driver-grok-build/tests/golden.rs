//! The recordings of real `grok` output in `tests/fixtures/`, read into the exact event list each
//! one produces.
//!
//! `01` to `11` are byte-identical copies of recordings of `grok` 1.0.77 against a mock OpenAI
//! Responses endpoint and a mock MCP bridge named `murmur_bridge`. `12-bridge-image-result` is
//! `grok` 1.0.77 run with exactly the argv, environment switches and files `launch` plans for a
//! bridge named `murmur`, against the same kind of mocks, with a bridge tool that answers the way
//! murmur's does when a tool returns an image. Every one authenticated with an API key, so every
//! one reports `auth: "api-key"`.

use driver_grok_build::{
    describe, parse_line, parse_lines, read_login, tool_name_prefix, Event, FailureKind,
    LoginCheck, LoginOutput, LoginState, ParseContext, ParseState, PathAccess, SessionInfo,
    TokenUsage, ToolCallInfo, ToolCallProgress, ToolCallStart, ToolResultInfo, TurnFailure,
};
use serde_json::Value;

/// The MCP server name recordings `01` to `11` were made with.
const RECORDED_SERVER: &str = "murmur_bridge";

/// The server name murmur gives its bridge, which recording `12` was launched with.
const MURMUR_SERVER: &str = "murmur";

/// The model every recording's `init` line reports.
const RECORDED_MODEL: &str = "grok-4.6";

/// Every recording, with the line index a mid-message batch boundary falls on: the `assistant`
/// line carrying a message whose deltas came before it, or — for the recordings that stream no
/// deltas — immediately before the `result` line.
const RECORDINGS: [(&str, usize); 12] = [
    ("01-text-new-session", 7),
    ("02-resume-same-session", 9),
    ("03-new-session-id-in-use", 1),
    ("04-resume-missing-session", 1),
    ("05-bridge-tool-call", 17),
    ("06-operator-tool-refused", 3),
    ("07-unqualified-tool-name", 4),
    ("08-thinking", 11),
    ("09-auth-401", 1),
    ("10-quota-429", 1),
    ("11-interrupt-sigint", 18),
    ("12-bridge-image-result", 17),
];

/// The recordings that stream a tool call's input, with the line index a batch boundary falls on
/// between the `use_tool` block start and its `input_json_delta`. Every other recording streams
/// none, which `events_of` checks.
const TOOL_INPUT_SPLITS: [(&str, usize); 2] =
    [("05-bridge-tool-call", 3), ("12-bridge-image-result", 3)];

// ── reading a recording ──────────────────────────────────────────────────────

/// The context `name` was launched with: a bridge named `murmur` for `12`, `murmur_bridge` for
/// the rest.
fn context_of(name: &str) -> ParseContext {
    let server = if name.starts_with("12-") {
        MURMUR_SERVER
    } else {
        RECORDED_SERVER
    };
    ParseContext::with_prefix(&tool_name_prefix(server))
}

fn recorded_lines(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/{name}.stdout.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{path} must be readable: {error}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// The events a recording produces, having first shown that the partition into batches does not
/// change them. Four partitions, each with its own fresh state: one batch, one line per batch, a
/// boundary inside a message, and — for a recording in [`TOOL_INPUT_SPLITS`] — a boundary
/// between a tool call's block start and its input.
fn events_of(name: &str, split: usize) -> Vec<Event> {
    let context = context_of(name);
    let lines = recorded_lines(name);
    assert_split_is_mid_message(name, &lines, split);

    let whole = parse_lines(&lines, &context, &mut ParseState::new());

    let mut one_by_one = ParseState::new();
    let one_at_a_time: Vec<Event> = lines
        .iter()
        .flat_map(|line| parse_line(line, &context, &mut one_by_one))
        .collect();
    assert_eq!(
        one_at_a_time, whole,
        "{name}: one line per batch must produce the same events as one batch"
    );

    let split_in_two = |at: usize| {
        let (head, tail) = lines.split_at(at);
        let mut across = ParseState::new();
        let mut events = parse_lines(head, &context, &mut across);
        events.extend(parse_lines(tail, &context, &mut across));
        events
    };
    assert_eq!(
        split_in_two(split),
        whole,
        "{name}: a batch boundary inside a message must produce the same events as one batch"
    );

    match tool_input_split(name) {
        Some(at) => {
            assert_split_is_mid_tool_call(name, &lines, at);
            assert_eq!(
                split_in_two(at),
                whole,
                "{name}: a batch boundary inside a tool call must produce the same events as one batch"
            );
        }
        None => assert!(
            !lines.iter().any(|line| is_input_json_delta(line)),
            "{name} streams tool input, so it needs a split in TOOL_INPUT_SPLITS"
        ),
    }

    whole
}

fn tool_input_split(name: &str) -> Option<usize> {
    TOOL_INPUT_SPLITS
        .iter()
        .find(|(recording, _)| *recording == name)
        .map(|(_, split)| *split)
}

/// The boundary really does fall between a `use_tool` block start and its first input delta.
fn assert_split_is_mid_tool_call(name: &str, lines: &[String], split: usize) {
    assert!(
        split > 0 && split < lines.len(),
        "{name}: the tool-call split must fall inside the recording"
    );
    let start = stream_event(&lines[split - 1]);
    assert!(
        start["type"] == "content_block_start"
            && start["content_block"]["type"] == "tool_use"
            && start["content_block"]["name"] == "use_tool",
        "{name}: line {} must be a use_tool block start",
        split - 1
    );
    assert!(
        is_input_json_delta(&lines[split]),
        "{name}: line {split} must be an input_json_delta"
    );
}

/// The `event` of a `stream_event` line, or `null` for any other line.
fn stream_event(line: &str) -> Value {
    let value: Value = serde_json::from_str(line).unwrap_or_default();
    if value["type"] == "stream_event" {
        value["event"].clone()
    } else {
        Value::Null
    }
}

fn is_input_json_delta(line: &str) -> bool {
    stream_event(line)["delta"]["type"] == "input_json_delta"
}

/// The boundary `events_of` splits on really does fall inside a message.
fn assert_split_is_mid_message(name: &str, lines: &[String], split: usize) {
    assert!(
        split > 0 && split < lines.len(),
        "{name}: the split must fall inside the recording"
    );
    let streams_deltas = lines
        .iter()
        .any(|line| line.contains(r#""type":"content_block_delta""#));
    if streams_deltas {
        assert!(
            lines[split].starts_with(r#"{"type":"assistant""#),
            "{name}: line {split} must be an assistant line"
        );
        assert!(
            lines[..split]
                .iter()
                .rev()
                .take_while(|line| !line.starts_with(r#"{"type":"assistant""#))
                .any(|line| line.contains(r#""type":"text_delta""#)
                    || line.contains(r#""type":"thinking_delta""#)),
            "{name}: the split must follow the text deltas of the message at line {split}"
        );
    } else {
        assert!(
            is_result_line(&lines[split]),
            "{name}: a recording that streams no deltas splits before its result line"
        );
    }
}

fn is_result_line(line: &str) -> bool {
    line.starts_with(r#"{"type":"result""#)
}

// ── expected events, spelled out ─────────────────────────────────────────────

fn session(id: &str) -> Event {
    Event::SessionStarted(SessionInfo {
        id: id.to_string(),
        auth: "api-key".to_string(),
        model: Some(RECORDED_MODEL.to_string()),
    })
}

fn delta(text: &str) -> Event {
    Event::TextDelta(text.to_string())
}

fn text(text: &str) -> Event {
    Event::Text(text.to_string())
}

fn end(result: &str) -> Event {
    Event::TurnEnd(result.to_string())
}

fn note(text: &str) -> Event {
    Event::Note(text.to_string())
}

fn failed(kind: FailureKind, message: &str) -> Event {
    Event::TurnFailed(TurnFailure {
        kind,
        message: message.to_string(),
    })
}

/// A reading in which `grok` wrote the four counts it reports.
fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> TokenUsage {
    TokenUsage {
        input: Some(input),
        output: Some(output),
        cache_read: Some(cache_read),
        cache_creation: Some(cache_creation),
        thinking: None,
    }
}

fn spent(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Event {
    Event::Usage(usage(input, output, cache_read, cache_creation))
}

const TOOL_REPLY: &str = "MOCKREPLY tool result received";

/// The tool-result replies of `05` and `12`, as streamed: three deltas of the mock's twelve-byte
/// chunks.
fn tool_reply_deltas() -> Vec<Event> {
    vec![
        delta("MOCKREPLY to"),
        delta("ol result re"),
        delta("ceived"),
    ]
}

const AUTH_401_MESSAGE: &str = "Internal error: {\n  \"message\": \"Unauthorized (401) from http://127.0.0.1:18100/v1/responses: invalid_request_error: mock 401\\n\\n  Model:     grok-4.6\\n  Auth:      ApiKey\\n  Version:   1.0.77\\n  Available: grok-4.6, grok-4.5\",\n  \"http_status\": 401\n}";

/// The base64 of the PNG the image bridge returned in recording `12`: 88 bytes, sha256
/// `51b9d2ba7bccb8df170a166c194c8698b282c5471d03e0b398f657afb2deb13a`.
const RECORDED_IMAGE_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAABgAAAAQCAIAAACDRijCAAAAH0lEQVR42mM4ISdHEMnZnCCIGEYNGjVo1KBRgygzCACqlZyQfrF9rQAAAABJRU5ErkJggg==";

// ── one test per recording ───────────────────────────────────────────────────

#[test]
fn golden_01_text_new_session() {
    assert_eq!(
        events_of("01-text-new-session", 7),
        vec![
            session("e09e81d3-e274-44c9-a103-d36ef44f178a"),
            delta("MOCKREPLY ok"),
            text("MOCKREPLY ok"),
            spent(100, 15, 20, 0),
            end("MOCKREPLY ok"),
        ]
    );
}

#[test]
fn golden_02_resume_same_session() {
    assert_eq!(
        events_of("02-resume-same-session", 9),
        vec![
            session("e09e81d3-e274-44c9-a103-d36ef44f178a"),
            delta("MOCKREPLY sa"),
            delta("w_secret=Tru"),
            delta("e"),
            text("MOCKREPLY saw_secret=True"),
            spent(100, 15, 20, 0),
            end("MOCKREPLY saw_secret=True"),
        ]
    );
}

#[test]
fn golden_03_new_session_id_in_use() {
    // `init` carries an empty session id, which is never reported; the all-zero usage is none.
    assert_eq!(
        events_of("03-new-session-id-in-use", 1),
        vec![failed(
            FailureKind::HarnessError,
            "Session ID e09e81d3-e274-44c9-a103-d36ef44f178a is already in use."
        )]
    );
}

#[test]
fn golden_04_resume_missing_session() {
    assert_eq!(
        events_of("04-resume-missing-session", 1),
        vec![failed(
            FailureKind::HarnessError,
            "Failed to authenticate for session restore: error sending request for url (https://auth.x.ai/.well-known/openid-configuration)"
        )]
    );
}

#[test]
fn golden_05_bridge_tool_call() {
    let mut expected = vec![
        session("01a1cd97-3cb4-7090-aa52-58a6bcc6f437"),
        // Sent once the one `input_json_delta` names `murmur_bridge__echo_tool`.
        Event::ToolCallStarted(ToolCallStart {
            id: "call_1".to_string(),
            name: "echo_tool".to_string(),
        }),
        // `{"tool_name":"murmur_bridge__echo_tool","tool_input":{"text":"hi"}}` is 67 bytes.
        Event::ToolCallProgress(ToolCallProgress {
            id: "call_1".to_string(),
            input_bytes: 67,
        }),
        Event::ToolCall(ToolCallInfo {
            id: "call_1".to_string(),
            name: "echo_tool".to_string(),
            input: r#"{"text":"hi"}"#.to_string(),
        }),
        Event::ToolResult(ToolResultInfo {
            id: "call_1".to_string(),
            output: "echoed: hi".to_string(),
            is_error: false,
        }),
    ];
    expected.extend(tool_reply_deltas());
    expected.extend([text(TOOL_REPLY), spent(200, 30, 40, 0), end(TOOL_REPLY)]);
    assert_eq!(events_of("05-bridge-tool-call", 17), expected);
}

#[test]
fn golden_06_operator_tool_refused() {
    assert_eq!(
        events_of("06-operator-tool-refused", 3),
        vec![
            session("01a1cd92-e251-7722-9175-533150782542"),
            note("grok called use_tool for 'operator_srv__echo_tool', which is not one of the capsule's tools (call call_1)"),
            note("grok answered its own call call_1: 104 bytes, is_error true"),
            spent(100, 15, 20, 0),
            failed(FailureKind::Canceled, "cancelled"),
        ]
    );
}

#[test]
fn golden_07_unqualified_tool_name() {
    assert_eq!(
        events_of("07-unqualified-tool-name", 4),
        vec![
            session("01a1cd92-30ac-7b53-bf2c-95a8905116d4"),
            note("grok called use_tool for 'echo_tool', which is not one of the capsule's tools (call call_1)"),
            note("grok answered its own call call_1: 211 bytes, is_error true"),
            text(TOOL_REPLY),
            spent(200, 30, 40, 0),
            end(TOOL_REPLY),
        ]
    );
}

#[test]
fn golden_08_thinking() {
    assert_eq!(
        events_of("08-thinking", 11),
        vec![
            session("01a1cd93-941d-7d22-8adc-4ddb0107a902"),
            Event::ThinkingDelta("Thinking ".to_string()),
            Event::ThinkingDelta("about it.".to_string()),
            delta("MOCKREPLY ok"),
            Event::Thinking("Thinking about it.".to_string()),
            text("MOCKREPLY ok"),
            spent(100, 15, 20, 0),
            end("MOCKREPLY ok"),
        ]
    );
}

#[test]
fn golden_09_auth_401() {
    assert_eq!(
        events_of("09-auth-401", 1),
        vec![
            session("01a1cd93-8aa6-74f2-9158-2663e7c89820"),
            failed(FailureKind::Auth, AUTH_401_MESSAGE),
        ]
    );
}

#[test]
fn golden_10_quota_429() {
    assert_eq!(
        events_of("10-quota-429", 1),
        vec![
            session("01a1cd93-8b75-7041-aa82-c81555384a63"),
            failed(FailureKind::Quota, "rate_limit_exceeded: mock 429"),
        ]
    );
}

#[test]
fn golden_11_interrupt_sigint() {
    let mut expected = vec![session("01a1cd93-72d4-7bf2-9fa2-fe38e5ae9404")];
    expected.extend((0..12).map(|tick| delta(&format!("tick{tick} "))));
    expected.extend([
        text("tick0 tick1 tick2 tick3 tick4 tick5 tick6 tick7 tick8 tick9 tick10 tick11 "),
        // The closing `result` reports all-zero usage, which is no reading.
        failed(FailureKind::Canceled, "cancelled"),
    ]);
    assert_eq!(events_of("11-interrupt-sigint", 18), expected);
}

#[test]
fn golden_12_bridge_image_result() {
    let mut expected = vec![
        session("019a7c3e-5b10-7a4c-9e21-3f6d2b8a1c05"),
        Event::ToolCallStarted(ToolCallStart {
            id: "call_1".to_string(),
            name: "image_tool".to_string(),
        }),
        // `{"tool_name":"murmur__image_tool","tool_input":{"text":"hi"}}` is 61 bytes.
        Event::ToolCallProgress(ToolCallProgress {
            id: "call_1".to_string(),
            input_bytes: 61,
        }),
        Event::ToolCall(ToolCallInfo {
            id: "call_1".to_string(),
            name: "image_tool".to_string(),
            input: r#"{"text":"hi"}"#.to_string(),
        }),
        Event::ToolResult(ToolResultInfo {
            id: "call_1".to_string(),
            // The bridge's text item, then `grok`'s data-URL line for its image item, read as the
            // image line.
            output: "IMAGE-TOOL-RAN\n[image attached: picture.png, image/png, 88 bytes]\n[image: image/png, 88 bytes]".to_string(),
            is_error: false,
        }),
    ];
    expected.extend(tool_reply_deltas());
    expected.extend([text(TOOL_REPLY), spent(200, 30, 40, 0), end(TOOL_REPLY)]);
    assert_eq!(events_of("12-bridge-image-result", 17), expected);
}

#[test]
fn the_recorded_image_reaches_the_feed_as_its_line_and_never_as_its_bytes() {
    let user_line = recorded_lines("12-bridge-image-result")
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("every line is JSON"))
        .find(|value| value["type"] == "user")
        .expect("the recording carries the tool result's user line");

    // What `grok` printed: the bridged result as a JSON string, the image written into the
    // `OkayOutput` text as a data URL.
    let content = user_line["message"]["content"][0]["content"]
        .as_str()
        .expect("grok writes a bridged result as a string");
    let wrapper: Value = serde_json::from_str(content).expect("the string is JSON");
    assert_eq!(wrapper["type"], "MCP");
    assert_eq!(wrapper["server_name"], MURMUR_SERVER);
    let output = wrapper["output"]["OkayOutput"]
        .as_str()
        .expect("the output is one string");
    assert!(output.ends_with(&format!("\ndata:image/png;base64,{RECORDED_IMAGE_BASE64}")));

    let events = events_of("12-bridge-image-result", 17);
    let fed = format!("{events:?}");
    assert!(fed.contains("[image: image/png, 88 bytes]"));
    for window in RECORDED_IMAGE_BASE64.as_bytes().windows(16) {
        let run = std::str::from_utf8(window).expect("base64 is ASCII");
        assert!(!fed.contains(run), "{run} leaked into the events");
    }
}

// ── what holds across all of them ────────────────────────────────────────────

fn all_events() -> Vec<(&'static str, Vec<Event>)> {
    RECORDINGS
        .iter()
        .map(|(name, split)| (*name, events_of(name, *split)))
        .collect()
}

#[test]
fn only_the_two_foreign_call_recordings_produce_notes_two_each() {
    for (name, events) in all_events() {
        let notes = events
            .iter()
            .filter(|event| matches!(event, Event::Note(_)))
            .count();
        let expected = match name {
            "06-operator-tool-refused" | "07-unqualified-tool-name" => 2,
            _ => 0,
        };
        assert_eq!(notes, expected, "{name}: {events:?}");
    }
}

#[test]
fn every_recording_ends_with_a_terminal_event() {
    // So the runtime never reaches `classify-exit` for any of them, including the four that exit
    // non-zero.
    for (name, events) in all_events() {
        assert!(
            matches!(
                events.last(),
                Some(Event::TurnEnd(_) | Event::TurnFailed(_))
            ),
            "{name} ends on {:?}",
            events.last()
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::TurnEnd(_) | Event::TurnFailed(_)))
                .count(),
            1,
            "{name}"
        );
    }
}

#[test]
fn a_recording_whose_harness_reported_an_error_never_ends_the_turn_successfully() {
    let failing: Vec<&str> = RECORDINGS
        .iter()
        .filter(|(name, _)| {
            recorded_lines(name)
                .iter()
                .any(|line| is_result_line(line) && line.contains(r#""is_error":true"#))
        })
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        failing,
        [
            "03-new-session-id-in-use",
            "04-resume-missing-session",
            "06-operator-tool-refused",
            "09-auth-401",
            "10-quota-429",
            "11-interrupt-sigint",
        ]
    );
    for (name, events) in all_events() {
        if failing.contains(&name) {
            assert!(
                matches!(events.last(), Some(Event::TurnFailed(_))),
                "{name} ends on {:?}",
                events.last()
            );
        }
    }
}

#[test]
fn a_session_that_could_not_start_reports_no_session() {
    for (name, events) in all_events() {
        let started = events
            .iter()
            .filter(|event| matches!(event, Event::SessionStarted(_)))
            .count();
        let expected = match name {
            "03-new-session-id-in-use" | "04-resume-missing-session" => 0,
            _ => 1,
        };
        assert_eq!(started, expected, "{name}");
        for event in &events {
            if let Event::SessionStarted(info) = event {
                assert!(!info.id.is_empty(), "{name}");
            }
        }
    }
}

#[test]
fn the_declared_text_streaming_is_what_the_recordings_do() {
    assert!(describe().streams_text);
    assert!(all_events()
        .iter()
        .any(|(_, events)| events.iter().any(|e| matches!(e, Event::TextDelta(_)))));
}

#[test]
fn a_recorded_call_under_another_prefix_is_never_a_capsule_call() {
    // Read with murmur's server name, recording 05's `murmur_bridge__echo_tool` call is not the
    // capsule's: only the prefix `launch` built comes off.
    let events = parse_lines(
        &recorded_lines("05-bridge-tool-call"),
        &ParseContext::with_prefix(&tool_name_prefix(MURMUR_SERVER)),
        &mut ParseState::new(),
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolCall(_) | Event::ToolCallStarted(_) | Event::ToolResult(_)
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Note(_)))
            .count(),
        2
    );
}

// ── tokens ───────────────────────────────────────────────────────────────────

fn usage_in(events: &[Event]) -> Vec<TokenUsage> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Usage(usage) => Some(usage.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_recording_reports_exactly_the_counts_it_recorded() {
    // Read off the fixture bytes and written down here, so a change to a recording or to the
    // reader has to be argued with rather than absorbed. All-zero blocks report nothing.
    let expected: [(&str, Vec<TokenUsage>); 12] = [
        ("01-text-new-session", vec![usage(100, 15, 20, 0)]),
        ("02-resume-same-session", vec![usage(100, 15, 20, 0)]),
        ("03-new-session-id-in-use", vec![]),
        ("04-resume-missing-session", vec![]),
        ("05-bridge-tool-call", vec![usage(200, 30, 40, 0)]),
        ("06-operator-tool-refused", vec![usage(100, 15, 20, 0)]),
        ("07-unqualified-tool-name", vec![usage(200, 30, 40, 0)]),
        ("08-thinking", vec![usage(100, 15, 20, 0)]),
        ("09-auth-401", vec![]),
        ("10-quota-429", vec![]),
        ("11-interrupt-sigint", vec![]),
        ("12-bridge-image-result", vec![usage(200, 30, 40, 0)]),
    ];
    for (name, counts) in expected {
        let split = RECORDINGS
            .iter()
            .find(|(recording, _)| *recording == name)
            .expect("a named recording is in the table")
            .1;
        assert_eq!(usage_in(&events_of(name, split)), counts, "{name}");
    }
}

#[test]
fn no_line_but_a_result_line_reports_a_count() {
    for (name, _) in RECORDINGS {
        for line in recorded_lines(name) {
            let events = parse_line(&line, &context_of(name), &mut ParseState::new());
            if !usage_in(&events).is_empty() {
                assert!(is_result_line(&line), "{name}: {line}");
            }
        }
    }
}

// ── describe and the login check ─────────────────────────────────────────────

fn login_output(exit_code: Option<i32>, stdout: &str) -> LoginOutput {
    LoginOutput {
        exit_code,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

#[test]
fn the_login_check_reads_the_first_line_grok_models_printed() {
    // The three first lines `grok models` 1.0.77 was observed printing on 2026-11-11.
    assert_eq!(
        read_login(&login_output(
            Some(0),
            "You are logged in with grok.com.\n\nAvailable models:\n  grok-4.6\n"
        )),
        LoginState::LoggedIn
    );
    assert_eq!(
        read_login(&login_output(Some(0), "You are using XAI_API_KEY.\n")),
        LoginState::LoggedIn
    );
    assert!(matches!(
        read_login(&login_output(Some(0), "You are not authenticated.\n")),
        LoginState::LoggedOut(reason) if reason.contains("grok login") && !reason.contains("You are")
    ));
    assert!(matches!(
        read_login(&login_output(Some(1), "error: boom\n")),
        LoginState::Unknown(reason) if !reason.contains("boom")
    ));
}

#[test]
fn the_driver_declares_its_login_command_and_needs() {
    let description = describe();
    assert_eq!(
        description.login_check,
        LoginCheck::Command(vec!["models".to_string()])
    );
    assert_eq!(description.harness_needs.paths.len(), 1);
    assert_eq!(description.harness_needs.paths[0].path, "~/.grok");
    assert_eq!(
        description.harness_needs.paths[0].access,
        PathAccess::ReadWrite
    );
    assert_eq!(description.harness_needs.hosts, ["api.x.ai", "auth.x.ai"]);
    assert_eq!(description.tested_versions, ["1.0.77"]);
}
