// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use murmur_driver_stream::Failure;
use serde::Deserialize;
use serde_json::{json, Map, Value};

// ── Model table ───────────────────────────────────────────────────────────────

/// Whether a backend reads reasoning sent back on an earlier assistant message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replay {
    /// The backend documents reading it back. `default` is what an unset `replay_reasoning`
    /// means.
    Accepted { default: bool },
    /// The backend documents no way to send it back, so the driver never does, and refuses a
    /// `replay_reasoning` key naming `reason`.
    Refused { reason: &'static str },
}

/// One model this driver serves, pinned to one backend of Hugging Face's router, with the limits
/// and reasoning controls that backend documents.
#[derive(Debug)]
struct ModelEntry {
    /// `<org>/<model>:<provider>`, sent verbatim as the body's `model`. The suffix pins the
    /// router to one backend, so context size, tool support and reasoning do not change between
    /// runs.
    id: &'static str,
    /// The largest `max_tokens` the backend accepts for the model. A larger value is refused here
    /// rather than spent on a provider 400.
    max_output_tokens: u64,
    /// The router's `context_length` for this backend. No request path reads it: it is the
    /// number `murmur.yaml` declares under `models:`, and a test keeps the two in step.
    #[cfg(test)]
    context_window: u64,
    /// The member reasoning arrives in, `message.<field>` and streaming `delta.<field>`, and the
    /// member it goes back in on an earlier assistant message.
    reasoning_field: &'static str,
    /// The `reasoning_effort` values the backend documents for the model, lowercased.
    effort_values: &'static [&'static str],
    /// The backend's own default effort, stamped on every request that sets none.
    default_effort: &'static str,
    /// Whether `replay_reasoning` is offered, and its default.
    replay: Replay,
}

/// Every model this driver serves. This table is the one place the crate branches on a model:
/// nothing else names an id or a backend, and `murmur.yaml`'s `models:` block must declare the
/// same numbers for each entry.
const SUPPORTED_MODELS: &[ModelEntry] = &[
    // Groq's model page: context window 131,072, max output tokens 65,536. Groq's reasoning
    // guide: `low`, `medium`, `high` for GPT-OSS, default `medium`; reasoning in
    // `message.reasoning`; nothing on sending it back.
    ModelEntry {
        id: "openai/gpt-oss-120b:groq",
        max_output_tokens: 65_536,
        #[cfg(test)]
        context_window: 131_072,
        reasoning_field: "reasoning",
        effort_values: &["low", "medium", "high"],
        default_effort: "medium",
        replay: Replay::Refused {
            reason: "Groq's reasoning guide documents no way to send reasoning back on an earlier \
                     assistant message, so driver-huggingface never sends it",
        },
    },
    // The router reports Cerebras's Free Trial context, 65,536, so the output cap is that tier's
    // 32,768 rather than the paid tiers' 40,960. Cerebras's reasoning guide: `none`, `low`,
    // `medium`, `high`, default `high`; `message.reasoning` / `delta.reasoning`; an earlier
    // assistant message's `reasoning` is read back while `clear_thinking` is left at `false`.
    ModelEntry {
        id: "Qwen/Qwen3.8-27B:cerebras",
        max_output_tokens: 32_768,
        #[cfg(test)]
        context_window: 65_536,
        reasoning_field: "reasoning",
        effort_values: &["none", "low", "medium", "high"],
        default_effort: "high",
        replay: Replay::Accepted { default: true },
    },
];

/// The router's backend-selection policies. Each lets the router pick a different backend from
/// one run to the next, so each is refused by name.
const POLICY_SUFFIXES: &[&str] = &["fastest", "cheapest", "preferred"];

/// The table's ids, joined for an error message.
fn supported_ids() -> String {
    SUPPORTED_MODELS
        .iter()
        .map(|entry| entry.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Look the model up by its exact id — no case folding, no trimming, no prefix match — before a
/// request body is built, so an unsupported model costs no HTTP call. A refused id gets the
/// message for its kind: no backend named, a router policy named, or any other id.
fn validate_model(model: &str) -> Result<&'static ModelEntry, String> {
    if let Some(entry) = SUPPORTED_MODELS.iter().find(|entry| entry.id == model) {
        return Ok(entry);
    }
    let ids = supported_ids();
    match model.rsplit_once(':') {
        None => Err(format!(
            "driver: model '{model}' names no backend. driver-huggingface serves only the \
             <org>/<model>:<provider> ids it lists, so the router cannot switch backends between \
             runs. Supported models: {ids}."
        )),
        Some((_, suffix)) if POLICY_SUFFIXES.contains(&suffix) => Err(format!(
            "driver: model '{model}' asks the router to choose the backend (':{suffix}'). \
             driver-huggingface pins one backend per model so a capsule's runs stay repeatable. \
             Supported models: {ids}."
        )),
        Some(_) => Err(format!(
            "driver: model '{model}' is not supported by driver-huggingface. Supported models: \
             {ids}."
        )),
    }
}

/// Refuse an output cap above the entry's ceiling rather than clamping it silently.
fn validate_max_tokens(entry: &ModelEntry, max_tokens: u64) -> Result<(), String> {
    if max_tokens > entry.max_output_tokens {
        return Err(format!(
            "driver: inference.max_tokens {max_tokens} exceeds the max_tokens ceiling of {} for {}",
            entry.max_output_tokens, entry.id
        ));
    }
    Ok(())
}

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a hard error on the first inference call, before
// any HTTP request is dispatched — a declared setting that is parsed and ignored is a defect,
// not a convenience. Whether a value suits the model is decided by [`resolve_config`], once the
// model is known.

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &["reasoning_effort", "replay_reasoning"];

/// A `reasoning_effort` as the capsule wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredEffort {
    /// Trimmed and lowercased: the value matched against the entry and sent.
    normalised: String,
    /// Verbatim, for the refusal that quotes it back.
    as_written: String,
}

/// The dials read from `inference.driver.config`, before any model is known. `None` is a key the
/// capsule left unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawDriverConfig {
    reasoning_effort: Option<ConfiguredEffort>,
    replay_reasoning: Option<bool>,
}

/// The dials a request is built with, once the entry's values and defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriverConfig {
    /// One of the entry's `effort_values`, lowercased; stamped on every request.
    reasoning_effort: String,
    /// Whether earlier assistant messages carry the entry's reasoning field. Always `false` on a
    /// [`Replay::Refused`] entry.
    replay_reasoning: bool,
}

/// Read the driver-config JSON into the raw dial set. Runs as the first fallible step of
/// `run_inner`, before any environment read other than `MURMUR_INFERENCE_DRIVER_CONFIG` itself,
/// so a rejected config cannot reach the network.
///
/// An absent config, `{}` and a whitespace-only value all yield [`RawDriverConfig::default`].
fn parse_driver_config(driver_config: Option<&str>) -> Result<RawDriverConfig, String> {
    let Some(raw) = driver_config else {
        return Ok(RawDriverConfig::default());
    };
    if raw.trim().is_empty() {
        return Ok(RawDriverConfig::default());
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

    Ok(RawDriverConfig {
        reasoning_effort: parse_reasoning_effort(config.get("reasoning_effort"))?,
        replay_reasoning: parse_replay_reasoning(config.get("replay_reasoning"))?,
    })
}

/// Only a JSON string is accepted; its value is checked against the model in [`resolve_config`].
fn parse_reasoning_effort(value: Option<&Value>) -> Result<Option<ConfiguredEffort>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(raw)) => Ok(Some(ConfiguredEffort {
            normalised: raw.trim().to_ascii_lowercase(),
            as_written: raw.clone(),
        })),
        Some(other) => Err(format!(
            "driver: inference.driver.config 'reasoning_effort' must be a string, got {other}"
        )),
    }
}

/// Only a JSON boolean is accepted: `"false"`, `0` and `null` are refused rather than coerced.
fn parse_replay_reasoning(value: Option<&Value>) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some(Value::Bool(replay)) => Ok(Some(*replay)),
        Some(other) => Err(format!(
            "driver: inference.driver.config 'replay_reasoning' must be a boolean, got {other}"
        )),
    }
}

/// Apply the entry to the raw dials: refuse an effort the backend does not document and a
/// `replay_reasoning` on a backend that reads no reasoning back, and fill unset dials with the
/// entry's defaults. The refused value is quoted back as written, beside the accepted set.
fn resolve_config(entry: &ModelEntry, raw: &RawDriverConfig) -> Result<DriverConfig, String> {
    let reasoning_effort = match &raw.reasoning_effort {
        None => entry.default_effort.to_string(),
        Some(effort) if entry.effort_values.contains(&effort.normalised.as_str()) => {
            effort.normalised.clone()
        }
        Some(effort) => {
            return Err(format!(
                "driver: inference.driver.config 'reasoning_effort' must be one of {} for {}, got \
                 \"{}\"",
                entry.effort_values.join(", "),
                entry.id,
                effort.as_written
            ))
        }
    };
    let replay_reasoning = match (entry.replay, raw.replay_reasoning) {
        (Replay::Accepted { default }, None) => default,
        (Replay::Accepted { .. }, Some(replay)) => replay,
        (Replay::Refused { .. }, None) => false,
        (Replay::Refused { reason }, Some(_)) => {
            return Err(format!(
                "driver: inference.driver.config 'replay_reasoning' is not accepted for {}: \
                 {reason}",
                entry.id
            ))
        }
    };
    Ok(DriverConfig {
        reasoning_effort,
        replay_reasoning,
    })
}

// ── Murmur request types ──────────────────────────────────────────────────────

/// The host's request envelope. Its reserved `prompt_cache_key` is not declared, so serde drops
/// it: Hugging Face's router documents no cache-key field and no affinity header to put it in.
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
    // Reasoning persisted from a prior driver response. The runtime stores and replays whatever
    // content blocks a driver emits without inspecting them, so this block — emitted by this
    // driver and read back by it — is where preserved reasoning lives.
    Thinking {
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

/// Extract reasoning text from the first non-empty `Thinking` block in a message's content.
///
/// `None` is the ordinary post-compaction state, not an error: a committed compaction replaces
/// the live context with a summary that crosses the hook boundary as a single `text` block, so
/// historical assistant messages carry no `thinking` block afterwards.
fn thinking_text(content: &[MurmurContentBlock]) -> Option<String> {
    content.iter().find_map(|block| match block {
        MurmurContentBlock::Thinking { text } if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
}

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
/// credential header is the runtime's to attach, never the driver's, and the router documents no
/// cache-key or affinity header.
fn request_headers(body_len: usize) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/json".to_string()),
        ("content-length", body_len.to_string()),
    ]
}

// ── Request translation ───────────────────────────────────────────────────────

/// The body `run_inner` puts on the wire, and the entry it was built for.
#[derive(Debug)]
struct ProviderRequest {
    entry: &'static ModelEntry,
    body: Value,
}

/// Every check that can be made without building a body or touching the network, in order:
/// the model, then the config against it, then the output cap, then images.
fn preflight_request(
    request: &MurmurRequest,
    raw: &RawDriverConfig,
) -> Result<(&'static ModelEntry, DriverConfig), String> {
    let entry = validate_model(&request.model)?;
    let config = resolve_config(entry, raw)?;
    validate_max_tokens(entry, request.max_tokens)?;
    let has_image = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, MurmurContentBlock::Image { .. }))
    });
    if has_image {
        return Err(
            "driver: driver-huggingface sends text only and refuses an image block".to_string(),
        );
    }
    Ok((entry, config))
}

/// Pre-flight, translate, stamp the streaming flags. Tests assert on this rather than on a bare
/// translation.
fn build_provider_request(
    request: &MurmurRequest,
    raw: &RawDriverConfig,
) -> Result<ProviderRequest, String> {
    let (entry, config) = preflight_request(request, raw)?;
    let mut body = translate_murmur_request_to_huggingface(request, entry, &config)?;
    stamp_streaming_flags(&mut body);
    Ok(ProviderRequest { entry, body })
}

/// The Chat Completions body. It holds only members the router's Chat Completions spec documents
/// and the backend reads: no `max_completion_tokens`, `tool_choice`, `parallel_tool_calls` or
/// `prompt_cache_key`, and none of the backends' own reasoning switches (`reasoning_format`,
/// `include_reasoning`, `clear_thinking`, `enable_thinking`, `chat_template_args`, `thinking`).
fn translate_murmur_request_to_huggingface(
    request: &MurmurRequest,
    entry: &ModelEntry,
    config: &DriverConfig,
) -> Result<Value, String> {
    let mut messages = Vec::new();

    if let Some(system) = request
        .system
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        messages.push(json!({
            "role": "system",
            "content": system,
        }));
    }

    for message in &request.messages {
        match message.role.as_str() {
            "user" => messages.push(translate_user_message(message)),
            "assistant" => messages.push(translate_assistant_message(
                message,
                entry.reasoning_field,
                config.replay_reasoning,
            )?),
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
    // The id with its `:<provider>` suffix: the router reads the suffix as the backend to use.
    body.insert("model".to_string(), Value::String(request.model.clone()));
    // The router's spec spells the output cap `max_tokens`; Cerebras reads it as an alias of
    // `max_completion_tokens`.
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    // Top-level, and stamped on every request including the unconfigured one, so the effective
    // effort is visible in a recorded request and cannot drift with the backend's default.
    body.insert(
        "reasoning_effort".to_string(),
        Value::String(config.reasoning_effort.clone()),
    );

    // The one params pass-through loop: a key the body already holds wins.
    for (key, value) in &request.params {
        if !body.contains_key(key) {
            body.insert(key.clone(), value.clone());
        }
    }

    Ok(Value::Object(body))
}

/// Image blocks never reach here: [`preflight_request`] refuses them.
fn translate_user_message(message: &MurmurMessage) -> Value {
    json!({"role": "user", "content": joined_text(&message.content)})
}

/// Rebuild an assistant turn as the backend returned it, with its reasoning when
/// `replay_reasoning` is set.
///
/// With `replay_reasoning`, the turn's reasoning goes back as `<reasoning_field>`, on a turn that
/// ended in tool calls and on a plain-text turn alike. When no `thinking` block is present the
/// member is omitted entirely — never sent as an empty string — because that is the ordinary
/// state of a history that has been compacted.
///
/// The text a tool-call turn carried beside its calls is kept as `content`; `content` is `null`
/// only on a tool-call turn with no text.
fn translate_assistant_message(
    message: &MurmurMessage,
    reasoning_field: &str,
    replay_reasoning: bool,
) -> Result<Value, String> {
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

    let mut msg = if tool_calls.is_empty() {
        json!({"role": "assistant", "content": text})
    } else {
        let content = if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        };
        json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        })
    };

    if replay_reasoning {
        if let Some(reasoning) = thinking_text(&message.content) {
            msg[reasoning_field] = json!(reasoning);
        }
    }

    Ok(msg)
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

// ── Provider token usage ──────────────────────────────────────────────────────

/// The backend's raw token counts, carried onto the reserved top-level `usage` object of the
/// translated response. Every member is independently optional: a count the backend did not
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
    /// — so `input_tokens + cached_tokens` is the backend's total. It is omitted when the total
    /// is unreported or the cache count exceeds it: such a report is inconsistent, and no value
    /// is invented for it. `thinking_tokens` is part of `output_tokens`. No backend here reports
    /// a cache-write count, so `cache_write_tokens` is never emitted. `None` when no member
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
fn extract_huggingface_usage(usage: &Value) -> UsageTokens {
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

/// Map a `finish_reason` to a murmur `stop_reason`: `stop`, `length` and `tool_calls`. Any other
/// reason is an error naming it rather than a guess. Each caller refuses a missing reason with
/// its own path's message.
fn map_finish_reason(finish_reason: &str) -> Result<&'static str, Failure> {
    match finish_reason {
        "stop" => Ok("end_turn"),
        "tool_calls" => Ok("tool_call"),
        "length" => Ok("max_tokens"),
        other => Err(Failure::plain(format!(
            "driver: unsupported Hugging Face finish_reason '{other}'"
        ))),
    }
}

// ── Provider errors ───────────────────────────────────────────────────────────

/// Whether murmur makes a call answered with `status` again by itself: `408`, `429`, and every
/// `5xx` but `501` and `505`.
fn murmur_retries_status(status: u64) -> bool {
    matches!(status, 408 | 429) || ((500..=599).contains(&status) && status != 501 && status != 505)
}

/// The HTTP status an `error` member names, read from `code`, `status` or `status_code` in that
/// order, each as a JSON number or an all-digit string. `None` when none of them holds one.
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

/// The failure for a chunk whose top-level `error` is present and non-null, the member
/// serialised compactly: `driver: Hugging Face stream error: <error>`.
///
/// The router documents no error vocabulary for a failure inside a `2xx` stream. An error naming
/// a status murmur would retry had it arrived as the response status is transient, so a backend
/// that runs out of capacity mid-stream gets the same treatment as one that refuses up front;
/// every other error is plain.
fn stream_error_failure(error: &Value) -> Failure {
    let message = format!("driver: Hugging Face stream error: {error}");
    if error_status(error).is_some_and(murmur_retries_status) {
        Failure::transient(message)
    } else {
        Failure::plain(message)
    }
}

/// The failure for a provider status `>= 400`: `HTTP <status>: <body>`, carrying the router's
/// own body so the operator sees what it said. Always plain: murmur retries the router's `429`
/// and its `5xx` (a backend that is down) by itself, and never retries `402`, which the router
/// answers when the account has no credits.
fn status_failure(status: u16, body: &str) -> Failure {
    Failure::plain(format!("HTTP {status}: {body}"))
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
    serde_json::from_str(arguments).map_err(|err| {
        format!("driver: failed to parse Hugging Face tool call arguments JSON: {err}")
    })
}

// ── Response translation (non-streaming fallback) ─────────────────────────────

/// The non-streaming JSON path, reached when the response body's first non-whitespace byte is
/// `{` or `[` — scripted test servers, and backends that ignore `stream: true`. Reasoning is
/// read from `message.<reasoning_field>`.
fn translate_huggingface_response_to_murmur(
    response: &Value,
    reasoning_field: &str,
) -> Result<Value, Failure> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: Hugging Face response missing choices[0]".to_string())?;

    // Absent, `null`, non-string or empty: the turn did not finish.
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| "driver: Hugging Face response has no finish_reason".to_string())?;
    let stop_reason = map_finish_reason(finish_reason)?;

    let message = choice.get("message").unwrap_or(&Value::Null);

    let reasoning = message
        .get(reasoning_field)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();

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

    let usage = response
        .get("usage")
        .map(extract_huggingface_usage)
        .unwrap_or_default();

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

/// A tool-call fact read from the provider stream. Carries no part of the call's input.
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
/// sets `state.saw_done`, and `Err` for a chunk carrying a non-null top-level `error`
/// ([`stream_error_failure`]) or a tool call index past
/// [`murmur_driver_stream::MAX_SLOTS_PER_TURN`].
///
/// `delta.<reasoning_field>` is routed to `emit_thinking`, `delta.content` to `emit_text`; the
/// two never mix, and nothing in this crate scans the text for reasoning sentinels. A tool call
/// arrives either in fragments — the first carrying its `id` and `function.name`, later ones
/// only `arguments` pieces — or whole in one chunk; both are accumulated by `index`.
fn process_huggingface_sse_line(
    line: &str,
    reasoning_field: &str,
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

    // An error ends the turn here, so it is reported as the provider's and never as a cut stream.
    if let Some(error) = data.get("error").filter(|error| !error.is_null()) {
        return Err(stream_error_failure(error));
    }

    // Read from any chunk, before the `choices`-shaped early returns below: `include_usage` puts
    // the final report on a chunk whose `choices` is empty, and a backend may also send running
    // usage beside `choices`.
    if let Some(reported) = data.get("usage").filter(|value| !value.is_null()) {
        state.usage.merge_from(extract_huggingface_usage(reported));
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

    let Some(delta) = choice.and_then(|c| c.get("delta")) else {
        return Ok(false);
    };

    if let Some(reasoning) = delta.get(reasoning_field).and_then(Value::as_str) {
        if !reasoning.is_empty() {
            emit_thinking(reasoning);
            state.reasoning.push_str(reasoning);
        }
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
/// A stream that never sent `data: [DONE]` was cut after the provider's `2xx`, whatever it
/// carried before, and that is checked first; one that sent it with no `finish_reason` ended
/// before the turn did. Both fail transiently.
fn assemble_huggingface_streaming_response(state: StreamState) -> Result<Value, Failure> {
    if !state.saw_done {
        return Err(Failure::transient(
            "driver: Hugging Face stream ended before data: [DONE]",
        ));
    }
    let finish_reason = state.stop_reason.as_deref().ok_or_else(|| {
        Failure::transient("driver: Hugging Face stream ended with no finish_reason")
    })?;
    let stop_reason = map_finish_reason(finish_reason)?;

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

    Ok(murmur_response(
        stop_reason,
        ordered_content(&state.reasoning, &state.text, tool_calls),
        state.usage,
    ))
}

/// Parse a complete SSE body string, driving the same line processor the wasm module drives.
/// This is how the streaming assertions run host-side, with no network and no provider account.
#[cfg(test)]
fn parse_huggingface_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    reasoning_field: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, Failure> {
    parse_huggingface_sse_body_with_signals(body, reasoning_field, emit_text, emit_thinking).0
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
fn parse_huggingface_sse_body_with_signals<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    reasoning_field: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> (Result<Value, Failure>, Vec<RecordedSignal>) {
    let mut state = StreamState::default();
    let mut signals = Vec::new();

    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        match process_huggingface_sse_line(
            line,
            reasoning_field,
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

    (assemble_huggingface_streaming_response(state), signals)
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
        assemble_huggingface_streaming_response, build_provider_request, error_payload,
        parse_driver_config, process_huggingface_sse_line, request_headers, status_failure,
        translate_huggingface_response_to_murmur, MurmurRequest, StreamState, ToolCallSignal,
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

    pub struct HuggingFaceDriver;

    impl exports::murmur::tool::run::Guest for HuggingFaceDriver {
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

        // Model, config-against-model, output-cap and image checks happen inside here,
        // pre-flight: a rejected request costs no HTTP call.
        let provider_request = build_provider_request(&murmur_request, &config)?;
        let reasoning_field = provider_request.entry.reasoning_field;

        let body = serde_json::to_vec(&provider_request.body)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));

        let headers = request_headers(body.len());

        let response = dispatch_request(&url, headers, &body)?;
        let status = response.status();

        if status >= 400 {
            let text = consume_body_as_string(response, status)?;
            return Err(status_failure(status, &text));
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
            // Non-streaming fallback (test servers, backends ignoring stream:true).
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
            let json: Value = serde_json::from_str(&text).map_err(|err| {
                format!("driver: failed to parse Hugging Face response JSON: {err}")
            })?;
            translate_huggingface_response_to_murmur(&json, reasoning_field)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut state = StreamState::default();

            let handle_line = |line: &[u8], state: &mut StreamState| -> Result<bool, Failure> {
                let line = String::from_utf8_lossy(line);
                let line = line.trim_end_matches('\r');
                process_huggingface_sse_line(
                    line,
                    reasoning_field,
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
            assemble_huggingface_streaming_response(state)?
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

    export!(HuggingFaceDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Nothing here talks to the provider. Every assertion is driven by a fixture and the host-side
// whole-stream parser.

#[cfg(test)]
mod tests {
    use super::{
        build_provider_request, error_payload, map_finish_reason, parse_driver_config,
        parse_huggingface_sse_body, parse_huggingface_sse_body_with_signals,
        process_huggingface_sse_line, request_headers, resolve_config, status_failure,
        stream_error_failure, translate_huggingface_response_to_murmur,
        translate_murmur_request_to_huggingface, validate_model, DriverConfig, ModelEntry,
        MurmurRequest, RawDriverConfig, RecordedSignal, Replay, StreamState, POLICY_SUFFIXES,
        SUPPORTED_MODELS,
    };
    use murmur_driver_stream::Failure;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    // ── Fixtures and helpers ──────────────────────────────────────────────────

    const GPT: &str = "openai/gpt-oss-120b:groq";
    const QWEN: &str = "Qwen/Qwen3.8-27B:cerebras";
    const IDS: &str = "openai/gpt-oss-120b:groq, Qwen/Qwen3.8-27B:cerebras";

    /// The member both entries return and replay reasoning in.
    const FIELD: &str = "reasoning";

    /// Members the body never carries, at any depth: other providers' and backends' own reasoning
    /// switches, the cache key, and controls the driver does not offer.
    const FORBIDDEN_KEYS: &[&str] = &[
        "chat_template_args",
        "clear_thinking",
        "enable_thinking",
        "thinking",
        "reasoning_format",
        "include_reasoning",
        "prompt_cache_key",
        "max_completion_tokens",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning_content",
    ];

    fn entry(id: &str) -> &'static ModelEntry {
        validate_model(id).expect("fixture model must be listed")
    }

    fn request_from(value: Value) -> MurmurRequest {
        serde_json::from_value(value).expect("fixture must deserialize as a murmur request")
    }

    fn raw(config: &str) -> RawDriverConfig {
        parse_driver_config(Some(config)).expect("fixture config must parse")
    }

    fn no_config() -> RawDriverConfig {
        RawDriverConfig::default()
    }

    fn effort(value: &str) -> RawDriverConfig {
        raw(&json!({ "reasoning_effort": value }).to_string())
    }

    fn body_for(request: &MurmurRequest, config: &RawDriverConfig) -> Value {
        build_provider_request(request, config)
            .expect("fixture request must translate")
            .body
    }

    fn refusal(request: &MurmurRequest, config: &RawDriverConfig) -> String {
        build_provider_request(request, config).expect_err("fixture request must be refused")
    }

    fn contains_key_anywhere(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(key) || map.values().any(|v| contains_key_anywhere(v, key))
            }
            Value::Array(items) => items.iter().any(|v| contains_key_anywhere(v, key)),
            _ => false,
        }
    }

    fn assert_no_forbidden_key(body: &Value) {
        for absent in FORBIDDEN_KEYS {
            assert!(
                !contains_key_anywhere(body, absent),
                "'{absent}' must not appear at any nesting level: {body}"
            );
        }
    }

    fn parse_quiet(body: &str) -> Result<Value, Failure> {
        parse_huggingface_sse_body(body, FIELD, &mut |_: &str| {}, &mut |_: &str| {})
    }

    fn request_for_model(model: &str, max_tokens: u64) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }))
    }

    /// The canonical envelope the host builds, with the reserved `prompt_cache_key` present.
    fn simple_request(model: &str) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": 8192,
            "system": "You are a careful assistant.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Why is the sky blue?"}]}
            ],
            "tools": [],
            "params": {},
            "prompt_cache_key": "hf-check:0.1.0:ctx_01"
        }))
    }

    /// One streamed chunk whose `delta` is `delta`.
    fn chunk(delta: Value) -> String {
        let chunk = json!({"choices": [{"index": 0, "delta": delta}]});
        format!("data: {chunk}\n")
    }

    /// One streamed chunk carrying a `tool_calls` fragment for `index`.
    fn tool_delta(
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> String {
        let mut call = json!({"index": index, "type": "function", "function": {}});
        if let Some(id) = id {
            call["id"] = json!(id);
        }
        if let Some(name) = name {
            call["function"]["name"] = json!(name);
        }
        if let Some(arguments) = arguments {
            call["function"]["arguments"] = json!(arguments);
        }
        chunk(json!({"tool_calls": [call]}))
    }

    const TOOL_CALLS_FINISH: &str =
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\ndata: [DONE]\n";

    const TEXT_DELTA: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n";

    fn parse_with_signals(body: &str) -> (Value, Vec<RecordedSignal>) {
        let (result, signals) =
            parse_huggingface_sse_body_with_signals(body, FIELD, &mut |_| {}, &mut |_| {});
        (result.unwrap(), signals)
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

    /// `marker` sits only inside a call's arguments, so no signal may carry it.
    fn assert_no_signal_carries(signals: &[RecordedSignal], marker: &str) {
        for signal in signals {
            assert!(
                !format!("{signal:?}").contains(marker),
                "a tool-call signal carries argument text: {signal:?}"
            );
        }
    }

    // ── S1 — the model table and its refusals ─────────────────────────────────

    #[test]
    fn every_listed_id_is_accepted_and_sent_verbatim_with_its_backend() {
        for entry in SUPPORTED_MODELS {
            let built = build_provider_request(&request_for_model(entry.id, 1024), &no_config())
                .expect("a listed id must build");
            assert_eq!(
                built.entry.id, entry.id,
                "the request is built for, and streams with, its own entry"
            );
            assert_eq!(
                built.body["model"],
                json!(entry.id),
                "the :<provider> suffix is kept"
            );
        }
        assert_eq!(
            SUPPORTED_MODELS.iter().map(|e| e.id).collect::<Vec<_>>(),
            [GPT, QWEN]
        );
    }

    #[test]
    fn every_entry_id_names_exactly_one_backend_that_is_not_a_router_policy() {
        for entry in SUPPORTED_MODELS {
            assert_eq!(
                entry.id.matches(':').count(),
                1,
                "{} must hold exactly one ':'",
                entry.id
            );
            let (model, provider) = entry.id.split_once(':').unwrap();
            assert!(
                model.contains('/'),
                "{}: <org>/<model> before the ':'",
                entry.id
            );
            assert!(
                !provider.is_empty(),
                "{}: a provider after the ':'",
                entry.id
            );
            assert!(
                !POLICY_SUFFIXES.contains(&provider),
                "{}: the provider must not be a router policy",
                entry.id
            );
            assert!(
                entry.max_output_tokens >= 1 && entry.max_output_tokens <= entry.context_window,
                "{}: 1 <= max_output_tokens <= context_window",
                entry.id
            );
            assert!(
                entry.effort_values.contains(&entry.default_effort),
                "{}: the default effort is one of its values",
                entry.id
            );
            for value in entry.effort_values {
                assert_eq!(*value, value.trim().to_ascii_lowercase(), "{}", entry.id);
            }
        }
    }

    #[test]
    fn a_bare_model_name_is_refused_as_naming_no_backend() {
        for model in [
            "openai/gpt-oss-120b",
            "Qwen/Qwen3.8-27B",
            "gpt-oss-120b",
            "",
        ] {
            assert_eq!(
                refusal(&request_for_model(model, 1024), &no_config()),
                format!(
                    "driver: model '{model}' names no backend. driver-huggingface serves only \
                     the <org>/<model>:<provider> ids it lists, so the router cannot switch \
                     backends between runs. Supported models: {IDS}."
                )
            );
        }
    }

    #[test]
    fn the_router_policy_suffixes_are_refused_by_name() {
        for (model, suffix) in [
            ("openai/gpt-oss-120b:fastest", "fastest"),
            ("openai/gpt-oss-120b:cheapest", "cheapest"),
            ("openai/gpt-oss-120b:preferred", "preferred"),
            ("Qwen/Qwen3.8-27B:cheapest", "cheapest"),
            ("openai/gpt-oss-120b:groq:fastest", "fastest"),
        ] {
            assert_eq!(
                refusal(&request_for_model(model, 1024), &no_config()),
                format!(
                    "driver: model '{model}' asks the router to choose the backend \
                     (':{suffix}'). driver-huggingface pins one backend per model so a \
                     capsule's runs stay repeatable. Supported models: {IDS}."
                )
            );
        }
        assert_eq!(POLICY_SUFFIXES, ["fastest", "cheapest", "preferred"]);
    }

    #[test]
    fn any_other_id_is_refused_matched_exactly_without_trimming_or_case_folding() {
        for model in [
            "openai/gpt-oss-120b:cerebras",
            "openai/gpt-oss-120b:Groq",
            "openai/GPT-OSS-120B:groq",
            " openai/gpt-oss-120b:groq",
            "openai/gpt-oss-120b:groq ",
            "qwen/qwen3.8-27b:cerebras",
            "openai/gpt-oss-120b:FASTEST",
            "openai/gpt-oss-120b:",
            "zai-org/GLM-5.3:groq",
        ] {
            assert_eq!(
                refusal(&request_for_model(model, 1024), &no_config()),
                format!(
                    "driver: model '{model}' is not supported by driver-huggingface. Supported \
                     models: {IDS}."
                ),
                "model {model:?}"
            );
        }
    }

    #[test]
    fn the_model_is_refused_before_any_other_request_check() {
        // An oversized cap, an image, an orphan tool message and a config no entry accepts would
        // each be refused too; the model check is the one that answers, so it runs first.
        let request = request_from(json!({
            "model": "openai/gpt-oss-120b:fastest",
            "max_tokens": 10_000_000u64,
            "messages": [
                {"role": "tool", "content": []},
                {"role": "user", "content": [{"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}}]}
            ]
        }));
        let err = refusal(&request, &effort("ultra"));
        assert!(
            err.contains("asks the router to choose the backend"),
            "got {err}"
        );
    }

    #[test]
    fn a_pre_flight_refusal_is_a_plain_failure() {
        for model in ["openai/gpt-oss-120b", "openai/gpt-oss-120b:fastest", "x:y"] {
            let failure = Failure::from(refusal(&request_for_model(model, 1024), &no_config()));
            assert_eq!(failure.retry, None);
            assert!(failure.metadata().is_empty());
            assert!(failure.message.starts_with("driver: "));
        }
    }

    // ── S2 — output cap ───────────────────────────────────────────────────────

    #[test]
    fn output_cap_is_written_as_max_tokens_up_to_each_entrys_ceiling() {
        for (id, cap) in [(GPT, 65_536u64), (QWEN, 32_768)] {
            let body = body_for(&request_for_model(id, cap), &no_config());
            assert_eq!(body["max_tokens"], json!(cap));
            assert_eq!(
                refusal(&request_for_model(id, cap + 1), &no_config()),
                format!(
                    "driver: inference.max_tokens {} exceeds the max_tokens ceiling of {cap} for \
                     {id}",
                    cap + 1
                )
            );
        }
    }

    #[test]
    fn the_config_is_checked_against_the_model_before_the_output_cap() {
        let err = refusal(&request_for_model(GPT, 10_000_000), &effort("none"));
        assert!(
            err.contains("'reasoning_effort' must be one of"),
            "got {err}"
        );
    }

    // ── S3 — config: parse, then resolve against the entry ────────────────────

    #[test]
    fn absent_empty_whitespace_and_empty_object_configs_take_each_entrys_defaults() {
        for config in [None, Some(""), Some("   "), Some("{}")] {
            let raw = parse_driver_config(config).expect("a default config must parse");
            assert_eq!(raw, RawDriverConfig::default(), "config {config:?}");
            assert_eq!(
                resolve_config(entry(GPT), &raw).unwrap(),
                DriverConfig {
                    reasoning_effort: "medium".to_string(),
                    replay_reasoning: false
                }
            );
            assert_eq!(
                resolve_config(entry(QWEN), &raw).unwrap(),
                DriverConfig {
                    reasoning_effort: "high".to_string(),
                    replay_reasoning: true
                }
            );
        }
        assert_eq!(
            body_for(&simple_request(GPT), &no_config())["reasoning_effort"],
            json!("medium"),
            "the default is stamped"
        );
        assert_eq!(
            body_for(&simple_request(QWEN), &no_config())["reasoning_effort"],
            json!("high"),
            "the default is stamped"
        );
    }

    #[test]
    fn each_documented_effort_is_sent_lowercased_at_the_top_level() {
        for (id, values) in [
            (GPT, &["low", "medium", "high"][..]),
            (QWEN, &["none", "low", "medium", "high"][..]),
        ] {
            assert_eq!(entry(id).effort_values, values);
            for value in values {
                for written in [value.to_string(), format!(" {} ", value.to_uppercase())] {
                    let body = body_for(&simple_request(id), &effort(&written));
                    assert_eq!(body["reasoning_effort"], json!(value), "{id} {written:?}");
                    assert!(
                        body.get("reasoning").is_none(),
                        "effort is top-level: {body}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_effort_the_backend_does_not_document_is_refused_with_the_raw_value_quoted() {
        for (id, values, rejected) in [
            (
                GPT,
                "low, medium, high",
                &["none", "xhigh", "max", "default", " Minimal "][..],
            ),
            (
                QWEN,
                "none, low, medium, high",
                &["xhigh", "max", "default", "minimal", "Ultra\t"][..],
            ),
        ] {
            for written in rejected {
                assert_eq!(
                    refusal(&simple_request(id), &effort(written)),
                    format!(
                        "driver: inference.driver.config 'reasoning_effort' must be one of \
                         {values} for {id}, got \"{written}\""
                    )
                );
            }
        }
    }

    #[test]
    fn a_non_string_reasoning_effort_is_refused() {
        for (config, shown) in [
            (r#"{"reasoning_effort":3}"#, "3"),
            (r#"{"reasoning_effort":true}"#, "true"),
            (r#"{"reasoning_effort":null}"#, "null"),
            (
                r#"{"reasoning_effort":{"level":"high"}}"#,
                r#"{"level":"high"}"#,
            ),
        ] {
            assert_eq!(
                parse_driver_config(Some(config)).unwrap_err(),
                format!(
                    "driver: inference.driver.config 'reasoning_effort' must be a string, got \
                     {shown}"
                )
            );
        }
    }

    #[test]
    fn replay_reasoning_on_a_backend_that_reads_none_back_is_refused_true_and_false_alike() {
        for config in [
            r#"{"replay_reasoning":true}"#,
            r#"{"replay_reasoning":false}"#,
        ] {
            assert_eq!(
                refusal(&simple_request(GPT), &raw(config)),
                "driver: inference.driver.config 'replay_reasoning' is not accepted for \
                 openai/gpt-oss-120b:groq: Groq's reasoning guide documents no way to send \
                 reasoning back on an earlier assistant message, so driver-huggingface never \
                 sends it"
            );
        }
        assert!(matches!(entry(GPT).replay, Replay::Refused { .. }));
        assert_eq!(entry(QWEN).replay, Replay::Accepted { default: true });
    }

    #[test]
    fn replay_reasoning_on_qwen_takes_either_boolean() {
        for (config, replay) in [
            (r#"{"replay_reasoning":true}"#, true),
            (r#"{"replay_reasoning":false}"#, false),
        ] {
            assert_eq!(
                resolve_config(entry(QWEN), &raw(config)).unwrap(),
                DriverConfig {
                    reasoning_effort: "high".to_string(),
                    replay_reasoning: replay
                }
            );
        }
    }

    #[test]
    fn a_non_boolean_replay_reasoning_is_refused_by_name() {
        for (value, shown) in [
            (r#""false""#, r#""false""#),
            (r#""true""#, r#""true""#),
            ("0", "0"),
            ("1", "1"),
            ("null", "null"),
            ("{}", "{}"),
            ("[]", "[]"),
        ] {
            assert_eq!(
                parse_driver_config(Some(&format!(r#"{{"replay_reasoning":{value}}}"#)))
                    .unwrap_err(),
                format!(
                    "driver: inference.driver.config 'replay_reasoning' must be a boolean, got \
                     {shown}"
                ),
                "value {value}"
            );
        }
    }

    #[test]
    fn unrecognised_config_keys_are_reported_sorted_with_the_accepted_set() {
        assert_eq!(
            parse_driver_config(Some(
                r#"{"thinking":"enabled","clear_thinking":true,"include_reasoning":false,"reasoning_effort":"high","replay_reasoning":false}"#
            ))
            .unwrap_err(),
            "driver: unrecognised inference.driver.config key(s): clear_thinking, \
             include_reasoning, thinking. Accepted keys: reasoning_effort, replay_reasoning"
        );
    }

    #[test]
    fn a_config_that_is_not_a_json_object_is_refused() {
        let not_json = parse_driver_config(Some("not json")).unwrap_err();
        assert!(
            not_json.starts_with("driver: inference.driver.config is not valid JSON: "),
            "got {not_json}"
        );
        for config in ["[]", "\"high\"", "3"] {
            assert_eq!(
                parse_driver_config(Some(config)).unwrap_err(),
                "driver: inference.driver.config must be a JSON object"
            );
        }
    }

    #[test]
    fn the_same_config_is_accepted_for_one_entry_and_refused_for_the_other() {
        let none = effort("none");
        assert_eq!(
            body_for(&simple_request(QWEN), &none)["reasoning_effort"],
            json!("none")
        );
        assert!(refusal(&simple_request(GPT), &none).contains("for openai/gpt-oss-120b:groq"));

        let no_replay = raw(r#"{"replay_reasoning":false}"#);
        assert!(build_provider_request(&simple_request(QWEN), &no_replay).is_ok());
        assert!(refusal(&simple_request(GPT), &no_replay).contains("is not accepted for"));
    }

    // ── S4 — request translation with tools ───────────────────────────────────

    fn tools_request(model: &str) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": 16384,
            "system": "  Be brief.  ",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Weather in Paris and Tokyo?"}]}
            ],
            "tools": [
                {
                    "name": "get_weather",
                    "description": " Weather for a city. ",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                },
                {
                    "name": "read_file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }
            ],
            "params": {"temperature": 0.6, "stream": false, "stream_options": {"include_usage": false}},
            "prompt_cache_key": "hf-check:0.1.0:ctx_01"
        }))
    }

    #[test]
    fn a_request_with_tools_becomes_the_chat_completions_body() {
        for (id, effort_value) in [(GPT, "high"), (QWEN, "low")] {
            let body = body_for(&tools_request(id), &effort(effort_value));

            let keys: Vec<&str> = body
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                [
                    "max_tokens",
                    "messages",
                    "model",
                    "reasoning_effort",
                    "stream",
                    "stream_options",
                    "temperature",
                    "tools",
                ],
                "the body holds these members only"
            );

            assert_eq!(body["model"], json!(id));
            assert_eq!(body["max_tokens"], json!(16384));
            assert_eq!(
                body["messages"],
                json!([
                    {"role": "system", "content": "Be brief."},
                    {"role": "user", "content": "Weather in Paris and Tokyo?"}
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
                        "name": "read_file",
                        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                    }}
                ])
            );
            assert_eq!(body["reasoning_effort"], json!(effort_value));
            assert_eq!(
                body["stream"],
                json!(true),
                "params cannot turn streaming off"
            );
            assert_eq!(
                body["stream_options"],
                json!({"include_usage": true}),
                "params cannot turn the final usage off"
            );
            assert_eq!(
                body["temperature"],
                json!(0.6),
                "an unclaimed param passes through"
            );
            assert_no_forbidden_key(&body);
            assert!(
                !body.to_string().contains("hf-check:0.1.0"),
                "the cache key is dropped: {body}"
            );
        }
    }

    #[test]
    fn a_params_key_the_body_already_holds_does_not_override_it() {
        let request = request_from(json!({
            "model": QWEN,
            "max_tokens": 1000,
            "messages": [],
            "params": {
                "model": "Qwen/Qwen3.8-27B:fastest",
                "max_tokens": 5,
                "messages": [{"role": "user", "content": "injected"}],
                "reasoning_effort": "none"
            }
        }));
        let body = body_for(&request, &no_config());
        assert_eq!(body["model"], json!(QWEN));
        assert_eq!(body["max_tokens"], json!(1000));
        assert_eq!(body["messages"], json!([]));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn request_headers_are_content_type_and_content_length_only() {
        assert_eq!(
            request_headers(321),
            vec![
                ("content-type", "application/json".to_string()),
                ("content-length", "321".to_string()),
            ]
        );
    }

    // ── S5 — a hook-shaped request with no tools ──────────────────────────────

    #[test]
    fn a_hook_shaped_request_without_tools_sends_no_tools_member_and_parses_to_end_turn() {
        for entry in SUPPORTED_MODELS {
            let request = request_from(json!({
                "model": entry.id,
                "max_tokens": 4096,
                "system": "Summarise the conversation.",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "Edit the README."}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Done."}]},
                    {"role": "user", "content": [{"type": "text", "text": "Summarise."}]}
                ]
            }));
            let body = body_for(&request, &no_config());

            assert!(
                body.get("tools").is_none(),
                "no tools declared means no member: {body}"
            );
            assert_eq!(body["reasoning_effort"], json!(entry.default_effort));
            assert_eq!(body["stream"], json!(true));
            assert_eq!(body["stream_options"], json!({"include_usage": true}));
            assert_eq!(
                body["messages"][2],
                json!({"role": "assistant", "content": "Done."})
            );
            assert_no_forbidden_key(&body);
        }

        let summary = format!(
            "{}{}data: {}\ndata: [DONE]\n",
            chunk(json!({"reasoning": "Summarise briefly."})),
            chunk(json!({"content": "The README was edited."})),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        );
        let response = parse_quiet(&summary).unwrap();
        assert_eq!(response["stop_reason"], json!("end_turn"));
        assert_eq!(
            response["content"],
            json!([
                {"type": "thinking", "text": "Summarise briefly."},
                {"type": "text", "text": "The README was edited."}
            ])
        );
    }

    // ── S6 — reasoning replayed on the next turn ──────────────────────────────

    const REASONING: &str = "Two independent calls: Paris, then Tokyo. \"Quote\" — ü.";

    /// Reasoning deltas, one content delta, two calls in fragments at index 0 and 1, a
    /// `finish_reason: "tool_calls"` chunk, a usage-only chunk with `choices: []`, `[DONE]`.
    fn live_tool_call_stream() -> String {
        [
            chunk(json!({"role": "assistant", "reasoning": "Two independent calls: "})),
            chunk(json!({"reasoning": "Paris, then Tokyo. \"Quote\""})),
            chunk(json!({"reasoning": " — ü."})),
            chunk(json!({"content": "I'll check both."})),
            tool_delta(0, Some("call_paris"), Some("get_weather"), Some("{\"")),
            tool_delta(0, None, None, Some("city\":\"")),
            tool_delta(0, None, None, Some("Paris\"}")),
            tool_delta(1, Some("call_tokyo"), Some("get_weather"), Some("{\"")),
            tool_delta(1, None, None, Some("city\":\"Tok")),
            tool_delta(1, None, None, Some("yo\"}")),
            format!(
                "data: {}\n",
                json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}]})
            ),
            format!(
                "data: {}\n",
                json!({
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 170,
                        "completion_tokens": 23,
                        "total_tokens": 193,
                        "prompt_tokens_details": {"cached_tokens": 0},
                        "completion_tokens_details": {"reasoning_tokens": 12}
                    }
                })
            ),
            "data: [DONE]\n".to_string(),
        ]
        .concat()
    }

    #[test]
    fn a_tool_call_turn_parses_to_thinking_then_text_then_calls() {
        let response = parse_quiet(&live_tool_call_stream()).unwrap();
        assert_eq!(response["stop_reason"], json!("tool_call"));
        assert_eq!(
            response["content"],
            json!([
                {"type": "thinking", "text": REASONING},
                {"type": "text", "text": "I'll check both."},
                {"type": "tool_call", "id": "call_paris", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_call", "id": "call_tokyo", "name": "get_weather", "input": {"city": "Tokyo"}}
            ])
        );
        assert_eq!(
            response["usage"],
            json!({"input_tokens": 170, "output_tokens": 23, "cached_tokens": 0, "thinking_tokens": 12})
        );
    }

    fn follow_up_after(model: &str, content: &Value) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Weather in Paris and Tokyo?"}]},
                {"role": "assistant", "content": content},
                {"role": "tool", "tool_call_id": "call_paris", "content": [{"type": "text", "text": "18C"}]},
                {"role": "tool", "tool_call_id": "call_tokyo", "content": [{"type": "text", "text": "22C"}]}
            ]
        }))
    }

    #[test]
    fn qwen_replays_reasoning_and_tool_calls_byte_for_byte_on_the_next_turn() {
        let response = parse_quiet(&live_tool_call_stream()).unwrap();
        let body = body_for(&follow_up_after(QWEN, &response["content"]), &no_config());
        let messages = body["messages"].as_array().unwrap();

        let assistant = &messages[1];
        assert_eq!(
            assistant["reasoning"].as_str().map(str::as_bytes),
            Some(REASONING.as_bytes()),
            "the streamed reasoning goes back byte for byte"
        );
        assert_eq!(assistant["content"], json!("I'll check both."));
        assert_eq!(
            assistant["tool_calls"],
            json!([
                {"id": "call_paris", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                {"id": "call_tokyo", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}}
            ])
        );
        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "call_paris", "content": "18C"})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_tokyo", "content": "22C"})
        );
        assert_no_forbidden_key(&body);
    }

    #[test]
    fn gpt_oss_never_replays_reasoning_but_keeps_the_rest_of_the_turn() {
        let response = parse_quiet(&live_tool_call_stream()).unwrap();
        let body = body_for(&follow_up_after(GPT, &response["content"]), &no_config());
        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": "I'll check both.",
                "tool_calls": [
                    {"id": "call_paris", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                    {"id": "call_tokyo", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}}
                ]
            })
        );
        assert!(!contains_key_anywhere(&body, "reasoning"), "{body}");
        assert_eq!(body["reasoning_effort"], json!("medium"));
    }

    /// System, user, a tool-call turn with reasoning and text, its tool result, a plain turn with
    /// reasoning, then the next user turn.
    fn replay_request(model: &str) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": 8192,
            "system": "You are a careful assistant.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "How long is notes.txt?"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "Read the file first. \"Quoted\" — ü."},
                    {"type": "text", "text": "Reading it."},
                    {"type": "tool_call", "id": "call_1", "name": "read_file", "input": {"path": "notes.txt"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "a\nb\nc"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "Three lines."},
                    {"type": "text", "text": "It has three lines."}
                ]},
                {"role": "user", "content": [{"type": "text", "text": "And how many words?"}]}
            ],
            "tools": [
                {"name": "read_file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
            ],
            "params": {},
            "prompt_cache_key": "replay-check:0.1.0:ctx_01"
        }))
    }

    #[test]
    fn qwen_replays_reasoning_on_every_assistant_message_by_default() {
        let body = body_for(&replay_request(QWEN), &no_config());
        assert_eq!(
            body.to_string(),
            r#"{"max_tokens":8192,"messages":[{"content":"You are a careful assistant.","role":"system"},{"content":"How long is notes.txt?","role":"user"},{"content":"Reading it.","reasoning":"Read the file first. \"Quoted\" — ü.","role":"assistant","tool_calls":[{"function":{"arguments":"{\"path\":\"notes.txt\"}","name":"read_file"},"id":"call_1","type":"function"}]},{"content":"a\nb\nc","role":"tool","tool_call_id":"call_1"},{"content":"It has three lines.","reasoning":"Three lines.","role":"assistant"},{"content":"And how many words?","role":"user"}],"model":"Qwen/Qwen3.8-27B:cerebras","reasoning_effort":"high","stream":true,"stream_options":{"include_usage":true},"tools":[{"function":{"name":"read_file","parameters":{"properties":{"path":{"type":"string"}},"type":"object"}},"type":"function"}]}"#
        );
        assert_eq!(
            body_for(&replay_request(QWEN), &raw(r#"{"replay_reasoning":true}"#)),
            body,
            "true is the default"
        );
    }

    #[test]
    fn qwen_with_replay_reasoning_false_sends_no_reasoning_and_nothing_else_differs() {
        let body = body_for(&replay_request(QWEN), &raw(r#"{"replay_reasoning":false}"#));
        assert!(!contains_key_anywhere(&body, "reasoning"), "{body}");

        let mut expected = body_for(&replay_request(QWEN), &no_config());
        for message in expected["messages"].as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("reasoning");
        }
        assert_eq!(body, expected);
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "assistant",
                "content": "Reading it.",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"}}]
            })
        );
    }

    #[test]
    fn an_assistant_turn_without_thinking_sends_no_reasoning_member() {
        let request = request_from(json!({
            "model": QWEN,
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Summary of earlier work."}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": ""},
                    {"type": "text", "text": "Continuing."}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_call", "id": "call_1", "name": "read_file", "input": {"path": "a"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "x"}]}
            ]
        }));
        let body = body_for(&request, &no_config());
        for assistant in [&body["messages"][1], &body["messages"][2]] {
            assert!(
                assistant.get("reasoning").is_none(),
                "the member is omitted entirely, never sent as an empty string: {assistant}"
            );
        }
    }

    #[test]
    fn a_tool_call_turn_with_no_text_sends_null_content() {
        let request = request_from(json!({
            "model": QWEN,
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read it."}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "Read first."},
                    {"type": "tool_call", "id": "call_1", "name": "read_file", "input": {"path": "a"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "x"}]}
            ]
        }));
        let assistant = &body_for(&request, &no_config())["messages"][1];
        assert_eq!(assistant["content"], Value::Null);
        assert!(
            assistant.get("content").is_some(),
            "content is sent as null, not left out"
        );
        assert_eq!(assistant["reasoning"], json!("Read first."));
        assert_eq!(assistant["tool_calls"][0]["id"], json!("call_1"));
    }

    #[test]
    fn no_forbidden_member_appears_for_any_entry_or_config() {
        for entry in SUPPORTED_MODELS {
            let replays: &[Option<bool>] = match entry.replay {
                Replay::Accepted { .. } => &[None, Some(true), Some(false)],
                Replay::Refused { .. } => &[None],
            };
            let efforts = std::iter::once(None).chain(entry.effort_values.iter().map(Some));
            for effort_value in efforts {
                for replay in replays {
                    let mut config = serde_json::Map::new();
                    if let Some(value) = effort_value {
                        config.insert("reasoning_effort".to_string(), json!(value));
                    }
                    if let Some(replay) = replay {
                        config.insert("replay_reasoning".to_string(), json!(replay));
                    }
                    let config = raw(&Value::Object(config).to_string());
                    for request in [
                        replay_request(entry.id),
                        tools_request(entry.id),
                        simple_request(entry.id),
                    ] {
                        let body = body_for(&request, &config);
                        assert_no_forbidden_key(&body);
                        assert_eq!(
                            body["reasoning_effort"],
                            json!(effort_value.copied().unwrap_or(entry.default_effort))
                        );
                    }
                }
            }
        }
    }

    // ── S7 — streaming paths ──────────────────────────────────────────────────

    const INTERLEAVED_STREAM: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"The sky \"}}]}\n",
        "\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Rayleigh \"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"scatters blue.\"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"scattering.\"},\"finish_reason\":\"stop\"}]}\n",
        "data: [DONE]\n",
    );

    #[test]
    fn streamed_reasoning_and_text_go_to_separate_emitters_in_order() {
        let mut text_seen: Vec<String> = Vec::new();
        let mut thinking_seen: Vec<String> = Vec::new();

        let response = parse_huggingface_sse_body(
            INTERLEAVED_STREAM,
            FIELD,
            &mut |t: &str| text_seen.push(t.to_string()),
            &mut |t: &str| thinking_seen.push(t.to_string()),
        )
        .expect("the fixture stream must parse");

        assert_eq!(thinking_seen, vec!["The sky ", "scatters blue."]);
        assert_eq!(text_seen, vec!["Rayleigh ", "scattering."]);
        assert_eq!(
            response["content"],
            json!([
                {"type": "thinking", "text": "The sky scatters blue."},
                {"type": "text", "text": "Rayleigh scattering."}
            ])
        );
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn only_the_entrys_reasoning_field_is_read_as_reasoning() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"elsewhere\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"here\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );
        let mut thinking: Vec<String> = Vec::new();
        let response = parse_huggingface_sse_body(stream, FIELD, &mut |_| {}, &mut |t| {
            thinking.push(t.to_string())
        })
        .unwrap();
        assert_eq!(thinking, vec!["here"]);
        assert_eq!(
            response["content"][0],
            json!({"type": "thinking", "text": "here"})
        );
        assert_eq!(response["content"].as_array().unwrap().len(), 2);

        // The field is a parameter, not a constant: another field name routes that member.
        let other =
            parse_huggingface_sse_body(stream, "reasoning_content", &mut |_| {}, &mut |_| {})
                .unwrap();
        assert_eq!(
            other["content"][0],
            json!({"type": "thinking", "text": "elsewhere"})
        );
    }

    #[test]
    fn a_tool_call_delivered_whole_in_one_chunk_parses() {
        let body = [
            tool_delta(
                0,
                Some("call_1"),
                Some("read_file"),
                Some(r#"{"path":"README.md"}"#),
            ),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();
        let (result, signals) = parse_with_signals(&body);
        assert_eq!(
            result["content"],
            json!([{"type": "tool_call", "id": "call_1", "name": "read_file", "input": {"path": "README.md"}}])
        );
        assert_eq!(
            signals,
            vec![started("call_1", "read_file"), input_bytes("call_1", 20)]
        );
    }

    #[test]
    fn tool_call_signals_its_start_then_rising_input_byte_totals() {
        let body = [
            tool_delta(0, Some("call_1"), Some("bash"), Some("")),
            tool_delta(0, None, None, Some(r#"{"cmd":"#)),
            tool_delta(0, None, None, Some(r#""ZQXJ ü"}"#)),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let (result, signals) = parse_with_signals(&body);

        assert_eq!(
            signals,
            vec![
                started("call_1", "bash"),
                input_bytes("call_1", 7),
                input_bytes("call_1", 17),
            ]
        );
        assert_eq!(r#"{"cmd":"ZQXJ ü"}"#.len(), 17);
        let call = &result["content"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["name"], "bash");
        assert_eq!(call["input"], json!({"cmd": "ZQXJ ü"}));
        assert_no_signal_carries(&signals, "ZQXJ");
    }

    #[test]
    fn tool_calls_at_two_indexes_keep_separate_ids_and_totals() {
        let body = [
            tool_delta(0, Some("call_a"), Some("read"), Some("")),
            tool_delta(1, Some("call_b"), Some("bash"), Some("")),
            tool_delta(0, None, None, Some(r#"{"path":"#)),
            tool_delta(1, None, None, Some(r#"{"cmd":"#)),
            tool_delta(0, None, None, Some(r#""ZQXJ.md"}"#)),
            tool_delta(1, None, None, Some(r#""ls ZQXJ"}"#)),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let (result, signals) = parse_with_signals(&body);

        assert_eq!(
            signals,
            vec![
                started("call_a", "read"),
                started("call_b", "bash"),
                input_bytes("call_a", 8),
                input_bytes("call_b", 7),
                input_bytes("call_a", 18),
                input_bytes("call_b", 17),
            ]
        );
        assert_eq!(result["content"][0]["id"], "call_a");
        assert_eq!(result["content"][0]["input"], json!({"path": "ZQXJ.md"}));
        assert_eq!(result["content"][1]["id"], "call_b");
        assert_eq!(result["content"][1]["input"], json!({"cmd": "ls ZQXJ"}));
        assert_no_signal_carries(&signals, "ZQXJ");
    }

    #[test]
    fn tool_call_whose_id_arrives_late_is_reported_once_it_does() {
        let head = [
            tool_delta(0, None, Some("bash"), Some(r#"{"cmd":"#)),
            tool_delta(0, None, None, Some(r#""ZQXJ"}"#)),
        ]
        .concat();
        let (_, before_the_id) =
            parse_huggingface_sse_body_with_signals(&head, FIELD, &mut |_| {}, &mut |_| {});
        assert_eq!(before_the_id, vec![]);

        let body = [
            head,
            tool_delta(0, Some("call_late"), None, None),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let (result, signals) = parse_with_signals(&body);

        assert_eq!(
            signals,
            vec![started("call_late", "bash"), input_bytes("call_late", 14)]
        );
        assert_eq!(result["content"][0]["id"], "call_late");
        assert_eq!(result["content"][0]["input"], json!({"cmd": "ZQXJ"}));
        assert_no_signal_carries(&signals, "ZQXJ");
    }

    #[test]
    fn tool_call_that_never_gets_an_id_signals_nothing_and_is_returned() {
        let body = [
            tool_delta(0, None, Some("bash"), Some(r#"{"cmd":"#)),
            tool_delta(0, None, None, Some(r#""ZQXJ"}"#)),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let (result, signals) = parse_with_signals(&body);

        assert_eq!(signals, vec![]);
        assert_eq!(
            result["content"],
            json!([{"type": "tool_call", "id": "", "name": "bash", "input": {"cmd": "ZQXJ"}}])
        );
    }

    #[test]
    fn tool_call_with_no_arguments_signals_only_its_start() {
        let body = [
            tool_delta(0, Some("call_1"), Some("list"), Some("")),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let (result, signals) = parse_with_signals(&body);

        assert_eq!(signals, vec![started("call_1", "list")]);
        assert_eq!(result["content"][0]["input"], json!({}));
    }

    #[test]
    fn tool_call_signals_leave_reasoning_and_content_chunks_unchanged() {
        let body = [
            chunk(json!({"reasoning": "Check the "})),
            chunk(json!({"reasoning": "tree."})),
            chunk(json!({"content": "Listing."})),
            tool_delta(0, Some("call_1"), Some("bash"), Some("")),
            chunk(json!({"reasoning": " Then stop."})),
            tool_delta(0, None, None, Some(r#"{"cmd":"ZQXJ"}"#)),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let mut text: Vec<String> = Vec::new();
        let mut thinking: Vec<String> = Vec::new();
        let (result, signals) = parse_huggingface_sse_body_with_signals(
            &body,
            FIELD,
            &mut |c| text.push(c.to_string()),
            &mut |c| thinking.push(c.to_string()),
        );

        assert_eq!(text, vec!["Listing."]);
        assert_eq!(thinking, vec!["Check the ", "tree.", " Then stop."]);
        assert_eq!(
            result.unwrap()["content"],
            json!([
                {"type": "thinking", "text": "Check the tree. Then stop."},
                {"type": "text", "text": "Listing."},
                {"type": "tool_call", "id": "call_1", "name": "bash", "input": {"cmd": "ZQXJ"}}
            ])
        );
        assert_eq!(
            signals,
            vec![started("call_1", "bash"), input_bytes("call_1", 14)]
        );
        assert_no_signal_carries(&signals, "ZQXJ");
    }

    #[test]
    fn a_stream_without_done_fails_transiently_even_after_a_finish_reason() {
        let finished_but_cut = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"The answer \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        let cut_mid_text = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"Let me \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"The result is\"},\"finish_reason\":null}]}\n",
        );
        let partial_tool_call = tool_delta(0, Some("call_1"), Some("bash"), Some("{\"cmd\":"));
        let done_as_text = "data: {\"choices\":[{\"delta\":{\"content\":\"data: [DONE]\"},\"finish_reason\":\"stop\"}]}\n";
        for body in [
            finished_but_cut,
            cut_mid_text,
            partial_tool_call.as_str(),
            done_as_text,
            "",
        ] {
            let failure = parse_quiet(body).unwrap_err();
            assert_eq!(
                failure,
                Failure::transient("driver: Hugging Face stream ended before data: [DONE]"),
                "body = {body:?}"
            );
            assert_eq!(
                failure.metadata(),
                vec![("transient".to_string(), "true".to_string())]
            );
        }
    }

    #[test]
    fn a_stream_with_done_and_no_finish_reason_fails_transiently() {
        let no_reason = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"\"}]}\n",
            "data: [DONE]\n",
        );
        for body in [no_reason, "data: [DONE]\n"] {
            assert_eq!(
                parse_quiet(body).unwrap_err(),
                Failure::transient("driver: Hugging Face stream ended with no finish_reason"),
                "body = {body:?}"
            );
        }
    }

    #[test]
    fn a_last_done_line_with_no_trailing_newline_still_ends_the_stream() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]",
        );
        assert_eq!(
            parse_quiet(stream).unwrap()["stop_reason"],
            json!("end_turn")
        );
    }

    #[test]
    fn a_streamed_length_finish_reason_maps_to_max_tokens() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"length\"}]}\n",
            "data: [DONE]\n",
        );
        assert_eq!(
            parse_quiet(stream).unwrap()["stop_reason"],
            json!("max_tokens")
        );
    }

    fn error_chunk(error: Value) -> String {
        format!("data: {}\n", json!({ "error": error }))
    }

    #[test]
    fn an_error_chunk_naming_a_status_murmur_retries_fails_transiently() {
        for error in [
            json!({"code": 503, "message": "backend unavailable"}),
            json!({"code": "503", "message": "backend unavailable"}),
            json!({"code": 429, "message": "rate limited"}),
            json!({"code": 408}),
            json!({"code": 500}),
            json!({"code": 502}),
            json!({"code": 504}),
            json!({"code": 529, "type": "overloaded"}),
            json!({"code": 599}),
            json!({"status": 503}),
            json!({"status_code": "429"}),
            json!({"code": "rate_limit_exceeded", "status": 429}),
        ] {
            let expected =
                Failure::transient(format!("driver: Hugging Face stream error: {error}"));
            assert_eq!(stream_error_failure(&error), expected, "error {error}");
            for body in [
                error_chunk(error.clone()),
                format!("{TEXT_DELTA}{}", error_chunk(error.clone())),
                format!("{}data: [DONE]\n", error_chunk(error.clone())),
            ] {
                assert_eq!(parse_quiet(&body).unwrap_err(), expected, "body = {body:?}");
            }
        }
    }

    #[test]
    fn every_other_error_chunk_fails_plainly() {
        for error in [
            json!({"code": 501}),
            json!({"code": 505}),
            json!({"code": 400, "message": "bad request"}),
            json!({"code": 401}),
            json!({"code": 402, "message": "no credits"}),
            json!({"code": 404}),
            json!({"code": 422}),
            json!({"code": 600}),
            json!({"code": "rate_limit_exceeded"}),
            json!({"code": "5O3"}),
            json!({"code": ""}),
            json!({"code": 503.0}),
            json!({"code": -503}),
            json!({"message": "boom"}),
            json!("Error making prediction"),
            json!(503),
        ] {
            let expected = Failure::plain(format!("driver: Hugging Face stream error: {error}"));
            assert_eq!(stream_error_failure(&error), expected, "error {error}");
            assert_eq!(
                parse_quiet(&error_chunk(error.clone())).unwrap_err(),
                expected
            );
            assert!(expected.metadata().is_empty());
        }

        // A null `error` member is no error.
        let null_error = format!(
            "data: {}\ndata: [DONE]\n",
            json!({"error": null, "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
        );
        assert_eq!(
            parse_quiet(&null_error).unwrap()["stop_reason"],
            json!("end_turn")
        );
    }

    #[test]
    fn an_error_chunk_takes_precedence_over_a_later_finish_reason() {
        let body = format!(
            "{}data: {}\ndata: [DONE]\n",
            error_chunk(json!({"message": "boom"})),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        );
        assert_eq!(
            parse_quiet(&body).unwrap_err(),
            Failure::plain(r#"driver: Hugging Face stream error: {"message":"boom"}"#)
        );
    }

    #[test]
    fn usage_on_every_chunk_is_merged_and_the_last_report_wins() {
        let running = |prompt: u64, completion: u64, cached: u64, reasoning: u64| {
            json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "total_tokens": prompt + completion,
                "prompt_tokens_details": {"cached_tokens": cached},
                "completion_tokens_details": {"reasoning_tokens": reasoning}
            })
        };
        let body = [
            format!(
                "data: {}\n",
                json!({"choices": [{"delta": {"reasoning": "Hm."}}], "usage": running(900, 1, 800, 1)})
            ),
            format!(
                "data: {}\n",
                json!({"choices": [{"delta": {"content": "Hi"}}], "usage": running(900, 2, 800, 1)})
            ),
            format!(
                "data: {}\n",
                json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": running(900, 3, 800, 1)})
            ),
            format!(
                "data: {}\n",
                json!({"choices": [], "usage": {"prompt_tokens": 900, "completion_tokens": 4}})
            ),
            "data: [DONE]\n".to_string(),
        ]
        .concat();
        assert_eq!(
            parse_quiet(&body).unwrap()["usage"],
            json!({"input_tokens": 100, "output_tokens": 4, "cached_tokens": 800, "thinking_tokens": 1})
        );
    }

    #[test]
    fn a_tool_call_index_past_the_limit_fails_the_turn_without_allocating() {
        for index in [1024, 4_294_967_296, u64::MAX] {
            let line = tool_delta(index, Some("call_x"), Some("bash"), Some(""));
            let mut state = StreamState::default();
            let mut signals = Vec::new();
            let result = process_huggingface_sse_line(
                line.trim_end_matches('\n'),
                FIELD,
                &mut state,
                &mut |_| {},
                &mut |_| {},
                &mut |s| signals.push(RecordedSignal::from(s)),
            );
            assert_eq!(
                result,
                Err(Failure::plain(format!(
                    "driver: tool call index {index} is past the limit of 1024 per turn"
                )))
            );
            assert_eq!(state.tool_states.capacity(), 0);
            assert_eq!(signals, vec![]);
        }

        let body = [
            tool_delta(0, Some("call_a"), Some("read"), Some("")),
            tool_delta(1024, Some("call_x"), Some("bash"), Some("")),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();
        let (result, _) =
            parse_huggingface_sse_body_with_signals(&body, FIELD, &mut |_| {}, &mut |_| {});
        assert_eq!(
            result,
            Err(Failure::plain(
                "driver: tool call index 1024 is past the limit of 1024 per turn"
            ))
        );
    }

    #[test]
    fn a_tool_call_at_the_last_index_under_the_limit_is_kept() {
        let body = [
            tool_delta(
                1023,
                Some("call_last"),
                Some("bash"),
                Some(r#"{"cmd":"ls"}"#),
            ),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let (result, _) = parse_with_signals(&body);

        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        let call = &result["content"][0];
        assert_eq!(call["id"], "call_last");
        assert_eq!(call["name"], "bash");
        assert_eq!(call["input"], json!({"cmd": "ls"}));
    }

    // ── S8 — usage mapping with cached tokens ─────────────────────────────────

    /// `input_tokens + cached_tokens + cache_write_tokens`, an absent member counted as `0` —
    /// the request count the runtime derives from a `usage` object.
    fn reported_request(usage: &Value) -> u64 {
        ["input_tokens", "cached_tokens", "cache_write_tokens"]
            .iter()
            .map(|key| usage.get(*key).and_then(Value::as_u64).unwrap_or(0))
            .sum()
    }

    fn buffered_with_usage(usage: Value) -> Value {
        let response = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "hi"}}],
            "usage": usage
        });
        translate_huggingface_response_to_murmur(&response, FIELD).unwrap()
    }

    /// The final usage on a chunk of its own with `choices: []`, as `include_usage` sends it.
    fn streamed_with_usage(usage: Value) -> Value {
        let finish =
            json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "hi"}}]});
        let usage = json!({"choices": [], "usage": usage});
        parse_quiet(&format!("data: {finish}\ndata: {usage}\ndata: [DONE]\n")).unwrap()
    }

    fn both_paths(usage: Value) -> [Value; 2] {
        [
            buffered_with_usage(usage.clone()),
            streamed_with_usage(usage),
        ]
    }

    /// An OpenAI-shaped usage object with every member Cerebras's Chat Completions reference
    /// lists, cache reads and reasoning included.
    const DOCUMENTED_USAGE: &str = r#"{"prompt_tokens":14820,"completion_tokens":3200,"total_tokens":18020,"prompt_tokens_details":{"cached_tokens":9216},"completion_tokens_details":{"reasoning_tokens":1100}}"#;

    #[test]
    fn cached_tokens_sit_beside_input_tokens_and_sum_to_the_prompt_total() {
        let documented: Value = serde_json::from_str(DOCUMENTED_USAGE).unwrap();
        for response in both_paths(documented) {
            let usage = &response["usage"];
            assert_eq!(
                usage,
                &json!({"input_tokens": 5604, "output_tokens": 3200, "cached_tokens": 9216, "thinking_tokens": 1100})
            );
            assert_eq!(
                reported_request(usage),
                14820,
                "input + cached is prompt_tokens"
            );
            assert!(usage.get("cache_write_tokens").is_none());
        }
    }

    #[test]
    fn a_reported_zero_cached_count_is_kept_and_input_equals_the_total() {
        for response in both_paths(json!({
            "prompt_tokens": 170,
            "completion_tokens": 23,
            "prompt_tokens_details": {"cached_tokens": 0}
        })) {
            assert_eq!(
                response["usage"],
                json!({"input_tokens": 170, "output_tokens": 23, "cached_tokens": 0})
            );
        }
    }

    #[test]
    fn with_no_details_input_equals_the_total_and_no_cached_count_is_reported() {
        for response in both_paths(json!({"prompt_tokens": 170, "completion_tokens": 23})) {
            assert_eq!(
                response["usage"],
                json!({"input_tokens": 170, "output_tokens": 23})
            );
        }
    }

    #[test]
    fn a_cached_count_above_the_total_leaves_input_tokens_out() {
        for response in both_paths(json!({
            "prompt_tokens": 100,
            "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 101}
        })) {
            assert_eq!(
                response["usage"],
                json!({"output_tokens": 4, "cached_tokens": 101})
            );
        }
    }

    #[test]
    fn a_malformed_cached_count_is_dropped_and_subtracted_as_zero() {
        for cached in [json!("9200"), json!(-1), json!(1.5), Value::Null] {
            for response in both_paths(json!({
                "prompt_tokens": 14820,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": cached.clone()}
            })) {
                assert_eq!(
                    response["usage"],
                    json!({"input_tokens": 14820, "output_tokens": 4}),
                    "cached_tokens = {cached}"
                );
            }
        }
    }

    #[test]
    fn no_usage_means_no_usage_key() {
        for response in both_paths(json!({})) {
            assert!(response.get("usage").is_none(), "got {response}");
        }
        let plain = json!({"choices": [{"finish_reason": "stop", "message": {"content": "hi"}}]});
        assert!(translate_huggingface_response_to_murmur(&plain, FIELD)
            .unwrap()
            .get("usage")
            .is_none());
        assert!(parse_quiet(INTERLEAVED_STREAM)
            .unwrap()
            .get("usage")
            .is_none());
    }

    #[test]
    fn streamed_usage_is_read_from_a_chunk_that_also_carries_choices() {
        let documented: Value = serde_json::from_str(DOCUMENTED_USAGE).unwrap();
        let chunk = json!({
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "hi"}}],
            "usage": documented
        });
        assert_eq!(
            parse_quiet(&format!("data: {chunk}\ndata: [DONE]\n")).unwrap()["usage"],
            json!({"input_tokens": 5604, "output_tokens": 3200, "cached_tokens": 9216, "thinking_tokens": 1100})
        );
    }

    // ── S9 — stop reasons ─────────────────────────────────────────────────────

    #[test]
    fn stop_tool_calls_and_length_map_to_the_runtime_stop_reasons_on_both_paths() {
        for (finish, stop) in [
            ("stop", "end_turn"),
            ("tool_calls", "tool_call"),
            ("length", "max_tokens"),
        ] {
            assert_eq!(map_finish_reason(finish), Ok(stop));
            let buffered =
                json!({"choices": [{"finish_reason": finish, "message": {"content": "x"}}]});
            assert_eq!(
                translate_huggingface_response_to_murmur(&buffered, FIELD).unwrap()["stop_reason"],
                json!(stop)
            );
            let stream = format!(
                "data: {}\ndata: [DONE]\n",
                json!({"choices": [{"delta": {"content": "x"}, "finish_reason": finish}]})
            );
            assert_eq!(parse_quiet(&stream).unwrap()["stop_reason"], json!(stop));
        }
    }

    #[test]
    fn every_other_finish_reason_is_a_plain_error_naming_it_on_both_paths() {
        for reason in ["content_filter", "abort", "bogus"] {
            let expected = Failure::plain(format!(
                "driver: unsupported Hugging Face finish_reason '{reason}'"
            ));
            let buffered =
                json!({"choices": [{"finish_reason": reason, "message": {"content": ""}}]});
            assert_eq!(
                translate_huggingface_response_to_murmur(&buffered, FIELD).unwrap_err(),
                expected
            );
            let stream = format!(
                "data: {}\ndata: [DONE]\n",
                json!({"choices": [{"delta": {"content": "x"}, "finish_reason": reason}]})
            );
            assert_eq!(parse_quiet(&stream).unwrap_err(), expected);
        }
    }

    #[test]
    fn a_response_with_no_finish_reason_is_refused() {
        for finish_reason in [None, Some(Value::Null), Some(json!("")), Some(json!(1))] {
            let mut choice = json!({"message": {"role": "assistant", "content": "partial"}});
            if let Some(finish_reason) = finish_reason.clone() {
                choice["finish_reason"] = finish_reason;
            }
            assert_eq!(
                translate_huggingface_response_to_murmur(&json!({"choices": [choice]}), FIELD)
                    .unwrap_err(),
                Failure::plain("driver: Hugging Face response has no finish_reason"),
                "finish_reason = {finish_reason:?}"
            );
        }
    }

    // ── S10 — failure paths never panic ───────────────────────────────────────

    #[test]
    fn an_image_block_is_refused_before_any_body_is_built() {
        let image = json!({"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}});
        for model in [GPT, QWEN] {
            for messages in [
                json!([{"role": "user", "content": [{"type": "text", "text": "What is this?"}, image]}]),
                json!([
                    {"role": "user", "content": [{"type": "text", "text": "first"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                    {"role": "user", "content": [image]}
                ]),
            ] {
                let request =
                    request_from(json!({"model": model, "max_tokens": 8192, "messages": messages}));
                assert_eq!(
                    refusal(&request, &no_config()),
                    "driver: driver-huggingface sends text only and refuses an image block"
                );
            }
        }
    }

    #[test]
    fn malformed_requests_are_refused_by_name() {
        let orphan_tool = request_from(json!({
            "model": QWEN,
            "max_tokens": 8192,
            "messages": [{"role": "tool", "content": [{"type": "text", "text": "out"}]}]
        }));
        let config = resolve_config(entry(QWEN), &no_config()).unwrap();
        assert_eq!(
            translate_murmur_request_to_huggingface(&orphan_tool, entry(QWEN), &config)
                .unwrap_err(),
            "driver: tool message is missing required field 'tool_call_id'"
        );

        let bad_role = request_from(json!({
            "model": QWEN,
            "max_tokens": 8192,
            "messages": [{"role": "developer", "content": []}]
        }));
        assert_eq!(
            refusal(&bad_role, &no_config()),
            "driver: unsupported message role 'developer'"
        );
    }

    #[test]
    fn non_json_tool_arguments_are_a_plain_error_on_both_paths() {
        let buffered = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {"tool_calls": [{"id": "call_1", "function": {"name": "bash", "arguments": "{not json"}}]}
            }]
        });
        let err = translate_huggingface_response_to_murmur(&buffered, FIELD).unwrap_err();
        assert!(
            err.message
                .starts_with("driver: failed to parse Hugging Face tool call arguments JSON: "),
            "got {err}"
        );
        assert_eq!(err.retry, None);

        let stream = [
            tool_delta(0, Some("call_1"), Some("bash"), Some("{not json")),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();
        let err = parse_quiet(&stream).unwrap_err();
        assert!(
            err.message
                .starts_with("driver: failed to parse Hugging Face tool call arguments JSON: "),
            "got {err}"
        );
        assert_eq!(err.retry, None);
    }

    #[test]
    fn a_response_with_no_choices_is_refused() {
        for response in [json!({}), json!({"choices": []}), json!({"choices": "x"})] {
            assert_eq!(
                translate_huggingface_response_to_murmur(&response, FIELD).unwrap_err(),
                Failure::plain("driver: Hugging Face response missing choices[0]")
            );
        }
    }

    #[test]
    fn every_provider_error_status_is_plain_and_carries_the_status_and_body() {
        for status in [
            400, 401, 402, 403, 404, 408, 413, 422, 429, 500, 502, 503, 504,
        ] {
            let body = r#"{"error":"x"}"#;
            let failure = status_failure(status, body);
            assert_eq!(
                failure,
                Failure::plain(format!("HTTP {status}: {body}")),
                "status = {status}"
            );
            assert_eq!(
                error_payload(&failure.message),
                json!({"stop_reason": "error", "error": format!("HTTP {status}: {body}")})
            );
        }
    }

    #[test]
    fn the_json_fallback_reads_the_entrys_field_and_orders_thinking_then_text_then_calls() {
        let buffered = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "reasoning": "Two independent calls.",
                    "reasoning_content": "not this one",
                    "content": "I'll check the weather for both cities.",
                    "tool_calls": [
                        {"id": "call_a", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                        {"id": "call_b", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}}
                    ]
                }
            }]
        });
        assert_eq!(
            translate_huggingface_response_to_murmur(&buffered, FIELD).unwrap()["content"],
            json!([
                {"type": "thinking", "text": "Two independent calls."},
                {"type": "text", "text": "I'll check the weather for both cities."},
                {"type": "tool_call", "id": "call_a", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_call", "id": "call_b", "name": "get_weather", "input": {"city": "Tokyo"}}
            ])
        );
    }

    #[test]
    fn an_empty_tool_calls_array_counts_as_no_calls() {
        let buffered = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": "Rayleigh scattering.", "tool_calls": []}
            }]
        });
        let response = translate_huggingface_response_to_murmur(&buffered, FIELD).unwrap();
        assert_eq!(
            response["content"],
            json!([{"type": "text", "text": "Rayleigh scattering."}])
        );
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn a_completed_turn_carries_no_transient_marker() {
        let stream = format!(
            "{TEXT_DELTA}data: {}\ndata: [DONE]\n",
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        );
        let response = parse_quiet(&stream).unwrap();
        assert_eq!(response["stop_reason"], json!("end_turn"));
        assert!(!contains_key_anywhere(&response, "transient"));
    }

    // ── S11 — source and manifest shape ───────────────────────────────────────

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
        // Where inference goes is the host's fact. A literal provider URL in a driver is a
        // value that would become silently effective the moment the host stopped supplying
        // one. The needles are assembled rather than written out so that this assertion does
        // not itself become the only occurrence of what it forbids.
        let source = include_str!("lib.rs");
        for provider_url in [
            concat!("https://", "api."),
            concat!("router.", "hugging"),
            concat!("huggingface", ".co"),
            concat!("hf", ".co"),
            concat!("groq", ".com"),
            concat!("cerebras", ".ai"),
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
            run_inner.contains("format!(\"{}/chat/completions\", endpoint.trim_end_matches('/'))"),
            "requests go to <endpoint>/chat/completions"
        );
        let names: Vec<&str> = request_headers(0).iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["content-type", "content-length"]);
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
            run_inner.contains("assemble_huggingface_streaming_response(state)"),
            "the guest loop must hand its whole state, saw_done included, to the assembler"
        );
        assert!(
            run_inner.contains("reasoning_field = provider_request.entry.reasoning_field"),
            "the guest reads reasoning from the entry's field"
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
        // Both backends deliver reasoning on its own delta field, so an inline reasoning-tag
        // scanner here would be a second, silently-wrong reasoning path. The needle is assembled
        // rather than written out so that this assertion does not itself become the only
        // occurrence.
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
            concat!("hf_", "token"),
            concat!("\"author", "ization\""),
            concat!("\"x-api", "-key\""),
            concat!("bear", "er "),
        ] {
            assert!(
                !non_test.contains(needle),
                "driver-huggingface: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn manifest_declares_the_upstream_auth_scheme_the_gateway_will_read() {
        // The runtime builds the auth header from these two fields, so a dropped quote pair
        // (`value: {key}` is a YAML flow mapping, not a string) or a renamed key leaves the
        // gateway with no scheme to apply and every request unauthenticated.
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "drivers/driver-huggingface/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[derive(serde::Deserialize)]
    struct Manifest {
        models: BTreeMap<String, ManifestLimits>,
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    struct ManifestLimits {
        max_output_tokens: u64,
        context_window: u64,
    }

    #[test]
    fn manifest_models_block_matches_the_source_model_table() {
        // Parsed with the crate murmur parses the bundled manifest with. Only the two limits are
        // allowed: no entry declares an input ceiling or vision.
        let manifest = include_str!("../murmur.yaml");
        let models = serde_norway::from_str::<Manifest>(manifest)
            .expect("murmur.yaml's models: block must parse")
            .models;
        assert_eq!(
            models.keys().map(String::as_str).collect::<Vec<_>>(),
            {
                let mut ids: Vec<&str> = SUPPORTED_MODELS.iter().map(|e| e.id).collect();
                ids.sort_unstable();
                ids
            },
            "murmur.yaml declares exactly the source table's ids"
        );
        for entry in SUPPORTED_MODELS {
            let limits = &models[entry.id];
            assert!(entry.id.contains(':'), "{} names its backend", entry.id);
            assert_eq!(
                limits.max_output_tokens, entry.max_output_tokens,
                "{}",
                entry.id
            );
            assert_eq!(limits.context_window, entry.context_window, "{}", entry.id);
            assert!(
                manifest.contains(&format!("\n  \"{}\":\n", entry.id)),
                "the models: key for {} is double-quoted",
                entry.id
            );
        }
        assert_eq!(
            SUPPORTED_MODELS
                .iter()
                .map(|e| (e.id, e.max_output_tokens, e.context_window))
                .collect::<Vec<_>>(),
            [(GPT, 65_536, 131_072), (QWEN, 32_768, 65_536)]
        );
    }

    #[test]
    fn readme_model_scope_row_states_the_source_limits() {
        let readme = include_str!("../README.md");
        let start = readme
            .find("\n## Model scope\n")
            .expect("README must have a Model scope section");
        let section = &readme[start + 1..];
        let section = &section[..section[1..].find("\n## ").map_or(section.len(), |i| i + 1)];
        for entry in SUPPORTED_MODELS {
            let row = section
                .lines()
                .find(|line| line.starts_with(&format!("| `{}` |", entry.id)))
                .unwrap_or_else(|| panic!("Model scope must hold a row for {}", entry.id));
            for number in [entry.max_output_tokens, entry.context_window] {
                assert!(
                    row.contains(&format!("`{number}`")),
                    "the {} row must state {number}: {row}",
                    entry.id
                );
            }
            assert!(
                row.contains("`none`"),
                "the {} row declares no max_input_tokens: {row}",
                entry.id
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
