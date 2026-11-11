//! The one request a call sends: where, with which headers, carrying what.
//!
//! No function here sees a key. The URL is built from the gateway address the runtime handed
//! the tool, the headers describe only the body, and the body is the envelope nexus's board
//! tool reads (ADR 0019); the runtime attaches the credential header on the way out.

use serde_json::{json, Value};

/// The path `nexus transit serve` answers calls on, appended to the gateway address.
pub const CALL_PATH: &str = "/v1/call";

/// The envelope version this relay speaks; serve refuses any other as `bad-envelope`.
pub const ENVELOPE_VERSION: u64 = 1;

/// The call URL: the gateway endpoint, trimmed, with any trailing `/` removed, plus
/// [`CALL_PATH`]. A path on the endpoint is kept, so serve's `404` can name it.
pub fn url(gateway_endpoint: &str) -> String {
    format!(
        "{}{CALL_PATH}",
        gateway_endpoint.trim().trim_end_matches('/')
    )
}

/// The path part of an `http://` or `https://` URL built by [`url`]: everything from the first
/// `/` after the authority.
pub fn path(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.find('/').map_or("/", |at| &rest[at..])
}

/// Exactly the request headers: the body's type, the accepted type and its length.
pub fn headers(body_len: usize) -> Vec<(String, String)> {
    vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("content-length".to_string(), body_len.to_string()),
    ]
}

/// The envelope: exactly `envelope`, `capsule`, `session`, `config` and `input`.
///
/// `config` is the entry's `config:` block as the runtime delivered it, or `null` when the
/// entry has none; `input` is the model's input. Neither is inspected here: nexus checks the
/// grant and the verb on every call.
pub fn build(capsule: &str, session: &str, config: Value, input: Value) -> Value {
    json!({
        "envelope": ENVELOPE_VERSION,
        "capsule": capsule,
        "session": session,
        "config": config,
        "input": input,
    })
}
