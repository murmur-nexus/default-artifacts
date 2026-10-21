//! The `usage` a `result` line reports keeps cache beside input, the shape the process-driver
//! interface's `record usage` requires: `input` counts the prompt tokens neither read from nor
//! written to the cache, so `input + cache-read + cache-creation` is the turn's whole request.
//! Claude's `usage` block is Anthropic-shaped and already reports `input_tokens` that way, so
//! the driver relays it member for member.

use driver_claude_code::{parse_context_for, parse_line, Event, ParseState, TokenUsage};
use serde_json::json;

/// A successful `result` line carrying the given `usage` block.
fn result_line(usage: serde_json::Value) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "done",
        "usage": usage
    })
    .to_string()
}

/// The single `Usage` event a `result` line produced.
fn usage_event(line: &str, state: &mut ParseState) -> TokenUsage {
    let events = parse_line(line, &parse_context_for(None), state);
    match events.as_slice() {
        [Event::Usage(usage), Event::TurnEnd(_)] => usage.clone(),
        other => panic!("expected a usage event then the turn end, got {other:?}"),
    }
}

/// `input + cache-read + cache-creation`, an absent member counted as `0`.
fn request(usage: &TokenUsage) -> u64 {
    [usage.input, usage.cache_read, usage.cache_creation]
        .iter()
        .map(|count| count.unwrap_or(0))
        .sum()
}

#[test]
fn usage_members_sum_to_the_provider_request_total() {
    let mut state = ParseState::new();

    let first = usage_event(
        &result_line(json!({
            "input_tokens": 3,
            "cache_read_input_tokens": 11780,
            "cache_creation_input_tokens": 260,
            "output_tokens": 40
        })),
        &mut state,
    );
    assert_eq!(
        first,
        TokenUsage {
            input: Some(3),
            output: Some(40),
            cache_read: Some(11780),
            cache_creation: Some(260),
            thinking: None,
        }
    );
    assert_eq!(request(&first), 12043);

    // A second turn on the same process: the run totals add each member on its own, so `input`
    // stays free of both cache counts.
    let second = usage_event(
        &result_line(json!({
            "input_tokens": 2,
            "cache_read_input_tokens": 12040,
            "cache_creation_input_tokens": 45,
            "output_tokens": 18
        })),
        &mut state,
    );
    assert_eq!(
        second,
        TokenUsage {
            input: Some(5),
            output: Some(58),
            cache_read: Some(23820),
            cache_creation: Some(305),
            thinking: None,
        }
    );
    assert_eq!(request(&second), 12043 + 12087);
}
