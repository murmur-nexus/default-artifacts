//! Output-envelope constructors.
//!
//! Every response is a single JSON object emitting BOTH protocols in one object,
//! mirroring `murmur-tool-git` and `murmur-tool-code-graph`:
//!   • the tool's own shape: `{ok, message, ...op fields}`
//!   • the capsule-runtime shape: `{status, summary, data, data_path, metadata}`
//!     — `status`/`summary`/`data` are what `dispatch_native_tool` reads today.
//!
//! `metadata` is emitted as an object with `resource_id`/`state_effect` keys per
//! the convention. NOTE: for native tools, `dispatch_native_tool` in the `murmur`
//! repo currently hardcodes `data_path`/`metadata` to empty, so those two fields
//! do not yet reach the live agent path — a documented, pre-existing gap, not
//! introduced here.

use serde_json::{json, Value};

/// Metadata for the redundant-call detector: the addressed resource and how the
/// call affected it (`"read"` or `"mutate"`).
pub struct Meta {
    pub resource_id: String,
    pub state_effect: String,
}

impl Meta {
    pub fn read(resource_id: impl Into<String>) -> Self {
        Meta { resource_id: resource_id.into(), state_effect: "read".into() }
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
