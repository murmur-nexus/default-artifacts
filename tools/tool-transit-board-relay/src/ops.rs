//! One call, end to end: check what the runtime handed the tool, send one envelope, and judge
//! the reply.
//!
//! Every check that can fail runs before the request, so a refused call sends nothing. There is
//! one request per call and no retry: the runtime's gateway already re-reads the credential and
//! resends once on a `401`, and a board write must never be sent twice.

use serde_json::Value;

use crate::answer::{self, relay_line};
use crate::envelope;

/// What the runtime handed this call, each `None` when the variable was not set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `MURMUR_GATEWAY_ENDPOINT`: the gateway address, set only when the entry binds `gateway:`.
    pub gateway_endpoint: Option<String>,
    /// `MURMUR_ARTIFACT_CONFIG`: the entry's `config:` block as compact JSON — the grant.
    pub artifact_config: Option<String>,
    /// `MURMUR_CAPSULE_NAME`: the capsule manifest's `name`.
    pub capsule_name: Option<String>,
    /// `MURMUR_SESSION_ID`: the session the call belongs to.
    pub session_id: Option<String>,
}

/// Sends the one request. The wasm adapter implements it over `wasi:http`; host tests
/// implement it with a recording fake.
pub trait Transport {
    fn post(
        &mut self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<HttpReply, String>;
}

/// What came back: the status and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Mirrors the WIT `status` enum without depending on the generated bindings, so this module
/// stays host-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpStatus {
    Passed,
    Failed,
    Error,
}

/// The tool result, field for field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: OpStatus,
    pub summary: String,
    pub data: String,
    pub data_path: Option<String>,
    pub truncated: bool,
    pub metadata: Vec<(String, String)>,
}

/// Run one call. `data` is the model's input, `None` when the runtime passed none.
pub fn run(env: &Env, data: Option<&str>, transport: &mut dyn Transport) -> Response {
    let gateway = match env.gateway_endpoint.as_deref().map(str::trim) {
        Some(endpoint) if endpoint.starts_with("http://") || endpoint.starts_with("https://") => {
            endpoint
        }
        _ => return relay_line(OpStatus::Error, answer::NO_GATEWAY.to_string()),
    };

    let Some(capsule) = present(env.capsule_name.as_deref()) else {
        return relay_line(
            OpStatus::Error,
            answer::no_identity(crate::CAPSULE_NAME_ENV),
        );
    };
    let Some(session) = present(env.session_id.as_deref()) else {
        return relay_line(OpStatus::Error, answer::no_identity(crate::SESSION_ID_ENV));
    };

    let config = match env.artifact_config.as_deref() {
        None => Value::Null,
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(config) => config,
            Err(err) => {
                return relay_line(OpStatus::Error, answer::config_not_json(&err.to_string()))
            }
        },
    };

    let Some(input) = parse_input(data) else {
        return relay_line(OpStatus::Failed, answer::INPUT_NOT_JSON.to_string());
    };

    let url = envelope::url(gateway);
    let body = envelope::build(capsule, session, config, input).to_string();
    match transport.post(&url, &envelope::headers(body.len()), body.as_bytes()) {
        Ok(reply) => answer::judge(reply.status, &reply.body, envelope::path(&url)),
        Err(message) => relay_line(OpStatus::Error, answer::transport_failed(&message)),
    }
}

/// `value` when it is set and not blank. The value is sent as set: nexus checks its format.
fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// The model's input as JSON. A JSON string that itself holds a JSON object is that object,
/// unwrapped once; any other JSON value is sent unchanged for nexus to answer. `None` when the
/// input is absent, blank or not JSON.
fn parse_input(data: Option<&str>) -> Option<Value> {
    let data = data.filter(|data| !data.trim().is_empty())?;
    let input = serde_json::from_str::<Value>(data).ok()?;
    if let Value::String(inner) = &input {
        if let Ok(object @ Value::Object(_)) = serde_json::from_str::<Value>(inner) {
            return Some(object);
        }
    }
    Some(input)
}
