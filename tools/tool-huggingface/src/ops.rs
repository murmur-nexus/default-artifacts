//! One call, end to end: validate the configuration, the input and the gateway address,
//! dispatch to the op, and map what came back to a tool result.
//!
//! Every check that can fail on its own runs before the first request, so a refused call
//! sends nothing. No request is repeated, nothing sleeps and nothing retries: a `429` is
//! reported with its `Retry-After` for the agent to act on.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::config::{parse_config, Config};
use crate::http::{ChainError, Client, Fetched, HttpFailure, Method, Route, Transport, USER_AGENT};
use crate::input::{parse_input, Input};
use crate::{download, hub, viewer};

/// Directory under the work folder a truncated result's full upstream response goes to.
pub const SPILL_DIR: &str = "hf-results";

/// Longest excerpt of upstream error text carried into a failure message, in characters.
const UPSTREAM_TEXT_CHARS: usize = 300;

/// The `error_kind` values a failure carries.
pub mod kind {
    pub const INVALID_INPUT: &str = "invalid_input";
    pub const CONFIG_INVALID: &str = "config_invalid";
    pub const GATEWAY_MISSING: &str = "gateway_missing";
    pub const NOT_FOUND: &str = "not_found";
    pub const GATED: &str = "gated";
    pub const UPSTREAM_UNAUTHORIZED: &str = "upstream_unauthorized";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const TOO_LARGE: &str = "too_large";
    pub const VIEWER_UNAVAILABLE: &str = "viewer_unavailable";
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    pub const TRANSPORT_ERROR: &str = "transport_error";
    pub const RESPONSE_INVALID: &str = "response_invalid";
    pub const WRITE_FAILED: &str = "write_failed";
}

/// Mirrors the WIT `status` enum without depending on the generated bindings.
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

/// What every op works with.
pub struct Ctx<'a> {
    pub client: Client<'a>,
    pub config: Config,
    /// Whether the entry has no `config:` block at all.
    pub defaults_in_use: bool,
    pub out_dir: &'a Path,
}

impl Ctx<'_> {
    /// The header line saying the defaults apply, or nothing.
    pub fn defaults_line(&self) -> &'static str {
        if self.defaults_in_use {
            "\n[defaults in use — no config: block on this entry]"
        } else {
            ""
        }
    }

    /// One `GET` chain to an API URL, buffered, with `Accept: application/json`.
    pub fn get_api(&mut self, url: &str, route: Route) -> Result<Fetched, Response> {
        let headers = api_headers();
        self.client
            .fetch(Method::Get, url, route, &headers, &|_| false)
            .map_err(chain_failure)
    }

    /// The gateway URL for `path_and_query`.
    pub fn hub(&self, path_and_query: &str) -> String {
        self.client.gateway_url(path_and_query)
    }
}

/// `Accept: application/json` and the `User-Agent`.
pub fn api_headers() -> Vec<(String, String)> {
    vec![
        ("accept".to_string(), "application/json".to_string()),
        ("user-agent".to_string(), USER_AGENT.to_string()),
    ]
}

/// Run one call.
///
/// `gateway_endpoint` is `MURMUR_GATEWAY_ENDPOINT` and `config_json` is
/// `MURMUR_ARTIFACT_CONFIG`, each `None` when the runtime did not set it. `out_dir` is the
/// work folder: spills go under [`SPILL_DIR`] and downloads to `dest` inside it.
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

    let input = match parse_input(data, config.max_download_bytes) {
        Ok(input) => input,
        Err(message) => return failure(kind::INVALID_INPUT, message, Map::new()),
    };
    if let Input::Download(download) = &input {
        if let Err(message) = download::check_dest(out_dir, &download.dest) {
            return failure(kind::INVALID_INPUT, message, Map::new());
        }
    }

    let gateway_endpoint = match gateway_endpoint.map(str::trim) {
        Some(endpoint) if endpoint.starts_with("http://") || endpoint.starts_with("https://") => {
            endpoint
        }
        _ => return failure(kind::GATEWAY_MISSING, gateway_missing_message(), Map::new()),
    };

    let mut ctx = Ctx {
        client: Client::new(transport, gateway_endpoint),
        config,
        defaults_in_use,
        out_dir,
    };
    match input {
        Input::Search(input) => hub::search(&mut ctx, &input),
        Input::Repo(input) => hub::repo(&mut ctx, &input),
        Input::Paper(input) => hub::paper(&mut ctx, &input),
        Input::Dataset(input) => viewer::dataset(&mut ctx, &input),
        Input::Download(input) => download::download(&mut ctx, &input),
    }
}

/// A successful read: `state_effect: read`, `resource_id: hf:<op>:<subject>`.
pub fn read_success(
    op: &str,
    subject: &str,
    summary: String,
    rendered: crate::render::Rendered,
) -> Response {
    Response {
        status: OpStatus::Passed,
        summary,
        data: rendered.text,
        data_path: rendered.data_path,
        truncated: rendered.truncated,
        metadata: vec![
            ("state_effect".to_string(), "read".to_string()),
            ("resource_id".to_string(), format!("hf:{op}:{subject}")),
        ],
    }
}

/// Write `content` to `<out_dir>/hf-results/<op>-<n>.json` at the first free `n` and return
/// the path relative to `out_dir`, or `None` when it could not be written. An existing file is
/// never replaced.
pub fn spill(out_dir: &Path, op: &str, content: &[u8]) -> Option<String> {
    let dir = out_dir.join(SPILL_DIR);
    fs::create_dir_all(&dir).ok()?;
    for n in 1u32..=100_000 {
        let name = format!("{op}-{n}.json");
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name));
        match opened {
            Ok(mut file) => {
                return file
                    .write_all(content)
                    .ok()
                    .map(|()| format!("{SPILL_DIR}/{name}"));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// An upstream body as JSON for a spill file: parsed when it parses, else the text as a string.
pub fn body_value(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
}

/// Parse a `2xx` JSON body, or `response_invalid` naming what it was.
pub fn parse_json(body: &[u8], what: &str) -> Result<Value, Response> {
    serde_json::from_slice(body).map_err(|e| {
        failure(
            kind::RESPONSE_INVALID,
            format!("{what} is not JSON: {e}"),
            Map::new(),
        )
    })
}

/// A request chain that ended without a usable response.
pub fn chain_failure(error: ChainError) -> Response {
    match error {
        ChainError::Redirect(message) => failure(kind::RESPONSE_INVALID, message, Map::new()),
        ChainError::Http {
            failure: http,
            host,
        } => http_failure(http, &host),
    }
}

/// A transport failure on a request addressed to `host`.
pub fn http_failure(error: HttpFailure, host: &str) -> Response {
    match error {
        HttpFailure::Denied { host } => {
            let mut extra = Map::new();
            extra.insert("host".to_string(), json!(host));
            failure(
                kind::TRANSPORT_ERROR,
                format!(
                    "`{host}` is not in the network allow-list this tool runs under; list it in \
                     the capsule's capabilities.network.allow, and in capabilities.network.allow \
                     on this tool's entry if the entry narrows it"
                ),
                extra,
            )
        }
        HttpFailure::TooLarge => failure(
            kind::RESPONSE_INVALID,
            format!(
                "the response from {host} exceeded {} bytes; not reading further",
                crate::http::MAX_RESPONSE_BYTES
            ),
            Map::new(),
        ),
        HttpFailure::Aborted(message) | HttpFailure::Transport(message) => failure(
            kind::TRANSPORT_ERROR,
            format!("the request to {host} failed: {message}"),
            Map::new(),
        ),
    }
}

/// A non-2xx Hub answer about `subject`.
///
/// `missing_means_not_found` is set for a read of one repo, file or paper, where a keyless
/// `401` with no error code is how the Hub says "does not exist, or is private".
pub fn hub_failure(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    subject: &str,
    missing_means_not_found: bool,
) -> Response {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    };
    let code = header("x-error-code").unwrap_or_default();
    let detail = upstream_text(header("x-error-message"), body);
    let mut extra = Map::new();
    extra.insert("http_status".to_string(), json!(status));
    let coded = if code.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status} {code}")
    };
    if status == 429 {
        let retry_after = header("retry-after");
        if let Some(retry_after) = retry_after {
            extra.insert("retry_after".to_string(), json!(retry_after));
        }
        let wait = retry_after
            .map(|value| format!("; Retry-After: {value}"))
            .unwrap_or_default();
        return failure(
            kind::RATE_LIMITED,
            format!("the Hub rate-limited the request ({coded}){wait}: {detail}"),
            extra,
        );
    }
    if code == "GatedRepo" && matches!(status, 401 | 403) {
        return failure(
            kind::GATED,
            format!(
                "{subject} is gated ({coded}): {detail}. Accept its conditions on the Hub with \
                 the account whose token is bound as gateway.api_key ({}); a keyless gateway \
                 cannot read gated files",
                crate::SUGGESTED_CREDENTIAL
            ),
            extra,
        );
    }
    if status == 404 || matches!(code, "RepoNotFound" | "EntryNotFound" | "RevisionNotFound") {
        return failure(
            kind::NOT_FOUND,
            format!("{subject} was not found on the Hub ({coded}): {detail}"),
            extra,
        );
    }
    if status == 401 && code.is_empty() && missing_means_not_found {
        return failure(
            kind::NOT_FOUND,
            format!(
                "{subject} does not exist, or is private and the token bound on this entry (if \
                 any) cannot read it ({coded}): {detail}"
            ),
            extra,
        );
    }
    if matches!(status, 401 | 403) {
        let reread = if status == 401 {
            " The runtime already re-read the credential and resent if its value had changed;"
        } else {
            ""
        };
        return failure(
            kind::UPSTREAM_UNAUTHORIZED,
            format!(
                "the Hub refused the request for {subject} ({coded}): {detail}.{reread} check \
                 gateway.api_key on this tool's entry and the token stored in {}",
                crate::SUGGESTED_CREDENTIAL
            ),
            extra,
        );
    }
    failure(
        kind::UPSTREAM_ERROR,
        format!("the Hub answered {coded} for {subject}: {detail}"),
        extra,
    )
}

/// The upstream's own error text: `x-error-message` when present, else the body's `error`,
/// else the raw body, without trailing periods, cut to [`UPSTREAM_TEXT_CHARS`] characters.
pub fn upstream_text(x_error_message: Option<&str>, body: &[u8]) -> String {
    let text = match x_error_message {
        Some(message) if !message.trim().is_empty() => message.to_string(),
        _ => {
            let parsed: Option<Value> = serde_json::from_slice(body).ok();
            match parsed
                .as_ref()
                .and_then(|value| value.get("error"))
                .and_then(Value::as_str)
            {
                Some(error) => error.to_string(),
                None => String::from_utf8_lossy(body).trim().to_string(),
            }
        }
    };
    // Messages append their own punctuation after the excerpt.
    let text = text.trim_end().trim_end_matches('.').trim_end().to_string();
    let text = if text.is_empty() {
        "(no error text)".to_string()
    } else {
        text
    };
    crate::render::cut_chars(&text, UPSTREAM_TEXT_CHARS).0
}

fn gateway_missing_message() -> String {
    format!(
        "this tool's entry binds no credential gateway, so there is nowhere to send the request \
         and nothing was sent; add gateway: {{endpoint: {endpoint}, api_key: {key}}} to the \
         tool-huggingface entry in the capsule manifest and store the token with mur config set \
         -g {credential} <token>, or, for public data only, gateway: {{endpoint: {endpoint}, \
         keyless: true}}",
        endpoint = crate::SUGGESTED_ENDPOINT,
        key = crate::SUGGESTED_KEY_REFERENCE,
        credential = crate::SUGGESTED_CREDENTIAL
    )
}

/// A failure: `data` is `{"ok":false,"error_kind":…,"message":…}` plus the kind's extra
/// fields, `summary` is the message, and metadata is empty.
pub fn failure(
    error_kind: &str,
    message: impl Into<String>,
    extra: Map<String, Value>,
) -> Response {
    let message = message.into();
    let mut data = Map::new();
    data.insert("ok".to_string(), json!(false));
    data.insert("error_kind".to_string(), json!(error_kind));
    data.insert("message".to_string(), json!(message));
    data.extend(extra);
    let status = match error_kind {
        kind::INVALID_INPUT
        | kind::NOT_FOUND
        | kind::GATED
        | kind::TOO_LARGE
        | kind::VIEWER_UNAVAILABLE => OpStatus::Failed,
        _ => OpStatus::Error,
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
