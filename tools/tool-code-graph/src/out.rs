//! Output-envelope constructors.
//!
//! Every response is a single JSON object carrying the tool's own shape
//! (`{ok, message, ...op fields}`) and the result shape
//! (`{status, summary, data, data_path, metadata}`), as the 0.6.0 tool printed it.
//! Only `status`, `summary` and `data` reach the model ([`crate::NativeOutcome`]);
//! the component returns no `data_path` or metadata.
//!
//! `metadata` is an object with `resource_id`/`state_effect` keys per the
//! convention of the `mur trace` redundant-call detector.

use serde_json::{json, Value};

/// Metadata for the redundant-call detector: the addressed resource and how the
/// call affected it (`"read"` or `"mutate"`).
pub struct Meta {
    pub resource_id: String,
    pub state_effect: String,
}

impl Meta {
    pub fn read(resource_id: impl Into<String>) -> Self {
        Meta {
            resource_id: resource_id.into(),
            state_effect: "read".into(),
        }
    }
    pub fn mutate(resource_id: impl Into<String>) -> Self {
        Meta {
            resource_id: resource_id.into(),
            state_effect: "mutate".into(),
        }
    }
    fn to_json(&self) -> Value {
        json!({ "resource_id": self.resource_id, "state_effect": self.state_effect })
    }
}

/// Successful result. `data` is an object whose keys are also flattened to the
/// top level (new-protocol convention). `data_path`, when `Some`, points at the
/// full payload on disk (used by `slice_symbol` for large slices).
pub fn passed(
    summary: impl Into<String>,
    data: Value,
    data_path: Option<String>,
    meta: Option<Meta>,
) -> Value {
    let msg = summary.into();
    let mut obj = json!({
        "ok": true,
        "message": &msg,
        "status": "passed",
        "summary": &msg,
        "data": data.clone(),
        "data_path": data_path,
        "metadata": meta.as_ref().map(Meta::to_json),
    });
    if let Value::Object(map) = data {
        for (k, v) in map {
            obj[k] = v;
        }
    }
    obj
}

/// Expected failure — invalid input, missing fields, unknown symbol, missing
/// repo. `status: "failed"`, matching the `passed|failed|error` enum.
pub fn failed(message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "message": &msg,
        "status": "failed",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

/// Unexpected internal error (e.g. a SQLite failure). `status: "error"`.
pub fn errored(message: impl Into<String>) -> Value {
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
