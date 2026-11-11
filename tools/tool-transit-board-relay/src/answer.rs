//! What the model gets back: serve's answer judged into a tool result, and every line the
//! relay writes itself when the call never reached the board.
//!
//! A board tool answer passes through byte for byte as `data`; only `status`, `summary` and
//! `state_effect` are read off it. Every relay line starts with [`PREFIX`], stays on one line,
//! and names what is missing and what the operator writes to fix it.

use serde_json::{Map, Value};

use crate::ops::{OpStatus, Response};

/// The start of every line the relay writes itself.
pub const PREFIX: &str = "tool-transit-board-relay: ";

/// Longest excerpt of serve's text, an answer field or a body carried into a line, in
/// characters, before `…`.
pub const EXCERPT_CHARS: usize = 300;

/// What a line says in place of serve's message when the reply had no body.
pub const NO_BODY: &str = "(no body)";

/// `PREFIX` followed by a literal, as one `&'static str`.
macro_rules! prefixed {
    ($text:expr) => {
        concat!("tool-transit-board-relay: ", $text)
    };
}

/// The entry binds no `gateway:`, so the runtime set no gateway address.
pub const NO_GATEWAY: &str = prefixed!(
    "this entry has no gateway, so there is no board tool to call: add gateway: { endpoint: \
     http://127.0.0.1:<port>, api_key: ${NEXUS_TRANSIT_TOOL_KEY} } to the \
     murmur/tool-transit-board-relay entry in the capsule manifest, with the port nexus \
     transit serve printed"
);

/// The model's input is absent, blank or not JSON.
pub const INPUT_NOT_JSON: &str = prefixed!(
    r#"the input is not JSON; send an object with "verb", such as {"verb": "help"}, which lists the verbs this capsule holds and the fields each takes"#
);

/// Serve answered `401`: the runtime attached no key serve holds.
pub const UNAUTHORIZED: &str = prefixed!(
    r#"nexus transit serve refused this entry's key (401): store the key from ~/.nexus/transit/tool.key on the host running serve with mur config set -g credentials.NEXUS_TRANSIT_TOOL_KEY "$(cat ~/.nexus/transit/tool.key)" and name it on this entry as gateway.api_key: ${NEXUS_TRANSIT_TOOL_KEY}; the next call uses the stored key"#
);

/// Serve answered `503`: it is closing.
pub const CLOSING: &str =
    prefixed!("nexus transit serve is shutting down (503); call again once it is serving");

/// The runtime set no `variable` (`MURMUR_CAPSULE_NAME` or `MURMUR_SESSION_ID`).
pub fn no_identity(variable: &str) -> String {
    format!(
        "{PREFIX}the runtime set no {variable}, so the call cannot say which capsule and session \
         made it; run this tool in a capsule launched by mur run"
    )
}

/// `MURMUR_ARTIFACT_CONFIG` did not parse; `error` is the parser's message.
pub fn config_not_json(error: &str) -> String {
    format!(
        "{PREFIX}MURMUR_ARTIFACT_CONFIG is not JSON ({}); the runtime writes this entry's config: \
         block there",
        excerpt(error)
    )
}

/// The request got no reply: the transport's own message, a body over the read cap, or a
/// denial the runtime returned.
pub fn transport_failed(message: &str) -> String {
    format!(
        "{PREFIX}no answer through the gateway ({}); check that nexus transit serve is running on \
         the port this entry's gateway.endpoint names",
        excerpt(message)
    )
}

/// A non-2xx reply, `3xx` included: the runtime returns a redirect rather than following it.
/// `path` is the path the relay sent.
pub fn http_refused(status: u16, path: &str, body: &[u8]) -> String {
    let message = serve_message(body);
    match status {
        401 => UNAUTHORIZED.to_string(),
        400 => format!("{PREFIX}nexus transit serve refused the envelope (400): {message}"),
        404 => format!(
            "{PREFIX}nothing answers {path} at the gateway (404): gateway.endpoint must be \
             http://127.0.0.1:<port> with no path, the address nexus transit serve printed; \
             serve said: {message}"
        ),
        405 => format!("{PREFIX}the gateway refused POST {path} (405): {message}"),
        413 => format!(
            "{PREFIX}the call is larger than nexus transit serve accepts (413): {message}; send \
             shorter fields"
        ),
        503 => CLOSING.to_string(),
        _ => format!("{PREFIX}the gateway answered HTTP {status}: {message}"),
    }
}

/// A 2xx reply whose body is not a board tool answer.
pub fn not_an_answer(status: u16, body: &[u8]) -> String {
    format!(
        "{PREFIX}the gateway answered {status} with a body that is not a board tool answer, an \
         object with a boolean \"ok\": {}",
        body_excerpt(body)
    )
}

/// Serve's own refusal text: `error.message` when the body is `{"error": {"message": <string>}}`,
/// else the body as text, each folded to one line and cut to [`EXCERPT_CHARS`]; [`NO_BODY`]
/// when there is nothing to show.
pub fn serve_message(body: &[u8]) -> String {
    let from_json = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")?
                .get("message")?
                .as_str()
                .map(excerpt)
                .filter(|message| !message.is_empty())
        });
    from_json.unwrap_or_else(|| body_excerpt(body))
}

/// The body as lossy UTF-8, folded and cut; [`NO_BODY`] when it is empty or blank.
fn body_excerpt(body: &[u8]) -> String {
    let text = excerpt(&String::from_utf8_lossy(body));
    if text.is_empty() {
        NO_BODY.to_string()
    } else {
        text
    }
}

/// `text` folded to one line, then cut to [`EXCERPT_CHARS`].
fn excerpt(text: &str) -> String {
    cut(&fold(text), EXCERPT_CHARS)
}

/// Every run of `\r`/`\n`, with the whitespace around it, becomes one space; the ends are
/// trimmed. The result holds no line break.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' || c == '\n' {
            let kept = out.trim_end().len();
            out.truncate(kept);
            while chars.peek().is_some_and(|next| next.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// `text` when it is at most `max_chars` characters, else its first `max_chars` followed by `…`.
pub fn cut(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((at, _)) => format!("{}…", &text[..at]),
    }
}

/// A result carrying one relay line as both `summary` and `data`, with no metadata.
pub fn relay_line(status: OpStatus, line: String) -> Response {
    Response {
        status,
        summary: line.clone(),
        data: line,
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}

/// Judge serve's reply. `path` is the path the relay sent, named by the `404` and `405` lines.
///
/// A 2xx body must be UTF-8 holding a JSON object whose `ok` is a boolean; it then becomes
/// `data` unchanged, `passed` when `ok` is true and `failed` when false, with `state_effect`
/// set only when `effect` is `read` or `mutate`. Any other reply is a relay line, `error`.
pub fn judge(status: u16, body: &[u8], path: &str) -> Response {
    if !(200..300).contains(&status) {
        return relay_line(OpStatus::Error, http_refused(status, path, body));
    }
    let Some((text, answer, ok)) = board_answer(body) else {
        return relay_line(OpStatus::Error, not_an_answer(status, body));
    };

    let verb = field(&answer, "verb");
    let metadata = match answer.get("effect").and_then(Value::as_str) {
        Some(effect @ ("read" | "mutate")) => {
            vec![("state_effect".to_string(), effect.to_string())]
        }
        _ => Vec::new(),
    };
    let summary = if ok {
        format!("transit {verb}: ok ({})", field(&answer, "effect"))
    } else {
        let error = answer.get("error").and_then(Value::as_object);
        let detail = |key: &str| error.map_or_else(|| "-".to_string(), |error| field(error, key));
        format!(
            "transit {verb}: refused {}: {}",
            detail("code"),
            detail("message")
        )
    };
    Response {
        status: if ok {
            OpStatus::Passed
        } else {
            OpStatus::Failed
        },
        summary,
        data: text.to_string(),
        data_path: None,
        truncated: false,
        metadata,
    }
}

/// The body as text, its object and its `ok`, when it is a board tool answer.
fn board_answer(body: &[u8]) -> Option<(&str, Map<String, Value>, bool)> {
    let text = std::str::from_utf8(body).ok()?;
    let Value::Object(answer) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let ok = answer.get("ok")?.as_bool()?;
    Some((text, answer, ok))
}

/// A string field of an answer, folded and cut, or `-` when it is absent, not a string or blank.
fn field(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(excerpt)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".to_string())
}
