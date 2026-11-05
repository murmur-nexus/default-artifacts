//! Output-envelope constructors.
//!
//! Every response is the single JSON object the native 0.5.0 tool printed, kept so
//! its messages and shape stay exactly as they were:
//!   • the tool's own shape: `{ok, message, ...op fields}`
//!   • the capsule-runtime shape: `{status, summary, data, data_path, metadata}`
//!
//! Only `status`, `summary` and `data` reach the model: `NativeOutcome::of`
//! reduces the envelope to them the way murmur's native dispatch did, and the
//! component returns nothing else.

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
    pub(crate) fn to_json(&self) -> Value {
        json!({ "resource_id": self.resource_id, "state_effect": self.state_effect })
    }
}

/// Result of a successful `parse`. The parse operation itself succeeded, so the
/// envelope carries the structured `data`. The `status`/`ok` fields mirror the
/// *test* outcome, matching the worked example in
/// `docs/murmur-tool-output-convention.md` (`{"status": "failed", "summary":
/// "44 passed, 3 failed", ...}`): `any_failed == false` → `passed`, otherwise
/// `failed`. `data`'s keys are also flattened to the top level (new-protocol
/// convention).
pub fn parse_result(
    summary: impl Into<String>,
    data: Value,
    data_path: Option<String>,
    meta: Option<Meta>,
    any_failed: bool,
) -> Value {
    let msg = summary.into();
    let status = if any_failed { "failed" } else { "passed" };
    let mut obj = json!({
        "ok": !any_failed,
        "message": &msg,
        "status": status,
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

/// Expected, actionable failure of the *operation* — bad input, missing field,
/// unreadable file, undetectable format. `status: "failed"`, `data` null.
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
