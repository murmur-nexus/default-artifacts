//! The `usage` a `turn.completed` line reports keeps cache beside input, the shape the
//! process-driver interface's `record usage` requires: `input` counts the prompt tokens neither
//! read from nor written to the cache, so `input + cache-read + cache-creation` is the request's
//! whole prompt. Codex's `input_tokens` counts every prompt token, both cache counts included,
//! so the driver takes them out of it.

use driver_codex_cli::{
    parse_context_for, parse_line, token_usage, Event, ParseState, SessionMode, TokenUsage,
};
use serde_json::json;

/// A `turn.completed` line carrying the given `usage` object.
fn completed_line(usage: serde_json::Value) -> String {
    json!({"type": "turn.completed", "usage": usage}).to_string()
}

/// The single `Usage` event a new session's `turn.completed` line produced.
fn usage_event(line: &str) -> TokenUsage {
    let events = parse_line(
        line,
        &parse_context_for(None, SessionMode::New),
        &mut ParseState::new(),
    );
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
fn the_cache_counts_sit_beside_input_and_the_members_sum_to_input_tokens() {
    let usage = usage_event(&completed_line(json!({
        "input_tokens": 12043,
        "cached_input_tokens": 11780,
        "cache_write_input_tokens": 260,
        "output_tokens": 40,
        "reasoning_output_tokens": 12
    })));
    assert_eq!(
        usage,
        TokenUsage {
            input: Some(3),
            output: Some(40),
            cache_read: Some(11780),
            cache_creation: Some(260),
            thinking: Some(12),
        }
    );
    assert_eq!(request(&usage), 12043);
}

#[test]
fn a_partial_block_reports_what_it_names_and_leaves_the_rest_absent() {
    let usage = usage_event(&completed_line(json!({
        "input_tokens": 120,
        "cached_input_tokens": 20,
        "output_tokens": 15
    })));
    assert_eq!(
        usage,
        TokenUsage {
            input: Some(100),
            output: Some(15),
            cache_read: Some(20),
            cache_creation: None,
            thinking: None,
        }
    );
    assert_eq!(request(&usage), 120);

    // Without `input_tokens` there is no input to report, whatever the cache counts say.
    assert_eq!(
        token_usage(&json!({"cached_input_tokens": 20, "output_tokens": 15})),
        Some(TokenUsage {
            input: None,
            output: Some(15),
            cache_read: Some(20),
            cache_creation: None,
            thinking: None,
        })
    );
}

#[test]
fn a_block_that_names_no_count_is_no_reading() {
    assert_eq!(token_usage(&json!({})), None);
    assert_eq!(
        token_usage(&json!({"input_tokens": "120", "output_tokens": null})),
        None
    );
    assert_eq!(
        parse_line(
            &completed_line(json!({})),
            &parse_context_for(None, SessionMode::New),
            &mut ParseState::new()
        ),
        vec![Event::TurnEnd(String::new())]
    );
}

#[test]
fn input_saturates_at_zero_when_the_cache_counts_exceed_it() {
    assert_eq!(
        token_usage(&json!({
            "input_tokens": 10,
            "cached_input_tokens": 8,
            "cache_write_input_tokens": 9
        }))
        .and_then(|usage| usage.input),
        Some(0)
    );
    assert_eq!(
        token_usage(&json!({"input_tokens": 5, "cached_input_tokens": u64::MAX}))
            .and_then(|usage| usage.input),
        Some(0)
    );
}

#[test]
fn a_reported_zero_is_zero() {
    assert_eq!(
        usage_event(&completed_line(json!({
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0
        }))),
        TokenUsage {
            input: Some(0),
            output: Some(0),
            cache_read: Some(0),
            cache_creation: Some(0),
            thinking: Some(0),
        }
    );
}
