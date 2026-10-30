// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use murmur_driver_stream::Failure;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

// ── Model table ───────────────────────────────────────────────────────────────

/// The limits Baseten publishes for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelLimits {
    /// The largest `max_tokens` Baseten accepts for the model. A larger value is refused here
    /// rather than spent on a provider 400.
    max_output_tokens: u64,
    /// The model's context window in tokens. No request path reads it: it is the number
    /// `murmur.yaml` declares under `models:`, and a test keeps the two in step.
    #[cfg(test)]
    context_window: u64,
}

/// Every model this driver serves, with its limits. This table is the classification seam a
/// second model would be added at — nothing else in the crate branches on the model name — and
/// `murmur.yaml`'s `models:` block must declare the same numbers for each entry.
const SUPPORTED_MODELS: &[(&str, ModelLimits)] = &[(
    "zai-org/GLM-5.3",
    ModelLimits {
        max_output_tokens: 262_144,
        #[cfg(test)]
        context_window: 1_048_576,
    },
)];

/// Look the model up by its exact id — no case folding, no prefix match — before a request
/// body is built, so an unsupported model costs no HTTP call. Baseten serves `-Fast` and
/// `-Flash` variants of the same model under their own ids; they are refused like any other.
fn validate_model(model: &str) -> Result<&'static ModelLimits, String> {
    SUPPORTED_MODELS
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, limits)| limits)
        .ok_or_else(|| {
            format!(
                "driver: model '{model}' is not supported by driver-baseten. Supported models: {}.",
                SUPPORTED_MODELS
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Refuse an output cap above the model's ceiling rather than clamping it silently.
fn validate_max_tokens(model: &str, limits: &ModelLimits, max_tokens: u64) -> Result<(), String> {
    if max_tokens > limits.max_output_tokens {
        return Err(format!(
            "driver: inference.max_tokens {max_tokens} exceeds the Baseten max_tokens ceiling of \
             {} for {model}",
            limits.max_output_tokens
        ));
    }
    Ok(())
}

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a hard error on the first inference call, before
// any HTTP request is dispatched — a declared setting that is parsed and ignored is a defect,
// not a convenience.

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &["reasoning_effort", "replay_reasoning"];

/// The effort tiers `zai-org/GLM-5.3` runs at on Baseten, in ascending order of spend. Baseten's
/// gateway enum also holds `minimal`, `medium` and `xhigh`, which its reasoning guide does not
/// list for GLM 5.3, so they are refused here by name and never mapped onto another tier.
const REASONING_EFFORT_VALUES: &[&str] = &["low", "high", "max"];

/// Baseten's own default for `zai-org/GLM-5.3`. Stamped on every request so the effective tier
/// is visible in a recorded request rather than left to drift with the provider's default.
const DEFAULT_REASONING_EFFORT: &str = "high";

/// `none` is refused with its own message rather than the generic wrong-value one: Baseten
/// accepts it for GLM 5.3 but runs it at `low`, so a capsule asking for no reasoning would get
/// reasoning it did not ask for and be billed for it.
const REASONING_EFFORT_NONE_REFUSAL: &str = "driver: inference.driver.config 'reasoning_effort' \
     cannot be \"none\" on driver-baseten: Baseten runs none at low on zai-org/GLM-5.3, which \
     always reasons. Set it to \"low\".";

/// Earlier assistant turns carry their `reasoning_content` unless a capsule sets
/// `replay_reasoning: false`, so an unconfigured capsule sends the history Baseten produced.
const DEFAULT_REPLAY_REASONING: bool = true;

/// The dials read from `inference.driver.config`.
#[derive(Debug, Clone, PartialEq)]
struct DriverConfig {
    /// `reasoning_effort` — one of [`REASONING_EFFORT_VALUES`], lowercased. An unconfigured
    /// capsule gets [`DEFAULT_REASONING_EFFORT`].
    reasoning_effort: String,
    /// `replay_reasoning` — whether earlier assistant messages carry `reasoning_content`. An
    /// unconfigured capsule gets [`DEFAULT_REPLAY_REASONING`].
    replay_reasoning: bool,
}

impl Default for DriverConfig {
    fn default() -> Self {
        DriverConfig {
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
            replay_reasoning: DEFAULT_REPLAY_REASONING,
        }
    }
}

/// Read the driver-config JSON into the dial set. Runs as the first fallible step of
/// `run_inner`, before any environment read other than `MURMUR_INFERENCE_DRIVER_CONFIG` itself,
/// so a rejected config cannot reach the network.
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

    let reasoning_effort = parse_reasoning_effort(config.get("reasoning_effort"))?;
    let replay_reasoning = parse_replay_reasoning(config.get("replay_reasoning"))?;
    Ok(DriverConfig {
        reasoning_effort,
        replay_reasoning,
    })
}

/// Trimmed and matched case-insensitively; sent lowercased. The rejected value appears verbatim
/// in the error alongside the accepted set, so a manifest holding `medium` says so.
fn parse_reasoning_effort(value: Option<&Value>) -> Result<String, String> {
    match value {
        None => Ok(DEFAULT_REASONING_EFFORT.to_string()),
        Some(Value::String(raw)) => {
            let normalised = raw.trim().to_ascii_lowercase();
            if REASONING_EFFORT_VALUES.contains(&normalised.as_str()) {
                Ok(normalised)
            } else if normalised == "none" {
                Err(REASONING_EFFORT_NONE_REFUSAL.to_string())
            } else {
                Err(format!(
                    "driver: inference.driver.config 'reasoning_effort' must be one of {}, got \"{raw}\"",
                    REASONING_EFFORT_VALUES.join(", ")
                ))
            }
        }
        Some(other) => Err(format!(
            "driver: inference.driver.config 'reasoning_effort' must be a string, got {other}"
        )),
    }
}

/// Only a JSON boolean is accepted: `"false"`, `0` and `null` are refused rather than coerced.
fn parse_replay_reasoning(value: Option<&Value>) -> Result<bool, String> {
    match value {
        None => Ok(DEFAULT_REPLAY_REASONING),
        Some(Value::Bool(replay)) => Ok(*replay),
        Some(other) => Err(format!(
            "driver: inference.driver.config 'replay_reasoning' must be a boolean, got {other}"
        )),
    }
}

// ── Murmur request types ──────────────────────────────────────────────────────

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
    /// The host's prompt-cache routing hint, fixed for every turn of a task. It is read only by
    /// [`session_affinity`], which hashes it into the `x-session-affinity` header; it never
    /// reaches the body, where Baseten's request schema defines no member for it. A null,
    /// non-string or blank value means "no key" and is never an error.
    #[serde(default)]
    prompt_cache_key: Value,
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
    // driver and read back by it — is where preserved reasoning lives. No runtime concept and no
    // interface widening is involved.
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

// ── reasoning_content helpers ─────────────────────────────────────────────────

/// Extract reasoning text from the first `Thinking` block in a message's content, if any.
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

// ── Session affinity ──────────────────────────────────────────────────────────

/// The request header Baseten routes on: requests carrying the same value go to the same
/// replica, where the shared prompt prefix is most likely still in the KV cache.
const SESSION_AFFINITY_HEADER: &str = "x-session-affinity";

/// The `x-session-affinity` value for a request: the lowercase hex SHA-256 of its trimmed
/// `prompt_cache_key`, or `None` when the key is absent, null, not a string or blank.
///
/// Baseten asks for an opaque value, a UUID or a hash of an internal conversation id. The raw
/// key opens with the capsule's name and version, so it is hashed rather than sent: the digest is
/// as stable across a task's turns as the key itself, and names nothing.
fn session_affinity(request: &MurmurRequest) -> Option<String> {
    let key = request
        .prompt_cache_key
        .as_str()
        .map(str::trim)
        .filter(|key| !key.is_empty())?;
    let digest = Sha256::digest(key.as_bytes());
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Every header `run_inner` sets on the request, in order: `content-type`, `content-length`,
/// then `x-session-affinity` when the request has a usable `prompt_cache_key`. The credential
/// header is the runtime's to attach, never the driver's.
fn request_headers(request: &MurmurRequest, body_len: usize) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("content-type", "application/json".to_string()),
        ("content-length", body_len.to_string()),
    ];
    if let Some(affinity) = session_affinity(request) {
        headers.push((SESSION_AFFINITY_HEADER, affinity));
    }
    headers
}

// ── Request translation ───────────────────────────────────────────────────────

/// Every check that can be made without building a body or touching the network.
fn preflight_request(request: &MurmurRequest) -> Result<(), String> {
    let limits = validate_model(&request.model)?;
    validate_max_tokens(&request.model, limits, request.max_tokens)?;
    let has_image = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, MurmurContentBlock::Image { .. }))
    });
    if has_image {
        return Err(
            "driver: driver-baseten sends text only and refuses an image block".to_string(),
        );
    }
    Ok(())
}

/// The body `run_inner` puts on the wire: pre-flight, translate, stamp the streaming flags.
/// Tests assert on this rather than on a bare translation.
fn build_provider_request(request: &MurmurRequest, config: &DriverConfig) -> Result<Value, String> {
    preflight_request(request)?;
    let mut body = translate_murmur_request_to_baseten(request, config)?;
    stamp_streaming_flags(&mut body);
    Ok(body)
}

/// Baseten's request object is `additionalProperties: false`, so the body holds only members
/// its Chat Completions schema defines: no top-level `thinking`, `clear_thinking` or
/// `tool_stream` (Z.ai's own), no `prompt_cache_key` and no `max_completion_tokens`. The one
/// open member, `chat_template_args`, holds only `clear_thinking`, the inverse of
/// `replay_reasoning`, and never `enable_thinking`.
fn translate_murmur_request_to_baseten(
    request: &MurmurRequest,
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
    body.insert("model".to_string(), Value::String(request.model.clone()));
    // Baseten's schema spells the output cap `max_tokens`, capped at 262144.
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    // Top-level, not nested under a `reasoning` object, and stamped on every request including
    // the unconfigured one.
    body.insert(
        "reasoning_effort".to_string(),
        Value::String(config.reasoning_effort.clone()),
    );

    // GLM-5.3's chat template reads `clear_thinking` (default `false`) and, when it is `true`,
    // renders each earlier turn's reasoning as an empty think block. Stamped on every request so
    // a recorded body shows which setting applied; `false` is the template's own default, so the
    // replaying body reaches the model as it would without the member.
    body.insert(
        "chat_template_args".to_string(),
        json!({"clear_thinking": !config.replay_reasoning}),
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

/// Rebuild an assistant turn as Baseten returned it, with its reasoning when `replay_reasoning`
/// is set.
///
/// With `replay_reasoning`, the turn's reasoning goes back as `reasoning_content`, on a turn that
/// ended in tool calls and on a plain-text turn alike. Baseten documents `reasoning_content` on
/// prior assistant messages (its reasoning guide requires it for Beam), GLM 5.3's chat template
/// consumes it, and replaying the bytes the model generated keeps the prompt prefix equal to what
/// the replica already holds, which keeps it cacheable. Baseten answered HTTP 200 to
/// `zai-org/GLM-5.3` requests replaying it on a tool-call turn in a live capsule run on
/// 2026-10-27. Without `replay_reasoning` no assistant message carries the member, including
/// the turns of the current tool loop.
///
/// The text a tool-call turn carried beside its calls is kept as `content`; `content` is `null`
/// only on a tool-call turn with no text. When no `thinking` block is present the
/// `reasoning_content` member is omitted entirely — never sent as an empty string — because that
/// is the ordinary state of a history that has been compacted.
fn translate_assistant_message(
    message: &MurmurMessage,
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
        if let Some(reasoning_content) = thinking_text(&message.content) {
            msg["reasoning_content"] = json!(reasoning_content);
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

// ── Provider token usage ──────────────────────────────────────────────────────

/// Baseten's raw token counts, carried onto the reserved top-level `usage` object of the
/// translated response. Every member is independently optional: a count the provider did not
/// report is omitted rather than sent as `0`, because the host records a reported `0` as a real
/// zero and a fabricated one reads as a cache miss on the `inference` trace event.
///
/// Baseten reports the request total with the cache reads inside it, while the usage contract
/// puts `cached_tokens` beside `input_tokens`. So the raw total is held here as `prompt_tokens`,
/// and the contract's `input_tokens` is derived from it only in `to_value`, after every streamed
/// chunk has been merged.
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
    /// Take every member `later` reports, keeping the current value for the rest. Baseten sends
    /// running usage on every chunk by default, so the last report of each member wins.
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
    /// — so `input_tokens + cached_tokens` is the provider's total. It is omitted when the total
    /// is unreported or the cache count exceeds it: such a report is inconsistent, and no value
    /// is invented for it. `thinking_tokens` is part of `output_tokens`, as Baseten reports it.
    /// Baseten reports no cache-write count, so `cache_write_tokens` is never emitted. `None`
    /// when no member survived, in which case the response carries no `usage` key at all.
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

/// Read a Baseten `usage` object. A member that is absent, null, or not a non-negative integer
/// is dropped; its siblings are kept. `total_tokens` and the speculative-decoding counters
/// `accepted_prediction_tokens` and `rejected_prediction_tokens` are not billing counts and are
/// not read.
fn extract_baseten_usage(usage: &Value) -> UsageTokens {
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

/// Force streaming on, overriding any `stream` or `stream_options` key from `params`, and ask
/// for the usage on the final chunk.
fn stamp_streaming_flags(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
        obj.insert("stream_options".to_string(), json!({"include_usage": true}));
    }
}

// ── Stop-reason mapping ───────────────────────────────────────────────────────

/// Map a Baseten `finish_reason` to a murmur `stop_reason`. Baseten documents `stop`, `length`
/// and `tool_calls`; any other reason is an error naming it rather than a guess. Each caller
/// refuses a missing reason with its own path's message.
fn map_finish_reason(finish_reason: &str) -> Result<&'static str, Failure> {
    match finish_reason {
        "stop" => Ok("end_turn"),
        "tool_calls" => Ok("tool_call"),
        "length" => Ok("max_tokens"),
        other => Err(Failure::plain(format!(
            "driver: unsupported Baseten finish_reason '{other}'"
        ))),
    }
}

// ── Provider errors ───────────────────────────────────────────────────────────

/// The failure for a chunk whose top-level `error` is present and non-null, the member
/// serialised compactly: `driver: Baseten stream error: <error>`.
///
/// Transient: Baseten documents no error vocabulary for a failure inside a `2xx` stream, and
/// tells a client to treat an explicit error event as a failed request and retry it as a new
/// one.
fn stream_error_failure(error: &Value) -> Failure {
    Failure::transient(format!("driver: Baseten stream error: {error}"))
}

/// The failure for a provider status `>= 400`: `HTTP <status>: <body>`, carrying Baseten's own
/// body so the operator sees what it said. Always plain: murmur retries `429` and the `5xx`
/// statuses Baseten names retryable (`500`, `502`, `503`, `529`) by itself, and Baseten's `429`
/// is a rate or capacity limit that clears; an exhausted balance is `402`, which murmur never
/// retries.
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
    serde_json::from_str(arguments)
        .map_err(|err| format!("driver: failed to parse Baseten tool call arguments JSON: {err}"))
}

// ── Response translation (non-streaming fallback) ─────────────────────────────

/// The non-streaming JSON path, reached when the response body's first non-whitespace byte is
/// `{` or `[` — scripted test servers, and providers that ignore `stream: true`.
fn translate_baseten_response_to_murmur(response: &Value) -> Result<Value, Failure> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: Baseten response missing choices[0]".to_string())?;

    // Absent, `null`, non-string or empty: the turn did not finish.
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| "driver: Baseten response has no finish_reason".to_string())?;
    let stop_reason = map_finish_reason(finish_reason)?;

    let message = choice.get("message").unwrap_or(&Value::Null);

    let reasoning = message
        .get("reasoning_content")
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
        .map(extract_baseten_usage)
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

/// Process one complete SSE line. Returns `Ok(true)` on `data: [DONE]`, and `Err` for a chunk
/// carrying a non-null top-level `error` ([`stream_error_failure`]) or a tool call index past
/// [`murmur_driver_stream::MAX_SLOTS_PER_TURN`].
///
/// Baseten streams OpenAI-shaped chat-completions chunks plus a separate `reasoning_content`
/// delta. `delta.reasoning_content` is routed to `emit_thinking`, `delta.content` to
/// `emit_text`; the two never mix, and nothing in this crate scans the text for reasoning
/// sentinels. A tool call arrives either in fragments — the first carrying its `id` and
/// `function.name`, later ones only `arguments` pieces — or whole in one chunk; both are
/// accumulated by `index`.
fn process_baseten_sse_line(
    line: &str,
    tool_states: &mut Vec<ToolCallState>,
    stop_reason: &mut Option<String>,
    usage: &mut UsageTokens,
    emit_text: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
    signal: &mut impl FnMut(ToolCallSignal<'_>),
) -> Result<bool, Failure> {
    if line == "data: [DONE]" {
        return Ok(true);
    }
    let Some(json_str) = line.strip_prefix("data: ") else {
        return Ok(false);
    };
    let Ok(data) = serde_json::from_str::<Value>(json_str) else {
        return Ok(false);
    };

    // An error ends the turn here, so it is reported as Baseten's and never as a cut stream.
    if let Some(error) = data.get("error").filter(|error| !error.is_null()) {
        return Err(stream_error_failure(error));
    }

    // Read from any chunk, before the `choices`-shaped early returns below: Baseten sends running
    // usage beside `choices` on every chunk, and the final report on a chunk whose `choices` is
    // empty.
    if let Some(reported) = data.get("usage").filter(|value| !value.is_null()) {
        usage.merge_from(extract_baseten_usage(reported));
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
        *stop_reason = Some(reason.to_string());
    }

    let Some(delta) = choice.and_then(|c| c.get("delta")) else {
        return Ok(false);
    };

    if let Some(rc) = delta.get("reasoning_content").and_then(Value::as_str) {
        if !rc.is_empty() {
            emit_thinking(rc);
        }
    }

    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            emit_text(content);
        }
    }

    if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in tc_arr {
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
            let state = murmur_driver_stream::slot_mut(tool_states, index)
                .map_err(|err| Failure::plain(format!("driver: tool call {err}")))?;
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                if state.id.is_empty() {
                    state.id = id.to_string();
                }
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(Value::as_str) {
                    if state.name.is_empty() {
                        state.name = name.to_string();
                    }
                }
                if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                    state.arguments.push_str(args);
                }
            }
            state.signal_due(signal);
        }
    }

    Ok(false)
}

/// Turn the accumulated stream state into the murmur response, its `content` in the order
/// [`ordered_content`] fixes.
///
/// Baseten ends every complete stream with `data: [DONE]`, and can end one early with no error
/// event when a request times out or the model becomes unavailable. So a stream that never sent
/// `[DONE]` was cut after the provider's `2xx`, whatever it carried before, and that is checked
/// first; one that sent it with no `finish_reason` ended before the turn did. Both fail
/// transiently.
fn assemble_baseten_streaming_response(
    text_acc: &str,
    reasoning_acc: &str,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
    saw_done: bool,
    usage: UsageTokens,
) -> Result<Value, Failure> {
    if !saw_done {
        return Err(Failure::transient(
            "driver: Baseten stream ended before data: [DONE]",
        ));
    }
    let finish_reason = stop_reason
        .as_deref()
        .ok_or_else(|| Failure::transient("driver: Baseten stream ended with no finish_reason"))?;
    let stop_reason_str = map_finish_reason(finish_reason)?;

    let mut tool_calls = Vec::new();
    for state in tool_states {
        if state.id.is_empty() && state.name.is_empty() {
            continue;
        }
        let input = parse_tool_arguments(&state.arguments)?;
        tool_calls.push(json!({
            "type": "tool_call",
            "id": state.id,
            "name": state.name,
            "input": input,
        }));
    }

    Ok(murmur_response(
        stop_reason_str,
        ordered_content(reasoning_acc, text_acc, tool_calls),
        usage,
    ))
}

/// Parse a complete SSE body string, driving the same line processor the wasm module drives.
/// This is how the streaming assertions run host-side, with no network and no provider account.
#[cfg(test)]
fn parse_baseten_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, Failure> {
    parse_baseten_sse_body_with_signals(body, emit_text, emit_thinking).0
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
fn parse_baseten_sse_body_with_signals<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> (Result<Value, Failure>, Vec<RecordedSignal>) {
    let mut text_acc = String::new();
    let mut reasoning_acc = String::new();
    let mut tool_states: Vec<ToolCallState> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut usage = UsageTokens::default();
    let mut saw_done = false;
    let mut signals = Vec::new();

    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        let done = {
            let mut et = |t: &str| {
                emit_text(t);
                text_acc.push_str(t);
            };
            let mut eth = |t: &str| {
                emit_thinking(t);
                reasoning_acc.push_str(t);
            };
            process_baseten_sse_line(
                line,
                &mut tool_states,
                &mut stop_reason,
                &mut usage,
                &mut et,
                &mut eth,
                &mut |s| signals.push(RecordedSignal::from(s)),
            )
        };
        match done {
            Ok(true) => {
                saw_done = true;
                break;
            }
            Ok(false) => {}
            Err(err) => return (Err(err), signals),
        }
    }

    let result = assemble_baseten_streaming_response(
        &text_acc,
        &reasoning_acc,
        tool_states,
        stop_reason,
        saw_done,
        usage,
    );
    (result, signals)
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
        assemble_baseten_streaming_response, build_provider_request, error_payload,
        parse_driver_config, process_baseten_sse_line, request_headers, status_failure,
        translate_baseten_response_to_murmur, MurmurRequest, ToolCallSignal, ToolCallState,
        UsageTokens,
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

    pub struct BasetenDriver;

    impl exports::murmur::tool::run::Guest for BasetenDriver {
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
            }
        }
    }

    fn run_inner(input: exports::murmur::tool::run::ToolInput) -> Result<Value, Failure> {
        // First fallible step: a config the driver cannot honour fails here, before any other
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

        // Model, output-cap and image checks happen inside here, pre-flight: a rejected request
        // costs no HTTP call.
        let provider_request = build_provider_request(&murmur_request, &config)?;

        let body = serde_json::to_vec(&provider_request)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));

        let headers = request_headers(&murmur_request, body.len());

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
            // Non-streaming fallback (test servers, providers ignoring stream:true).
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
                .map_err(|err| format!("driver: failed to parse Baseten response JSON: {err}"))?;
            translate_baseten_response_to_murmur(&json)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut text_acc = String::new();
            let mut reasoning_acc = String::new();
            let mut tool_states: Vec<ToolCallState> = Vec::new();
            let mut stop_reason: Option<String> = None;
            let mut usage = UsageTokens::default();
            let mut saw_done = false;

            let handle_line = |line: &str,
                               tool_states: &mut Vec<ToolCallState>,
                               stop_reason: &mut Option<String>,
                               usage: &mut UsageTokens,
                               text_acc: &mut String,
                               reasoning_acc: &mut String|
             -> Result<bool, Failure> {
                let mut et = |t: &str| {
                    murmur::stream::events::emit_chunk(t);
                    text_acc.push_str(t);
                };
                let mut eth = |t: &str| {
                    murmur::stream::events::emit_thinking_chunk(t);
                    reasoning_acc.push_str(t);
                };
                process_baseten_sse_line(
                    line,
                    tool_states,
                    stop_reason,
                    usage,
                    &mut et,
                    &mut eth,
                    &mut signal_tool_call,
                )
            };

            // Process bytes already read.
            for &b in &first {
                if b == b'\n' {
                    let line = String::from_utf8_lossy(&line_buf);
                    let line = line.trim_end_matches('\r');
                    saw_done = handle_line(
                        line,
                        &mut tool_states,
                        &mut stop_reason,
                        &mut usage,
                        &mut text_acc,
                        &mut reasoning_acc,
                    )?;
                    line_buf.clear();
                    if saw_done {
                        break;
                    }
                } else {
                    line_buf.push(b);
                }
            }

            // Continue reading remaining chunks.
            if !saw_done {
                'outer: loop {
                    let chunk = read_chunk(&stream, status)?;
                    if chunk.is_empty() {
                        break;
                    }
                    for &b in &chunk {
                        if b == b'\n' {
                            let line = String::from_utf8_lossy(&line_buf);
                            let line = line.trim_end_matches('\r');
                            saw_done = handle_line(
                                line,
                                &mut tool_states,
                                &mut stop_reason,
                                &mut usage,
                                &mut text_acc,
                                &mut reasoning_acc,
                            )?;
                            line_buf.clear();
                            if saw_done {
                                break 'outer;
                            }
                        } else {
                            line_buf.push(b);
                        }
                    }
                }
            }

            // A last line the body ended without a newline after, `data: [DONE]` included.
            if !saw_done && !line_buf.is_empty() {
                let line = String::from_utf8_lossy(&line_buf);
                let line = line.trim_end_matches('\r');
                saw_done = handle_line(
                    line,
                    &mut tool_states,
                    &mut stop_reason,
                    &mut usage,
                    &mut text_acc,
                    &mut reasoning_acc,
                )?;
            }

            drop(stream);
            let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
            assemble_baseten_streaming_response(
                &text_acc,
                &reasoning_acc,
                tool_states,
                stop_reason,
                saw_done,
                usage,
            )?
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

    export!(BasetenDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Nothing here talks to the provider. Every assertion is driven by a fixture and the host-side
// whole-stream parser.

#[cfg(test)]
mod tests {
    use super::{
        build_provider_request, error_payload, map_finish_reason, parse_baseten_sse_body,
        parse_baseten_sse_body_with_signals, parse_driver_config, process_baseten_sse_line,
        request_headers, session_affinity, status_failure, translate_baseten_response_to_murmur,
        translate_murmur_request_to_baseten, DriverConfig, MurmurRequest, RecordedSignal,
        UsageTokens, SESSION_AFFINITY_HEADER, SUPPORTED_MODELS,
    };
    use murmur_driver_stream::Failure;
    use serde_json::{json, Value};

    // ── Fixtures and helpers ──────────────────────────────────────────────────

    const MODEL: &str = "zai-org/GLM-5.3";

    /// A `prompt_cache_key` shaped as the host builds it, `<capsule-name>:<version>:<context-id>`.
    const CACHE_KEY: &str = "baseten-check:0.1.0:ctx_01";

    /// The lowercase hex SHA-256 of [`CACHE_KEY`], computed independently with `sha256sum`.
    const CACHE_KEY_SHA256: &str =
        "6b61ac41f6fbe712845f7a2665503097e5754bf480f0b89333fd6c3036e6197e";

    /// Members Baseten's request schema does not define, or that belong to another provider.
    /// `clear_thinking` is checked at the top level only, by [`assert_no_forbidden_key`]: it
    /// appears nested under `chat_template_args`.
    const FORBIDDEN_KEYS: &[&str] = &[
        "thinking",
        "tool_stream",
        "prompt_cache_key",
        "max_completion_tokens",
    ];

    fn request_from(value: Value) -> MurmurRequest {
        serde_json::from_value(value).expect("fixture must deserialize as a murmur request")
    }

    /// The canonical envelope the host builds, with the reserved `prompt_cache_key` present.
    fn simple_request() -> MurmurRequest {
        request_from(json!({
            "model": MODEL,
            "max_tokens": 8192,
            "system": "You are a careful assistant.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Why is the sky blue?"}]}
            ],
            "tools": [],
            "params": {},
            "prompt_cache_key": CACHE_KEY
        }))
    }

    fn config(raw: &str) -> DriverConfig {
        parse_driver_config(Some(raw)).expect("fixture config must parse")
    }

    fn effort_config(raw: &str) -> Result<DriverConfig, String> {
        parse_driver_config(Some(&json!({ "reasoning_effort": raw }).to_string()))
    }

    fn body_for(request: &MurmurRequest, config: &DriverConfig) -> Value {
        build_provider_request(request, config).expect("fixture request must translate")
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

    /// Also pins `chat_template_args` to its one member, so nothing else (`enable_thinking`
    /// above all) can ride in it.
    fn assert_no_forbidden_key(body: &Value) {
        for absent in FORBIDDEN_KEYS {
            assert!(
                !contains_key_anywhere(body, absent),
                "'{absent}' must not appear at any nesting level: {body}"
            );
        }
        assert!(
            body.get("clear_thinking").is_none(),
            "clear_thinking is not a top-level member: {body}"
        );
        let template_args = body["chat_template_args"]
            .as_object()
            .unwrap_or_else(|| panic!("chat_template_args is stamped on every request: {body}"));
        assert_eq!(
            template_args.keys().collect::<Vec<_>>(),
            ["clear_thinking"],
            "chat_template_args holds clear_thinking only: {body}"
        );
        assert!(template_args["clear_thinking"].is_boolean(), "{body}");
    }

    fn parse_quiet(body: &str) -> Result<Value, Failure> {
        parse_baseten_sse_body(body, &mut |_: &str| {}, &mut |_: &str| {})
    }

    fn request_for_model(model: &str, max_tokens: u64) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }))
    }

    fn request_with_cache_key(key: Option<Value>) -> MurmurRequest {
        let mut request = json!({
            "model": MODEL,
            "max_tokens": 8192,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        if let Some(key) = key {
            request["prompt_cache_key"] = key;
        }
        request_from(request)
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
        let (result, signals) = parse_baseten_sse_body_with_signals(body, &mut |_| {}, &mut |_| {});
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

    // ── S1 — model check ──────────────────────────────────────────────────────

    #[test]
    fn only_the_exact_model_id_zai_org_glm_5_3_is_accepted() {
        assert!(
            build_provider_request(&request_for_model(MODEL, 8192), &DriverConfig::default())
                .is_ok()
        );

        for rejected in [
            "zai-org/glm-5.3",
            "zai-org/GLM-5.3-Fast",
            "zai-org/GLM-5.3-Flash",
            "glm-5.3",
            "moonshotai/Kimi-K3",
        ] {
            let err = build_provider_request(
                &request_for_model(rejected, 8192),
                &DriverConfig::default(),
            )
            .expect_err("only zai-org/GLM-5.3 is served, matched exactly");
            assert_eq!(
                err,
                format!(
                    "driver: model '{rejected}' is not supported by driver-baseten. \
                     Supported models: zai-org/GLM-5.3."
                )
            );
        }
    }

    #[test]
    fn an_unsupported_model_is_refused_before_any_other_request_check() {
        // An oversized cap, an image and an orphan tool message would each be refused too; the
        // model check is the one that answers, so it runs before a body exists.
        let request = request_from(json!({
            "model": "glm-5.3",
            "max_tokens": 10_000_000u64,
            "messages": [
                {"role": "tool", "content": []},
                {"role": "user", "content": [{"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}}]}
            ]
        }));
        let err = build_provider_request(&request, &DriverConfig::default()).unwrap_err();
        assert!(
            err.contains("is not supported by driver-baseten"),
            "got {err}"
        );
    }

    // ── S2 — output cap ───────────────────────────────────────────────────────

    #[test]
    fn output_cap_is_written_as_max_tokens_up_to_the_262144_ceiling() {
        let body = body_for(&request_for_model(MODEL, 262_144), &DriverConfig::default());
        assert_eq!(body["max_tokens"], json!(262_144));
        assert!(
            !contains_key_anywhere(&body, "max_completion_tokens"),
            "the cap is spelled max_tokens: {body}"
        );

        let err =
            build_provider_request(&request_for_model(MODEL, 262_145), &DriverConfig::default())
                .expect_err("a cap above the ceiling must be refused pre-flight");
        assert_eq!(
            err,
            "driver: inference.max_tokens 262145 exceeds the Baseten max_tokens ceiling of 262144 \
             for zai-org/GLM-5.3"
        );
    }

    // ── S3 — each config value is accepted or refused by name ─────────────────

    #[test]
    fn absent_empty_whitespace_and_empty_object_configs_stamp_high() {
        let expected = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(expected["reasoning_effort"], json!("high"));
        for raw in [None, Some(""), Some("   "), Some("{}")] {
            let config = parse_driver_config(raw).expect("a default config must parse");
            assert_eq!(config, DriverConfig::default(), "config {raw:?}");
            assert_eq!(
                body_for(&simple_request(), &config),
                expected,
                "config {raw:?}"
            );
        }
    }

    #[test]
    fn low_high_and_max_are_sent_lowercased_at_the_top_level() {
        for (raw, sent) in [
            ("low", "low"),
            (" HIGH ", "high"),
            ("max", "max"),
            ("Max\t", "max"),
        ] {
            let body = body_for(&simple_request(), &effort_config(raw).unwrap());
            assert_eq!(body["reasoning_effort"], json!(sent), "effort {raw:?}");
            assert!(
                body.get("reasoning").is_none(),
                "effort is top-level: {body}"
            );
        }
    }

    #[test]
    fn none_is_refused_with_its_own_message_in_any_case_or_padding() {
        for raw in ["none", "NONE", " None ", "\tnone\n"] {
            assert_eq!(
                effort_config(raw).unwrap_err(),
                "driver: inference.driver.config 'reasoning_effort' cannot be \"none\" on \
                 driver-baseten: Baseten runs none at low on zai-org/GLM-5.3, which always \
                 reasons. Set it to \"low\".",
                "effort {raw:?}"
            );
        }
    }

    #[test]
    fn effort_tiers_glm_5_3_does_not_have_are_refused_with_the_raw_value_quoted() {
        for raw in ["medium", "xhigh", "minimal", " Medium ", "maximum"] {
            assert_eq!(
                effort_config(raw).unwrap_err(),
                format!(
                    "driver: inference.driver.config 'reasoning_effort' must be one of low, \
                     high, max, got \"{raw}\""
                )
            );
        }
    }

    #[test]
    fn a_non_string_reasoning_effort_is_refused() {
        for (raw, shown) in [
            (r#"{"reasoning_effort":3}"#, "3"),
            (r#"{"reasoning_effort":true}"#, "true"),
            (r#"{"reasoning_effort":null}"#, "null"),
            (
                r#"{"reasoning_effort":{"level":"max"}}"#,
                r#"{"level":"max"}"#,
            ),
        ] {
            assert_eq!(
                parse_driver_config(Some(raw)).unwrap_err(),
                format!(
                    "driver: inference.driver.config 'reasoning_effort' must be a string, got \
                     {shown}"
                )
            );
        }
    }

    #[test]
    fn unrecognised_config_keys_are_reported_sorted_with_the_accepted_set() {
        assert_eq!(
            parse_driver_config(Some(
                r#"{"thinking":"enabled","chat_template_args":{},"clear_thinking":true,"reasoning_effort":"max","replay_reasoning":false}"#
            ))
            .unwrap_err(),
            "driver: unrecognised inference.driver.config key(s): chat_template_args, \
             clear_thinking, thinking. Accepted keys: reasoning_effort, replay_reasoning"
        );
    }

    #[test]
    fn a_config_that_is_not_a_json_object_is_refused() {
        let not_json = parse_driver_config(Some("not json")).unwrap_err();
        assert!(
            not_json.starts_with("driver: inference.driver.config is not valid JSON: "),
            "got {not_json}"
        );
        for raw in ["[]", "\"max\"", "3"] {
            assert_eq!(
                parse_driver_config(Some(raw)).unwrap_err(),
                "driver: inference.driver.config must be a JSON object"
            );
        }
    }

    // ── S4 — request translation with tools ───────────────────────────────────

    fn tools_request() -> MurmurRequest {
        request_from(json!({
            "model": MODEL,
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
            "prompt_cache_key": CACHE_KEY
        }))
    }

    #[test]
    fn a_request_with_tools_becomes_the_chat_completions_body() {
        let body = body_for(&tools_request(), &config(r#"{"reasoning_effort":"max"}"#));

        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected_keys = vec![
            "chat_template_args",
            "max_tokens",
            "messages",
            "model",
            "reasoning_effort",
            "stream",
            "stream_options",
            "temperature",
            "tools",
        ];
        expected_keys.sort_unstable();
        assert_eq!(keys, expected_keys, "the body holds these members only");

        assert_eq!(body["model"], json!(MODEL));
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
        assert_eq!(body["reasoning_effort"], json!("max"));
        assert_eq!(body["chat_template_args"], json!({"clear_thinking": false}));
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
            !body.to_string().contains(CACHE_KEY),
            "the raw routing hint must not reach the provider body: {body}"
        );
    }

    #[test]
    fn a_params_key_the_body_already_holds_does_not_override_it() {
        let request = request_from(json!({
            "model": MODEL,
            "max_tokens": 1000,
            "messages": [],
            "params": {
                "model": "zai-org/GLM-5.3-Fast",
                "max_tokens": 5,
                "messages": [{"role": "user", "content": "injected"}],
                "reasoning_effort": "medium",
                "chat_template_args": {"enable_thinking": false, "clear_thinking": true}
            }
        }));
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(body["model"], json!(MODEL));
        assert_eq!(body["max_tokens"], json!(1000));
        assert_eq!(body["messages"], json!([]));
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(body["chat_template_args"], json!({"clear_thinking": false}));
    }

    // ── S5 — a hook-shaped request with no tools ──────────────────────────────

    #[test]
    fn a_hook_shaped_request_without_tools_sends_no_tools_member() {
        let request = request_from(json!({
            "model": MODEL,
            "max_tokens": 4096,
            "system": "Summarise the conversation.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Edit the README."}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Done."}]},
                {"role": "user", "content": [{"type": "text", "text": "Summarise."}]}
            ]
        }));
        let body = body_for(&request, &DriverConfig::default());

        assert!(
            body.get("tools").is_none(),
            "no tools declared means no member: {body}"
        );
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(body["chat_template_args"], json!({"clear_thinking": false}));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(
            body["messages"][2],
            json!({"role": "assistant", "content": "Done."})
        );
        assert_no_forbidden_key(&body);
        assert!(
            request_headers(&request, 10)
                .iter()
                .all(|(name, _)| *name != SESSION_AFFINITY_HEADER),
            "a request without a cache key sends no affinity header"
        );
    }

    // ── S6 — reasoning replayed on the next turn ──────────────────────────────

    const REASONING: &str = "Two independent calls: Paris, then Tokyo. \"Quote\" — ü.";

    /// Reasoning deltas, one content delta, two calls in fragments at index 0 and 1, a
    /// `finish_reason: "tool_calls"` chunk, a usage-only chunk with `choices: []`, `[DONE]`.
    fn live_tool_call_stream() -> String {
        [
            chunk(json!({"role": "assistant", "reasoning_content": "Two independent calls: "})),
            chunk(json!({"reasoning_content": "Paris, then Tokyo. \"Quote\""})),
            chunk(json!({"reasoning_content": " — ü."})),
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

    #[test]
    fn reasoning_and_tool_calls_are_replayed_byte_for_byte_on_the_next_turn() {
        let response = parse_quiet(&live_tool_call_stream()).unwrap();

        // The runtime replays that content array verbatim as the next turn's assistant message.
        let follow_up = request_from(json!({
            "model": MODEL,
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Weather in Paris and Tokyo?"}]},
                {"role": "assistant", "content": response["content"]},
                {"role": "tool", "tool_call_id": "call_paris", "content": [{"type": "text", "text": "18C"}]},
                {"role": "tool", "tool_call_id": "call_tokyo", "content": [{"type": "text", "text": "22C"}]}
            ]
        }));
        let body = body_for(&follow_up, &DriverConfig::default());
        let messages = body["messages"]
            .as_array()
            .expect("messages must be an array");

        let assistant = &messages[1];
        assert_eq!(
            assistant["reasoning_content"].as_str().map(str::as_bytes),
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
    }

    #[test]
    fn a_plain_assistant_turn_replays_its_reasoning() {
        let request = request_from(json!({
            "model": MODEL,
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Why?"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "text": "Because of scattering."},
                        {"type": "text", "text": "Rayleigh scattering."}
                    ]
                },
                {"role": "user", "content": [{"type": "text", "text": "Say more."}]}
            ]
        }));
        assert_eq!(
            body_for(&request, &DriverConfig::default())["messages"][1],
            json!({
                "role": "assistant",
                "content": "Rayleigh scattering.",
                "reasoning_content": "Because of scattering."
            })
        );
    }

    #[test]
    fn an_assistant_turn_without_thinking_sends_no_reasoning_content_key() {
        let request = request_from(json!({
            "model": MODEL,
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
        let body = build_provider_request(&request, &DriverConfig::default())
            .expect("a compacted history is the ordinary state, not an error");
        for assistant in [&body["messages"][1], &body["messages"][2]] {
            assert!(
                assistant.get("reasoning_content").is_none(),
                "the member is omitted entirely, never sent as an empty string: {assistant}"
            );
        }
    }

    #[test]
    fn a_tool_call_turn_with_no_text_sends_null_content() {
        let request = request_from(json!({
            "model": MODEL,
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
        let assistant = &body_for(&request, &DriverConfig::default())["messages"][1];
        assert_eq!(assistant["content"], Value::Null);
        assert!(
            assistant.get("content").is_some(),
            "content is sent as null, not left out"
        );
        assert_eq!(assistant["reasoning_content"], json!("Read first."));
        assert_eq!(assistant["tool_calls"][0]["id"], json!("call_1"));
    }

    // ── S7 — session affinity ─────────────────────────────────────────────────

    #[test]
    fn session_affinity_is_the_sha256_of_the_trimmed_key() {
        for key in [
            CACHE_KEY,
            "  baseten-check:0.1.0:ctx_01\n",
            "\tbaseten-check:0.1.0:ctx_01 ",
        ] {
            assert_eq!(
                session_affinity(&request_with_cache_key(Some(json!(key)))).as_deref(),
                Some(CACHE_KEY_SHA256),
                "key {key:?}"
            );
        }
        let other = session_affinity(&request_with_cache_key(Some(json!(
            "baseten-check:0.1.0:ctx_02"
        ))))
        .unwrap();
        assert_ne!(other, CACHE_KEY_SHA256, "another task gets another value");
        assert_eq!(other.len(), 64);
        assert!(other
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    #[test]
    fn session_affinity_is_absent_without_a_usable_key() {
        for key in [
            None,
            Some(Value::Null),
            Some(json!(42)),
            Some(json!("")),
            Some(json!("   ")),
            Some(json!(["baseten-check"])),
        ] {
            assert_eq!(
                session_affinity(&request_with_cache_key(key.clone())),
                None,
                "key {key:?}"
            );
        }
    }

    #[test]
    fn request_headers_carry_the_affinity_header_only_with_a_key() {
        assert_eq!(SESSION_AFFINITY_HEADER, "x-session-affinity");
        assert_eq!(
            request_headers(&request_with_cache_key(Some(json!(CACHE_KEY))), 321),
            vec![
                ("content-type", "application/json".to_string()),
                ("content-length", "321".to_string()),
                ("x-session-affinity", CACHE_KEY_SHA256.to_string()),
            ]
        );
        for key in [None, Some(json!("  ")), Some(json!(7))] {
            assert_eq!(
                request_headers(&request_with_cache_key(key), 321),
                vec![
                    ("content-type", "application/json".to_string()),
                    ("content-length", "321".to_string()),
                ]
            );
        }
        let headers = request_headers(&request_with_cache_key(Some(json!(CACHE_KEY))), 1);
        assert!(
            headers.iter().all(|(_, value)| !value.contains(CACHE_KEY)),
            "the raw key never leaves the guest: {headers:?}"
        );
    }

    // ── S8 — streaming paths ──────────────────────────────────────────────────

    const INTERLEAVED_STREAM: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"The sky \"}}]}\n",
        "\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Rayleigh \"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"scatters blue.\"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"scattering.\"},\"finish_reason\":\"stop\"}]}\n",
        "data: [DONE]\n",
    );

    #[test]
    fn s8a_streamed_reasoning_and_text_go_to_separate_emitters_in_order() {
        let mut text_seen: Vec<String> = Vec::new();
        let mut thinking_seen: Vec<String> = Vec::new();

        let response = parse_baseten_sse_body(
            INTERLEAVED_STREAM,
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
    fn s8b_a_tool_call_delivered_whole_in_one_chunk_parses() {
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
    fn s8c_tool_call_signals_its_start_then_rising_input_byte_totals() {
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
    fn s8c_tool_calls_at_two_indexes_keep_separate_ids_and_totals() {
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
    fn s8c_tool_call_whose_id_arrives_late_is_reported_once_it_does() {
        let head = [
            tool_delta(0, None, Some("bash"), Some(r#"{"cmd":"#)),
            tool_delta(0, None, None, Some(r#""ZQXJ"}"#)),
        ]
        .concat();
        let (_, before_the_id) =
            parse_baseten_sse_body_with_signals(&head, &mut |_| {}, &mut |_| {});
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
    fn s8c_tool_call_that_never_gets_an_id_signals_nothing_and_is_returned() {
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
    fn s8c_tool_call_with_no_arguments_signals_only_its_start() {
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
    fn s8c_tool_call_signals_leave_reasoning_and_content_chunks_unchanged() {
        let body = [
            chunk(json!({"reasoning_content": "Check the "})),
            chunk(json!({"reasoning_content": "tree."})),
            chunk(json!({"content": "Listing."})),
            tool_delta(0, Some("call_1"), Some("bash"), Some("")),
            chunk(json!({"reasoning_content": " Then stop."})),
            tool_delta(0, None, None, Some(r#"{"cmd":"ZQXJ"}"#)),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();

        let mut text: Vec<String> = Vec::new();
        let mut thinking: Vec<String> = Vec::new();
        let (result, signals) = parse_baseten_sse_body_with_signals(
            &body,
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
    fn s8d_a_stream_without_done_fails_transiently_even_after_a_finish_reason() {
        let finished_but_cut = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"The answer \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
        );
        let cut_mid_text = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me \"},\"finish_reason\":null}]}\n",
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
                Failure::transient("driver: Baseten stream ended before data: [DONE]"),
                "body = {body:?}"
            );
            assert_eq!(
                failure.metadata(),
                vec![("transient".to_string(), "true".to_string())]
            );
        }
    }

    #[test]
    fn s8e_a_stream_with_done_and_no_finish_reason_fails_transiently() {
        let no_reason = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"\"}]}\n",
            "data: [DONE]\n",
        );
        for body in [no_reason, "data: [DONE]\n"] {
            assert_eq!(
                parse_quiet(body).unwrap_err(),
                Failure::transient("driver: Baseten stream ended with no finish_reason"),
                "body = {body:?}"
            );
        }
    }

    #[test]
    fn s8f_a_streamed_length_finish_reason_maps_to_max_tokens() {
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
    fn s8g_an_error_chunk_fails_the_turn_transiently_with_the_error_serialised() {
        let overloaded = json!({"code": 529, "message": "overloaded", "type": "server_error"});
        let expected = Failure::transient(
            r#"driver: Baseten stream error: {"code":529,"message":"overloaded","type":"server_error"}"#,
        );
        for body in [
            error_chunk(overloaded.clone()),
            format!("{TEXT_DELTA}{}", error_chunk(overloaded.clone())),
            format!("{}data: [DONE]\n", error_chunk(overloaded.clone())),
            format!(
                "event: error\ndata: {}\n",
                json!({"error": overloaded, "choices": []})
            ),
        ] {
            assert_eq!(parse_quiet(&body).unwrap_err(), expected, "body = {body:?}");
        }

        assert_eq!(
            parse_quiet(&error_chunk(json!("Error making prediction"))).unwrap_err(),
            Failure::transient(r#"driver: Baseten stream error: "Error making prediction""#)
        );

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
    fn s8g_an_error_chunk_takes_precedence_over_a_later_finish_reason() {
        let body = format!(
            "{}data: {}\ndata: [DONE]\n",
            error_chunk(json!({"message": "boom"})),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        );
        assert_eq!(
            parse_quiet(&body).unwrap_err(),
            Failure::transient(r#"driver: Baseten stream error: {"message":"boom"}"#)
        );
    }

    #[test]
    fn s8h_usage_on_every_chunk_is_merged_and_the_last_report_wins() {
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
                json!({"choices": [{"delta": {"reasoning_content": "Hm."}}], "usage": running(900, 1, 800, 1)})
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
    fn s8i_a_tool_call_index_past_the_limit_fails_the_turn_without_allocating() {
        for index in [1024, 4_294_967_296, u64::MAX] {
            let line = tool_delta(index, Some("call_x"), Some("bash"), Some(""));
            let mut tool_states = Vec::new();
            let mut signals = Vec::new();
            let result = process_baseten_sse_line(
                line.trim_end_matches('\n'),
                &mut tool_states,
                &mut None,
                &mut UsageTokens::default(),
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
            assert_eq!(tool_states.capacity(), 0);
            assert_eq!(signals, vec![]);
        }

        let body = [
            tool_delta(0, Some("call_a"), Some("read"), Some("")),
            tool_delta(1024, Some("call_x"), Some("bash"), Some("")),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();
        let (result, _) = parse_baseten_sse_body_with_signals(&body, &mut |_| {}, &mut |_| {});
        assert_eq!(
            result,
            Err(Failure::plain(
                "driver: tool call index 1024 is past the limit of 1024 per turn"
            ))
        );
    }

    #[test]
    fn s8i_a_tool_call_at_the_last_index_under_the_limit_is_kept() {
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

    // ── S9 — usage mapping with cached tokens ─────────────────────────────────

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
        translate_baseten_response_to_murmur(&response).unwrap()
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

    /// The counts of Baseten's own usage example for `zai-org/GLM-5.3`, with the
    /// speculative-decoding counters beside them.
    const DOCUMENTED_USAGE: &str = r#"{"prompt_tokens":14820,"completion_tokens":3200,"total_tokens":18020,"prompt_tokens_details":{"cached_tokens":9200},"completion_tokens_details":{"reasoning_tokens":1100,"accepted_prediction_tokens":900,"rejected_prediction_tokens":4000}}"#;

    #[test]
    fn cached_tokens_sit_beside_input_tokens_and_sum_to_the_prompt_total() {
        let documented: Value = serde_json::from_str(DOCUMENTED_USAGE).unwrap();
        for response in both_paths(documented) {
            let usage = &response["usage"];
            assert_eq!(
                usage.to_string(),
                r#"{"cached_tokens":9200,"input_tokens":5620,"output_tokens":3200,"thinking_tokens":1100}"#
            );
            assert_eq!(
                usage,
                &json!({"input_tokens": 5620, "output_tokens": 3200, "cached_tokens": 9200, "thinking_tokens": 1100})
            );
            assert_eq!(
                reported_request(usage),
                14820,
                "input + cached is prompt_tokens"
            );
            assert!(
                usage.get("cache_write_tokens").is_none(),
                "Baseten reports no cache write"
            );
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
        assert!(translate_baseten_response_to_murmur(&plain)
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
            json!({"input_tokens": 5620, "output_tokens": 3200, "cached_tokens": 9200, "thinking_tokens": 1100})
        );
    }

    // ── S10 — stop reasons ────────────────────────────────────────────────────

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
                translate_baseten_response_to_murmur(&buffered).unwrap()["stop_reason"],
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
                "driver: unsupported Baseten finish_reason '{reason}'"
            ));
            let buffered =
                json!({"choices": [{"finish_reason": reason, "message": {"content": ""}}]});
            assert_eq!(
                translate_baseten_response_to_murmur(&buffered).unwrap_err(),
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
                translate_baseten_response_to_murmur(&json!({"choices": [choice]})).unwrap_err(),
                Failure::plain("driver: Baseten response has no finish_reason"),
                "finish_reason = {finish_reason:?}"
            );
        }
    }

    // ── S11 — failure paths never panic ───────────────────────────────────────

    #[test]
    fn an_image_block_is_refused_before_any_body_is_built() {
        let image = json!({"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}});
        for messages in [
            json!([{"role": "user", "content": [{"type": "text", "text": "What is this?"}, image]}]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "first"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "user", "content": [image]}
            ]),
        ] {
            let request =
                request_from(json!({"model": MODEL, "max_tokens": 8192, "messages": messages}));
            assert_eq!(
                build_provider_request(&request, &DriverConfig::default()).unwrap_err(),
                "driver: driver-baseten sends text only and refuses an image block"
            );
        }
    }

    #[test]
    fn malformed_requests_are_refused_by_name() {
        let orphan_tool = request_from(json!({
            "model": MODEL,
            "max_tokens": 8192,
            "messages": [{"role": "tool", "content": [{"type": "text", "text": "out"}]}]
        }));
        assert_eq!(
            translate_murmur_request_to_baseten(&orphan_tool, &DriverConfig::default())
                .unwrap_err(),
            "driver: tool message is missing required field 'tool_call_id'"
        );

        let bad_role = request_from(json!({
            "model": MODEL,
            "max_tokens": 8192,
            "messages": [{"role": "developer", "content": []}]
        }));
        assert_eq!(
            build_provider_request(&bad_role, &DriverConfig::default()).unwrap_err(),
            "driver: unsupported message role 'developer'"
        );
    }

    #[test]
    fn non_json_tool_arguments_are_an_error_on_both_paths() {
        let buffered = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {"tool_calls": [{"id": "call_1", "function": {"name": "bash", "arguments": "{not json"}}]}
            }]
        });
        let err = translate_baseten_response_to_murmur(&buffered).unwrap_err();
        assert!(
            err.message
                .starts_with("driver: failed to parse Baseten tool call arguments JSON: "),
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
                .starts_with("driver: failed to parse Baseten tool call arguments JSON: "),
            "got {err}"
        );
        assert_eq!(err.retry, None);
    }

    #[test]
    fn a_response_with_no_choices_is_refused() {
        for response in [json!({}), json!({"choices": []}), json!({"choices": "x"})] {
            assert_eq!(
                translate_baseten_response_to_murmur(&response).unwrap_err(),
                Failure::plain("driver: Baseten response missing choices[0]")
            );
        }
    }

    #[test]
    fn a_provider_error_status_carries_the_status_and_body() {
        let body = r#"{"error":"Rate limit exceeded"}"#;
        let failure = status_failure(429, body);
        assert_eq!(
            error_payload(&failure.message),
            json!({"stop_reason": "error", "error": format!("HTTP 429: {body}")})
        );
        assert_eq!(failure.retry, None);
    }

    #[test]
    fn every_status_is_left_to_murmur() {
        for status in [400, 401, 402, 403, 404, 413, 429, 500, 502, 503, 504, 529] {
            let body = r#"{"error":"x"}"#;
            assert_eq!(
                status_failure(status, body),
                Failure::plain(format!("HTTP {status}: {body}")),
                "status = {status}"
            );
        }
    }

    #[test]
    fn the_json_fallback_orders_thinking_then_text_then_calls() {
        let buffered = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "reasoning_content": "Two independent calls.",
                    "content": "I'll check the weather for both cities.",
                    "tool_calls": [
                        {"id": "call_a", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                        {"id": "call_b", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}}
                    ]
                }
            }]
        });
        assert_eq!(
            translate_baseten_response_to_murmur(&buffered).unwrap()["content"],
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
        let response = translate_baseten_response_to_murmur(&buffered).unwrap();
        assert_eq!(
            response["content"],
            json!([{"type": "text", "text": "Rayleigh scattering."}])
        );
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn a_pre_flight_refusal_is_a_plain_failure() {
        let request = request_for_model("glm-5.3", 8192);
        let failure =
            Failure::from(build_provider_request(&request, &DriverConfig::default()).unwrap_err());
        assert_eq!(failure.retry, None);
        assert!(failure.metadata().is_empty());
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

    // ── S12 — source and manifest shape ───────────────────────────────────────

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
        // one, and it would carry the capsule's credentials with it. The needles are assembled
        // rather than written out so that this assertion does not itself become the only
        // occurrence of what it forbids.
        let source = include_str!("lib.rs");
        for provider_url in [concat!("https://", "api."), concat!("baseten", ".co")] {
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
            run_inner.contains("let headers = request_headers(&murmur_request, body.len());"),
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
            run_inner.contains("saw_done,\n                usage,"),
            "the guest loop must hand saw_done to assemble_baseten_streaming_response"
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
        // Baseten delivers reasoning on its own delta field, so an inline reasoning-tag scanner
        // here would be a second, silently-wrong reasoning path. The needle is assembled rather
        // than written out so that this assertion does not itself become the only occurrence.
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
            concat!("\"author", "ization\""),
            concat!("\"x-api", "-key\""),
            concat!("bear", "er "),
        ] {
            assert!(
                !non_test.contains(needle),
                "driver-baseten: non-test source must not contain {needle}"
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
            "drivers/driver-baseten/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[test]
    fn manifest_models_block_matches_the_source_model_table() {
        let mut block = String::from("models:\n");
        for (id, limits) in SUPPORTED_MODELS {
            block.push_str(&format!(
                "  {id}:\n    max_output_tokens: {}\n    context_window: {}\n",
                limits.max_output_tokens, limits.context_window
            ));
        }
        assert_eq!(
            block,
            "models:\n  zai-org/GLM-5.3:\n    max_output_tokens: 262144\n    context_window: 1048576\n"
        );
        assert!(
            include_str!("../murmur.yaml").contains(&block),
            "drivers/driver-baseten/murmur.yaml must contain verbatim:\n{block}"
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
        for (id, limits) in SUPPORTED_MODELS {
            let row = section
                .lines()
                .find(|line| line.starts_with(&format!("| `{id}` |")))
                .unwrap_or_else(|| panic!("Model scope must hold a row for {id}"));
            for number in [limits.max_output_tokens, limits.context_window] {
                assert!(
                    row.contains(&format!("`{number}`")),
                    "the {id} row must state {number}: {row}"
                );
            }
            assert!(
                row.contains("`none`"),
                "the {id} row declares no max_input_tokens: {row}"
            );
        }
    }

    // ── replay_reasoning — earlier turns' reasoning on or off ─────────────────

    /// System, user, a tool-call turn with reasoning and text, its tool result, a plain turn with
    /// reasoning, then the next user turn.
    fn replay_request() -> MurmurRequest {
        request_from(json!({
            "model": MODEL,
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

    /// The S2 fixture's body as driver-baseten 0.2.0 built it, captured from `fc72bb4`.
    const BODY_AT_0_2_0: &str = r#"{"max_tokens":8192,"messages":[{"content":"You are a careful assistant.","role":"system"},{"content":"How long is notes.txt?","role":"user"},{"content":"Reading it.","reasoning_content":"Read the file first. \"Quoted\" — ü.","role":"assistant","tool_calls":[{"function":{"arguments":"{\"path\":\"notes.txt\"}","name":"read_file"},"id":"call_1","type":"function"}]},{"content":"a\nb\nc","role":"tool","tool_call_id":"call_1"},{"content":"It has three lines.","reasoning_content":"Three lines.","role":"assistant"},{"content":"And how many words?","role":"user"}],"model":"zai-org/GLM-5.3","reasoning_effort":"high","stream":true,"stream_options":{"include_usage":true},"tools":[{"function":{"name":"read_file","parameters":{"properties":{"path":{"type":"string"}},"type":"object"}},"type":"function"}]}"#;

    /// Drop every `reasoning_content` member from the body's messages.
    fn without_reasoning_content(mut body: Value) -> Value {
        for message in body["messages"].as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("reasoning_content");
        }
        body
    }

    #[test]
    fn the_default_body_is_0_2_0_s_plus_the_chat_template_args_stamp() {
        let body = body_for(&replay_request(), &DriverConfig::default());
        assert_eq!(
            body.to_string(),
            BODY_AT_0_2_0.replacen('{', r#"{"chat_template_args":{"clear_thinking":false},"#, 1),
            "the replaying body is 0.2.0's plus one member, messages byte for byte"
        );
        let mut without_stamp = body.clone();
        without_stamp
            .as_object_mut()
            .unwrap()
            .remove("chat_template_args");
        assert_eq!(without_stamp.to_string(), BODY_AT_0_2_0);
        assert_no_forbidden_key(&body);
    }

    #[test]
    fn absent_empty_whitespace_empty_object_and_true_configs_build_the_same_body() {
        let expected = body_for(&replay_request(), &DriverConfig::default());
        for raw in [
            None,
            Some(""),
            Some(" \n "),
            Some("{}"),
            Some(r#"{"replay_reasoning":true}"#),
        ] {
            let config = parse_driver_config(raw).expect("a replaying config must parse");
            assert_eq!(config, DriverConfig::default(), "config {raw:?}");
            assert_eq!(
                body_for(&replay_request(), &config).to_string(),
                expected.to_string(),
                "config {raw:?}"
            );
        }
    }

    #[test]
    fn replay_reasoning_false_sends_no_reasoning_content_and_stamps_clear_thinking_true() {
        let config = config(r#"{"replay_reasoning":false}"#);
        assert!(!config.replay_reasoning);
        let body = body_for(&replay_request(), &config);

        assert!(
            !contains_key_anywhere(&body, "reasoning_content"),
            "no assistant message carries reasoning, the current tool loop's included: {body}"
        );
        assert_eq!(body["chat_template_args"], json!({"clear_thinking": true}));
        assert_no_forbidden_key(&body);

        let mut expected =
            without_reasoning_content(body_for(&replay_request(), &DriverConfig::default()));
        expected["chat_template_args"] = json!({"clear_thinking": true});
        assert_eq!(body, expected, "nothing else differs from the default body");
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "assistant",
                "content": "Reading it.",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"}}]
            })
        );
        assert_eq!(
            body["messages"][4],
            json!({"role": "assistant", "content": "It has three lines."})
        );
    }

    #[test]
    fn replay_reasoning_false_keeps_null_content_on_a_text_less_tool_call_turn() {
        let request = request_from(json!({
            "model": MODEL,
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
        let body = body_for(&request, &config(r#"{"replay_reasoning":false}"#));
        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}}]
            })
        );
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
    fn replay_reasoning_false_combines_with_reasoning_effort() {
        let config = config(r#"{"reasoning_effort":"max","replay_reasoning":false}"#);
        assert_eq!(
            config,
            DriverConfig {
                reasoning_effort: "max".to_string(),
                replay_reasoning: false,
            }
        );
        let body = body_for(&replay_request(), &config);
        assert_eq!(body["reasoning_effort"], json!("max"));
        assert_eq!(body["chat_template_args"], json!({"clear_thinking": true}));
        assert!(!contains_key_anywhere(&body, "reasoning_content"), "{body}");
    }

    #[test]
    fn current_turn_reasoning_parses_to_a_thinking_block_whatever_the_setting() {
        // Response parsing takes no config, so the setting cannot reach it: both paths emit the
        // turn's reasoning as a leading `thinking` block.
        let streamed = parse_quiet(&live_tool_call_stream()).unwrap();
        assert_eq!(
            streamed["content"][0],
            json!({"type": "thinking", "text": REASONING})
        );
        let fallback = translate_baseten_response_to_murmur(&json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "reasoning_content": "Hm.", "content": "Done."}
            }]
        }))
        .unwrap();
        assert_eq!(
            fallback["content"][0],
            json!({"type": "thinking", "text": "Hm."})
        );
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
