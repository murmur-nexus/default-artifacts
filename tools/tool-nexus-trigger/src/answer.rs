//! What the model gets back: a Nexus answer mapped into a tool result, and every line the tool
//! writes itself.
//!
//! Nexus's lines are copied unchanged. The tool's own words are [`AWAITS_APPROVAL`] and the lines
//! starting [`PREFIX`]; each of those stays on one line and says whether anything was written.

use serde_json::Value;

use crate::ops::{OpStatus, Response};
use crate::request::{Op, Refusal, LABEL_MAX_CHARS, REQUEST_MAX_BYTES};

/// The start of every line the tool writes itself.
pub const PREFIX: &str = "tool-nexus-trigger: ";

/// Longest excerpt of a model-sent name or a system error carried into a line, in characters,
/// before `…`.
pub const EXCERPT_CHARS: usize = 60;

/// `PREFIX` followed by a literal, as one `&'static str`.
macro_rules! prefixed {
    ($text:expr) => {
        concat!("tool-nexus-trigger: ", $text)
    };
}

/// The summary of every `add` Nexus answered with exit 0, before Nexus's first line.
pub const AWAITS_APPROVAL: &str =
    "Requested: it waits for a person's approval in a Nexus window and does not run until then.";

/// The entry declares no scope, so it reaches the whole work folder.
pub const WHOLE_WORKDIR: &str = prefixed!(
    "this entry reaches the whole work folder; give it capabilities.filesystem.scope: \
     .nexus/triggers/inbox so it reaches the trigger inbox and nothing else. Nothing was written."
);

/// Nothing answered within [`crate::inbox::ANSWER_WAIT`], and the request was removed untaken.
pub const NO_ANSWER: &str = prefixed!(
    "no Nexus window answered within 5 s, so the request was removed and no trigger was \
     created. Only a Nexus window open on this work folder answers the trigger inbox (the \
     transit daemon does not read it), and this entry's capabilities.filesystem.scope must be \
     .nexus/triggers/inbox."
);

/// Nothing answered in time, but a window had taken the request.
pub const TAKEN_NO_ANSWER: &str = prefixed!(
    "a Nexus window took the request but sent no answer within 5 s, so a trigger may have been \
     created; call list to see."
);

/// The input is absent, blank, not JSON or not an object.
pub const INPUT_NOT_OBJECT: &str = prefixed!(
    r#"the input is not a JSON object; send {"op": "list"}, or op add, show or cancel with that op's fields. Nothing was written."#
);

/// The input names no op.
pub const NO_OP: &str =
    prefixed!("the input has no op; op is one of add, list, show, cancel. Nothing was written.");

/// `yes` is not a boolean.
pub const YES_NOT_BOOL: &str =
    prefixed!("yes must be true or false, for cancel. Nothing was written.");

/// `approve`, `refuse` or `renew`: `<op> happens only in a Nexus window, by a person: /trigger
/// <op> <id>`.
pub fn person_only(op: &str, id: Option<&str>) -> String {
    let id = id.map_or_else(|| "<id>".to_string(), excerpt);
    format!(
        "{PREFIX}{op} happens only in a Nexus window, by a person: /trigger {op} {id}. Nothing \
         was written."
    )
}

/// An `op` that is none of the four.
pub fn unknown_op(name: &str) -> String {
    format!(
        "{PREFIX}op \"{}\" is not one of add, list, show, cancel. Nothing was written.",
        excerpt(name)
    )
}

/// A field `op` does not take.
pub fn unknown_field(op: Op, field: &str) -> String {
    format!(
        "{PREFIX}{} does not take \"{}\"; {}. Nothing was written.",
        op.name(),
        excerpt(field),
        takes(op)
    )
}

/// A text field holding something else.
pub fn not_text(op: Op, field: &str) -> String {
    format!(
        "{PREFIX}{field} must be text; {}. Nothing was written.",
        takes(op)
    )
}

/// `show` or `cancel` without an id.
pub fn missing_id(op: Op) -> String {
    format!(
        "{PREFIX}{} needs id, the trigger's id such as t3 (list shows them); {}. Nothing was \
         written.",
        op.name(),
        takes(op)
    )
}

/// A label over [`LABEL_MAX_CHARS`].
pub fn label_too_long(chars: usize) -> String {
    format!(
        "{PREFIX}label is {chars} characters, over Nexus's limit of {LABEL_MAX_CHARS}. Nothing \
         was written."
    )
}

/// A request over [`REQUEST_MAX_BYTES`].
pub fn request_too_large(bytes: usize) -> String {
    format!(
        "{PREFIX}the request is {bytes} bytes, over the trigger inbox's limit of \
         {REQUEST_MAX_BYTES} bytes; shorten until, wake or label. Nothing was written."
    )
}

/// The request could not be written.
pub fn cannot_write(error: &str) -> String {
    format!(
        "{PREFIX}cannot write the trigger inbox: {}. Nothing was written.",
        excerpt(error)
    )
}

/// Nothing answered in time, and the request could not be removed.
pub fn not_removed(error: &str) -> String {
    format!(
        "{PREFIX}no Nexus window answered within 5 s, and the request could not be removed ({}), \
         so a window may still take it; call list to see.",
        excerpt(error)
    )
}

/// The answer's name held something this tool does not read as an answer.
pub fn malformed_answer(reason: &str) -> String {
    format!(
        "{PREFIX}the answer from Nexus cannot be read ({reason}); the request may have taken \
         effect, so call list to see."
    )
}

/// Nexus answered with no line at all.
pub fn no_lines(exit: i64) -> String {
    format!("{PREFIX}Nexus answered exit {exit} with no lines.")
}

/// `it takes <fields>` for `op`, or `it takes no field but op`.
fn takes(op: Op) -> String {
    let fields = op.fields();
    if fields.is_empty() {
        format!("{} takes no field but op", op.name())
    } else {
        format!("{} takes {}", op.name(), fields.join(", "))
    }
}

/// The text cut to [`EXCERPT_CHARS`] characters, with `…` when cut, and on one line.
fn excerpt(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() <= EXCERPT_CHARS {
        flat
    } else {
        let mut cut: String = flat.chars().take(EXCERPT_CHARS).collect();
        cut.push('…');
        cut
    }
}

/// A result carrying one tool line: `summary` is the line, and `data` is
/// `{"ok":false,"op":<op or null>,"error":<line>}`.
pub fn tool_line(status: OpStatus, op: Option<Op>, line: String) -> Response {
    let data = format!(
        "{{\"ok\":false,\"op\":{},\"error\":{}}}",
        op.map_or(Value::Null, |op| Value::from(op.name())),
        Value::from(line.as_str())
    );
    Response {
        status,
        summary: line,
        data,
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}

/// A refusal from [`crate::request::build`], as a `failed` result.
pub fn refused(op: Option<Op>, refusal: &Refusal) -> Response {
    let line = match refusal {
        Refusal::NotAnObject => INPUT_NOT_OBJECT.to_string(),
        Refusal::NoOp => NO_OP.to_string(),
        Refusal::PersonOnly { op, id } => person_only(op, id.as_deref()),
        Refusal::UnknownOp(name) => unknown_op(name),
        Refusal::UnknownField { op, field } => unknown_field(*op, field),
        Refusal::NotText { op, field } => not_text(*op, field),
        Refusal::YesNotBool => YES_NOT_BOOL.to_string(),
        Refusal::MissingId(op) => missing_id(*op),
        Refusal::LabelTooLong(chars) => label_too_long(*chars),
        Refusal::TooLarge(bytes) => request_too_large(*bytes),
    };
    tool_line(OpStatus::Failed, op, line)
}

/// A protocol v1 answer: exactly `v`, `exit`, `out` and `err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub exit: i64,
    pub out: Vec<String>,
    pub err: Vec<String>,
}

/// The answer's bytes checked against protocol v1, or why they do not match.
pub fn parse_answer(bytes: &[u8]) -> Result<Answer, &'static str> {
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(bytes) else {
        return Err("not a JSON object");
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != ["err", "exit", "out", "v"] {
        return Err("its keys are not exactly v, exit, out and err");
    }
    if object["v"].as_u64() != Some(crate::request::PROTOCOL_VERSION) {
        return Err("v is not 1");
    }
    let Some(exit) = object["exit"].as_i64() else {
        return Err("exit is not an integer");
    };
    let lines = |value: &Value| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|line| line.as_str().map(str::to_string))
            .collect()
    };
    let out = lines(&object["out"]).ok_or("out is not a list of lines")?;
    let err = lines(&object["err"]).ok_or("err is not a list of lines")?;
    Ok(Answer { exit, out, err })
}

/// Nexus's answer to `op` as the tool result: `passed` on exit 0, `failed` otherwise.
pub fn from_nexus(op: Op, bytes: &[u8]) -> Response {
    let answer = match parse_answer(bytes) {
        Ok(answer) => answer,
        Err(reason) => {
            return tool_line(OpStatus::Error, Some(op), malformed_answer(reason));
        }
    };
    let ok = answer.exit == 0;
    let first = |lines: &[String]| lines.first().cloned();
    // A refusal's reason is on stderr; a success's text on stdout.
    let line = if ok {
        first(&answer.out).or_else(|| first(&answer.err))
    } else {
        first(&answer.err).or_else(|| first(&answer.out))
    };
    let summary = match (op, ok, line) {
        (Op::Add, true, Some(line)) => format!("{AWAITS_APPROVAL} {line}"),
        (Op::Add, true, None) => AWAITS_APPROVAL.to_string(),
        (_, _, Some(line)) => line,
        (_, _, None) => no_lines(answer.exit),
    };
    // The model reads `data`, not `summary`, so the summary rides in it too: a passed add's
    // AWAITS_APPROVAL reaches the model this way.
    let data = format!(
        "{{\"ok\":{ok},\"op\":{},\"exit\":{},\"summary\":{},\"out\":{},\"err\":{}}}",
        Value::from(op.name()),
        answer.exit,
        Value::from(summary.as_str()),
        Value::from(answer.out),
        Value::from(answer.err)
    );
    Response {
        status: if ok {
            OpStatus::Passed
        } else {
            OpStatus::Failed
        },
        summary,
        data,
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}
