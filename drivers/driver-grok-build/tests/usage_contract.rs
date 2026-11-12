//! The `usage` a `result` line reports keeps cache beside input, the shape the process-driver
//! interface's `record usage` requires: `input` counts the prompt tokens neither read from nor
//! written to the cache, so `input + cache-read + cache-creation` is the run's whole request.
//! `grok`'s `usage` block already reports `input_tokens` uncached, so the driver relays it member
//! for member. All four at zero is `grok` saying it knows none of them, and reports nothing.

use driver_grok_build::{parse_context_for, parse_line, Event, ParseState, TokenUsage};
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

fn events(line: &str, state: &mut ParseState) -> Vec<Event> {
    parse_line(line, &parse_context_for(None), state)
}

/// The single `Usage` event a `result` line produced.
fn usage_event(line: &str, state: &mut ParseState) -> TokenUsage {
    match events(line, state).as_slice() {
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
fn cache_counts_sit_beside_input() {
    let mut state = ParseState::new();
    let usage = usage_event(
        &result_line(json!({
            "input_tokens": 200,
            "output_tokens": 30,
            "cache_read_input_tokens": 40,
            "cache_creation_input_tokens": 7,
            "server_tool_use": { "web_search_requests": 0 }
        })),
        &mut state,
    );
    assert_eq!(
        usage,
        TokenUsage {
            input: Some(200),
            output: Some(30),
            cache_read: Some(40),
            cache_creation: Some(7),
            thinking: None,
        }
    );
    assert_eq!(request(&usage), 247);
}

#[test]
fn all_zero_reports_nothing() {
    let mut state = ParseState::new();
    assert_eq!(
        events(
            &result_line(json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "server_tool_use": { "web_search_requests": 0 }
            })),
            &mut state,
        ),
        vec![Event::TurnEnd("done".to_string())]
    );
}

#[test]
fn a_partial_block_reports_what_it_carries_and_leaves_the_rest_absent() {
    let mut state = ParseState::new();
    let usage = usage_event(
        &result_line(json!({ "input_tokens": 12, "cache_read_input_tokens": 0 })),
        &mut state,
    );
    assert_eq!(
        usage,
        TokenUsage {
            input: Some(12),
            output: None,
            cache_read: Some(0),
            cache_creation: None,
            thinking: None,
        }
    );
    assert_eq!(request(&usage), 12);
}
