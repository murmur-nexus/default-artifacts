//! What one tool call runs through, whichever tool it is: the envelope and its field checks,
//! the mapping of the result onto a [`ToolOutcome`], the error kinds, the output constructors
//! and the helpers the tools share.
//!
//! Each tool module's `run` and `dispatch` call [`run_with`] and [`dispatch_with`] with its own
//! [`Tool`] and its own handler, so every call is static and a component links only the handler
//! it names. An item only some tools use is compiled only with their features.

use serde_json::{json, Value};

use crate::{HostRoot, OutcomeAttachment, OutcomeStatus, Tool, ToolOutcome};

// ── Error kind constants ────────────────────────────────────────────────────────

// Each kind is compiled only with the features of the tools that return it.
pub(crate) mod err {
    pub const NOT_FOUND: &str = "not_found";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const IO_ERROR: &str = "io_error";
    // A `tool-edit` pair whose `old` occurs nowhere in the text it applies to.
    #[cfg(feature = "edit")]
    pub const STRING_NOT_FOUND: &str = "string_not_found";
    // A `tool-edit` pair whose `old` occurs more than once in the text it applies to.
    #[cfg(feature = "edit")]
    pub const MULTIPLE_MATCHES: &str = "multiple_matches";
    // A key the call or an `edits` element does not accept.
    pub const UNKNOWN_FIELD: &str = "unknown_field";
    #[cfg(feature = "find")]
    pub const INVALID_PATTERN: &str = "invalid_pattern";
    #[cfg(feature = "find")]
    pub const RESULT_SIZE_EXCEEDED: &str = "result_size_exceeded";
    // tool-read range validation: start_line < 1, an inverted span (start > end), or a
    // start_line past the end of the file. All three are caller mistakes about *which* lines to
    // read, so they share one kind; the message states which one occurred.
    #[cfg(feature = "read")]
    pub const INVALID_RANGE: &str = "invalid_range";
    // tool-read explicit start_line/end_line span exceeds MAX_READ_LINES — a valid but too-wide
    // range.
    #[cfg(feature = "read")]
    pub const RANGE_TOO_LARGE: &str = "range_too_large";
    // tool-find context_lines is negative or exceeds MAX_CONTEXT_LINES.
    #[cfg(feature = "find")]
    pub const CONTEXT_TOO_LARGE: &str = "context_too_large";
    // An absolute `path` or `dir` no held host root covers, or an absolute `glob`, which is
    // always matched below `dir`.
    pub const ABSOLUTE_PATH: &str = "absolute_path";
    // A relative `path` or `dir` whose first segment is `murmur-host`, the name the host roots
    // are opened under, while the tool holds at least one root.
    pub const RESERVED_NAME: &str = "reserved_name";
    // tool-find `dir` has a `..` that climbs above the capsule workdir.
    #[cfg(feature = "find")]
    pub const OUTSIDE_WORKDIR: &str = "outside_workdir";
    // tool-read `path` is named as an image but its header is not a PNG, JPEG, GIF or WebP one.
    #[cfg(feature = "read")]
    pub const NOT_AN_IMAGE: &str = "not_an_image";
}

// ── The envelope every tool runs through ────────────────────────────────────────

/// Parse a stdin-envelope string `{"data": <string-or-object>, "log_path": ...}`, run `handle`,
/// the handler of `tool`, on the call `data` holds, and return the old-protocol result `Value`.
///
/// The call is checked in this order, and the first failure is returned: the envelope parses
/// and carries `data`; `data` is a call object, or a JSON string encoding one; the call holds no
/// key outside [`Tool::fields`], whatever its value; then `handle` validates its fields and only
/// then touches the filesystem, so it finds only keys from [`Tool::fields`]. A successful result
/// carries the tool's [`state_effect`](Tool::state_effect) in `metadata`.
///
/// `roots` are the host folders the tool may open an absolute path in, in preopen order; empty
/// when the session opened none, and then every absolute path is refused.
pub(crate) fn run_with(
    tool: Tool,
    handle: impl Fn(&Value, &[HostRoot]) -> Value,
    raw: &str,
    roots: &[HostRoot],
) -> Value {
    if raw.trim().is_empty() {
        return fail_msg("missing input on stdin");
    }

    let envelope: Value = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => return fail_msg(format!("invalid stdin JSON: {e}")),
    };

    let data_value = match envelope.get("data") {
        None | Some(Value::Null) => return fail_msg("missing data field"),
        Some(v) => v.clone(),
    };

    // data may be a JSON-encoded string (double-encoded) or a JSON object directly
    let call: Value = match &data_value {
        Value::String(s) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => return fail_msg(format!("invalid data JSON string: {e}")),
        },
        Value::Object(_) => data_value,
        _ => return fail_msg("data must be a JSON string or object"),
    };
    let Some(object) = call.as_object() else {
        return fail_msg("data must be a JSON string or object");
    };

    // Every key the tool does not take, whatever its value: this is how an operation object of
    // tool-editor 0.12.0 (`{"read": …}`), a field of another tool, or a field of an earlier input
    // shape (`operation`, `dest_path`, …) is refused.
    let mut unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !tool.fields().contains(key))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return err_result(
            err::UNKNOWN_FIELD,
            format!(
                "unknown field {}: {} takes {}",
                quoted(&unknown),
                tool.name(),
                tool.fields().join(", ")
            ),
        );
    }

    with_state_effect(handle(&call, roots), tool.state_effect())
}

/// What a component's `Guest::run` calls with `ToolInput.data`, through its tool module's
/// `dispatch`: [`run_with`] for `tool` and its handler `handle`, mapped onto a [`ToolOutcome`].
///
/// The host places the tool-call arguments in `data`. They are rewrapped into the
/// `{"data": ...}` envelope [`run_with`] parses — a value that is not JSON travels on as a
/// string, so [`run_with`] reports it — and an absent `data` becomes an envelope with no `data`
/// key. The result is mapped as the host's native dispatch mapped a tool's stdout
/// (crates/capsule-runtime `dispatch_native_tool`): `data` is the result's `data` field, a
/// string used verbatim, else a non-null value re-serialized, else `None`. `attachments` is the
/// result's top-level `attachments` array: each `{"media_type", "path"}` entry holding both as
/// strings, in order, and empty when the result has none. `roots` are passed on to [`run_with`].
pub(crate) fn dispatch_with(
    tool: Tool,
    handle: impl Fn(&Value, &[HostRoot]) -> Value,
    data: Option<&str>,
    roots: &[HostRoot],
) -> ToolOutcome {
    let raw = match data {
        Some(d) => {
            let data_val =
                serde_json::from_str::<Value>(d).unwrap_or_else(|_| Value::String(d.to_string()));
            json!({ "data": data_val }).to_string()
        }
        None => "{}".to_string(),
    };

    let result = run_with(tool, handle, &raw, roots);

    let status = match result.get("status").and_then(Value::as_str) {
        Some("passed") => OutcomeStatus::Passed,
        Some("failed") => OutcomeStatus::Failed,
        _ => OutcomeStatus::Error,
    };
    let summary = result
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string);
    let state_effect = result
        .get("metadata")
        .and_then(|m| m.get("state_effect"))
        .and_then(Value::as_str)
        .map(|_| tool.state_effect());
    let data = result
        .get("data")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            result
                .get("data")
                .filter(|d| !d.is_null())
                .map(|d| d.to_string())
        });
    let truncated = result
        .get("data")
        .is_some_and(|d| d.get("capped").is_some() || d.get("truncated").is_some());
    let attachments = result
        .get("attachments")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some(OutcomeAttachment {
                        media_type: entry.get("media_type")?.as_str()?.to_string(),
                        path: entry.get("path")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    ToolOutcome {
        status,
        summary,
        data,
        truncated,
        state_effect,
        attachments,
    }
}

// ── Helpers the tools share ─────────────────────────────────────────────────────

/// `keys` quoted, with quotes, backslashes and control characters escaped, and
/// comma-separated, so a key holding a quote or a newline keeps the message on one line.
pub(crate) fn quoted(keys: &[&str]) -> String {
    keys.iter()
        .map(|key| format!("{key:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Attach the reserved `state_effect` metadata key to a successful result. Failures are left
/// untouched (metadata stays `null`), so a call that did not complete declares no effect.
pub(crate) fn with_state_effect(mut result: Value, effect: &str) -> Value {
    if result.get("ok").and_then(Value::as_bool) == Some(true) {
        result["metadata"] = json!({ "state_effect": effect });
    }
    result
}

/// The non-empty string `field` of `call`, or the missing-field failure naming it. A `null`
/// value counts as absent.
#[cfg(any(feature = "read", feature = "write", feature = "edit"))]
pub(crate) fn required_path(call: &Value, field: &str) -> Result<String, Value> {
    match call.get(field).and_then(Value::as_str) {
        Some(p) if !p.is_empty() => Ok(p.to_string()),
        _ => Err(fail_msg(format!("missing required field: {field}"))),
    }
}

pub(crate) fn io_error_kind(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => err::NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => err::PERMISSION_DENIED,
        _ => err::IO_ERROR,
    }
}

/// `bytes` as whole KB when it is a multiple of 1024, else as bytes.
#[cfg(any(feature = "read", feature = "find"))]
pub(crate) fn format_result_size(bytes: usize) -> String {
    if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

// ── Output constructors ─────────────────────────────────────────────────────────
//
// Mirrors the git-tool protocol so the capsule runtime can extract data/summary.
// Fields: ok, message, status, summary, data, data_path, metadata.
// Error results additionally carry error_kind at the top level.

#[cfg(any(feature = "read", feature = "find"))]
pub(crate) fn ok_with(
    message: impl Into<String>,
    data: Value,
    summary: impl Into<String>,
) -> Value {
    let msg = message.into();
    let sum = summary.into();
    let data_clone = data.clone();
    let mut obj = json!({
        "ok": true,
        "message": &msg,
        "status": "passed",
        "summary": &sum,
        "data": data_clone,
        "data_path": null,
        "metadata": null,
    });
    // Flatten data fields at the top level for new-protocol callers.
    if let Value::Object(map) = data {
        for (k, v) in map {
            obj[k] = v;
        }
    }
    obj
}

#[cfg(any(feature = "read", feature = "write", feature = "edit"))]
pub(crate) fn ok_summary_only(message: impl Into<String>, summary: impl Into<String>) -> Value {
    let msg = message.into();
    let sum = summary.into();
    json!({
        "ok": true,
        "message": &msg,
        "status": "passed",
        "summary": &sum,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

pub(crate) fn fail_msg(message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "message": &msg,
        "status": "error",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

pub(crate) fn err_result(error_kind: &str, message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "error_kind": error_kind,
        "message": &msg,
        "status": "error",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}
