//! The ten recordings of real `claude` 2.1.278 output in `tests/fixtures/`, read into the exact
//! event list each one produces.
//!
//! The recordings are byte-identical copies of `.nexus/roadmap/fixtures/claude-code-2.1.278/`,
//! recorded against a mock Anthropic endpoint and a mock MCP bridge named `claude_bridge`. Every
//! one authenticated with an API key, so every one reports `auth: "api-key"`; the subscription
//! reading is covered by a unit test instead.

use murmur_driver_claude_code::{
    describe, parse_line, parse_line_usage, parse_lines, parse_lines_usage, tool_name_prefix,
    Event, FailureKind, ParseContext, RetryInfo, SessionInfo, TokenUsage, ToolCallInfo,
    ToolResultInfo, TurnFailure,
};

/// The MCP server name the recordings were made with. `launch` builds the harness's tool prefix
/// from the bridge the runtime names, and `parse` strips that same prefix back off.
const RECORDED_SERVER: &str = "claude_bridge";

/// The model every recording's `init` line reports.
const RECORDED_MODEL: &str = "claude-opus-5[1m]";

/// Every recording, with the line index a mid-message batch boundary falls on: between a
/// message's `content_block_delta` lines and the `assistant` line carrying its full text, or —
/// for the three recordings that stream no deltas — immediately before the `result` line.
const RECORDINGS: [(&str, usize); 10] = [
    ("01-text-new-session", 5),
    ("02-resume-same-session", 5),
    ("03-bridge-tool-call", 14),
    ("04-thinking", 12),
    ("05-auth-401", 3),
    ("06-quota-429", 3),
    ("07-quota-429-after-retries", 5),
    ("08-interrupt-control-request", 9),
    ("09-interrupt-sigint", 9),
    ("10-resume-after-sigint", 5),
];

// ── reading a recording ──────────────────────────────────────────────────────

/// The context a run bridged the way the recordings were reads its output against.
fn recorded_context() -> ParseContext {
    ParseContext::with_prefix(&tool_name_prefix(RECORDED_SERVER))
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
/// change them: the parser holds nothing across lines, so a batch is exactly its lines.
fn events_of(name: &str, split: usize) -> Vec<Event> {
    let lines = recorded_lines(name);
    let context = recorded_context();
    assert_split_is_mid_message(name, &lines, split);

    let whole = parse_lines(&lines, &context);

    let one_at_a_time: Vec<Event> = lines
        .iter()
        .flat_map(|line| parse_line(line, &context))
        .collect();
    assert_eq!(
        one_at_a_time, whole,
        "{name}: one line per batch must produce the same events as one batch"
    );

    let (head, tail) = lines.split_at(split);
    let mut split_in_two = parse_lines(head, &context);
    split_in_two.extend(parse_lines(tail, &context));
    assert_eq!(
        split_in_two, whole,
        "{name}: a batch boundary inside a message must produce the same events as one batch"
    );

    whole
}

/// The boundary `events_of` splits on really does fall inside a message: a batch that ends
/// after a message's deltas but before the `assistant` line carrying its full text is the case
/// a parser holding partial state would get wrong.
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
            "{name}: line {split} must be the assistant line carrying a message's full text"
        );
        assert!(
            lines[..split]
                .iter()
                .rev()
                .take_while(|line| !line.starts_with(r#"{"type":"assistant""#))
                .any(|line| line.contains(r#""type":"content_block_delta""#)),
            "{name}: the split must follow the deltas of the message at line {split}"
        );
    } else {
        assert!(
            is_result_line(&lines[split]),
            "{name}: a recording that streams no deltas splits before its result line"
        );
    }
}

fn is_result_line(line: &str) -> bool {
    line.contains(r#""type":"result""#)
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

fn failed(kind: FailureKind, message: &str) -> Event {
    Event::TurnFailed(TurnFailure {
        kind,
        message: message.to_string(),
    })
}

/// The reply the mock endpoint gives when it has not seen the first message.
const FRESH_REPLY: &str = "MOCKREPLY saw_secret=False tools_offered=1";

/// The same reply once the session carries the first message.
const RESUMED_REPLY: &str = "MOCKREPLY saw_secret=True tools_offered=1";

const QUOTA_MESSAGE: &str = "API Error: Request rejected (429) · MOCK: rate limit window exhausted";

const ABORTED_MESSAGE: &str = "error_during_execution, terminal_reason: aborted_streaming";

// ── one test per recording ───────────────────────────────────────────────────

#[test]
fn golden_01_text_new_session() {
    assert_eq!(
        events_of("01-text-new-session", 5),
        vec![
            session("aaaaaaaa-1111-4222-8333-444444444444"),
            delta(RESUMED_REPLY),
            text(RESUMED_REPLY),
            end(RESUMED_REPLY),
        ]
    );
}

#[test]
fn golden_02_resume_same_session() {
    assert_eq!(
        events_of("02-resume-same-session", 5),
        vec![
            session("aaaaaaaa-1111-4222-8333-444444444444"),
            delta(RESUMED_REPLY),
            text(RESUMED_REPLY),
            end(RESUMED_REPLY),
        ]
    );
}

#[test]
fn golden_03_bridge_tool_call() {
    assert_eq!(
        events_of("03-bridge-tool-call", 14),
        vec![
            session("690ceb7b-0888-4823-ac71-b61cb3dd7d55"),
            Event::ToolCall(ToolCallInfo {
                id: "toolu_1".to_string(),
                // The `mcp__claude_bridge__` the harness printed is off: it is the prefix this
                // driver put there in `launch`.
                name: "echo_tool".to_string(),
                input: r#"{"text":"hi"}"#.to_string(),
            }),
            Event::ToolResult(ToolResultInfo {
                id: "toolu_1".to_string(),
                output: r#"ECHO:{"text": "hi"}"#.to_string(),
                is_error: false,
            }),
            delta(FRESH_REPLY),
            text(FRESH_REPLY),
            end(FRESH_REPLY),
        ]
    );
}

#[test]
fn golden_04_thinking() {
    assert_eq!(
        events_of("04-thinking", 12),
        vec![
            session("cdc34982-961d-49f5-9c14-7d9e0efdbb7a"),
            Event::ThinkingDelta("Let me consider the request.".to_string()),
            Event::Thinking("Let me consider the request.".to_string()),
            delta("Answer after thinking. "),
            delta(FRESH_REPLY),
            text(&format!("Answer after thinking. {FRESH_REPLY}")),
            end(&format!("Answer after thinking. {FRESH_REPLY}")),
        ]
    );
}

#[test]
fn golden_05_auth_401() {
    let message = "Invalid API key · Fix external API key";
    assert_eq!(
        events_of("05-auth-401", 3),
        vec![
            session("237dbd29-7562-4247-8522-b1c8e7668544"),
            // The `<synthetic>` message `claude` writes for an API error is read like any other.
            text(message),
            failed(FailureKind::Auth, message),
        ]
    );
}

#[test]
fn golden_06_quota_429() {
    assert_eq!(
        events_of("06-quota-429", 3),
        vec![
            session("4577d02b-aa7a-4cec-9b8a-25e13414fd7a"),
            text(QUOTA_MESSAGE),
            failed(FailureKind::Quota, QUOTA_MESSAGE),
        ]
    );
}

#[test]
fn golden_07_quota_429_after_retries() {
    let retry = |attempt| {
        Event::Retry(RetryInfo {
            attempt,
            reason: "rate_limit (429)".to_string(),
        })
    };
    assert_eq!(
        events_of("07-quota-429-after-retries", 5),
        vec![
            session("b1fcdb7a-94fb-4254-aa09-702891535502"),
            retry(1),
            retry(2),
            text(QUOTA_MESSAGE),
            failed(FailureKind::Quota, QUOTA_MESSAGE),
        ]
    );
}

#[test]
fn golden_08_interrupt_control_request() {
    // The interrupt ends the first turn; the harness then answers a second message in the same
    // process, so the recording carries two turns and two `init` lines.
    assert_eq!(
        events_of("08-interrupt-control-request", 9),
        vec![
            session("bbbbbbbb-1111-4222-8333-444444444444"),
            delta("tick0 "),
            delta("tick1 "),
            delta("tick2 "),
            delta("tick3 "),
            text("tick0 tick1 tick2 tick3 "),
            failed(FailureKind::Canceled, ABORTED_MESSAGE),
            session("bbbbbbbb-1111-4222-8333-444444444444"),
            delta(FRESH_REPLY),
            text(FRESH_REPLY),
            end(FRESH_REPLY),
        ]
    );
}

#[test]
fn golden_09_interrupt_sigint() {
    assert_eq!(
        events_of("09-interrupt-sigint", 9),
        vec![
            session("cccccccc-1111-4222-8333-444444444444"),
            delta("tick0 "),
            delta("tick1 "),
            delta("tick2 "),
            delta("tick3 "),
            delta("tick4 "),
            text("tick0 tick1 tick2 tick3 tick4 "),
            failed(FailureKind::Canceled, ABORTED_MESSAGE),
        ]
    );
}

#[test]
fn golden_10_resume_after_sigint() {
    assert_eq!(
        events_of("10-resume-after-sigint", 5),
        vec![
            session("cccccccc-1111-4222-8333-444444444444"),
            delta(FRESH_REPLY),
            text(FRESH_REPLY),
            end(FRESH_REPLY),
        ]
    );
}

// ── what holds across all ten ────────────────────────────────────────────────

fn all_events() -> Vec<(&'static str, Vec<Event>)> {
    RECORDINGS
        .iter()
        .map(|(name, split)| (*name, events_of(name, *split)))
        .collect()
}

#[test]
fn a_recording_whose_harness_reported_an_error_never_ends_the_turn_successfully() {
    // The defect this driver closes, measured on the recordings rather than argued about: a
    // `result` line saying `is_error: true` must never reach the runtime as an answer.
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
            "05-auth-401",
            "06-quota-429",
            "07-quota-429-after-retries",
            "08-interrupt-control-request",
            "09-interrupt-sigint",
        ]
    );

    let kind_of = |name: &str| {
        let split = RECORDINGS
            .iter()
            .find(|(recording, _)| *recording == name)
            .expect("a named recording is in the table")
            .1;
        events_of(name, split)
            .into_iter()
            .find_map(|event| match event {
                Event::TurnFailed(failure) => Some(failure.kind),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name} carries a failing result line"))
    };

    assert_eq!(kind_of("05-auth-401"), FailureKind::Auth);
    assert_eq!(kind_of("06-quota-429"), FailureKind::Quota);
    assert_eq!(kind_of("07-quota-429-after-retries"), FailureKind::Quota);
    assert_eq!(
        kind_of("08-interrupt-control-request"),
        FailureKind::Canceled
    );
    assert_eq!(kind_of("09-interrupt-sigint"), FailureKind::Canceled);
}

#[test]
fn subtype_alone_cannot_have_produced_the_auth_and_quota_answers() {
    for (name, expected) in [
        ("05-auth-401", FailureKind::Auth),
        ("06-quota-429", FailureKind::Quota),
    ] {
        let result_line = recorded_lines(name)
            .into_iter()
            .find(|line| is_result_line(line))
            .expect("each recording ends with a result line");
        assert!(
            result_line.contains(r#""subtype":"success""#),
            "{name}: the recording really does report this failure as a success"
        );
        let Some(Event::TurnFailed(failure)) = events_of(name, 3).pop() else {
            panic!("{name} ends with a turn-failed");
        };
        assert_eq!(failure.kind, expected);
    }
}

#[test]
fn no_recording_produces_a_note() {
    for (name, events) in all_events() {
        let notes: Vec<&Event> = events
            .iter()
            .filter(|event| matches!(event, Event::Note(_)))
            .collect();
        assert!(
            notes.is_empty(),
            "{name}: the driver could not read {notes:?}"
        );
    }
}

#[test]
fn every_recording_ends_with_a_terminal_event() {
    // So the runtime never reaches `classify-exit` for any of them — including the two that
    // exit `1`, whose failure is read out of the stream rather than guessed from the code.
    for (name, events) in all_events() {
        assert!(
            matches!(
                events.last(),
                Some(Event::TurnEnd(_) | Event::TurnFailed(_))
            ),
            "{name}: ended on {:?}",
            events.last()
        );
    }
}

#[test]
fn the_declared_text_streaming_is_what_the_recordings_do() {
    assert!(describe().streams_text);
    assert!(
        all_events().iter().any(|(_, events)| events
            .iter()
            .any(|event| matches!(event, Event::TextDelta(_)))),
        "describe declares streamed text, so some recording must stream some"
    );
}

#[test]
fn a_recorded_tool_name_keeps_a_prefix_this_driver_did_not_add() {
    let harness_name = "mcp__claude_bridge__echo_tool";
    let lines = recorded_lines("03-bridge-tool-call");

    let called = |context: &ParseContext| {
        parse_lines(&lines, context)
            .into_iter()
            .find_map(|event| match event {
                Event::ToolCall(call) => Some(call.name),
                _ => None,
            })
            .expect("the recording calls a tool")
    };

    assert_eq!(called(&recorded_context()), "echo_tool");
    assert_eq!(called(&ParseContext::none()), harness_name);
    assert_eq!(
        called(&ParseContext::with_prefix(&tool_name_prefix(
            "other_server"
        ))),
        harness_name
    );
}

// ── the tokens each recording reports ────────────────────────────────────────

/// The token counts a recording reports, having first shown that the partition into batches
/// does not change them either: the reading of a batch is the readings of its lines.
fn usage_of(name: &str, split: usize) -> Vec<TokenUsage> {
    let lines = recorded_lines(name);

    let whole = parse_lines_usage(&lines);

    let one_at_a_time: Vec<TokenUsage> = lines
        .iter()
        .filter_map(|line| parse_line_usage(line))
        .collect();
    assert_eq!(
        one_at_a_time, whole,
        "{name}: one line per batch must report the same counts as one batch"
    );

    let (head, tail) = lines.split_at(split);
    let mut split_in_two = parse_lines_usage(head);
    split_in_two.extend(parse_lines_usage(tail));
    assert_eq!(
        split_in_two, whole,
        "{name}: a batch boundary inside a message must report the same counts as one batch"
    );

    whole
}

/// A reading in which the harness wrote all five counts, which is what all ten recordings do.
fn usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    thinking: u64,
) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_tokens: Some(cache_read),
        cache_creation_tokens: Some(cache_creation),
        thinking_tokens: Some(thinking),
    }
}

#[test]
fn golden_usage_01_text_new_session() {
    assert_eq!(
        usage_of("01-text-new-session", 5),
        vec![usage(1, 5, 0, 0, 0)]
    );
}

#[test]
fn golden_usage_03_bridge_tool_call_reports_the_turn_once() {
    // Two assistant messages, each reporting one input and no output of its own; two
    // `message_delta` lines, each reporting a running five output tokens for its message. The
    // turn spent 2 and 10, and the `result` line is the only line that says so. Summing the
    // deltas would report 10 a second time; adding them to the result would report 20.
    let lines = recorded_lines("03-bridge-tool-call");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains(r#""type":"message_delta""#))
            .count(),
        2,
        "the recording really does carry two running output counts"
    );

    assert_eq!(
        usage_of("03-bridge-tool-call", 14),
        vec![usage(2, 10, 0, 0, 0)]
    );
}

#[test]
fn golden_usage_08_interrupt_reports_each_of_its_two_turns() {
    // The canceled turn spent nothing and said so; the turn that followed it in the same
    // process reported its own totals.
    assert_eq!(
        usage_of("08-interrupt-control-request", 9),
        vec![usage(0, 0, 0, 0, 0), usage(1, 5, 0, 0, 0)]
    );
}

#[test]
fn every_recording_reports_one_reading_per_result_line() {
    for (name, split) in RECORDINGS {
        let result_lines = recorded_lines(name)
            .iter()
            .filter(|line| is_result_line(line))
            .count();
        let readings = usage_of(name, split);
        assert_eq!(
            readings.len(),
            result_lines,
            "{name}: {result_lines} result lines must give {result_lines} readings, not {}",
            readings.len()
        );
    }
}

#[test]
fn every_recording_reports_exactly_the_counts_it_recorded() {
    // Read off the fixture bytes with jq and written down here, so a change to a recording or
    // to the reader has to be argued with rather than absorbed.
    let expected: [(&str, Vec<TokenUsage>); 10] = [
        ("01-text-new-session", vec![usage(1, 5, 0, 0, 0)]),
        ("02-resume-same-session", vec![usage(1, 5, 0, 0, 0)]),
        ("03-bridge-tool-call", vec![usage(2, 10, 0, 0, 0)]),
        ("04-thinking", vec![usage(1, 5, 0, 0, 0)]),
        ("05-auth-401", vec![usage(0, 0, 0, 0, 0)]),
        ("06-quota-429", vec![usage(0, 0, 0, 0, 0)]),
        ("07-quota-429-after-retries", vec![usage(0, 0, 0, 0, 0)]),
        (
            "08-interrupt-control-request",
            vec![usage(0, 0, 0, 0, 0), usage(1, 5, 0, 0, 0)],
        ),
        ("09-interrupt-sigint", vec![usage(0, 0, 0, 0, 0)]),
        ("10-resume-after-sigint", vec![usage(1, 5, 0, 0, 0)]),
    ];

    for (name, counts) in expected {
        let split = RECORDINGS
            .iter()
            .find(|(recording, _)| *recording == name)
            .expect("a named recording is in the table")
            .1;
        assert_eq!(usage_of(name, split), counts, "{name}");
    }
}

#[test]
fn a_failing_recording_keeps_its_verdict_and_reports_only_what_it_recorded() {
    // The counts are read off the same line as the verdict, and neither decides the other: a
    // turn nobody could authenticate still reports the zeros its harness wrote.
    for (name, split, kind) in [
        ("05-auth-401", 3, FailureKind::Auth),
        ("06-quota-429", 3, FailureKind::Quota),
        ("07-quota-429-after-retries", 5, FailureKind::Quota),
    ] {
        let events = events_of(name, split);
        assert!(
            matches!(events.last(), Some(Event::TurnFailed(failure)) if failure.kind == kind),
            "{name}: ended on {:?}",
            events.last()
        );
        assert_eq!(usage_of(name, split), vec![usage(0, 0, 0, 0, 0)], "{name}");
    }

    // The two retries still precede the quota failure they were retrying towards.
    let retries: Vec<Event> = events_of("07-quota-429-after-retries", 5)
        .into_iter()
        .filter(|event| matches!(event, Event::Retry(_)))
        .collect();
    let retry = |attempt| {
        Event::Retry(RetryInfo {
            attempt,
            reason: "rate_limit (429)".to_string(),
        })
    };
    assert_eq!(retries, vec![retry(1), retry(2)]);
}

#[test]
fn no_line_but_a_result_line_reports_a_count() {
    // Across all ten recordings, every reading is a result line's and every result line gives
    // one — measured line by line rather than in aggregate.
    for (name, _) in RECORDINGS {
        for line in recorded_lines(name) {
            let reported = parse_line_usage(&line).is_some();
            assert_eq!(
                reported,
                is_result_line(&line),
                "{name}: {line} reported {reported}"
            );
        }
    }
}
