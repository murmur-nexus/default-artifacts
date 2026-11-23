//! The model's typed input as one inbox request: the structured form of murmur-nexus's trigger
//! inbox protocol v1, `{"v":1,"op":…,…}`.
//!
//! The input is checked against the protocol's shape (which op takes which field, and each
//! field's JSON type) and against the protocol's request size. The one other check is the label's
//! length. Every other rule — durations, ranges, cron syntax, the check command's length, the
//! live and awaiting limits, approval — belongs to Nexus, which answers it.

use serde_json::{Map, Value};

/// Largest request the inbox takes, in bytes: the protocol's `limits.requestMaxBytes`. A window
/// removes a larger request without answering it.
pub const REQUEST_MAX_BYTES: usize = 16_384;

/// Longest label Nexus takes, in Unicode scalar values (`TRIGGER_LIMITS.labelMaxChars`).
pub const LABEL_MAX_CHARS: usize = 80;

/// The protocol version every request carries as `v`.
pub const PROTOCOL_VERSION: u64 = 1;

/// The fields a structured `add` takes, in the protocol's order, which is also the order they are
/// written in.
pub const ADD_FIELDS: [&str; 13] = [
    "until",
    "every",
    "for",
    "label",
    "at",
    "cron",
    "wake",
    "scope",
    "reaches",
    "onEvent",
    "card",
    "transitEvent",
    "eventCard",
];

/// The ops a person does in a Nexus window and a request cannot.
pub const PERSON_OPS: [&str; 3] = ["approve", "refuse", "renew"];

/// One of the protocol's four structured ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    List,
    Show,
    Cancel,
}

impl Op {
    pub const ALL: [Op; 4] = [Op::Add, Op::List, Op::Show, Op::Cancel];

    /// The op's name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::List => "list",
            Op::Show => "show",
            Op::Cancel => "cancel",
        }
    }

    /// The fields the op takes besides `op`, in the protocol's order.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            Op::Add => &ADD_FIELDS,
            Op::List => &[],
            Op::Show => &["id"],
            Op::Cancel => &["id", "yes"],
        }
    }

    fn from_name(name: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|op| op.name() == name)
    }
}

/// One encoded request, ready to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    pub bytes: Vec<u8>,
}

/// Why an input was refused before anything was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The input is absent, blank, not JSON, or not a JSON object.
    NotAnObject,
    /// No `op`, or an `op` that is not text.
    NoOp,
    /// `approve`, `refuse` or `renew`, with the `id` when one was given as text.
    PersonOnly { op: String, id: Option<String> },
    /// An `op` that is none of the four.
    UnknownOp(String),
    /// A field the op does not take.
    UnknownField { op: Op, field: String },
    /// A text field holding something else.
    NotText { op: Op, field: &'static str },
    /// `yes` holding something other than `true` or `false`.
    YesNotBool,
    /// `show` or `cancel` without an `id`.
    MissingId(Op),
    /// A label longer than [`LABEL_MAX_CHARS`]; the count is its length.
    LabelTooLong(usize),
    /// A request larger than [`REQUEST_MAX_BYTES`]; the count is its size.
    TooLarge(usize),
}

/// The model's input as JSON. A JSON string that itself holds a JSON object is that object,
/// unwrapped once. `None` when the input is absent, blank or not JSON.
pub fn parse_input(data: Option<&str>) -> Option<Value> {
    let data = data.filter(|data| !data.trim().is_empty())?;
    let input = serde_json::from_str::<Value>(data).ok()?;
    if let Value::String(inner) = &input {
        if let Ok(object @ Value::Object(_)) = serde_json::from_str::<Value>(inner) {
            return Some(object);
        }
    }
    Some(input)
}

/// The op the input names, when it names one of the four; used to label a refusal's data.
pub fn op_of(data: Option<&str>) -> Option<Op> {
    parse_input(data)?
        .get("op")
        .and_then(Value::as_str)
        .and_then(Op::from_name)
}

/// Check the input and encode the one request it stands for. Fields the model did not send, or
/// sent as `null`, are left out; nothing is added that it did not send.
pub fn build(data: Option<&str>) -> Result<Request, Refusal> {
    let Some(Value::Object(input)) = parse_input(data) else {
        return Err(Refusal::NotAnObject);
    };
    let Some(op_name) = input.get("op").and_then(Value::as_str) else {
        return Err(Refusal::NoOp);
    };
    if PERSON_OPS.contains(&op_name) {
        return Err(Refusal::PersonOnly {
            op: op_name.to_string(),
            id: input.get("id").and_then(Value::as_str).map(str::to_string),
        });
    }
    let Some(op) = Op::from_name(op_name) else {
        return Err(Refusal::UnknownOp(op_name.to_string()));
    };

    let given: Map<String, Value> = input
        .into_iter()
        .filter(|(key, value)| key != "op" && !value.is_null())
        .collect();
    if let Some(field) = given
        .keys()
        .find(|key| !op.fields().contains(&key.as_str()))
    {
        return Err(Refusal::UnknownField {
            op,
            field: field.clone(),
        });
    }
    for &field in op.fields() {
        match (field, given.get(field)) {
            (_, None) | ("yes", Some(Value::Bool(_))) => {}
            ("yes", Some(_)) => return Err(Refusal::YesNotBool),
            (_, Some(Value::String(_))) => {}
            (_, Some(_)) => return Err(Refusal::NotText { op, field }),
        }
    }
    if matches!(op, Op::Show | Op::Cancel) && !given.contains_key("id") {
        return Err(Refusal::MissingId(op));
    }
    if let Some(Value::String(label)) = given.get("label") {
        let chars = label.chars().count();
        if chars > LABEL_MAX_CHARS {
            return Err(Refusal::LabelTooLong(chars));
        }
    }

    let bytes = encode(op, &given).into_bytes();
    if bytes.len() > REQUEST_MAX_BYTES {
        return Err(Refusal::TooLarge(bytes.len()));
    }
    Ok(Request { op, bytes })
}

/// `{"v":1,"op":"<op>"` followed by each given field in the op's order, as compact JSON.
fn encode(op: Op, given: &Map<String, Value>) -> String {
    let mut text = format!(
        "{{\"v\":{PROTOCOL_VERSION},\"op\":{}",
        Value::from(op.name())
    );
    for &field in op.fields() {
        if let Some(value) = given.get(field) {
            text.push(',');
            text.push_str(&Value::from(field).to_string());
            text.push(':');
            text.push_str(&value.to_string());
        }
    }
    text.push('}');
    text
}
