//! Reading a WIT `message`'s `content` back into text.
//!
//! The lifecycle `message` record carries `content` as one string, so a tool result —
//! which needs a `tool_call_id` and an `is_error` flag beside its body — is folded into a
//! JSON envelope before it is handed to a hook. Any hook that forwards recorded messages
//! to a driver has to undo that, and has to agree with the host on the marker's spelling
//! and the envelope's field names, which is why this lives in one place.

use serde_json::Value;

/// Marker the host folds into a `"tool"`-role message's `content`, because the WIT
/// `message` record has no room for the sibling `tool_call_id`/`is_error` fields every
/// inference driver requires. Kept in sync with `TOOL_MARKER` in murmur's
/// `capsule-runtime/src/agent.rs`.
pub const TOOL_MARKER: &str = "__murmur_tool_msg__";

/// Readable text out of a message's `content`, which is stored as its JSON serialization
/// — an array of content blocks, or a plain string. Content that is neither is returned
/// verbatim.
pub fn extract_text(content: &str) -> String {
    if let Ok(Value::Array(blocks)) = serde_json::from_str(content) {
        let parts: Vec<&str> = blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    if let Ok(Value::String(s)) = serde_json::from_str(content) {
        return s;
    }
    content.to_string()
}

/// Unwrap a [`TOOL_MARKER`] envelope into readable text naming the call it answers and
/// flagging a failure. `None` when the content is not an envelope — not JSON, not an
/// object, or the marker absent or `false` — so the caller falls back to plain
/// [`extract_text`] handling.
///
/// Forwarding an envelope as-is would reach the driver as a `"tool"` message with no
/// `tool_call_id`, which is a hard driver error.
pub fn unwrap_tool_envelope(content: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(content).ok()?;
    if parsed.get(TOOL_MARKER).and_then(Value::as_bool) != Some(true) {
        return None;
    }

    let tool_call_id = parsed
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let failed = parsed.get("is_error").and_then(Value::as_bool) == Some(true);
    let body = match parsed.get("body") {
        None | Some(Value::Null) => String::new(),
        Some(body) => extract_text(&body.to_string()),
    };

    let status = if failed { " (error)" } else { "" };
    Some(format!(
        "[tool result for call {tool_call_id}{status}]\n{body}"
    ))
}
