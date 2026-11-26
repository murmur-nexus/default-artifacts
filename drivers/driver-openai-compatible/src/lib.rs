// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use murmur_driver_stream::Failure;
use serde::Deserialize;
use serde_json::{json, Map, Number, Value};

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a hard error on the first inference call, before
// any HTTP request is dispatched: a self-hosted server ignores body members it does not know
// without saying so, so a free-form pass-through would let a misspelt setting run unconstrained.

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &["structured_outputs", "surface", "temperature"];

/// The OpenAI-shaped endpoint a request goes to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Surface {
    /// `POST <endpoint>/chat/completions`: system and messages, tool calls, reasoning.
    #[default]
    Chat,
    /// `POST <endpoint>/completions`: one `prompt` string, text only.
    Completions,
}

impl Surface {
    /// The path appended to the endpoint, trailing `/` trimmed off the endpoint first.
    fn path(self) -> &'static str {
        match self {
            Surface::Chat => "/chat/completions",
            Surface::Completions => "/completions",
        }
    }
}

/// The dials read from `inference.driver.config`. Every key is optional; `None` is a key the
/// capsule left unset, and nothing is sent for it.
#[derive(Debug, Clone, Default, PartialEq)]
struct DriverConfig {
    surface: Surface,
    /// Sent as the body's `temperature` exactly as written: a `Number` keeps `1` as `1`.
    temperature: Option<Number>,
    /// Sent verbatim as the body's top-level `structured_outputs`.
    structured_outputs: Option<Map<String, Value>>,
}

/// Read the driver-config JSON. Runs as the first fallible step of `run_inner`, before any
/// environment read other than `MURMUR_INFERENCE_DRIVER_CONFIG` itself, so a rejected config
/// cannot reach the network.
///
/// An absent config, `{}` and a whitespace-only value all yield [`DriverConfig::default`].
fn parse_driver_config(driver_config: Option<&str>) -> Result<DriverConfig, String> {
    let Some(raw) = driver_config else {
        return Ok(DriverConfig::default());
    };
    if raw.trim().is_empty() {
        return Ok(DriverConfig::default());
    }
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|err| format!("driver: inference.driver.config is not valid JSON: {err}"))?;
    let Some(config) = parsed.as_object() else {
        return Err("driver: inference.driver.config must be a JSON object".to_string());
    };

    let unrecognised = config
        .keys()
        .filter(|key| !ACCEPTED_CONFIG_KEYS.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unrecognised.is_empty() {
        // `config` is a serde_json Map, i.e. sorted, so the offending keys come out in a stable
        // order and one manifest fix clears every one of them.
        return Err(format!(
            "driver: unrecognised inference.driver.config key(s): {}. Accepted keys: {}",
            unrecognised.join(", "),
            ACCEPTED_CONFIG_KEYS.join(", ")
        ));
    }

    Ok(DriverConfig {
        surface: parse_surface(config.get("surface"))?,
        temperature: parse_temperature(config.get("temperature"))?,
        structured_outputs: parse_structured_outputs(config.get("structured_outputs"))?,
    })
}

/// A JSON string, trimmed and matched case-insensitively. The refusal quotes the value as
/// written.
fn parse_surface(value: Option<&Value>) -> Result<Surface, String> {
    let Some(value) = value else {
        return Ok(Surface::default());
    };
    match value
        .as_str()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("chat") => Ok(Surface::Chat),
        Some("completions") => Ok(Surface::Completions),
        _ => Err(format!(
            "driver: inference.driver.config 'surface' must be \"chat\" or \"completions\", got \
             {value}"
        )),
    }
}

/// A finite JSON number at least 0, kept as written. No upper bound: the server judges.
fn parse_temperature(value: Option<&Value>) -> Result<Option<Number>, String> {
    match value {
        None => Ok(None),
        Some(Value::Number(number))
            if number.as_f64().is_some_and(|t| t.is_finite() && t >= 0.0) =>
        {
            Ok(Some(number.clone()))
        }
        Some(other) => Err(format!(
            "driver: inference.driver.config 'temperature' must be a number at least 0, got \
             {other}"
        )),
    }
}

/// A non-empty JSON object, forwarded verbatim. Its `choice`, when present, must be a non-empty
/// array of strings; every other member is the server's to read.
fn parse_structured_outputs(value: Option<&Value>) -> Result<Option<Map<String, Value>>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(object) = value.as_object().filter(|object| !object.is_empty()) else {
        return Err(format!(
            "driver: inference.driver.config 'structured_outputs' must be a non-empty object, \
             got {value}"
        ));
    };
    if let Some(choice) = object.get("choice") {
        let valid = choice
            .as_array()
            .is_some_and(|items| !items.is_empty() && items.iter().all(Value::is_string));
        if !valid {
            return Err(format!(
                "driver: inference.driver.config 'structured_outputs.choice' must be a non-empty \
                 array of strings, got {choice}"
            ));
        }
    }
    Ok(Some(object.clone()))
}

// ── Murmur request types ──────────────────────────────────────────────────────

/// The host's request envelope. Its reserved `prompt_cache_key` is not declared, so serde drops
/// it: the OpenAI compatible servers this driver reaches define no cache-key field.
#[derive(Debug, Deserialize)]
struct MurmurRequest {
    model: String,
    max_tokens: u64,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    messages: Vec<MurmurMessage>,
    #[serde(default)]
    tools: Vec<MurmurTool>,
    #[serde(default)]
    params: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct MurmurMessage {
    role: String,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    content: Vec<MurmurContentBlock>,
}

#[derive(Debug, Deserialize)]
struct MurmurTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_object")]
    parameters: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MurmurContentBlock {
    Text {
        text: String,
    },
    // Reasoning this driver returned on an earlier turn. Declared so it deserializes; it is
    // never sent back, on either surface.
    Thinking {
        #[allow(dead_code)]
        text: String,
    },
    // Declared so that it is recognised and refused by name: this driver sends text only. Its
    // source is never read.
    Image {},
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

fn default_object() -> Value {
    json!({})
}

// ── Content helpers ───────────────────────────────────────────────────────────

/// The `text` blocks of a message, joined by newlines.
fn joined_text(content: &[MurmurContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            MurmurContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every header `run_inner` sets on the request: `content-type` and `content-length`. The
/// credential header is the runtime's to attach, never the driver's.
fn request_headers(body_len: usize) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/json".to_string()),
        ("content-length", body_len.to_string()),
    ]
}

/// The path-and-query of `url` as it goes on the wire: everything after the authority, `/` when
/// there is nothing. Names the request in the route-404 hint.
fn request_path(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    match rest.find('/') {
        Some(at) => &rest[at..],
        None => "/",
    }
}

// ── Request translation ───────────────────────────────────────────────────────

/// Every check that can be made without building a body or touching the network, in order:
/// the model id, image blocks, then the surface's own history checks.
fn preflight_request(request: &MurmurRequest, surface: Surface) -> Result<(), String> {
    if request.model.trim().is_empty() {
        return Err(
            "driver: inference.model is empty: driver-openai-compatible sends the model \
                    id to the server unchanged, so it must be an id the server serves (its GET \
                    /models lists them)"
                .to_string(),
        );
    }
    let has_image = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, MurmurContentBlock::Image { .. }))
    });
    if has_image {
        return Err(
            "driver: driver-openai-compatible sends text only and refuses an image block"
                .to_string(),
        );
    }
    if surface == Surface::Completions {
        for (index, message) in request.messages.iter().enumerate() {
            let holds = if message.role == "tool" {
                "tool result"
            } else if message
                .content
                .iter()
                .any(|block| matches!(block, MurmurContentBlock::ToolCall { .. }))
            {
                "tool call"
            } else {
                continue;
            };
            return Err(format!(
                "driver: the completions surface carries no tool calls, but message {index} \
                 holds a {holds}; use surface: chat for a model that calls tools"
            ));
        }
    }
    Ok(())
}

/// Pre-flight, translate for the configured surface, stamp the streaming flags. Tests assert on
/// this rather than on a bare translation.
fn build_provider_request(request: &MurmurRequest, config: &DriverConfig) -> Result<Value, String> {
    preflight_request(request, config.surface)?;
    let mut body = match config.surface {
        Surface::Chat => translate_chat_body(request, config)?,
        Surface::Completions => translate_completions_body(request, config)?,
    };
    stamp_streaming_flags(&mut body);
    Ok(body)
}

/// The Chat Completions body: `model`, `max_tokens`, `messages`, `tools` when any, the
/// configured members, then `params`. No `tool_choice`, `parallel_tool_calls`,
/// `max_completion_tokens`, `reasoning_effort` or reasoning member on an assistant message: the
/// server's defaults stand, and reasoning is never sent back.
fn translate_chat_body(request: &MurmurRequest, config: &DriverConfig) -> Result<Value, String> {
    let mut messages = Vec::new();

    if let Some(system) = trimmed_system(request) {
        messages.push(json!({
            "role": "system",
            "content": system,
        }));
    }

    for message in &request.messages {
        match message.role.as_str() {
            "user" => messages.push(translate_user_message(message)),
            "assistant" => messages.push(translate_assistant_message(message)?),
            "tool" => messages.push(translate_tool_message(message)?),
            other => return Err(format!("driver: unsupported message role '{other}'")),
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("name".to_string(), Value::String(tool.name.clone()));
            function.insert("parameters".to_string(), tool.parameters.clone());
            if let Some(desc) = tool
                .description
                .as_ref()
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())
            {
                function.insert("description".to_string(), Value::String(desc.to_string()));
            }
            json!({"type": "function", "function": function})
        })
        .collect::<Vec<_>>();

    let mut body = Map::new();
    // Byte for byte: whatever id the server serves, never trimmed, case-folded or looked up.
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    insert_configured_members(&mut body, config);
    pass_through_params(&mut body, &request.params);
    Ok(Value::Object(body))
}

/// The Completions body: `model`, `max_tokens`, `prompt`, the configured members, then
/// `params`. Tool definitions are not sent: the surface has no tool calling.
fn translate_completions_body(
    request: &MurmurRequest,
    config: &DriverConfig,
) -> Result<Value, String> {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert(
        "prompt".to_string(),
        Value::String(completions_prompt(request)?),
    );
    insert_configured_members(&mut body, config);
    pass_through_params(&mut body, &request.params);
    Ok(Value::Object(body))
}

/// The one `prompt` string: the trimmed system text when non-empty, then each message's text
/// that is non-empty, in order, joined by a blank line. Message text is not trimmed, so the
/// prompt ends with the last message's text byte for byte. Thinking blocks are left out. Tool
/// messages never reach here: [`preflight_request`] refuses them on this surface.
fn completions_prompt(request: &MurmurRequest) -> Result<String, String> {
    let mut parts = Vec::new();
    if let Some(system) = trimmed_system(request) {
        parts.push(system.to_string());
    }
    for message in &request.messages {
        match message.role.as_str() {
            "user" | "assistant" => {}
            other => return Err(format!("driver: unsupported message role '{other}'")),
        }
        let text = joined_text(&message.content);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    Ok(parts.join("\n\n"))
}

fn trimmed_system(request: &MurmurRequest) -> Option<&str> {
    request
        .system
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `temperature` and `structured_outputs`, each only when configured.
fn insert_configured_members(body: &mut Map<String, Value>, config: &DriverConfig) {
    if let Some(temperature) = &config.temperature {
        body.insert(
            "temperature".to_string(),
            Value::Number(temperature.clone()),
        );
    }
    if let Some(structured_outputs) = &config.structured_outputs {
        body.insert(
            "structured_outputs".to_string(),
            Value::Object(structured_outputs.clone()),
        );
    }
}

/// The one params pass-through loop: a key the body already holds wins.
fn pass_through_params(body: &mut Map<String, Value>, params: &Map<String, Value>) {
    for (key, value) in params {
        if !body.contains_key(key) {
            body.insert(key.clone(), value.clone());
        }
    }
}

/// Image blocks never reach here: [`preflight_request`] refuses them.
fn translate_user_message(message: &MurmurMessage) -> Value {
    json!({"role": "user", "content": joined_text(&message.content)})
}

/// Rebuild an assistant turn: its text, and its tool calls with `content: null` when it carried
/// no text. A `thinking` block is never sent back.
fn translate_assistant_message(message: &MurmurMessage) -> Result<Value, String> {
    let text = joined_text(&message.content);

    let mut tool_calls = Vec::new();
    for block in &message.content {
        if let MurmurContentBlock::ToolCall { id, name, input } = block {
            let arguments = serde_json::to_string(input)
                .map_err(|err| format!("driver: failed to serialize tool_call.input: {err}"))?;
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }));
        }
    }

    if tool_calls.is_empty() {
        return Ok(json!({"role": "assistant", "content": text}));
    }
    let content = if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    Ok(json!({
        "role": "assistant",
        "content": content,
        "tool_calls": tool_calls,
    }))
}

fn translate_tool_message(message: &MurmurMessage) -> Result<Value, String> {
    let tool_call_id = message.tool_call_id.clone().ok_or_else(|| {
        "driver: tool message is missing required field 'tool_call_id'".to_string()
    })?;

    Ok(json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": joined_text(&message.content),
    }))
}

/// Force streaming on, overriding any `stream` or `stream_options` key from `params`, and ask
/// for the usage on the final chunk.
fn stamp_streaming_flags(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
        obj.insert("stream_options".to_string(), json!({"include_usage": true}));
    }
}

// ── Token usage ───────────────────────────────────────────────────────────────

/// The server's raw token counts, carried onto the reserved top-level `usage` object of the
/// translated response. Every member is independently optional: a count the server did not
/// report is omitted rather than sent as `0`, because the host records a reported `0` as a real
/// zero and a fabricated one reads as a cache miss on the `inference` trace event.
///
/// The OpenAI-shaped usage object reports the request total with the cache reads inside it, while
/// the usage contract puts `cached_tokens` beside `input_tokens`. So the raw total is held here as
/// `prompt_tokens`, and the contract's `input_tokens` is derived from it only in `to_value`, after
/// every streamed chunk has been merged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTokens {
    /// `usage.prompt_tokens` — the request total, cache reads included.
    prompt_tokens: Option<u64>,
    /// `usage.completion_tokens` — the output total, reasoning included.
    completion_tokens: Option<u64>,
    /// `usage.prompt_tokens_details.cached_tokens` — the part of `prompt_tokens` read from the
    /// cache.
    cached_tokens: Option<u64>,
    /// `usage.completion_tokens_details.reasoning_tokens` — the part of `completion_tokens`
    /// spent reasoning.
    reasoning_tokens: Option<u64>,
}

impl UsageTokens {
    /// Take every member `later` reports, keeping the current value for the rest, so the last
    /// report of each member wins.
    fn merge_from(&mut self, later: UsageTokens) {
        if later.prompt_tokens.is_some() {
            self.prompt_tokens = later.prompt_tokens;
        }
        if later.completion_tokens.is_some() {
            self.completion_tokens = later.completion_tokens;
        }
        if later.cached_tokens.is_some() {
            self.cached_tokens = later.cached_tokens;
        }
        if later.reasoning_tokens.is_some() {
            self.reasoning_tokens = later.reasoning_tokens;
        }
    }

    /// The contract `usage` object. `input_tokens` is the request tokens not read from the
    /// cache — `prompt_tokens` minus `cached_tokens`, an unreported cache count counting as `0`
    /// — so `input_tokens + cached_tokens` is the server's total. It is omitted when the total
    /// is unreported or the cache count exceeds it: such a report is inconsistent, and no value
    /// is invented for it. `thinking_tokens` is part of `output_tokens`. `None` when no member
    /// survived, in which case the response carries no `usage` key at all.
    fn to_value(self) -> Option<Value> {
        let input_tokens = self
            .prompt_tokens
            .and_then(|total| total.checked_sub(self.cached_tokens.unwrap_or(0)));
        let mut obj = Map::new();
        for (key, count) in [
            ("input_tokens", input_tokens),
            ("output_tokens", self.completion_tokens),
            ("cached_tokens", self.cached_tokens),
            ("thinking_tokens", self.reasoning_tokens),
        ] {
            if let Some(count) = count {
                obj.insert(key.to_string(), Value::from(count));
            }
        }
        if obj.is_empty() {
            None
        } else {
            Some(Value::Object(obj))
        }
    }
}

/// Read an OpenAI-shaped `usage` object. A member that is absent, null, or not a non-negative
/// integer is dropped; its siblings are kept. `total_tokens` is the sum of the other two and is
/// not read.
fn extract_usage(usage: &Value) -> UsageTokens {
    UsageTokens {
        prompt_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
        completion_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
        cached_tokens: usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    }
}

/// Build the murmur response envelope, attaching `usage` only when a count survived.
fn murmur_response(stop_reason: &str, content: Vec<Value>, usage: UsageTokens) -> Value {
    let mut response = json!({
        "stop_reason": stop_reason,
        "content": content,
    });
    if let Some(usage) = usage.to_value() {
        response["usage"] = usage;
    }
    response
}

// ── Stop-reason mapping ───────────────────────────────────────────────────────

/// Map a `finish_reason` to a murmur `stop_reason`. `stop` beside returned tool calls on chat is
/// `tool_call`: some servers say `stop` there, and `end_turn` would drop the calls. `abort` is
/// vLLM's word for a request it gave up on, transient. Any other reason, and `tool_calls` on the
/// completions surface, is a plain error naming it. Each caller refuses a missing reason with
/// its own path's message.
fn map_finish_reason(
    finish_reason: &str,
    surface: Surface,
    has_tool_calls: bool,
) -> Result<&'static str, Failure> {
    match (finish_reason, surface) {
        ("stop", Surface::Chat) if has_tool_calls => Ok("tool_call"),
        ("stop", _) => Ok("end_turn"),
        ("length", _) => Ok("max_tokens"),
        ("tool_calls", Surface::Chat) => Ok("tool_call"),
        ("abort", _) => Err(Failure::transient(
            "driver: the server aborted the turn (finish_reason 'abort')",
        )),
        (other, _) => Err(Failure::plain(format!(
            "driver: unsupported finish_reason '{other}'"
        ))),
    }
}

// ── Server errors ─────────────────────────────────────────────────────────────

/// Whether murmur makes a call answered with `status` again by itself: `408`, `429`, and every
/// `5xx` but `501` and `505`.
fn murmur_retries_status(status: u64) -> bool {
    matches!(status, 408 | 429) || ((500..=599).contains(&status) && status != 501 && status != 505)
}

/// The HTTP status an error names, read from `code`, `status` or `status_code` in that order,
/// each as a JSON number or an all-digit string. `None` when none of them holds one.
fn error_status(error: &Value) -> Option<u64> {
    ["code", "status", "status_code"]
        .iter()
        .find_map(|key| match error.get(key)? {
            Value::Number(number) => number.as_u64(),
            Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().ok()
            }
            _ => None,
        })
}

/// The error a JSON value carries: its top-level `error` member when present and non-null
/// (vLLM's `ErrorResponse` today, and the OpenAI shape), else the whole value when its `object`
/// is `"error"` (vLLM's older flat `ErrorResponse`). `None` for anything else.
fn carried_error(value: &Value) -> Option<&Value> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Some(error);
    }
    (value.get("object").and_then(Value::as_str) == Some("error")).then_some(value)
}

/// `message`, transient when `error` names a status murmur would retry had it arrived as the
/// response status, else plain: a server that runs out of capacity after its `2xx` head gets
/// the same treatment as one that refuses up front.
fn error_failure(message: String, error: &Value) -> Failure {
    if error_status(error).is_some_and(murmur_retries_status) {
        Failure::transient(message)
    } else {
        Failure::plain(message)
    }
}

/// The failure for an error chunk inside a `2xx` stream, the error serialised compactly.
fn stream_error_failure(error: &Value) -> Failure {
    error_failure(format!("driver: server stream error: {error}"), error)
}

/// The failure for a `2xx` JSON body that is itself an error.
fn json_error_failure(error: &Value) -> Failure {
    error_failure(format!("driver: server error: {error}"), error)
}

/// The failure for a server status `>= 400`, always plain: murmur retries `408`, `429` and
/// `5xx` by itself, and a self-hosted server has no quota whose exhaustion would veto that.
///
/// `model` is the id as sent and `path` the request's path as sent to the gateway. A refusal of
/// the model — the error's `param` is `"model"`, or a `404` whose message names the id — says the
/// id went through unchanged; any other `404` points at `gateway.endpoint`.
fn status_failure(status: u16, body: &str, model: &str, path: &str) -> Failure {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|value| {
        value
            .get("error")
            .filter(|error| error.is_object())
            .or_else(|| {
                (value.get("object").and_then(Value::as_str) == Some("error")).then_some(value)
            })
    });
    let names_model = error.is_some_and(|error| {
        error.get("param").and_then(Value::as_str) == Some("model")
            || (status == 404
                && error
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.contains(model)))
    });
    // The hint follows the body, so a body's trailing newline would split the message.
    let shown = body.trim_end();
    let message = if names_model {
        format!(
            "driver: the server refused model '{model}': HTTP {status}: {shown} — \
             driver-openai-compatible sends inference.model unchanged; use an id the server's \
             GET /models lists (vLLM: --served-model-name)"
        )
    } else if status == 404 {
        format!(
            "HTTP 404: {shown} — driver: no route at {path}; gateway.endpoint must be the \
             server's OpenAI base URL, usually ending in /v1"
        )
    } else {
        format!("HTTP {status}: {body}")
    };
    Failure::plain(message)
}

/// The response `content` in its one order: the `thinking` block, then the `text` block, then
/// the `tool_call` blocks, each part present only when non-empty. Both response paths build
/// their content here.
fn ordered_content(reasoning: &str, text: &str, tool_calls: Vec<Value>) -> Vec<Value> {
    let mut content = Vec::new();
    if !reasoning.is_empty() {
        content.push(json!({"type": "thinking", "text": reasoning}));
    }
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content.extend(tool_calls);
    content
}

fn parse_tool_arguments(arguments: &str) -> Result<Value, String> {
    let arguments = if arguments.is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(arguments)
        .map_err(|err| format!("driver: failed to parse tool call arguments JSON: {err}"))
}

/// A chat delta's or message's reasoning: `reasoning` when it is a string, else
/// `reasoning_content`. Servers differ in which they use; one carrying both is read from
/// `reasoning` alone.
fn reasoning_of(value: &Value) -> &str {
    value
        .get("reasoning")
        .and_then(Value::as_str)
        .or_else(|| value.get("reasoning_content").and_then(Value::as_str))
        .unwrap_or_default()
}

// ── Response translation (non-streaming fallback) ─────────────────────────────

/// The non-streaming JSON path, reached when the response body's first non-whitespace byte is
/// `{` or `[` — scripted test servers, and servers that ignore `stream: true`. Chat reads
/// `choices[0].message`; completions reads `choices[0].text`. A body that is itself an error
/// fails with it.
fn translate_json_response(response: &Value, surface: Surface) -> Result<Value, Failure> {
    if let Some(error) = carried_error(response) {
        return Err(json_error_failure(error));
    }

    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: response missing choices[0]".to_string())?;

    // Absent, `null`, non-string or empty: the turn did not finish.
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| "driver: response has no finish_reason".to_string())?;

    let (reasoning, text, tool_calls) = match surface {
        Surface::Completions => (
            "",
            choice
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            Vec::new(),
        ),
        Surface::Chat => {
            let message = choice.get("message").unwrap_or(&Value::Null);
            // An empty `tool_calls: []` is no calls, and leaves the text standing.
            let mut tool_calls = Vec::new();
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = call.get("function");
                let arguments = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tool_calls.push(json!({
                    "type": "tool_call",
                    "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "name": function
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "input": parse_tool_arguments(arguments)?,
                }));
            }
            (
                reasoning_of(message),
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                tool_calls,
            )
        }
    };
    let stop_reason = map_finish_reason(finish_reason, surface, !tool_calls.is_empty())?;

    let usage = response.get("usage").map(extract_usage).unwrap_or_default();

    Ok(murmur_response(
        stop_reason,
        ordered_content(reasoning, text, tool_calls),
        usage,
    ))
}

// ── SSE streaming ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
    /// `Started` has been signalled for this call.
    started: bool,
    /// The last `InputBytes` total signalled for this call.
    reported_bytes: u64,
}

impl ToolCallState {
    /// Signal what this call's state newly supports, after a delta has been merged into it:
    /// its start, once both its id and name are known, then its argument text's byte length
    /// whenever that has grown past the last total. A call whose id has not arrived yet is not
    /// reported; once it arrives, the start and the total so far go out together.
    fn signal_due(&mut self, signal: &mut impl FnMut(ToolCallSignal<'_>)) {
        if !self.started {
            if self.id.is_empty() || self.name.is_empty() {
                return;
            }
            signal(ToolCallSignal::Started {
                id: &self.id,
                name: &self.name,
            });
            self.started = true;
        }
        let bytes = self.arguments.len() as u64;
        if bytes > self.reported_bytes {
            signal(ToolCallSignal::InputBytes {
                id: &self.id,
                bytes,
            });
            self.reported_bytes = bytes;
        }
    }
}

/// A tool-call fact read from the server stream. Carries no part of the call's input.
///
/// The id and name are the `id` and `name` the returned `tool_call` block carries. `InputBytes`
/// is the running UTF-8 byte length of the call's argument text, so it only rises and ends at
/// the length of the text parsed into the returned `input`.
enum ToolCallSignal<'a> {
    Started { id: &'a str, name: &'a str },
    InputBytes { id: &'a str, bytes: u64 },
}

/// Everything a streamed turn has accumulated so far.
#[derive(Default)]
struct StreamState {
    text: String,
    reasoning: String,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
    usage: UsageTokens,
    saw_done: bool,
}

/// Process one complete SSE line into `state`. Returns `Ok(true)` on `data: [DONE]`, which also
/// sets `state.saw_done`, and `Err` for an error chunk ([`carried_error`],
/// [`stream_error_failure`]) or a tool call index past
/// [`murmur_driver_stream::MAX_SLOTS_PER_TURN`].
///
/// On chat, reasoning ([`reasoning_of`]) is routed to `emit_thinking` and `delta.content` to
/// `emit_text`; the two never mix, and nothing in this crate scans the text for reasoning
/// sentinels. A tool call arrives either in fragments — the first carrying its `id` and
/// `function.name`, later ones only `arguments` pieces — or whole in one chunk; both are
/// accumulated by `index`. On completions, `choices[0].text` goes to `emit_text` and nothing else
/// is read from the choice.
fn process_sse_line(
    line: &str,
    surface: Surface,
    state: &mut StreamState,
    emit_text: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
    signal: &mut impl FnMut(ToolCallSignal<'_>),
) -> Result<bool, Failure> {
    if line == "data: [DONE]" {
        state.saw_done = true;
        return Ok(true);
    }
    let Some(json_str) = line.strip_prefix("data: ") else {
        return Ok(false);
    };
    let Ok(data) = serde_json::from_str::<Value>(json_str) else {
        return Ok(false);
    };

    // An error ends the turn here, so it is reported as the server's and never as a cut stream.
    if let Some(error) = carried_error(&data) {
        return Err(stream_error_failure(error));
    }

    // Read from any chunk, before the `choices`-shaped early returns below: `include_usage` puts
    // the final report on a chunk whose `choices` is empty, and a server may also send running
    // usage beside `choices`.
    if let Some(reported) = data.get("usage").filter(|value| !value.is_null()) {
        state.usage.merge_from(extract_usage(reported));
    }

    let choice = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    if let Some(reason) = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        state.stop_reason = Some(reason.to_string());
    }

    if surface == Surface::Completions {
        if let Some(text) = choice.and_then(|c| c.get("text")).and_then(Value::as_str) {
            if !text.is_empty() {
                emit_text(text);
                state.text.push_str(text);
            }
        }
        return Ok(false);
    }

    let Some(delta) = choice.and_then(|c| c.get("delta")) else {
        return Ok(false);
    };

    let reasoning = reasoning_of(delta);
    if !reasoning.is_empty() {
        emit_thinking(reasoning);
        state.reasoning.push_str(reasoning);
    }

    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            emit_text(content);
            state.text.push_str(content);
        }
    }

    if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in tc_arr {
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
            let call = murmur_driver_stream::slot_mut(&mut state.tool_states, index)
                .map_err(|err| Failure::plain(format!("driver: tool call {err}")))?;
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                if call.id.is_empty() {
                    call.id = id.to_string();
                }
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(Value::as_str) {
                    if call.name.is_empty() {
                        call.name = name.to_string();
                    }
                }
                if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                    call.arguments.push_str(args);
                }
            }
            call.signal_due(signal);
        }
    }

    Ok(false)
}

/// Turn the accumulated stream state into the murmur response, its `content` in the order
/// [`ordered_content`] fixes.
///
/// A stream that never sent `data: [DONE]` was cut after the server's `2xx`, whatever it carried
/// before, and that is checked first; one that sent it with no `finish_reason` ended before the
/// turn did. Both fail transiently.
fn assemble_streaming_response(state: StreamState, surface: Surface) -> Result<Value, Failure> {
    if !state.saw_done {
        return Err(Failure::transient(
            "driver: stream ended before data: [DONE]",
        ));
    }
    let finish_reason = state
        .stop_reason
        .as_deref()
        .ok_or_else(|| Failure::transient("driver: stream ended with no finish_reason"))?;

    let mut tool_calls = Vec::new();
    for call in state.tool_states {
        if call.id.is_empty() && call.name.is_empty() {
            continue;
        }
        let input = parse_tool_arguments(&call.arguments)?;
        tool_calls.push(json!({
            "type": "tool_call",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    let stop_reason = map_finish_reason(finish_reason, surface, !tool_calls.is_empty())?;

    Ok(murmur_response(
        stop_reason,
        ordered_content(&state.reasoning, &state.text, tool_calls),
        state.usage,
    ))
}

/// Parse a complete SSE body string, driving the same line processor the wasm module drives.
/// This is how the streaming assertions run host-side, with no network and no server.
#[cfg(test)]
fn parse_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    surface: Surface,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, Failure> {
    parse_sse_body_with_signals(body, surface, emit_text, emit_thinking).0
}

/// A tool-call signal as the tests collect it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedSignal {
    Started { id: String, name: String },
    InputBytes { id: String, bytes: u64 },
}

#[cfg(test)]
impl From<ToolCallSignal<'_>> for RecordedSignal {
    fn from(signal: ToolCallSignal<'_>) -> Self {
        match signal {
            ToolCallSignal::Started { id, name } => Self::Started {
                id: id.to_string(),
                name: name.to_string(),
            },
            ToolCallSignal::InputBytes { id, bytes } => Self::InputBytes {
                id: id.to_string(),
                bytes,
            },
        }
    }
}

/// Parse a complete SSE body string, also returning every tool-call signal in the order it was
/// sent (used in tests).
#[cfg(test)]
fn parse_sse_body_with_signals<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    surface: Surface,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> (Result<Value, Failure>, Vec<RecordedSignal>) {
    let mut state = StreamState::default();
    let mut signals = Vec::new();

    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        match process_sse_line(
            line,
            surface,
            &mut state,
            emit_text,
            emit_thinking,
            &mut |s| signals.push(RecordedSignal::from(s)),
        ) {
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => return (Err(err), signals),
        }
    }

    (assemble_streaming_response(state, surface), signals)
}

// ── Error payloads ────────────────────────────────────────────────────────────

fn error_payload(message: &str) -> Value {
    json!({
        "stop_reason": "error",
        "error": message,
    })
}

// ── WASM driver module ────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_driver {
    use super::{
        assemble_streaming_response, build_provider_request, error_payload, parse_driver_config,
        process_sse_line, request_headers, request_path, status_failure, translate_json_response,
        MurmurRequest, StreamState, ToolCallSignal,
    };
    use murmur_driver_stream::Failure;
    use murmur_wasi_http::{write_body, BodyWrite};
    use serde_json::Value;

    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "driver",
        generate_all,
    });

    /// Hand a tool-call signal to the host, which writes it as a `tool-call-started` or
    /// `tool-call-progress` frame.
    fn signal_tool_call(signal: ToolCallSignal<'_>) {
        match signal {
            ToolCallSignal::Started { id, name } => {
                murmur::stream::events::tool_call_started(id, name)
            }
            ToolCallSignal::InputBytes { id, bytes } => {
                murmur::stream::events::tool_call_input_bytes(id, bytes)
            }
        }
    }

    pub struct OpenAiCompatibleDriver;

    impl exports::murmur::tool::run::Guest for OpenAiCompatibleDriver {
        fn run(
            input: exports::murmur::tool::run::ToolInput,
        ) -> exports::murmur::tool::run::ToolResult {
            let (response, metadata) = match run_inner(input) {
                Ok(value) => (value, vec![]),
                Err(failure) => (error_payload(&failure.message), failure.metadata()),
            };

            let stop_reason = response
                .get("stop_reason")
                .and_then(Value::as_str)
                .unwrap_or("error");

            let status = if stop_reason == "error" {
                exports::murmur::tool::run::Status::Error
            } else {
                exports::murmur::tool::run::Status::Passed
            };

            let summary = if stop_reason == "error" {
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|v| v.to_string())
            } else {
                None
            };

            exports::murmur::tool::run::ToolResult {
                status,
                summary,
                data: Some(response.to_string()),
                data_path: None,
                truncated: false,
                metadata,
                attachments: Vec::new(),
            }
        }
    }

    fn run_inner(input: exports::murmur::tool::run::ToolInput) -> Result<Value, Failure> {
        // First fallible step: a config the driver cannot read fails here, before any other
        // environment read and before any HTTP request is dispatched.
        let driver_config = std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").ok();
        let config = parse_driver_config(driver_config.as_deref())?;
        let surface = config.surface;

        let endpoint = murmur_driver_env::require_endpoint(
            std::env::var(murmur_driver_env::INFERENCE_ENDPOINT_VAR)
                .ok()
                .as_deref(),
        )?;

        let raw = input
            .data
            .ok_or_else(|| "driver: missing tool-input.data".to_string())?;

        let murmur_request: MurmurRequest = serde_json::from_str(&raw)
            .map_err(|err| format!("driver: failed to parse tool-input.data: {err}"))?;

        // Model, image and surface history checks happen inside here, pre-flight: a rejected
        // request costs no HTTP call.
        let provider_body = build_provider_request(&murmur_request, &config)?;

        let body = serde_json::to_vec(&provider_body)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}{}", endpoint.trim_end_matches('/'), surface.path());

        let headers = request_headers(body.len());

        let response = dispatch_request(&url, headers, &body)?;
        let status = response.status();

        if status >= 400 {
            let text = consume_body_as_string(response, status)?;
            return Err(status_failure(
                status,
                &text,
                &murmur_request.model,
                request_path(&url),
            ));
        }

        // Declared body first, then its stream: locals drop in reverse order, so an early return
        // anywhere below releases the stream before the body that owns it.
        let incoming_body = response
            .consume()
            .map_err(|()| "driver: failed to consume response body".to_string())?;
        let stream = incoming_body
            .stream()
            .map_err(|()| "driver: failed to stream response body".to_string())?;

        // Detect format from first read: JSON bodies start with '{' or '['.
        let first = read_chunk(&stream, status)?;
        let is_json = first
            .iter()
            .find(|&&b| !b.is_ascii_whitespace())
            .map(|&b| b == b'{' || b == b'[')
            .unwrap_or(false);

        let result = if is_json {
            // Non-streaming fallback (test servers, servers ignoring stream:true).
            let mut all = first;
            loop {
                let chunk = read_chunk(&stream, status)?;
                if chunk.is_empty() {
                    break;
                }
                all.extend_from_slice(&chunk);
            }
            drop(stream);
            let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
            let text = String::from_utf8(all)
                .map_err(|err| format!("driver: response body is not UTF-8: {err}"))?;
            let json: Value = serde_json::from_str(&text)
                .map_err(|err| format!("driver: failed to parse response JSON: {err}"))?;
            translate_json_response(&json, surface)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut state = StreamState::default();

            let handle_line = |line: &[u8], state: &mut StreamState| -> Result<bool, Failure> {
                let line = String::from_utf8_lossy(line);
                let line = line.trim_end_matches('\r');
                process_sse_line(
                    line,
                    surface,
                    state,
                    &mut |t: &str| murmur::stream::events::emit_chunk(t),
                    &mut |t: &str| murmur::stream::events::emit_thinking_chunk(t),
                    &mut signal_tool_call,
                )
            };

            let mut chunk = first;
            'outer: loop {
                if chunk.is_empty() {
                    break;
                }
                for &b in &chunk {
                    if b == b'\n' {
                        let done = handle_line(&line_buf, &mut state)?;
                        line_buf.clear();
                        if done {
                            break 'outer;
                        }
                    } else {
                        line_buf.push(b);
                    }
                }
                chunk = read_chunk(&stream, status)?;
            }

            // A last line the body ended without a newline after, `data: [DONE]` included.
            if !state.saw_done && !line_buf.is_empty() {
                handle_line(&line_buf, &mut state)?;
            }

            drop(stream);
            let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
            assemble_streaming_response(state, surface)?
        };

        Ok(result)
    }

    fn dispatch_request(
        url: &str,
        headers: Vec<(&str, String)>,
        body: &[u8],
    ) -> Result<wasip2::http::types::IncomingResponse, String> {
        let (scheme, authority, path_with_query) = split_url(url)?;

        let fields = wasip2::http::types::Fields::new();
        for (name, value) in headers {
            fields
                .append(name, &value.into_bytes())
                .map_err(|err| format!("driver: failed to set header '{name}': {err:?}"))?;
        }

        let request = wasip2::http::types::OutgoingRequest::new(fields);
        request
            .set_method(&wasip2::http::types::Method::Post)
            .map_err(|()| "driver: failed to set method".to_string())?;
        request
            .set_scheme(Some(&scheme))
            .map_err(|()| "driver: failed to set scheme".to_string())?;
        request
            .set_authority(Some(&authority))
            .map_err(|()| "driver: failed to set authority".to_string())?;
        request
            .set_path_with_query(Some(&path_with_query))
            .map_err(|()| "driver: failed to set path".to_string())?;

        let outgoing_body = request
            .body()
            .map_err(|()| "driver: failed to acquire request body".to_string())?;
        let future = wasip2::http::outgoing_handler::handle(request, None)
            .map_err(|err| format!("driver: failed to dispatch HTTP request: {err:?}"))?;
        match write_body(&outgoing_body, body) {
            Ok(()) => wasip2::http::types::OutgoingBody::finish(outgoing_body, None)
                .map_err(|err| format!("driver: failed to finalize request body: {err:?}"))?,
            // The host dropped the request without reading the rest of its body; the response
            // future reports why.
            Err(BodyWrite::Closed) => drop(outgoing_body),
            Err(BodyWrite::Failed(message)) => return Err(format!("driver: {message}")),
        }

        await_response(future)
    }

    /// Read a `>= 400` response's body. A read that fails here stays plain: murmur decides from
    /// `status` alone.
    fn consume_body_as_string(
        response: wasip2::http::types::IncomingResponse,
        status: u16,
    ) -> Result<String, Failure> {
        let incoming_body = response
            .consume()
            .map_err(|()| "driver: failed to consume error response body".to_string())?;
        let stream = incoming_body
            .stream()
            .map_err(|()| "driver: failed to stream error response body".to_string())?;
        let mut bytes = Vec::new();
        loop {
            let chunk = read_chunk(&stream, status)?;
            if chunk.is_empty() {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        drop(stream);
        let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
        String::from_utf8(bytes).map_err(|err| {
            Failure::plain(format!("driver: error response body is not UTF-8: {err}"))
        })
    }

    /// The next chunk of a response body that arrived with `status`, empty at its end. A failed
    /// read is classified by [`murmur_driver_stream::read_failure`].
    fn read_chunk(
        stream: &wasip2::io::streams::InputStream,
        status: u16,
    ) -> Result<Vec<u8>, Failure> {
        match stream.blocking_read(16 * 1024) {
            Ok(chunk) => Ok(chunk),
            Err(wasip2::io::streams::StreamError::Closed) => Ok(Vec::new()),
            Err(err) => Err(murmur_driver_stream::read_failure(
                status,
                &format!("{err:?}"),
            )),
        }
    }

    fn await_response(
        future: wasip2::http::types::FutureIncomingResponse,
    ) -> Result<wasip2::http::types::IncomingResponse, String> {
        loop {
            match future.get() {
                Some(Ok(Ok(response))) => return Ok(response),
                Some(Ok(Err(err))) => {
                    return Err(format!(
                        "driver: transport error while awaiting response: {err:?}"
                    ));
                }
                Some(Err(())) => {
                    return Err("driver: response future already consumed".to_string());
                }
                None => {
                    let pollable = future.subscribe();
                    pollable.block();
                }
            }
        }
    }

    fn split_url(url: &str) -> Result<(wasip2::http::types::Scheme, String, String), String> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (wasip2::http::types::Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (wasip2::http::types::Scheme::Http, rest)
        } else {
            return Err(format!(
                "driver: endpoint must start with http:// or https://: '{url}'"
            ));
        };

        let mut parts = rest.splitn(2, '/');
        let authority = parts.next().unwrap_or_default().trim().to_string();
        if authority.is_empty() {
            return Err(format!("driver: endpoint missing authority: '{url}'"));
        }

        let path = match parts.next() {
            Some("") | None => "/".to_string(),
            Some(path) => format!("/{path}"),
        };

        Ok((scheme, authority, path))
    }

    export!(OpenAiCompatibleDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Nothing here talks to a server. Every assertion is driven by a fixture and the host-side
// whole-stream parser.

#[cfg(test)]
mod tests {
    use super::{
        build_provider_request, error_payload, map_finish_reason, parse_driver_config,
        parse_sse_body, parse_sse_body_with_signals, process_sse_line, request_headers,
        request_path, status_failure, translate_json_response, DriverConfig, MurmurRequest,
        RecordedSignal, StreamState, Surface, ACCEPTED_CONFIG_KEYS,
    };
    use murmur_driver_stream::{Failure, Retry};
    use serde_json::{json, Value};

    // ── Fixtures and helpers ──────────────────────────────────────────────────

    /// A served-model name with every character class an operator might use: `/`, `:`, `.`,
    /// upper case and a space. It must reach the body byte for byte.
    const MODEL: &str = "meta-llama/Llama-3.1-8B-Instruct:Q4 v2";

    /// Members neither surface's body ever carries, at any depth.
    const FORBIDDEN_KEYS: &[&str] = &[
        "tool_choice",
        "parallel_tool_calls",
        "max_completion_tokens",
        "prompt_cache_key",
        "reasoning_effort",
        "reasoning",
        "reasoning_content",
        "guided_choice",
        "guided_json",
        "guided_regex",
        "guided_grammar",
        "extra_body",
    ];

    fn request_from(value: Value) -> MurmurRequest {
        serde_json::from_value(value).expect("fixture must deserialize as a murmur request")
    }

    fn cfg(config: &str) -> DriverConfig {
        parse_driver_config(Some(config)).expect("fixture config must parse")
    }

    fn completions() -> DriverConfig {
        cfg(r#"{"surface":"completions"}"#)
    }

    fn config_refusal(config: &str) -> String {
        parse_driver_config(Some(config)).expect_err("config must be refused")
    }

    fn body_for(request: &MurmurRequest, config: &DriverConfig) -> Value {
        build_provider_request(request, config).expect("request must translate")
    }

    fn refusal(request: &MurmurRequest, config: &DriverConfig) -> String {
        build_provider_request(request, config).expect_err("request must be refused")
    }

    fn keys(body: &Value) -> Vec<&str> {
        body.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    fn contains_key_anywhere(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => map
                .iter()
                .any(|(k, v)| k == key || contains_key_anywhere(v, key)),
            Value::Array(items) => items.iter().any(|v| contains_key_anywhere(v, key)),
            _ => false,
        }
    }

    fn assert_no_forbidden_key(body: &Value) {
        for key in FORBIDDEN_KEYS {
            assert!(
                !contains_key_anywhere(body, key),
                "the body must not carry {key}: {body}"
            );
        }
    }

    fn simple_request(model: &str) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        }))
    }

    fn parse_quiet(body: &str, surface: Surface) -> Result<Value, Failure> {
        parse_sse_body(body, surface, &mut |_| {}, &mut |_| {})
    }

    fn chunk(delta: Value) -> String {
        format!(
            "data: {}\n",
            json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta}]})
        )
    }

    fn finish(reason: &str) -> String {
        format!(
            "data: {}\n",
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
        )
    }

    fn text_chunk(text: &str, finish_reason: Option<&str>) -> String {
        format!(
            "data: {}\n",
            json!({"object": "text_completion",
                   "choices": [{"index": 0, "text": text, "finish_reason": finish_reason}]})
        )
    }

    fn usage_chunk(usage: Value) -> String {
        format!("data: {}\n", json!({"choices": [], "usage": usage}))
    }

    const DONE: &str = "data: [DONE]\n";

    fn tool_delta(index: u64, id: Option<&str>, name: Option<&str>, arguments: &str) -> String {
        let mut call = json!({"index": index, "function": {"arguments": arguments}});
        if let Some(id) = id {
            call["id"] = json!(id);
            call["type"] = json!("function");
        }
        if let Some(name) = name {
            call["function"]["name"] = json!(name);
        }
        chunk(json!({"tool_calls": [call]}))
    }

    fn parse_with_signals(body: &str) -> (Value, Vec<RecordedSignal>) {
        let (result, signals) =
            parse_sse_body_with_signals(body, Surface::Chat, &mut |_| {}, &mut |_| {});
        (result.expect("stream must parse"), signals)
    }

    fn started(id: &str, name: &str) -> RecordedSignal {
        RecordedSignal::Started {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn input_bytes(id: &str, bytes: u64) -> RecordedSignal {
        RecordedSignal::InputBytes {
            id: id.to_string(),
            bytes,
        }
    }

    fn assert_transient(result: Result<Value, Failure>, message: &str) {
        let failure = result.expect_err("must fail");
        assert_eq!(failure.message, message);
        assert_eq!(failure.retry, Some(Retry::Transient), "{message}");
        assert_eq!(
            failure.metadata(),
            [("transient".to_string(), "true".to_string())]
        );
    }

    fn assert_plain(result: Result<Value, Failure>, message: &str) {
        let failure = result.expect_err("must fail");
        assert_eq!(failure.message, message);
        assert_eq!(failure.retry, None, "{message}");
        assert!(failure.metadata().is_empty());
    }

    const SURFACES: [Surface; 2] = [Surface::Chat, Surface::Completions];

    /// A minimal complete stream on `surface`: some text, `finish_reason`, `[DONE]`.
    fn complete_stream(surface: Surface, reason: &str) -> String {
        match surface {
            Surface::Chat => format!(
                "{}{}{DONE}",
                chunk(json!({"content": "hi"})),
                finish(reason)
            ),
            Surface::Completions => format!(
                "{}{}{DONE}",
                text_chunk("hi", None),
                text_chunk("", Some(reason))
            ),
        }
    }

    // ── Config ────────────────────────────────────────────────────────────────

    #[test]
    fn the_accepted_config_keys_are_the_three_documented_ones_sorted() {
        assert_eq!(
            ACCEPTED_CONFIG_KEYS,
            ["structured_outputs", "surface", "temperature"]
        );
    }

    #[test]
    fn absent_blank_and_empty_object_configs_give_the_defaults() {
        for config in [None, Some(""), Some("  \n\t"), Some("{}")] {
            let parsed = parse_driver_config(config).unwrap();
            assert_eq!(parsed, DriverConfig::default(), "{config:?}");
            assert_eq!(parsed.surface, Surface::Chat, "chat is the default surface");
            let body = body_for(&simple_request(MODEL), &parsed);
            assert!(body.get("messages").is_some(), "the chat body: {body}");
            for absent in ["temperature", "structured_outputs", "prompt"] {
                assert!(body.get(absent).is_none(), "{absent} unset: {body}");
            }
        }
    }

    #[test]
    fn surface_is_trimmed_and_matched_case_insensitively() {
        for (written, surface) in [
            ("chat", Surface::Chat),
            (" CHAT ", Surface::Chat),
            ("completions", Surface::Completions),
            ("\tCompletions\n", Surface::Completions),
        ] {
            let config = parse_driver_config(Some(&json!({"surface": written}).to_string()))
                .unwrap_or_else(|err| panic!("{written:?}: {err}"));
            assert_eq!(config.surface, surface, "{written:?}");
        }
        assert_eq!(Surface::Chat.path(), "/chat/completions");
        assert_eq!(Surface::Completions.path(), "/completions");
    }

    #[test]
    fn a_surface_other_than_chat_or_completions_is_refused_as_written() {
        for (config, shown) in [
            (r#"{"surface":"Responses"}"#, r#""Responses""#),
            (r#"{"surface":"completion"}"#, r#""completion""#),
            (r#"{"surface":""}"#, r#""""#),
            (r#"{"surface":1}"#, "1"),
            (r#"{"surface":null}"#, "null"),
            (r#"{"surface":["chat"]}"#, r#"["chat"]"#),
        ] {
            assert_eq!(
                config_refusal(config),
                format!(
                    "driver: inference.driver.config 'surface' must be \"chat\" or \
                     \"completions\", got {shown}"
                )
            );
        }
    }

    #[test]
    fn temperature_is_sent_exactly_as_written_on_both_surfaces() {
        for (written, wire) in [
            ("1", "\"temperature\":1"),
            ("0", "\"temperature\":0"),
            ("0.7", "\"temperature\":0.7"),
            ("1.0", "\"temperature\":1.0"),
            ("2.5", "\"temperature\":2.5"),
            ("7", "\"temperature\":7"),
        ] {
            for surface in ["chat", "completions"] {
                let config = cfg(&format!(
                    r#"{{"surface":"{surface}","temperature":{written}}}"#
                ));
                let body = body_for(&simple_request(MODEL), &config).to_string();
                assert!(body.contains(wire), "{surface} {written}: {body}");
            }
        }
    }

    #[test]
    fn a_negative_or_non_number_temperature_is_refused() {
        for written in ["-0.1", "-1", "\"1\"", "null", "true", "{}"] {
            assert_eq!(
                config_refusal(&format!(r#"{{"temperature":{written}}}"#)),
                format!(
                    "driver: inference.driver.config 'temperature' must be a number at least 0, \
                     got {written}"
                )
            );
        }
    }

    #[test]
    fn structured_outputs_is_forwarded_verbatim_at_the_top_level_on_both_surfaces() {
        for structured in [
            json!({"choice": ["accept", "reject"]}),
            json!({"regex": "[AB]"}),
            json!({"json": {"type": "object", "properties": {"x": {"type": "integer"}}}}),
            json!({"grammar": "root ::= \"A\" | \"B\"", "whitespace_pattern": null}),
        ] {
            for surface in ["chat", "completions"] {
                let config =
                    cfg(&json!({"surface": surface, "structured_outputs": structured}).to_string());
                let body = body_for(&simple_request(MODEL), &config);
                assert_eq!(body["structured_outputs"], structured, "{surface}");
            }
        }
    }

    #[test]
    fn structured_outputs_must_be_a_non_empty_object() {
        for written in ["{}", "[]", "\"choice\"", "null", "1"] {
            assert_eq!(
                config_refusal(&format!(r#"{{"structured_outputs":{written}}}"#)),
                format!(
                    "driver: inference.driver.config 'structured_outputs' must be a non-empty \
                     object, got {written}"
                )
            );
        }
    }

    #[test]
    fn structured_outputs_choice_must_be_a_non_empty_array_of_strings() {
        for written in ["[]", "[\"A\",1]", "\"A\"", "null", "[[\"A\"]]"] {
            assert_eq!(
                config_refusal(&format!(
                    r#"{{"structured_outputs":{{"choice":{written}}}}}"#
                )),
                format!(
                    "driver: inference.driver.config 'structured_outputs.choice' must be a \
                     non-empty array of strings, got {written}"
                )
            );
        }
    }

    #[test]
    fn unrecognised_config_keys_are_reported_sorted_with_the_accepted_set() {
        let message = config_refusal(
            r#"{"zeta":1,"reasoning_effort":"low","surface":"chat","extra_body":{},
                "guided_choice":["A"],"temperature":1}"#,
        );
        assert_eq!(
            message,
            "driver: unrecognised inference.driver.config key(s): extra_body, guided_choice, \
             reasoning_effort, zeta. Accepted keys: structured_outputs, surface, temperature"
        );
        for key in ["extra_body", "guided_choice", "reasoning_effort"] {
            assert_eq!(
                config_refusal(&format!(r#"{{"{key}":1}}"#)),
                format!(
                    "driver: unrecognised inference.driver.config key(s): {key}. Accepted keys: \
                     structured_outputs, surface, temperature"
                )
            );
        }
    }

    #[test]
    fn a_config_that_is_not_json_or_not_an_object_is_refused() {
        assert!(config_refusal("{surface: chat}")
            .starts_with("driver: inference.driver.config is not valid JSON: "));
        for not_object in ["[]", "\"chat\"", "1", "null"] {
            assert_eq!(
                config_refusal(not_object),
                "driver: inference.driver.config must be a JSON object"
            );
        }
    }

    // ── Model id ──────────────────────────────────────────────────────────────

    const EMPTY_MODEL: &str = "driver: inference.model is empty: driver-openai-compatible sends \
                               the model id to the server unchanged, so it must be an id the \
                               server serves (its GET /models lists them)";

    #[test]
    fn an_empty_or_whitespace_only_model_is_refused_before_any_body() {
        for model in ["", " ", "\t\n "] {
            for config in [DriverConfig::default(), completions()] {
                assert_eq!(refusal(&simple_request(model), &config), EMPTY_MODEL);
            }
        }
    }

    #[test]
    fn a_model_id_with_slash_colon_and_spaces_is_sent_unchanged_on_both_surfaces() {
        for model in [
            MODEL,
            " padded id ",
            "UPPER/lower:Tag",
            "llama31-8b-instruct",
        ] {
            for config in [DriverConfig::default(), completions()] {
                let body = body_for(&simple_request(model), &config);
                assert_eq!(
                    body["model"].as_str().map(str::as_bytes),
                    Some(model.as_bytes())
                );
            }
        }
    }

    #[test]
    fn max_tokens_is_sent_as_the_envelope_gives_it_with_no_ceiling() {
        for max_tokens in [1u64, 4, 1_000_000, u64::from(u32::MAX) + 7] {
            let mut request = simple_request(MODEL);
            request.max_tokens = max_tokens;
            for config in [DriverConfig::default(), completions()] {
                assert_eq!(body_for(&request, &config)["max_tokens"], json!(max_tokens));
            }
        }
    }

    #[test]
    fn the_crate_defines_no_model_table() {
        let source = include_str!("lib.rs");
        for table in [
            concat!("SUPPORTED_", "MODELS"),
            concat!("MODEL_", "ROUTES"),
            concat!("fn validate_", "model"),
        ] {
            assert!(!source.contains(table), "lib.rs must not define {table}");
        }
    }

    // ── Pre-flight ────────────────────────────────────────────────────────────

    fn image_request(model: &str) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": [{"type": "text", "text": "x"}]}
            ]
        }))
    }

    #[test]
    fn an_image_block_is_refused_before_any_body_is_built() {
        for config in [DriverConfig::default(), completions()] {
            assert_eq!(
                refusal(&image_request(MODEL), &config),
                "driver: driver-openai-compatible sends text only and refuses an image block"
            );
        }
    }

    #[test]
    fn preflight_checks_run_model_then_image_then_surface_history() {
        // An empty model with an image and a tool result: the model is named first.
        assert_eq!(refusal(&image_request(""), &completions()), EMPTY_MODEL);
        // An image and a tool result on completions: the image is named first.
        assert!(refusal(&image_request(MODEL), &completions()).contains("image block"));
    }

    #[test]
    fn a_preflight_refusal_is_a_plain_failure() {
        let failure = Failure::from(refusal(&simple_request(""), &DriverConfig::default()));
        assert_eq!(failure.retry, None);
        assert!(failure.metadata().is_empty());
    }

    #[test]
    fn malformed_messages_are_refused_by_name() {
        let missing_id = request_from(json!({
            "model": MODEL, "max_tokens": 8,
            "messages": [{"role": "tool", "content": [{"type": "text", "text": "x"}]}]
        }));
        assert_eq!(
            refusal(&missing_id, &DriverConfig::default()),
            "driver: tool message is missing required field 'tool_call_id'"
        );
        let odd_role = request_from(json!({
            "model": MODEL, "max_tokens": 8,
            "messages": [{"role": "developer", "content": [{"type": "text", "text": "x"}]}]
        }));
        for config in [DriverConfig::default(), completions()] {
            assert_eq!(
                refusal(&odd_role, &config),
                "driver: unsupported message role 'developer'"
            );
        }
    }

    // ── Chat body ─────────────────────────────────────────────────────────────

    fn round_trip_request() -> MurmurRequest {
        request_from(json!({
            "model": MODEL,
            "max_tokens": 2048,
            "system": "  Be brief.  ",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Weather in Paris?"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "I should call get_weather."},
                    {"type": "tool_call", "id": "call_1", "name": "get_weather", "input": {"city": "Paris"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "18C, sunny"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "Done."},
                    {"type": "text", "text": "It is 18C."},
                    {"type": "tool_call", "id": "call_2", "name": "read_file", "input": {}}
                ]},
                {"role": "tool", "tool_call_id": "call_2", "content": [{"type": "text", "text": "ok"}]}
            ],
            "tools": [
                {"name": "get_weather", "description": " Weather for a city. ",
                 "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}},
                {"name": "read_file", "description": "   ",
                 "parameters": {"type": "object"}},
                {"name": "session-status"}
            ],
            "params": {},
            "prompt_cache_key": "capsule:0.1.0:ctx_01"
        }))
    }

    #[test]
    fn a_tool_round_trip_request_becomes_the_chat_completions_body() {
        let body = body_for(&round_trip_request(), &DriverConfig::default());
        assert_eq!(
            keys(&body),
            [
                "max_tokens",
                "messages",
                "model",
                "stream",
                "stream_options",
                "tools"
            ]
        );
        assert_eq!(body["model"], json!(MODEL));
        assert_eq!(body["max_tokens"], json!(2048));
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "Weather in Paris?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "18C, sunny"},
                {"role": "assistant", "content": "It is 18C.", "tool_calls": [
                    {"id": "call_2", "type": "function",
                     "function": {"name": "read_file", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_2", "content": "ok"}
            ])
        );
        assert_eq!(
            body["tools"],
            json!([
                {"type": "function", "function": {
                    "name": "get_weather",
                    "description": "Weather for a city.",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }},
                {"type": "function", "function": {
                    "name": "read_file", "parameters": {"type": "object"}
                }},
                {"type": "function", "function": {
                    "name": "session-status", "parameters": {}
                }}
            ])
        );
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_no_forbidden_key(&body);
    }

    #[test]
    fn a_thinking_block_is_never_sent_back() {
        let request = request_from(json!({
            "model": MODEL, "max_tokens": 8,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "SECRET-REASONING"},
                    {"type": "text", "text": "a"}
                ]},
                {"role": "user", "content": [{"type": "text", "text": "q2"}]}
            ]
        }));
        for config in [DriverConfig::default(), completions()] {
            let body = body_for(&request, &config);
            assert!(!body.to_string().contains("SECRET-REASONING"), "{body}");
            assert_no_forbidden_key(&body);
        }
        let chat = body_for(&request, &DriverConfig::default());
        assert_eq!(
            chat["messages"][1],
            json!({"role": "assistant", "content": "a"})
        );
    }

    #[test]
    fn a_request_without_tools_sends_no_tools_member() {
        let body = body_for(&simple_request(MODEL), &DriverConfig::default());
        assert!(body.get("tools").is_none(), "{body}");
    }

    #[test]
    fn a_chat_body_with_every_config_key_carries_exactly_its_members() {
        let config = cfg(
            r#"{"surface":"chat","temperature":1,"structured_outputs":{"choice":["accept","reject"]}}"#,
        );
        let body = body_for(&round_trip_request(), &config);
        assert_eq!(
            keys(&body),
            [
                "max_tokens",
                "messages",
                "model",
                "stream",
                "stream_options",
                "structured_outputs",
                "temperature",
                "tools"
            ]
        );
        assert_eq!(
            body["structured_outputs"],
            json!({"choice": ["accept", "reject"]})
        );
        assert!(
            body.to_string().contains("\"temperature\":1,")
                || body.to_string().contains("\"temperature\":1}")
        );
    }

    // ── Completions body ──────────────────────────────────────────────────────

    fn minitaur_request() -> MurmurRequest {
        request_from(json!({
            "model": "minitaur-8b",
            "max_tokens": 4,
            "system": "\n  You are player two.  \n",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "  Offer: 3 of 10."},
                    {"type": "text", "text": "You choose option <<"}
                ]}
            ],
            "tools": [{"name": "session-status", "parameters": {"type": "object"}}],
            "params": {},
            "prompt_cache_key": "capsule:0.1.0:ctx_01"
        }))
    }

    #[test]
    fn the_completions_body_carries_exactly_its_members_and_a_prompt_ending_in_the_task_text() {
        let config = cfg(
            r#"{"surface":"completions","temperature":1,"structured_outputs":{"choice":["A","B"]}}"#,
        );
        let body = body_for(&minitaur_request(), &config);
        assert_eq!(
            keys(&body),
            [
                "max_tokens",
                "model",
                "prompt",
                "stream",
                "stream_options",
                "structured_outputs",
                "temperature"
            ],
            "no tools, no messages: {body}"
        );
        assert_eq!(body["model"], json!("minitaur-8b"));
        assert_eq!(body["max_tokens"], json!(4));
        assert_eq!(
            body["prompt"],
            json!("You are player two.\n\n  Offer: 3 of 10.\nYou choose option <<")
        );
        assert!(body["prompt"].as_str().unwrap().ends_with("<<"));
        assert_eq!(body["structured_outputs"], json!({"choice": ["A", "B"]}));
        assert_eq!(body["temperature"], json!(1));
        assert!(body.to_string().contains("\"temperature\":1"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_no_forbidden_key(&body);
    }

    #[test]
    fn completions_with_tools_but_a_plain_history_sends_no_tools() {
        let body = body_for(&minitaur_request(), &completions());
        assert!(body.get("tools").is_none(), "{body}");
        assert!(!body.to_string().contains("session-status"), "{body}");
    }

    #[test]
    fn the_completions_prompt_skips_empty_texts_and_thinking_and_keeps_message_text_untrimmed() {
        let request = request_from(json!({
            "model": MODEL, "max_tokens": 8,
            "system": "   ",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "first  "}]},
                {"role": "assistant", "content": [{"type": "thinking", "text": "hidden"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "hidden"},
                    {"type": "text", "text": "second"}
                ]},
                {"role": "user", "content": []},
                {"role": "user", "content": [{"type": "text", "text": "third <<"}]}
            ]
        }));
        let body = body_for(&request, &completions());
        assert_eq!(body["prompt"], json!("first  \n\nsecond\n\nthird <<"));
    }

    #[test]
    fn completions_refuses_a_tool_call_and_a_tool_result_by_message_index() {
        let with_call = request_from(json!({
            "model": MODEL, "max_tokens": 8,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_call", "id": "c1", "name": "probe", "input": {}}
                ]}
            ]
        }));
        assert_eq!(
            refusal(&with_call, &completions()),
            "driver: the completions surface carries no tool calls, but message 1 holds a tool \
             call; use surface: chat for a model that calls tools"
        );
        let with_result = request_from(json!({
            "model": MODEL, "max_tokens": 8,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "tool", "tool_call_id": "c1", "content": [{"type": "text", "text": "r"}]}
            ]
        }));
        assert_eq!(
            refusal(&with_result, &completions()),
            "driver: the completions surface carries no tool calls, but message 2 holds a tool \
             result; use surface: chat for a model that calls tools"
        );
        // The same histories are fine on chat.
        assert!(build_provider_request(&with_call, &DriverConfig::default()).is_ok());
        assert!(build_provider_request(&with_result, &DriverConfig::default()).is_ok());
    }

    // ── params ────────────────────────────────────────────────────────────────

    #[test]
    fn stream_flags_in_params_are_overridden_on_both_surfaces() {
        let mut request = simple_request(MODEL);
        request.params = serde_json::from_value(json!({
            "stream": false,
            "stream_options": {"include_usage": false},
            "top_p": 0.9
        }))
        .unwrap();
        for config in [DriverConfig::default(), completions()] {
            let body = body_for(&request, &config);
            assert_eq!(body["stream"], json!(true));
            assert_eq!(body["stream_options"], json!({"include_usage": true}));
            assert_eq!(
                body["top_p"],
                json!(0.9),
                "an unclaimed param passes through"
            );
        }
    }

    #[test]
    fn a_params_key_the_body_already_holds_does_not_override_it() {
        let mut request = simple_request(MODEL);
        request.params = serde_json::from_value(json!({
            "model": "other",
            "max_tokens": 1,
            "messages": [],
            "prompt": "other",
            "temperature": 0.2,
            "structured_outputs": {"regex": "x"}
        }))
        .unwrap();
        let configured = r#"{"temperature":1,"structured_outputs":{"choice":["A","B"]}}"#;
        let chat = body_for(&request, &cfg(configured));
        assert_eq!(chat["model"], json!(MODEL));
        assert_eq!(chat["max_tokens"], json!(1024));
        assert_eq!(chat["messages"][0]["content"], json!("hello"));
        assert_eq!(chat["temperature"], json!(1));
        assert_eq!(chat["structured_outputs"], json!({"choice": ["A", "B"]}));
        assert_eq!(
            chat["prompt"],
            json!("other"),
            "chat holds no prompt of its own"
        );

        let completion_config = cfg(
            r#"{"surface":"completions","temperature":1,"structured_outputs":{"choice":["A","B"]}}"#,
        );
        let completion = body_for(&request, &completion_config);
        assert_eq!(completion["prompt"], json!("hello"));
        assert_eq!(completion["model"], json!(MODEL));
        assert_eq!(completion["temperature"], json!(1));
    }

    #[test]
    fn prompt_cache_key_reaches_no_body() {
        for (request, config) in [
            (round_trip_request(), DriverConfig::default()),
            (minitaur_request(), completions()),
        ] {
            let body = body_for(&request, &config);
            assert!(!contains_key_anywhere(&body, "prompt_cache_key"), "{body}");
            assert!(!body.to_string().contains("ctx_01"), "{body}");
        }
    }

    #[test]
    fn request_headers_are_content_type_and_content_length_only() {
        assert_eq!(
            request_headers(42),
            vec![
                ("content-type", "application/json".to_string()),
                ("content-length", "42".to_string()),
            ]
        );
    }

    #[test]
    fn request_path_is_everything_after_the_authority() {
        for (url, path) in [
            (
                "http://gateway:9/v1/chat/completions",
                "/v1/chat/completions",
            ),
            ("https://host.example/v1/completions", "/v1/completions"),
            ("http://gateway:9/chat/completions", "/chat/completions"),
            ("http://gateway:9", "/"),
        ] {
            assert_eq!(request_path(url), path, "{url}");
        }
    }

    // ── Chat streaming ────────────────────────────────────────────────────────

    #[test]
    fn streamed_reasoning_and_text_go_to_separate_emitters_in_order() {
        let stream = format!(
            "{}{}{}{}{}{DONE}",
            chunk(json!({"role": "assistant", "reasoning": "Think "})),
            chunk(json!({"reasoning": "hard."})),
            chunk(json!({"content": "Hello "})),
            chunk(json!({"content": "world"})),
            finish("stop"),
        );
        let mut text = Vec::new();
        let mut thinking = Vec::new();
        let response = parse_sse_body(
            &stream,
            Surface::Chat,
            &mut |t| text.push(t.to_string()),
            &mut |t| thinking.push(t.to_string()),
        )
        .unwrap();
        assert_eq!(text, ["Hello ", "world"]);
        assert_eq!(thinking, ["Think ", "hard."]);
        assert_eq!(
            response,
            json!({"stop_reason": "end_turn", "content": [
                {"type": "thinking", "text": "Think hard."},
                {"type": "text", "text": "Hello world"}
            ]})
        );
    }

    #[test]
    fn reasoning_content_is_read_when_a_server_uses_it() {
        let stream = format!(
            "{}{}{}{DONE}",
            chunk(json!({"reasoning_content": "Older field."})),
            chunk(json!({"content": "ok"})),
            finish("stop"),
        );
        let mut thinking = String::new();
        let response = parse_sse_body(&stream, Surface::Chat, &mut |_| {}, &mut |t| {
            thinking.push_str(t)
        })
        .unwrap();
        assert_eq!(thinking, "Older field.");
        assert_eq!(
            response["content"][0],
            json!({"type": "thinking", "text": "Older field."})
        );
    }

    #[test]
    fn a_chunk_carrying_both_reasoning_fields_is_read_from_reasoning_only() {
        let stream = format!(
            "{}{}{DONE}",
            chunk(json!({"reasoning": "A", "reasoning_content": "A"})),
            finish("stop"),
        );
        let mut thinking = String::new();
        let response = parse_sse_body(&stream, Surface::Chat, &mut |_| {}, &mut |t| {
            thinking.push_str(t)
        })
        .unwrap();
        assert_eq!(thinking, "A", "not doubled");
        assert_eq!(
            response["content"],
            json!([{"type": "thinking", "text": "A"}])
        );
    }

    #[test]
    fn usage_from_a_choices_empty_chunk_is_merged_and_the_last_report_wins() {
        let stream = format!(
            "{}data: {}\n{}{}{DONE}",
            chunk(json!({"content": "x"})),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": null}],
                   "usage": {"prompt_tokens": 5, "completion_tokens": 1}}),
            finish("stop"),
            usage_chunk(json!({
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 64},
                "completion_tokens_details": {"reasoning_tokens": 7}
            })),
        );
        let response = parse_quiet(&stream, Surface::Chat).unwrap();
        assert_eq!(
            response["usage"],
            json!({"input_tokens": 36, "output_tokens": 20, "cached_tokens": 64, "thinking_tokens": 7})
        );
    }

    fn fragmented_tool_call_stream() -> String {
        format!(
            "{}{}{}{}{}{DONE}",
            chunk(json!({"content": "Let me check."})),
            tool_delta(0, Some("call_w"), Some("get_weather"), ""),
            tool_delta(0, None, None, "{\"city\":"),
            tool_delta(0, None, None, "\"Paris\"}"),
            finish("tool_calls"),
        )
    }

    #[test]
    fn a_fragmented_tool_call_parses_and_signals_its_start_then_rising_byte_totals() {
        let (response, signals) = parse_with_signals(&fragmented_tool_call_stream());
        assert_eq!(
            response,
            json!({"stop_reason": "tool_call", "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_call", "id": "call_w", "name": "get_weather", "input": {"city": "Paris"}}
            ]})
        );
        assert_eq!(
            signals,
            [
                started("call_w", "get_weather"),
                input_bytes("call_w", 8),
                input_bytes("call_w", 16),
            ]
        );
    }

    #[test]
    fn a_tool_call_delivered_whole_in_one_chunk_parses() {
        let stream = format!(
            "{}{}{DONE}",
            tool_delta(0, Some("call_1"), Some("probe"), "{\"cmd\":\"ls\"}"),
            finish("tool_calls"),
        );
        let (response, signals) = parse_with_signals(&stream);
        assert_eq!(
            response["content"],
            json!([{"type": "tool_call", "id": "call_1", "name": "probe", "input": {"cmd": "ls"}}])
        );
        assert_eq!(
            signals,
            [started("call_1", "probe"), input_bytes("call_1", 12)]
        );
    }

    #[test]
    fn tool_calls_at_two_indexes_keep_separate_ids_and_totals() {
        let stream = format!(
            "{}{}{}{DONE}",
            tool_delta(0, Some("a"), Some("one"), "{}"),
            tool_delta(1, Some("b"), Some("two"), "{\"k\":1}"),
            finish("tool_calls"),
        );
        let (response, signals) = parse_with_signals(&stream);
        assert_eq!(response["content"][0]["id"], json!("a"));
        assert_eq!(response["content"][1]["input"], json!({"k": 1}));
        assert_eq!(
            signals,
            [
                started("a", "one"),
                input_bytes("a", 2),
                started("b", "two"),
                input_bytes("b", 7),
            ]
        );
    }

    #[test]
    fn stop_beside_returned_tool_calls_is_a_tool_call_on_both_paths() {
        let stream = format!(
            "{}{}{DONE}",
            tool_delta(0, Some("call_1"), Some("probe"), "{}"),
            finish("stop"),
        );
        assert_eq!(
            parse_quiet(&stream, Surface::Chat).unwrap()["stop_reason"],
            json!("tool_call")
        );
        let buffered = json!({"choices": [{"finish_reason": "stop", "message": {
            "content": null,
            "tool_calls": [{"id": "call_1", "type": "function",
                            "function": {"name": "probe", "arguments": "{}"}}]
        }}]});
        let response = translate_json_response(&buffered, Surface::Chat).unwrap();
        assert_eq!(response["stop_reason"], json!("tool_call"));
        assert_eq!(response["content"][0]["name"], json!("probe"));
    }

    #[test]
    fn a_tool_call_index_past_the_limit_fails_the_turn_plainly() {
        let stream = format!(
            "{}{}{DONE}",
            tool_delta(1024, Some("x"), Some("probe"), "{}"),
            finish("tool_calls"),
        );
        let (result, signals) =
            parse_sse_body_with_signals(&stream, Surface::Chat, &mut |_| {}, &mut |_| {});
        assert_plain(
            result,
            "driver: tool call index 1024 is past the limit of 1024 per turn",
        );
        assert!(signals.is_empty(), "nothing is reported for it");
    }

    #[test]
    fn a_tool_call_at_the_last_index_under_the_limit_is_kept() {
        let stream = format!(
            "{}{}{DONE}",
            tool_delta(1023, Some("last"), Some("probe"), "{}"),
            finish("tool_calls"),
        );
        let response = parse_quiet(&stream, Surface::Chat).unwrap();
        assert_eq!(
            response["content"],
            json!([{"type": "tool_call", "id": "last", "name": "probe", "input": {}}])
        );
    }

    #[test]
    fn non_json_tool_arguments_are_a_plain_error_on_both_paths() {
        let stream = format!(
            "{}{}{DONE}",
            tool_delta(0, Some("c"), Some("probe"), "{not json"),
            finish("tool_calls"),
        );
        let streamed = parse_quiet(&stream, Surface::Chat).unwrap_err();
        assert!(streamed
            .message
            .starts_with("driver: failed to parse tool call arguments JSON: "));
        assert_eq!(streamed.retry, None);

        let buffered = json!({"choices": [{"finish_reason": "tool_calls", "message": {
            "tool_calls": [{"id": "c", "function": {"name": "probe", "arguments": "{not json"}}]
        }}]});
        let failure = translate_json_response(&buffered, Surface::Chat).unwrap_err();
        assert!(failure
            .message
            .starts_with("driver: failed to parse tool call arguments JSON: "));
        assert_eq!(failure.retry, None);
    }

    #[test]
    fn the_chat_json_fallback_reads_reasoning_text_and_calls_in_order() {
        for field in ["reasoning", "reasoning_content"] {
            let mut message = json!({
                "content": "Calling.",
                "tool_calls": [{"id": "c", "type": "function",
                                "function": {"name": "probe", "arguments": ""}}]
            });
            message[field] = json!("Because.");
            let response = translate_json_response(
                &json!({"choices": [{"finish_reason": "tool_calls", "message": message}],
                        "usage": {"prompt_tokens": 9, "completion_tokens": 3}}),
                Surface::Chat,
            )
            .unwrap();
            assert_eq!(
                response,
                json!({"stop_reason": "tool_call", "content": [
                    {"type": "thinking", "text": "Because."},
                    {"type": "text", "text": "Calling."},
                    {"type": "tool_call", "id": "c", "name": "probe", "input": {}}
                ], "usage": {"input_tokens": 9, "output_tokens": 3}}),
                "{field}"
            );
        }
    }

    // ── Completions streaming ─────────────────────────────────────────────────

    #[test]
    fn a_completions_stream_parses_to_text_and_usage() {
        let stream = format!(
            "{}{}{}{DONE}",
            text_chunk("A", None),
            text_chunk("", Some("stop")),
            usage_chunk(json!({"prompt_tokens": 40, "completion_tokens": 1, "total_tokens": 41})),
        );
        let mut text = String::new();
        let mut thinking = String::new();
        let response = parse_sse_body(
            &stream,
            Surface::Completions,
            &mut |t| text.push_str(t),
            &mut |t| thinking.push_str(t),
        )
        .unwrap();
        assert_eq!(text, "A");
        assert!(thinking.is_empty());
        assert_eq!(
            response,
            json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "A"}],
                   "usage": {"input_tokens": 40, "output_tokens": 1}})
        );
    }

    #[test]
    fn the_completions_surface_reads_no_reasoning_or_tool_calls_from_a_chunk() {
        let stream = format!(
            "data: {}\n{}{DONE}",
            json!({"choices": [{"index": 0, "text": "B",
                "delta": {"reasoning": "no", "content": "no",
                          "tool_calls": [{"index": 0, "id": "x", "function": {"name": "y"}}]}}]}),
            text_chunk("", Some("length")),
        );
        let (result, signals) =
            parse_sse_body_with_signals(&stream, Surface::Completions, &mut |_| {}, &mut |_| {});
        assert_eq!(
            result.unwrap(),
            json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "B"}]})
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn the_completions_json_fallback_reads_choices_text() {
        let response = translate_json_response(
            &json!({"object": "text_completion",
                    "choices": [{"index": 0, "text": "A", "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 1,
                              "prompt_tokens_details": {"cached_tokens": 8}}}),
            Surface::Completions,
        )
        .unwrap();
        assert_eq!(
            response,
            json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "A"}],
                   "usage": {"input_tokens": 4, "output_tokens": 1, "cached_tokens": 8}})
        );
    }

    #[test]
    fn tool_calls_as_a_finish_reason_is_unsupported_on_completions() {
        let stream = complete_stream(Surface::Completions, "tool_calls");
        assert_plain(
            parse_quiet(&stream, Surface::Completions),
            "driver: unsupported finish_reason 'tool_calls'",
        );
        assert_plain(
            translate_json_response(
                &json!({"choices": [{"text": "", "finish_reason": "tool_calls"}]}),
                Surface::Completions,
            ),
            "driver: unsupported finish_reason 'tool_calls'",
        );
    }

    // ── Stop reasons ──────────────────────────────────────────────────────────

    #[test]
    fn stop_length_and_tool_calls_map_to_the_runtime_stop_reasons_on_both_paths() {
        for (surface, reason, expected) in [
            (Surface::Chat, "stop", "end_turn"),
            (Surface::Chat, "length", "max_tokens"),
            (Surface::Chat, "tool_calls", "tool_call"),
            (Surface::Completions, "stop", "end_turn"),
            (Surface::Completions, "length", "max_tokens"),
        ] {
            let streamed = parse_quiet(&complete_stream(surface, reason), surface).unwrap();
            assert_eq!(
                streamed["stop_reason"],
                json!(expected),
                "{surface:?} {reason}"
            );
            let buffered = translate_json_response(
                &json!({"choices": [{"text": "hi", "message": {"content": "hi"},
                                     "finish_reason": reason}]}),
                surface,
            )
            .unwrap();
            assert_eq!(
                buffered["stop_reason"],
                json!(expected),
                "{surface:?} {reason}"
            );
        }
    }

    #[test]
    fn every_other_finish_reason_is_a_plain_error_naming_it_on_both_paths() {
        for surface in SURFACES {
            for reason in ["repetition", "content_filter", "STOP", "function_call"] {
                let message = format!("driver: unsupported finish_reason '{reason}'");
                assert_plain(
                    parse_quiet(&complete_stream(surface, reason), surface),
                    &message,
                );
                assert_plain(
                    translate_json_response(
                        &json!({"choices": [{"text": "", "finish_reason": reason}]}),
                        surface,
                    ),
                    &message,
                );
            }
        }
    }

    #[test]
    fn map_finish_reason_reads_stop_beside_calls_only_on_chat() {
        assert_eq!(
            map_finish_reason("stop", Surface::Chat, true).unwrap(),
            "tool_call"
        );
        assert_eq!(
            map_finish_reason("stop", Surface::Chat, false).unwrap(),
            "end_turn"
        );
        assert_eq!(
            map_finish_reason("stop", Surface::Completions, true).unwrap(),
            "end_turn"
        );
    }

    // ── Transient and plain failures ──────────────────────────────────────────

    #[test]
    fn a_stream_without_done_fails_transiently_even_after_a_finish_reason() {
        for surface in SURFACES {
            let full = complete_stream(surface, "stop");
            let cut = full.strip_suffix(DONE).unwrap();
            assert_transient(
                parse_quiet(cut, surface),
                "driver: stream ended before data: [DONE]",
            );
            // An empty 2xx body and a body of comments only were cut too.
            assert_transient(
                parse_quiet("", surface),
                "driver: stream ended before data: [DONE]",
            );
            assert_transient(
                parse_quiet(": keep-alive\n\n", surface),
                "driver: stream ended before data: [DONE]",
            );
        }
    }

    #[test]
    fn done_with_no_finish_reason_fails_transiently() {
        for surface in SURFACES {
            let body = match surface {
                Surface::Chat => format!("{}{DONE}", chunk(json!({"content": "partial"}))),
                Surface::Completions => format!("{}{DONE}", text_chunk("partial", None)),
            };
            assert_transient(
                parse_quiet(&body, surface),
                "driver: stream ended with no finish_reason",
            );
            assert_transient(
                parse_quiet(DONE, surface),
                "driver: stream ended with no finish_reason",
            );
        }
    }

    #[test]
    fn a_last_done_line_with_no_trailing_newline_still_ends_the_stream() {
        let body = complete_stream(Surface::Chat, "stop");
        let body = body.trim_end_matches('\n');
        assert_eq!(
            parse_quiet(body, Surface::Chat).unwrap()["stop_reason"],
            json!("end_turn")
        );
    }

    #[test]
    fn abort_is_transient_on_both_surfaces_and_both_paths() {
        let message = "driver: the server aborted the turn (finish_reason 'abort')";
        for surface in SURFACES {
            assert_transient(
                parse_quiet(&complete_stream(surface, "abort"), surface),
                message,
            );
            assert_transient(
                translate_json_response(
                    &json!({"choices": [{"text": "", "message": {}, "finish_reason": "abort"}]}),
                    surface,
                ),
                message,
            );
        }
    }

    fn error_chunk(chunk: Value) -> String {
        format!("data: {chunk}\n")
    }

    #[test]
    fn an_error_chunk_naming_a_status_murmur_retries_fails_transiently() {
        for surface in SURFACES {
            for (chunk_value, error) in [
                (
                    json!({"error": {"message": "overloaded", "type": "ServiceUnavailableError", "param": null, "code": 503}}),
                    json!({"message": "overloaded", "type": "ServiceUnavailableError", "param": null, "code": 503}),
                ),
                (
                    json!({"object": "error", "message": "busy", "type": "InternalServerError", "param": null, "code": 500}),
                    json!({"object": "error", "message": "busy", "type": "InternalServerError", "param": null, "code": 500}),
                ),
                (
                    json!({"error": {"status": "429"}}),
                    json!({"status": "429"}),
                ),
                (
                    json!({"error": {"status_code": 408}}),
                    json!({"status_code": 408}),
                ),
                (json!({"error": {"code": 502}}), json!({"code": 502})),
            ] {
                let body = format!(
                    "{}{}{DONE}",
                    match surface {
                        Surface::Chat => chunk(json!({"content": "partial"})),
                        Surface::Completions => text_chunk("partial", None),
                    },
                    error_chunk(chunk_value),
                );
                assert_transient(
                    parse_quiet(&body, surface),
                    &format!("driver: server stream error: {error}"),
                );
            }
        }
    }

    #[test]
    fn every_other_error_chunk_fails_plainly() {
        for surface in SURFACES {
            for error in [
                json!({"message": "bad request", "code": 400}),
                json!({"code": 501}),
                json!({"code": 505}),
                json!({"message": "no status"}),
                json!({"code": "server_error"}),
                json!("a bare string"),
            ] {
                let body = format!("{}{DONE}", error_chunk(json!({"error": error})));
                assert_plain(
                    parse_quiet(&body, surface),
                    &format!("driver: server stream error: {error}"),
                );
            }
        }
    }

    #[test]
    fn a_null_error_member_is_not_an_error_chunk() {
        let body = format!(
            "data: {}\n{}{DONE}",
            json!({"error": null, "choices": [{"index": 0, "delta": {"content": "ok"}}]}),
            finish("stop")
        );
        assert_eq!(
            parse_quiet(&body, Surface::Chat).unwrap()["content"],
            json!([{"type": "text", "text": "ok"}])
        );
    }

    #[test]
    fn an_error_chunk_takes_precedence_over_a_later_finish_reason() {
        let body = format!(
            "{}{}{DONE}",
            error_chunk(json!({"error": {"code": 400, "message": "nope"}})),
            finish("stop")
        );
        assert_plain(
            parse_quiet(&body, Surface::Chat),
            r#"driver: server stream error: {"code":400,"message":"nope"}"#,
        );
    }

    #[test]
    fn a_2xx_json_body_that_is_an_error_fails_by_the_same_status_rule() {
        for surface in SURFACES {
            assert_transient(
                translate_json_response(
                    &json!({"error": {"code": 503, "message": "down"}}),
                    surface,
                ),
                r#"driver: server error: {"code":503,"message":"down"}"#,
            );
            let flat = json!({"object": "error", "message": "bad", "code": 400});
            assert_plain(
                translate_json_response(&flat, surface),
                &format!("driver: server error: {flat}"),
            );
        }
    }

    #[test]
    fn a_failed_body_read_after_a_2xx_head_is_transient_and_after_an_error_status_plain() {
        // `read_chunk` in the guest maps a failed read through this function.
        let after_ok = murmur_driver_stream::read_failure(200, "LastOperationFailed");
        assert_eq!(
            after_ok.message,
            "driver: failed to read response stream: LastOperationFailed"
        );
        assert_eq!(after_ok.retry, Some(Retry::Transient));
        assert_eq!(
            murmur_driver_stream::read_failure(500, "LastOperationFailed").retry,
            None
        );
    }

    #[test]
    fn a_response_with_no_choices_or_no_finish_reason_is_refused_plainly() {
        for surface in SURFACES {
            for body in [json!({}), json!({"choices": []}), json!({"choices": "x"})] {
                assert_plain(
                    translate_json_response(&body, surface),
                    "driver: response missing choices[0]",
                );
            }
            for choice in [
                json!({"text": "A", "message": {"content": "A"}}),
                json!({"text": "A", "finish_reason": null}),
                json!({"text": "A", "finish_reason": ""}),
                json!({"text": "A", "finish_reason": 1}),
            ] {
                assert_plain(
                    translate_json_response(&json!({"choices": [choice]}), surface),
                    "driver: response has no finish_reason",
                );
            }
        }
    }

    #[test]
    fn a_completed_turn_carries_no_transient_marker() {
        for surface in SURFACES {
            let response = parse_quiet(&complete_stream(surface, "stop"), surface).unwrap();
            assert_eq!(response["stop_reason"], json!("end_turn"));
            assert!(!contains_key_anywhere(&response, "transient"));
        }
    }

    #[test]
    fn nothing_is_ever_marked_permanent() {
        let source = include_str!("lib.rs");
        let non_test = &source[..source.find("\nmod tests {").expect("mod tests must exist")];
        assert!(!non_test.contains(concat!("Failure::", "permanent")));
        assert!(!non_test.contains(concat!("Retry::", "Permanent")));
        assert!(
            !non_test.contains(concat!("\"trans", "ient\"")),
            "the tuple comes from Failure::metadata only"
        );
    }

    // ── Status failures ───────────────────────────────────────────────────────

    const CHAT_PATH: &str = "/v1/chat/completions";

    fn model_refusal(model: &str, status: u16, body: &str) -> String {
        format!(
            "driver: the server refused model '{model}': HTTP {status}: {body} — \
             driver-openai-compatible sends inference.model unchanged; use an id the server's \
             GET /models lists (vLLM: --served-model-name)"
        )
    }

    #[test]
    fn a_wrapped_model_404_names_the_model() {
        let body = r#"{"error":{"message":"The model `llama-3` does not exist.","type":"NotFoundError","param":"model","code":404}}"#;
        let failure = status_failure(404, body, "llama-3", CHAT_PATH);
        assert_eq!(failure.message, model_refusal("llama-3", 404, body));
        assert_eq!(failure.retry, None);
    }

    #[test]
    fn a_flat_model_404_names_the_model() {
        let body = r#"{"object":"error","message":"The model `llama-3` does not exist.","type":"NotFoundError","param":null,"code":404}"#;
        let failure = status_failure(404, body, "llama-3", CHAT_PATH);
        assert_eq!(failure.message, model_refusal("llama-3", 404, body));
        assert_eq!(failure.retry, None);
    }

    #[test]
    fn an_ollama_shaped_model_404_names_the_model() {
        let body = r#"{"error":{"message":"model \"does-not-exist\" not found, try pulling it first","type":"api_error","param":null,"code":null}}"#;
        let failure = status_failure(404, body, "does-not-exist", CHAT_PATH);
        assert_eq!(failure.message, model_refusal("does-not-exist", 404, body));
    }

    #[test]
    fn a_body_ending_in_a_newline_is_trimmed_before_the_hint_that_follows_it() {
        let body =
            "{\"error\":{\"message\":\"model 'x' not found\",\"param\":null,\"code\":null}}\n";
        assert_eq!(
            status_failure(404, body, "x", CHAT_PATH).message,
            model_refusal("x", 404, body.trim_end())
        );
        assert!(status_failure(404, "nope\n", "x", CHAT_PATH)
            .message
            .starts_with("HTTP 404: nope — driver: no route at "));
        assert_eq!(
            status_failure(500, "boom\n", "x", CHAT_PATH).message,
            "HTTP 500: boom\n",
            "the plain form keeps the body as sent"
        );
    }

    #[test]
    fn a_400_with_param_model_names_the_model() {
        let body = r#"{"error":{"message":"bad model","type":"BadRequestError","param":"model","code":400}}"#;
        let failure = status_failure(400, body, MODEL, CHAT_PATH);
        assert_eq!(failure.message, model_refusal(MODEL, 400, body));
        assert_eq!(failure.retry, None);
    }

    #[test]
    fn any_other_404_points_at_the_endpoint_and_names_the_path() {
        for (body, path) in [
            (r#"{"detail":"Not Found"}"#, CHAT_PATH),
            ("404 page not found", "/completions"),
            (
                r#"{"error":{"message":"no such route","type":"NotFoundError","param":null,"code":404}}"#,
                "/v1/completions",
            ),
        ] {
            let failure = status_failure(404, body, "llama-3", path);
            assert_eq!(
                failure.message,
                format!(
                    "HTTP 404: {body} — driver: no route at {path}; gateway.endpoint must be the \
                     server's OpenAI base URL, usually ending in /v1"
                )
            );
            assert_eq!(failure.retry, None);
        }
    }

    #[test]
    fn every_other_status_is_plain_and_carries_the_status_and_body() {
        for (status, body) in [
            (500, r#"{"error":{"message":"boom","code":500}}"#),
            (401, r#"{"error":"Unauthorized"}"#),
            (429, "slow down"),
            (
                400,
                r#"{"error":{"message":"max_tokens too large","param":"max_tokens","code":400}}"#,
            ),
            (503, ""),
        ] {
            let failure = status_failure(status, body, "llama-3", CHAT_PATH);
            assert_eq!(failure.message, format!("HTTP {status}: {body}"));
            assert_eq!(failure.retry, None);
            assert!(failure.metadata().is_empty());
        }
    }

    // ── Usage ─────────────────────────────────────────────────────────────────

    fn both_paths(surface: Surface, usage: Value) -> [Value; 2] {
        let streamed = parse_quiet(
            &format!(
                "{}{}{DONE}",
                complete_stream(surface, "stop").strip_suffix(DONE).unwrap(),
                usage_chunk(usage.clone())
            ),
            surface,
        )
        .unwrap();
        let buffered = translate_json_response(
            &json!({"choices": [{"text": "hi", "message": {"content": "hi"}, "finish_reason": "stop"}],
                    "usage": usage}),
            surface,
        )
        .unwrap();
        [streamed, buffered]
    }

    #[test]
    fn cached_tokens_sit_beside_input_tokens_on_both_surfaces_and_paths() {
        for surface in SURFACES {
            for response in both_paths(
                surface,
                json!({"prompt_tokens": 1000, "completion_tokens": 50,
                       "prompt_tokens_details": {"cached_tokens": 800},
                       "completion_tokens_details": {"reasoning_tokens": 30}}),
            ) {
                assert_eq!(
                    response["usage"],
                    json!({"input_tokens": 200, "output_tokens": 50,
                           "cached_tokens": 800, "thinking_tokens": 30})
                );
            }
        }
    }

    #[test]
    fn an_unreported_cache_count_leaves_cached_tokens_out_and_input_equal_to_the_total() {
        for surface in SURFACES {
            for response in both_paths(
                surface,
                json!({"prompt_tokens": 70, "completion_tokens": 2, "prompt_tokens_details": null}),
            ) {
                assert_eq!(
                    response["usage"],
                    json!({"input_tokens": 70, "output_tokens": 2})
                );
            }
        }
    }

    #[test]
    fn a_cached_count_above_the_total_leaves_input_tokens_out() {
        for response in both_paths(
            Surface::Chat,
            json!({"prompt_tokens": 10, "completion_tokens": 2,
                   "prompt_tokens_details": {"cached_tokens": 11}}),
        ) {
            assert_eq!(
                response["usage"],
                json!({"output_tokens": 2, "cached_tokens": 11})
            );
        }
    }

    #[test]
    fn no_usage_means_no_usage_key() {
        for surface in SURFACES {
            let response = parse_quiet(&complete_stream(surface, "stop"), surface).unwrap();
            assert!(response.get("usage").is_none(), "{response}");
            for response in both_paths(surface, json!({})) {
                assert!(response.get("usage").is_none(), "{response}");
            }
        }
    }

    // ── Line processor and payload shape ──────────────────────────────────────

    #[test]
    fn non_data_lines_and_non_json_payloads_are_skipped() {
        let mut state = StreamState::default();
        for line in ["", ": ping", "event: message", "data: not json", "data:{}"] {
            assert!(!process_sse_line(
                line,
                Surface::Chat,
                &mut state,
                &mut |_| panic!("no text"),
                &mut |_| panic!("no thinking"),
                &mut |_| panic!("no signal"),
            )
            .unwrap());
        }
        assert!(!state.saw_done);
    }

    #[test]
    fn the_error_payload_is_stop_reason_error_with_the_message() {
        assert_eq!(
            error_payload("driver: x"),
            json!({"stop_reason": "error", "error": "driver: x"})
        );
    }

    // ── Source and manifest shape ─────────────────────────────────────────────

    /// The source of the `#[cfg(target_arch = "wasm32")]` module, which a host test cannot
    /// compile but can read. Ends at `mod tests` so needles below cannot match themselves.
    fn wasm_module_source() -> &'static str {
        let source = include_str!("lib.rs");
        let start = source
            .find("\nmod wasm_driver {")
            .expect("mod wasm_driver must exist");
        let end = source.find("\nmod tests {").expect("mod tests must exist");
        &source[start..end]
    }

    fn run_inner_source() -> &'static str {
        let module = wasm_module_source();
        let start = module.find("fn run_inner(").expect("run_inner must exist");
        let end = module[start..]
            .find("\n    fn dispatch_request(")
            .expect("dispatch_request follows run_inner");
        &module[start..start + end]
    }

    #[test]
    fn no_provider_url_is_hardcoded_anywhere_in_the_crate() {
        // Where inference goes is the capsule's fact, handed over by the host. A literal URL in
        // a driver would become silently effective the moment the host stopped supplying one.
        // The needles are assembled rather than written out so that this assertion does not
        // itself become the only occurrence of what it forbids.
        let source = include_str!("lib.rs");
        for provider_url in [
            concat!("https://", "api."),
            concat!("http://", "api."),
            concat!("local", "host"),
            concat!("127.0", ".0.1"),
            concat!("0.0", ".0.0"),
            concat!("vl", "lm"),
            concat!(":80", "00"),
            concat!(":114", "34"),
            concat!("open", "ai.com"),
        ] {
            assert!(
                !source.contains(provider_url),
                "the driver must not name a provider URL ({provider_url}); the endpoint comes \
                 from the host"
            );
        }
    }

    #[test]
    fn config_parse_precedes_every_other_environment_read_in_run_inner() {
        // A rejected config must fail before an HTTP request can be dispatched. `run_inner` is
        // wasm-only, so its statement order is asserted from the source.
        let run_inner = run_inner_source();
        let parse_at = run_inner
            .find("parse_driver_config(")
            .expect("run_inner must parse the driver config");
        for later in [
            "INFERENCE_ENDPOINT_VAR",
            "build_provider_request(",
            "request_headers(",
            "dispatch_request(",
        ] {
            let at = run_inner
                .find(later)
                .unwrap_or_else(|| panic!("run_inner must reference {later}"));
            assert!(
                parse_at < at,
                "the driver config must be parsed before {later}"
            );
        }
        let build_at = run_inner.find("build_provider_request(").unwrap();
        let dispatch_at = run_inner.find("dispatch_request(").unwrap();
        assert!(
            build_at < dispatch_at,
            "every pre-flight refusal must run before the request is dispatched"
        );
        assert_eq!(
            run_inner.matches("dispatch_request(").count(),
            1,
            "exactly one request per call, no internal retry"
        );
    }

    #[test]
    fn run_inner_resolves_the_endpoint_through_the_shared_contract() {
        // One implementation of "a missing endpoint is a refusal" exists, in
        // murmur-driver-env. An inline `std::env::var` read here would be a second one, free
        // to disagree with it. The needle is assembled so that this test is not its own evidence.
        assert!(
            run_inner_source().contains(concat!("require_", "endpoint(")),
            "run_inner must resolve the endpoint through murmur_driver_env::require_endpoint"
        );
    }

    #[test]
    fn run_inner_sends_exactly_the_headers_request_headers_builds() {
        let run_inner = run_inner_source();
        assert!(
            run_inner.contains("let headers = request_headers(body.len());"),
            "run_inner must take its header list from request_headers"
        );
        assert!(
            run_inner.contains("dispatch_request(&url, headers, &body)"),
            "run_inner must dispatch that list unchanged"
        );
        assert!(
            run_inner.contains("format!(\"{}{}\", endpoint.trim_end_matches('/'), surface.path())"),
            "requests go to <endpoint><surface path>"
        );
        let names: Vec<&str> = request_headers(0).iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["content-type", "content-length"]);
    }

    #[test]
    fn run_inner_reports_a_status_failure_with_the_model_and_the_path_sent() {
        let run_inner = run_inner_source();
        let at = run_inner
            .find("status_failure(")
            .expect("run_inner builds the status failure");
        let call = &run_inner[at..at + 200];
        assert!(call.contains("&murmur_request.model"), "{call}");
        assert!(call.contains("request_path(&url)"), "{call}");
    }

    #[test]
    fn run_returns_metadata_only_from_the_failure() {
        let module = wasm_module_source();
        assert!(module.contains("Ok(value) => (value, vec![]),"));
        assert!(module
            .contains("Err(failure) => (error_payload(&failure.message), failure.metadata()),"));
    }

    #[test]
    fn run_inner_declares_the_response_body_before_its_stream() {
        // Locals drop in reverse declaration order, so an early return inside the stream loop
        // releases the stream before the body that owns it; the other order traps the guest.
        let run_inner = run_inner_source();
        let body_at = run_inner
            .find("let incoming_body = response")
            .expect("run_inner must bind the response body");
        let stream_at = run_inner
            .find("let stream = incoming_body")
            .expect("run_inner must bind the body's stream");
        assert!(body_at < stream_at);
        assert!(
            run_inner.contains("if !state.saw_done && !line_buf.is_empty()"),
            "the guest loop must process a last line the body ended without a newline after"
        );
        assert!(
            run_inner.contains("assemble_streaming_response(state, surface)"),
            "the guest loop must hand its whole state, saw_done included, to the assembler"
        );
        assert!(
            run_inner.contains("translate_json_response(&json, surface)"),
            "the JSON fallback reads the configured surface"
        );
    }

    #[test]
    fn wasm_module_pairs_emit_thinking_chunk_with_emit_chunk_at_every_call_site() {
        let module = wasm_module_source();

        let text_sites: Vec<usize> = module
            .match_indices("murmur::stream::events::emit_chunk(")
            .map(|(index, _)| index)
            .collect();
        let thinking_sites: Vec<usize> = module
            .match_indices("murmur::stream::events::emit_thinking_chunk(")
            .map(|(index, _)| index)
            .collect();

        assert!(
            !text_sites.is_empty(),
            "the streaming loop must emit text chunks to the host"
        );
        assert_eq!(
            text_sites.len(),
            thinking_sites.len(),
            "every streaming call site must wire both emitters, or reasoning is dropped"
        );
        for (text_at, thinking_at) in text_sites.iter().zip(thinking_sites.iter()) {
            assert!(
                text_at.abs_diff(*thinking_at) < 400,
                "the two emitters must be wired together at the same call site"
            );
        }
    }

    #[test]
    fn wasm_driver_reports_tool_calls_on_murmur_stream_events_only() {
        let wasm = wasm_module_source();
        for call in [
            "murmur::stream::events::tool_call_started(",
            "murmur::stream::events::tool_call_input_bytes(",
        ] {
            assert!(wasm.contains(call), "mod wasm_driver must call {call}");
        }
        let source = include_str!("lib.rs");
        for retired in [concat!("murmur::", "text"), concat!("murmur:", "text")] {
            assert!(!source.contains(retired), "lib.rs must not name {retired}");
        }
    }

    #[test]
    fn no_reasoning_sentinel_scanner_anywhere_in_the_crate() {
        // Reasoning arrives on its own delta field, so an inline reasoning-tag scanner here would
        // be a second, silently-wrong reasoning path. The needle is assembled rather than written
        // out so that this assertion does not itself become the only occurrence.
        let sentinel = concat!("<", "think");
        assert!(!include_str!("lib.rs").contains(sentinel));
    }

    #[test]
    fn the_driver_neither_reads_a_credential_nor_builds_an_auth_header() {
        // The runtime attaches the header declared under `upstream_auth:`, so a driver-side
        // credential read would put the key back in the guest.
        let source = include_str!("lib.rs");
        let non_test =
            source[..source.find("\nmod tests {").expect("mod tests must exist")].to_lowercase();
        for needle in [
            concat!("murmur_inference_", "api_key"),
            concat!("vl", "lm_api_key"),
            concat!("\"author", "ization\""),
            concat!("\"x-api", "-key\""),
            concat!("bear", "er "),
        ] {
            assert!(
                !non_test.contains(needle),
                "driver-openai-compatible: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn manifest_declares_the_upstream_auth_scheme_the_gateway_will_read() {
        // The runtime builds the auth header from these two fields, so a dropped quote pair
        // (`value: {key}` is a YAML flow mapping, not a string) or a renamed key leaves the
        // gateway with no scheme to apply and an `api_key` capsule unauthenticated.
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "drivers/driver-openai-compatible/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    fn manifest() -> serde_norway::Mapping {
        serde_norway::from_str(include_str!("../murmur.yaml")).expect("murmur.yaml must parse")
    }

    fn manifest_version() -> String {
        manifest()
            .get("version")
            .and_then(|v| v.as_str())
            .expect("murmur.yaml declares a version")
            .to_string()
    }

    #[test]
    fn manifest_declares_no_models_block() {
        // The operator chooses and serves the model, so the driver declares no limits: a
        // capsule sets inference.max_tokens and context.max_tokens itself.
        let manifest = manifest();
        assert!(
            manifest.get("models").is_none(),
            "murmur.yaml must not declare models:"
        );
        assert_eq!(
            manifest.get("name").and_then(|v| v.as_str()),
            Some("driver-openai-compatible")
        );
        assert_eq!(
            manifest.get("runtime").and_then(|v| v.as_str()),
            Some("driver")
        );
        assert!(!include_str!("../murmur.yaml").contains("\nmodels:"));
    }

    #[test]
    fn readme_manifest_example_pins_the_version_in_murmur_yaml() {
        let readme = include_str!("../README.md");
        let pin = format!(
            "  - name: murmur/driver-openai-compatible\n    version: \"{}\"\n",
            manifest_version()
        );
        assert!(
            readme.contains(&pin),
            "README's manifest example must pin the murmur.yaml version:\n{pin}"
        );
    }

    #[test]
    fn readme_states_the_refusal_messages_verbatim() {
        let readme = include_str!("../README.md").replace('\n', " ");
        for message in [
            EMPTY_MODEL.to_string(),
            model_refusal("<model>", 404, "<body>").replace("HTTP 404", "HTTP <status>"),
        ] {
            assert!(
                readme.contains(&message),
                "README must state verbatim: {message}"
            );
        }
    }

    #[test]
    fn dispatch_request_sends_the_request_before_it_writes_the_body() {
        // Nothing drains the request body until `handle` passes the request to the host, so a
        // body written first blocks forever once it outgrows the host's buffer.
        let source = include_str!("lib.rs");
        let start = source
            .find("fn dispatch_request(")
            .expect("lib.rs defines dispatch_request");
        let rest = &source[start..];
        let dispatch = &rest[..rest.find("\n    fn ").unwrap_or(rest.len())];
        let position = |needle: &str| {
            dispatch
                .find(needle)
                .unwrap_or_else(|| panic!("dispatch_request must contain {needle}"))
        };
        let handle = position("outgoing_handler::handle(");
        let write = position("write_body(");
        let finish = position("OutgoingBody::finish(");
        let awaited = position("await_response(");
        assert!(handle < write, "handle must precede the body write");
        assert!(
            !dispatch[..handle].contains("check_write"),
            "no body write may precede handle"
        );
        assert!(write < finish, "the body is finished after it is written");
        assert!(finish < awaited, "the response is awaited after the body");
    }
}
