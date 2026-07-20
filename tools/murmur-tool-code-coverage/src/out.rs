//! Output-envelope constructors.
//!
//! Copied verbatim from `murmur-tool-code-graph`'s `src/out.rs` (these tools
//! share no library crate — each is `[[bin]]`-only — so the envelope is
//! duplicated, not imported). Every response is a single JSON object emitting
//! BOTH protocols in one object:
//!   • the tool's own shape: `{ok, message, ...op fields}`
//!   • the capsule-runtime shape: `{status, summary, data, data_path, metadata}`
//!
//! This tool keeps the three-way envelope (`passed`/`failed`/`errored`), unlike
//! `murmur-tool-test-report`'s two-way one, because it can hit genuine internal
//! errors: it opens, migrates, and writes a SQLite database whose schema it does
//! not own.

use serde_json::{json, Value};

/// Metadata for the redundant-call detector: the addressed resource and how the
/// call affected it (`"read"` or `"mutate"`).
pub struct Meta {
    pub resource_id: String,
    pub state_effect: String,
}

impl Meta {
    pub fn mutate(resource_id: impl Into<String>) -> Self {
        Meta { resource_id: resource_id.into(), state_effect: "mutate".into() }
    }
    fn to_json(&self) -> Value {
        json!({ "resource_id": self.resource_id, "state_effect": self.state_effect })
    }
}

/// Successful result. `data` is an object whose keys are also flattened to the
/// top level (new-protocol convention). `data_path`, when `Some`, points at the
/// full payload on disk (used when `top_suspects` is truncated).
pub fn passed(summary: impl Into<String>, data: Value, data_path: Option<String>, meta: Option<Meta>) -> Value {
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

/// Expected failure — invalid input, missing fields, missing db, no failing-test
/// coverage. `status: "failed"`, matching the `passed|failed|error` enum.
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
