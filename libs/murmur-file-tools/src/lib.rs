//! The file logic of the four flat file tools — `tool-read`, `tool-write`, `tool-edit` and
//! `tool-find` — held once and shared by their components.
//!
//! Each tool crate is a thin `wasm32-wasip2` component: it binds its own WIT world, calls
//! [`dispatch`] with its [`Tool`] variant, and maps the [`ToolOutcome`] field for field onto the
//! WIT `ToolResult`. Everything else — the stdin envelope, the field checks, every limit,
//! message and filesystem call — lives here, with no WASM bindings and no `cfg`, so it is unit
//! tested on the host with `cargo test`.
//!
//! The crate root holds the envelope, the dispatch, the output constructors and the helpers the
//! tools share; each tool's handler lives in its own module.

use std::path::Path;

use serde_json::{json, Value};

mod edit;
mod find;
mod read;
#[cfg(test)]
mod tests;
mod write;

// ── Error kind constants ────────────────────────────────────────────────────────

mod err {
    pub const NOT_FOUND: &str = "not_found";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const IO_ERROR: &str = "io_error";
    // A `tool-edit` pair whose `old` occurs nowhere in the text it applies to.
    pub const STRING_NOT_FOUND: &str = "string_not_found";
    // A `tool-edit` pair whose `old` occurs more than once in the text it applies to.
    pub const MULTIPLE_MATCHES: &str = "multiple_matches";
    // A key the call or an `edits` element does not accept.
    pub const UNKNOWN_FIELD: &str = "unknown_field";
    pub const INVALID_PATTERN: &str = "invalid_pattern";
    pub const RESULT_SIZE_EXCEEDED: &str = "result_size_exceeded";
    // tool-read range validation: start_line < 1, an inverted span (start > end), or a
    // start_line past the end of the file. All three are caller mistakes about *which* lines to
    // read, so they share one kind; the message states which one occurred.
    pub const INVALID_RANGE: &str = "invalid_range";
    // tool-read explicit start_line/end_line span exceeds MAX_READ_LINES — a valid but too-wide
    // range.
    pub const RANGE_TOO_LARGE: &str = "range_too_large";
    // tool-find context_lines is negative or exceeds MAX_CONTEXT_LINES.
    pub const CONTEXT_TOO_LARGE: &str = "context_too_large";
    // A path input (`path`, `dir` or `glob`) was given as an absolute value, which every tool's
    // description forbids: each path the tools take is relative to the capsule workdir.
    pub const ABSOLUTE_PATH: &str = "absolute_path";
    // tool-find `dir` has a `..` that climbs above the capsule workdir.
    pub const OUTSIDE_WORKDIR: &str = "outside_workdir";
}

// ── The tools ───────────────────────────────────────────────────────────────────

/// One of the four file tools. Its [`name`](Tool::name) is the bare artifact name the model
/// calls it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Read,
    Write,
    Edit,
    Find,
}

impl Tool {
    pub const ALL: [Tool; 4] = [Tool::Read, Tool::Write, Tool::Edit, Tool::Find];

    /// The tool's name: its artifact name, its staged directory, and so the name murmur gives
    /// the model.
    pub const fn name(self) -> &'static str {
        match self {
            Tool::Read => "tool-read",
            Tool::Write => "tool-write",
            Tool::Edit => "tool-edit",
            Tool::Find => "tool-find",
        }
    }

    /// The top-level fields the tool accepts, in the order its `input_schema` lists them. Every
    /// other key is refused before a file is touched, and the refusal lists these.
    pub const fn fields(self) -> &'static [&'static str] {
        match self {
            Tool::Read => &["path", "start_line", "end_line"],
            Tool::Write => &["path", "content"],
            Tool::Edit => &["path", "edits"],
            Tool::Find => &["pattern", "dir", "glob", "recursive", "context_lines"],
        }
    }

    /// The `state_effect` a successful call declares through the runtime's reserved metadata
    /// key (see the host's wit/tool.wit), which lets `mur trace` redundant-call detection
    /// reason about the tools without hardcoding their names. A failed call declares none: a
    /// failed read did not read, and a failed write did not mutate.
    pub const fn state_effect(self) -> &'static str {
        match self {
            Tool::Read | Tool::Find => "read",
            Tool::Write | Tool::Edit => "mutate",
        }
    }

    /// The tool's handler: the call object in, the old-protocol result out. Each handler finds
    /// only keys from [`Tool::fields`], because [`run`] refuses every other key first.
    fn handle(self, call: &Value) -> Value {
        match self {
            Tool::Read => read::read(call),
            Tool::Write => write::write(call),
            Tool::Edit => edit::edit(call),
            Tool::Find => find::find(call),
        }
    }
}

/// The keys an element of `tool-edit`'s `edits` accepts, in the order its `input_schema` lists
/// them.
pub const EDIT_PAIR_FIELDS: &[&str] = &["old", "new"];

// ── Dispatch entry points ───────────────────────────────────────────────────────

/// Parse a stdin-envelope string `{"data": <string-or-object>, "log_path": ...}`, run `tool` on
/// the call `data` holds, and return the old-protocol result `Value`.
///
/// The call is checked in this order, and the first failure is returned: the envelope parses
/// and carries `data`; `data` is a call object, or a JSON string encoding one; the call holds no
/// key outside [`Tool::fields`], whatever its value; then the tool's handler validates its
/// fields and only then touches the filesystem. A successful result carries the tool's
/// [`state_effect`](Tool::state_effect) in `metadata`.
pub fn run(tool: Tool, raw: &str) -> Value {
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

    with_state_effect(tool.handle(&call), tool.state_effect())
}

/// A tool call's result as a component's `Guest::run` hands it to the WIT `ToolResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: OutcomeStatus,
    /// The result's `summary`: on a failure, the message.
    pub summary: Option<String>,
    /// The result's `data`, serialized: a read's content and line counts, a find's matches;
    /// `None` for a write, an edit and every failure.
    pub data: Option<String>,
    /// Whether `data` carries a read's `truncated` note or a find's `capped` note.
    pub truncated: bool,
    /// The tool's [`state_effect`](Tool::state_effect) on success, `None` otherwise.
    pub state_effect: Option<&'static str>,
}

/// [`ToolOutcome::status`], one variant per WIT `status` case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeStatus {
    Passed,
    Failed,
    Error,
}

/// What a component's `Guest::run` calls with `ToolInput.data`.
///
/// The host places the tool-call arguments in `data`. They are rewrapped into the
/// `{"data": ...}` envelope [`run`] parses — a value that is not JSON travels on as a string,
/// so [`run`] reports it — and an absent `data` becomes an envelope with no `data` key. The
/// result is mapped as the host's native dispatch mapped a tool's stdout
/// (crates/capsule-runtime `dispatch_native_tool`): `data` is the result's `data` field, a
/// string used verbatim, else a non-null value re-serialized, else `None`.
pub fn dispatch(tool: Tool, data: Option<&str>) -> ToolOutcome {
    let raw = match data {
        Some(d) => {
            let data_val =
                serde_json::from_str::<Value>(d).unwrap_or_else(|_| Value::String(d.to_string()));
            json!({ "data": data_val }).to_string()
        }
        None => "{}".to_string(),
    };

    let result = run(tool, &raw);

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

    ToolOutcome {
        status,
        summary,
        data,
        truncated,
        state_effect,
    }
}

// ── Helpers the tools share ─────────────────────────────────────────────────────

/// `keys` quoted, with quotes, backslashes and control characters escaped, and
/// comma-separated, so a key holding a quote or a newline keeps the message on one line.
fn quoted(keys: &[&str]) -> String {
    keys.iter()
        .map(|key| format!("{key:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `unknown_field` refusal for an object holding a key outside `accepted`, or `None`.
/// `place` names where the keys were found (`edit 2 of 3`) and `subject` what takes the
/// accepted keys (`an edit`): `unknown field "x" in <place>: <subject> takes <accepted>`. A
/// non-object holds no keys and is left to the caller.
fn reject_unknown_fields(
    object: &Value,
    place: &str,
    subject: &str,
    accepted: &[&str],
) -> Option<Value> {
    let mut unknown: Vec<&str> = object
        .as_object()?
        .keys()
        .map(String::as_str)
        .filter(|key| !accepted.contains(key))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    unknown.sort_unstable();
    Some(err_result(
        err::UNKNOWN_FIELD,
        format!(
            "unknown field {} in {place}: {subject} takes {}",
            quoted(&unknown),
            accepted.join(", ")
        ),
    ))
}

/// Attach the reserved `state_effect` metadata key to a successful result. Failures are left
/// untouched (metadata stays `null`), so a call that did not complete declares no effect.
fn with_state_effect(mut result: Value, effect: &str) -> Value {
    if result.get("ok").and_then(Value::as_bool) == Some(true) {
        result["metadata"] = json!({ "state_effect": effect });
    }
    result
}

/// The non-empty string `field` of `call`, or the missing-field failure naming it. A `null`
/// value counts as absent.
fn required_path(call: &Value, field: &str) -> Result<String, Value> {
    match call.get(field).and_then(Value::as_str) {
        Some(p) if !p.is_empty() => Ok(p.to_string()),
        _ => Err(fail_msg(format!("missing required field: {field}"))),
    }
}

/// The refusal for an absolute path input, or `None` when `value` is relative.
///
/// Under the single WASI preopen a tool is dispatched with, an absolute value resolves *inside*
/// the preopen: `/app/results.txt` lands at `<workdir>/app/results.txt`, which a later read of
/// the same value resolves identically, so nothing inside the capsule can see the divergence
/// from what the caller named. Refusing is therefore the only way the contract in each tool's
/// description — every path is relative to the work folder — is observable at all.
///
/// `location` is the input the caller actually wrote, as murmur renders a flat input location
/// (`path`, `dir` or `glob`), so the message names the field to fix. The suggestion is the
/// value's final component: the tool cannot know which leading components the caller mistook
/// for a root, but under a `Sealed` capsule the workdir is the only writable location, so the
/// bare file name is the likely intended target. Values with no final component (`/`, or one
/// ending in `..`) get no suggestion rather than an empty one.
fn reject_absolute(location: &str, value: &str) -> Option<Value> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return None;
    }
    let suggestion = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!(" (did you mean '{name}'?)"),
        None => String::new(),
    };
    Some(err_result(
        err::ABSOLUTE_PATH,
        format!("'{location}' must be relative to the capsule workdir; got '{value}'{suggestion}"),
    ))
}

fn io_error_kind(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => err::NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => err::PERMISSION_DENIED,
        _ => err::IO_ERROR,
    }
}

/// `bytes` as whole KB when it is a multiple of 1024, else as bytes.
fn format_result_size(bytes: usize) -> String {
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

fn ok_with(message: impl Into<String>, data: Value, summary: impl Into<String>) -> Value {
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

fn ok_summary_only(message: impl Into<String>, summary: impl Into<String>) -> Value {
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

fn fail_msg(message: impl Into<String>) -> Value {
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

fn err_result(error_kind: &str, message: impl Into<String>) -> Value {
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
