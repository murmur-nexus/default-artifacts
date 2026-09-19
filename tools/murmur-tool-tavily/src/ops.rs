//! One call, end to end: validate the configuration, the input and the gateway address, send
//! one POST, and map the reply to a tool result.
//!
//! Every check that can fail runs before the request, so a refused call sends nothing. There
//! is one request per call and no retry: the runtime's gateway already re-reads the
//! credential and resends once on a `401`, and a `429` is reported with its `Retry-After`
//! for the agent to act on, not slept through.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::config::{parse_config, Config};
use crate::input::parse_input;
use crate::render::{self, Header, SearchResponse};
use crate::request;

/// Directory under the workdir a truncated search's full response is written to.
pub const SPILL_DIR: &str = "tavily-results";

/// Longest excerpt of Tavily's own error text carried into a failure message, in characters.
const UPSTREAM_TEXT_CHARS: usize = 300;

/// The `error_kind` values a failure carries.
pub mod kind {
    pub const INVALID_INPUT: &str = "invalid_input";
    pub const CONFIG_INVALID: &str = "config_invalid";
    pub const GATEWAY_MISSING: &str = "gateway_missing";
    pub const UPSTREAM_UNAUTHORIZED: &str = "upstream_unauthorized";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    pub const UPSTREAM_REJECTED: &str = "upstream_rejected";
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    pub const TRANSPORT_ERROR: &str = "transport_error";
    pub const RESPONSE_INVALID: &str = "response_invalid";
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

/// What came back: the status, the `Retry-After` header verbatim, and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

/// Mirrors the WIT `status` enum without depending on the generated bindings, so this
/// module stays host-testable.
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

/// Run one call.
///
/// `gateway_endpoint` is `MURMUR_GATEWAY_ENDPOINT` and `config_json` is
/// `MURMUR_ARTIFACT_CONFIG`, each `None` when the runtime did not set it. `out_dir` is where
/// a truncated search's full response is written under [`SPILL_DIR`]; the adapter passes the
/// workdir preopen `.`.
pub fn run(
    gateway_endpoint: Option<&str>,
    config_json: Option<&str>,
    data: &str,
    transport: &mut dyn Transport,
    out_dir: &Path,
) -> Response {
    let config = match config_json {
        None => None,
        Some(text) => match parse_config(text) {
            Ok(config) => Some(config),
            Err(message) => return failure(kind::CONFIG_INVALID, message, Map::new()),
        },
    };
    let defaults_in_use = config.is_none();
    let config = config.unwrap_or_default();

    let input = match parse_input(data) {
        Ok(input) => input,
        Err(message) => return failure(kind::INVALID_INPUT, message, Map::new()),
    };

    let gateway_endpoint = match gateway_endpoint.map(str::trim) {
        Some(endpoint) if endpoint.starts_with("http://") || endpoint.starts_with("https://") => {
            endpoint
        }
        _ => return failure(kind::GATEWAY_MISSING, gateway_missing_message(), Map::new()),
    };

    let requested = input
        .max_results
        .unwrap_or(config.max_results.default.into());
    let max_results = requested.min(config.max_results.max.into()) as u32;
    let body = request::body(&config, &input.query, max_results).to_string();
    let reply = match transport.post(
        &request::url(gateway_endpoint),
        &request::headers(body.len()),
        body.as_bytes(),
    ) {
        Ok(reply) => reply,
        Err(e) => {
            return failure(
                kind::TRANSPORT_ERROR,
                format!("the request to the credential gateway failed: {e}"),
                Map::new(),
            )
        }
    };

    if !(200..300).contains(&reply.status) {
        return upstream_failure(&reply);
    }
    let response = match render::parse_response(&reply.body) {
        Ok(response) => response,
        Err(message) => return failure(kind::RESPONSE_INVALID, message, Map::new()),
    };
    success(
        &config,
        &input.query,
        requested,
        defaults_in_use,
        &response,
        &reply.body,
        out_dir,
    )
}

fn success(
    config: &Config,
    query: &str,
    requested: u64,
    defaults_in_use: bool,
    response: &SearchResponse,
    raw_body: &[u8],
    out_dir: &Path,
) -> Response {
    let header = Header {
        query,
        requested,
        capped_at: config.max_results.max,
        depth: config.search_depth.as_str(),
        defaults_in_use,
    };
    let rendered = render::render(&header, response, config.max_output_bytes, || {
        spill(out_dir, response.request_id.as_deref(), raw_body)
    });

    let mut summary = format!("{} results for \"{query}\"", response.results.len());
    if let Some(credits) = &response.credits {
        summary.push_str(&format!("; credits: {credits}"));
    }
    Response {
        status: OpStatus::Passed,
        summary,
        data: rendered.text,
        data_path: rendered.data_path,
        truncated: rendered.truncated,
        metadata: vec![
            ("state_effect".to_string(), "read".to_string()),
            ("resource_id".to_string(), format!("tavily:{query}")),
        ],
    }
}

/// Write the full response, as received, to `<out_dir>/tavily-results/<file>` and return
/// the path relative to `out_dir`, or `None` when it could not be written.
///
/// The file is named for Tavily's `request_id` when that is a plain identifier, and
/// otherwise `search-<n>.json` with the first free `n`; an existing file is never replaced.
fn spill(out_dir: &Path, request_id: Option<&str>, raw_body: &[u8]) -> Option<String> {
    let dir = out_dir.join(SPILL_DIR);
    fs::create_dir_all(&dir).ok()?;
    let write = |name: &str| -> std::io::Result<String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))?;
        file.write_all(raw_body)?;
        Ok(format!("{SPILL_DIR}/{name}"))
    };
    let named = request_id
        .filter(|id| is_plain_request_id(id))
        .map(|id| format!("{id}.json"));
    let numbered = (1u32..).map(|n| format!("search-{n}.json"));
    for name in named.into_iter().chain(numbered) {
        match write(&name) {
            Ok(path) => return Some(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// `^[A-Za-z0-9-]{1,64}$`: safe as a file name on every host.
fn is_plain_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn upstream_failure(reply: &HttpReply) -> Response {
    let status = reply.status;
    let detail = upstream_text(&reply.body);
    let mut extra = Map::new();
    extra.insert("http_status".to_string(), json!(status));
    let (error_kind, message) = match status {
        401 | 403 => {
            // The gateway looks only at a 401: it re-reads the credential and resends once,
            // and only when the value changed. A 403 comes back untouched.
            let reread = if status == 401 {
                " The runtime already re-read the credential and resent if its value had changed;"
            } else {
                ""
            };
            (
                kind::UPSTREAM_UNAUTHORIZED,
                format!(
                    "Tavily refused the key (HTTP {status}): {detail}.{reread} check \
                     gateway.api_key on this tool's entry and the value stored in \
                     credentials.<NAME> it names"
                ),
            )
        }
        429 => {
            if let Some(retry_after) = &reply.retry_after {
                extra.insert("retry_after".to_string(), json!(retry_after));
            }
            let wait = reply
                .retry_after
                .as_deref()
                .map(|retry_after| format!("; Retry-After: {retry_after}"))
                .unwrap_or_default();
            (
                kind::RATE_LIMITED,
                format!("Tavily rate-limited the request (HTTP 429){wait}: {detail}"),
            )
        }
        432 | 433 => (
            kind::QUOTA_EXCEEDED,
            format!("the Tavily account is out of credits (HTTP {status}): {detail}"),
        ),
        400 | 422 => (
            kind::UPSTREAM_REJECTED,
            format!("Tavily rejected the request (HTTP {status}): {detail}"),
        ),
        _ => (
            kind::UPSTREAM_ERROR,
            format!("Tavily answered HTTP {status}: {detail}"),
        ),
    };
    failure(error_kind, message, extra)
}

/// Tavily's own error text — `detail.error`, else `detail`, else the raw body — cut to
/// [`UPSTREAM_TEXT_CHARS`] characters.
fn upstream_text(body: &[u8]) -> String {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let detail = parsed.as_ref().and_then(|value| value.get("detail"));
    let text = match detail {
        Some(detail) => match detail.get("error").and_then(Value::as_str) {
            Some(error) => error.to_string(),
            None => detail
                .as_str()
                .map_or_else(|| detail.to_string(), str::to_string),
        },
        None => String::from_utf8_lossy(body).into_owned(),
    };
    match text.char_indices().nth(UPSTREAM_TEXT_CHARS) {
        None => text,
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

fn gateway_missing_message() -> String {
    format!(
        "this tool's entry binds no credential gateway, so there is nowhere to send the \
         search; add gateway: {{endpoint: {}, api_key: {}}} to the murmur-tool-tavily entry in \
         the capsule manifest and store the key with mur config set -g {} <key>",
        crate::SUGGESTED_ENDPOINT,
        crate::SUGGESTED_KEY_REFERENCE,
        crate::SUGGESTED_CREDENTIAL
    )
}

/// A failure: `data` is `{"ok":false,"error_kind":…,"message":…}` plus the kind's extra
/// fields, `summary` is the message, and nothing is written.
fn failure(error_kind: &str, message: impl Into<String>, extra: Map<String, Value>) -> Response {
    let message = message.into();
    let mut data = Map::new();
    data.insert("ok".to_string(), json!(false));
    data.insert("error_kind".to_string(), json!(error_kind));
    data.insert("message".to_string(), json!(message));
    data.extend(extra);
    let status = if error_kind == kind::INVALID_INPUT {
        OpStatus::Failed
    } else {
        OpStatus::Error
    };
    Response {
        status,
        summary: message,
        data: Value::Object(data).to_string(),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}
