// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use murmur_driver_stream::Failure;
use serde::Deserialize;
use serde_json::{json, Map, Value};

// ── Model table ───────────────────────────────────────────────────────────────

/// The limits Z.ai publishes for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelLimits {
    /// The largest `max_tokens` Z.ai accepts for the model. A larger value is refused here
    /// rather than spent on a provider 400.
    max_output_tokens: u64,
    /// The model's context window in tokens. No request path reads it: it is the number
    /// `murmur.yaml` declares under `models:`, and a test keeps the two in step.
    #[cfg_attr(not(test), allow(dead_code))]
    context_window: u64,
}

/// Every model this driver serves, with its limits. This table is the classification seam a
/// second model would be added at — nothing else in the crate branches on the model name — and
/// `murmur.yaml`'s `models:` block must declare the same numbers for each entry.
const SUPPORTED_MODELS: &[(&str, ModelLimits)] = &[(
    "glm-5.3",
    ModelLimits {
        max_output_tokens: 131_072,
        context_window: 1_048_576,
    },
)];

/// Look the model up by its exact id — no case folding, no prefix match — before a request
/// body is built, so an unsupported model costs no HTTP call.
fn validate_model(model: &str) -> Result<&'static ModelLimits, String> {
    SUPPORTED_MODELS
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, limits)| limits)
        .ok_or_else(|| {
            format!(
                "driver: model '{model}' is not supported by driver-zai. Supported models: {}.",
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
            "driver: inference.max_tokens {max_tokens} exceeds the Z.ai max_tokens ceiling of {} \
             for {model}",
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
const ACCEPTED_CONFIG_KEYS: &[&str] = &["reasoning_effort", "replay_reasoning", "thinking"];

/// Z.ai's effort vocabulary for `glm-5.3`, in ascending order of spend. Z.ai answers `medium`
/// on `glm-5.3` with HTTP 400 code `1210` on both its standard and Coding Plan endpoints, so it
/// is refused here by name and never mapped onto another tier.
const REASONING_EFFORT_VALUES: &[&str] = &["low", "high", "max"];

/// Z.ai's own default. Kept rather than substituted with a cheaper tier, and stamped on every
/// request so the effective value is visible in a recorded request rather than left to drift
/// with the provider's default.
const DEFAULT_REASONING_EFFORT: &str = "max";

/// Earlier assistant turns carry their `reasoning_content` unless a capsule sets
/// `replay_reasoning: false`, so an unconfigured capsule sends the history Z.ai produced.
const DEFAULT_REPLAY_REASONING: bool = true;

/// The one `thinking` value `glm-5.3` accepts.
const THINKING_ENABLED: &str = "enabled";

/// `thinking: disabled` is refused with its own message rather than the generic wrong-value one:
/// `glm-5.3` always reasons, and Z.ai answers a disabled-thinking request with HTTP 400 code
/// `1210`.
const THINKING_DISABLED_REFUSAL: &str = "driver: inference.driver.config 'thinking' cannot be \
     \"disabled\" on driver-zai: glm-5.3 always reasons and Z.ai refuses to turn it off. Remove \
     the key or set it to \"enabled\".";

/// The dials read from `inference.driver.config`. `thinking` has a single accepted value, so it
/// is validated by [`parse_driver_config`] and carries no state here.
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
    if let Some(thinking) = config.get("thinking") {
        validate_thinking(thinking)?;
    }
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

/// Trimmed and matched case-insensitively. `enabled` is the only value that leaves the body as
/// it is without the key.
fn validate_thinking(value: &Value) -> Result<(), String> {
    match value {
        Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            THINKING_ENABLED => Ok(()),
            "disabled" => Err(THINKING_DISABLED_REFUSAL.to_string()),
            _ => Err(format!(
                "driver: inference.driver.config 'thinking' must be \"{THINKING_ENABLED}\", got \"{raw}\""
            )),
        },
        other => Err(format!(
            "driver: inference.driver.config 'thinking' must be a string, got {other}"
        )),
    }
}

// ── Murmur request types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MurmurRequest {
    // The host sends a prompt-cache routing hint as a reserved top-level `prompt_cache_key`
    // member. It is deliberately not declared here: Z.ai's context caching is automatic and its
    // Chat Completions API defines no cache-key field. An undeclared member is dropped by serde,
    // so it cannot reach the provider body at any nesting level — do not add a field and filter
    // it out later.
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
    // driver and read back by it — is where preserved reasoning lives. No runtime concept and no
    // interface widening is involved.
    Thinking {
        text: String,
    },
    // Declared so that it is recognised and refused by name: every model this driver serves is
    // text-only. Its source is never read.
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
        return Err(format!(
            "driver: model '{}' is text-only, so driver-zai refuses an image block",
            request.model
        ));
    }
    Ok(())
}

/// The body `run_inner` puts on the wire: pre-flight, translate, stamp the streaming flags.
/// Tests assert on this rather than on a bare translation.
fn build_provider_request(request: &MurmurRequest, config: &DriverConfig) -> Result<Value, String> {
    preflight_request(request)?;
    let mut body = translate_murmur_request_to_zai(request, config)?;
    stamp_streaming_flags(&mut body);
    Ok(body)
}

fn translate_murmur_request_to_zai(
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
    // Z.ai spells the output cap `max_tokens` and silently ignores `max_completion_tokens`.
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    // `clear_thinking` is stamped on every request because its default differs by endpoint:
    // Preserved Thinking is on by default on the Coding Plan endpoint and off on the standard
    // one. With `false` Z.ai keeps the reasoning of earlier assistant turns in context, provided
    // each one comes back complete and unmodified; with `true` it removes prior turns'
    // reasoning, and `translate_assistant_message` sends none.
    body.insert(
        "thinking".to_string(),
        json!({"type": THINKING_ENABLED, "clear_thinking": !config.replay_reasoning}),
    );

    // Top-level, not nested under a `reasoning` object, and stamped on every request including
    // the unconfigured one.
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

/// Rebuild an assistant turn exactly as Z.ai returned it, with its reasoning when
/// `replay_reasoning` is set. With `clear_thinking: false` Z.ai asks for the complete,
/// unmodified `reasoning_content` back, so it is restored on the tool-call turn and the
/// plain-text turn alike. Without `replay_reasoning` (sent as `clear_thinking: true`) no
/// assistant message carries the member, including the turns of the current tool loop, which
/// Z.ai's `clear_thinking` alone would keep. The text a tool-call turn carried beside its calls
/// is kept as `content` rather than dropped.
///
/// When no `thinking` block is present the member is omitted entirely — never sent as an empty
/// string — because that is the ordinary state of a history that has been compacted.
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

/// Z.ai's raw token counts, carried onto the reserved top-level `usage` object of the translated
/// response. Every member is independently optional: a count the provider did not report is
/// omitted rather than sent as `0`, because the host records a reported `0` as a real zero and a
/// fabricated one reads as a cache miss on the `inference` trace event.
///
/// Z.ai reports the request total with the cache reads inside it, while the usage contract puts
/// `cached_tokens` beside `input_tokens`. So the raw total is held here as `prompt_tokens`, and
/// the contract's `input_tokens` is derived from it only in `to_value`, after every streamed
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
    /// Take every member `later` reports, keeping the current value for the rest. Streaming
    /// usage may arrive across more than one chunk, each carrying only part of the picture.
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
    /// is invented for it. `thinking_tokens` is part of `output_tokens`, as Z.ai reports it.
    /// Z.ai reports no cache-write count, so `cache_write_tokens` is never emitted. `None` when
    /// no member survived, in which case the response carries no `usage` key at all.
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

/// Read a Z.ai `usage` object. A member that is absent, null, or not a non-negative integer is
/// dropped; its siblings are kept.
fn extract_zai_usage(usage: &Value) -> UsageTokens {
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

/// Force streaming on, overriding any `stream` key from `params`, and ask for tool-call
/// arguments in fragments as the model writes them. Z.ai puts the usage on the chunk that
/// carries `finish_reason` without being asked, and documents no `stream_options`, so none is
/// sent.
fn stamp_streaming_flags(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
        obj.insert("tool_stream".to_string(), json!(true));
    }
}

// ── Stop-reason mapping ───────────────────────────────────────────────────────

/// Map a Z.ai `finish_reason` to a murmur `stop_reason`. Each caller refuses a missing reason
/// with its own path's message. `network_error` is Z.ai's report that the turn broke off on its
/// side, so it fails the call transiently. Z.ai also documents `sensitive` and
/// `model_context_window_exceeded`; those and any unknown reason are a plain error naming it
/// rather than a guess.
fn map_finish_reason(finish_reason: &str) -> Result<&'static str, Failure> {
    match finish_reason {
        "stop" => Ok("end_turn"),
        "tool_calls" => Ok("tool_call"),
        "length" => Ok("max_tokens"),
        "network_error" => Err(Failure::transient(
            "Z.ai stream failed: finish_reason 'network_error'",
        )),
        other => Err(Failure::plain(format!(
            "driver: unsupported Z.ai finish_reason '{other}'"
        ))),
    }
}

// ── Provider errors ───────────────────────────────────────────────────────────

/// Z.ai business error codes that report a server fault, a lost connection, a rate limit or an
/// overload: `1200` API call error, `1230` API call process error, `1234` network error, `1302`
/// request rate limit, `1305` temporary overload. Z.ai sends them with a `5xx` or `429` status,
/// which murmur retries by itself; reported inside a `2xx` stream, they fail the call
/// transiently.
const TRANSIENT_ERROR_CODES: &[&str] = &["1200", "1230", "1234", "1302", "1305"];

/// Z.ai business error codes sent with a `429` that waiting a few seconds does not clear: `1113`
/// no balance or resource package, `1308` and `1310` plan usage limits that reset at a stated
/// time, `1309` and `1314` an expired plan, `1311` a plan without the model, `1313` a Fair Usage
/// restriction, `1315` a key limited to enterprise coding, and `1316`–`1321` 5-hour and 7-day
/// limits with no extra usage left.
const QUOTA_EXHAUSTED_CODES: &[&str] = &[
    "1113", "1308", "1309", "1310", "1311", "1313", "1314", "1315", "1316", "1317", "1318", "1319",
    "1320", "1321",
];

/// The business code of a Z.ai `error` object, which Z.ai sends as a string (`"1113"`) or a
/// number.
fn error_code(error: &Value) -> Option<String> {
    match error.get("code")? {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }
}

/// The failure for a top-level `error` object inside a `2xx` stream:
/// `Z.ai error: <code>: <message>`, a missing part read as `unknown`. Transient only for the
/// codes in [`TRANSIENT_ERROR_CODES`].
fn stream_error_failure(error: &Value) -> Failure {
    let code = error_code(error);
    let message = format!(
        "Z.ai error: {}: {}",
        code.as_deref().unwrap_or("unknown"),
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    match code {
        Some(code) if TRANSIENT_ERROR_CODES.contains(&code.as_str()) => Failure::transient(message),
        _ => Failure::plain(message),
    }
}

/// The failure for a provider status `>= 400`: `HTTP <status>: <body>`, carrying Z.ai's own body
/// so the operator sees what it said. A `429` whose `error.code` is in
/// [`QUOTA_EXHAUSTED_CODES`] vetoes murmur's retry; every other status is left to murmur.
fn status_failure(status: u16, body: &str) -> Failure {
    let message = format!("HTTP {status}: {body}");
    let quota_exhausted = status == 429
        && serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|body| body.get("error").and_then(error_code))
            .is_some_and(|code| QUOTA_EXHAUSTED_CODES.contains(&code.as_str()));
    if quota_exhausted {
        Failure::permanent(message)
    } else {
        Failure::plain(message)
    }
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
        .map_err(|err| format!("driver: failed to parse Z.ai tool call arguments JSON: {err}"))
}

// ── Response translation (non-streaming fallback) ─────────────────────────────

/// The non-streaming JSON path, reached when the response body's first non-whitespace byte is
/// `{` or `[` — scripted test servers, and providers that ignore `stream: true`.
fn translate_zai_response_to_murmur(response: &Value) -> Result<Value, Failure> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: Z.ai response missing choices[0]".to_string())?;

    // Absent, `null`, non-string or empty: the turn did not finish.
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| "driver: Z.ai response has no finish_reason".to_string())?;
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
        .map(extract_zai_usage)
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

/// Process one complete SSE line. Returns `Ok(true)` when the stream is done (`data: [DONE]`),
/// and `Err` for a chunk carrying a top-level `error` object ([`stream_error_failure`]) or a
/// tool call index past [`murmur_driver_stream::MAX_SLOTS_PER_TURN`].
///
/// Z.ai streams OpenAI-shaped chat-completions chunks plus a separate `reasoning_content`
/// delta. `delta.reasoning_content` is routed to `emit_thinking`, `delta.content` to
/// `emit_text`; the two never mix, and nothing in this crate scans the text for reasoning
/// sentinels. With `tool_stream: true` a call's first fragment carries its `id` and
/// `function.name` and later ones only `arguments` pieces; without it a call arrives whole in
/// one chunk. Both are accumulated by `index`.
fn process_zai_sse_line(
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

    // An error ends the turn here, so it is reported as Z.ai's and never as a cut stream.
    if let Some(error) = data.get("error").filter(|error| error.is_object()) {
        return Err(stream_error_failure(error));
    }

    // Read from any chunk, before the `choices`-shaped early returns below. Z.ai puts it on the
    // chunk that also carries `finish_reason`.
    if let Some(reported) = data.get("usage").filter(|value| !value.is_null()) {
        usage.merge_from(extract_zai_usage(reported));
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
/// [`ordered_content`] fixes. A stream that ended before any chunk named a `finish_reason` was
/// cut after the provider's `2xx`, so it fails transiently.
fn assemble_zai_streaming_response(
    text_acc: &str,
    reasoning_acc: &str,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
    usage: UsageTokens,
) -> Result<Value, Failure> {
    // No chunk carried a finish_reason, so the stream ended before the turn did.
    let finish_reason = stop_reason
        .as_deref()
        .ok_or_else(|| Failure::transient("driver: Z.ai stream ended with no finish_reason"))?;
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
fn parse_zai_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, Failure> {
    parse_zai_sse_body_with_signals(body, emit_text, emit_thinking).0
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
fn parse_zai_sse_body_with_signals<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> (Result<Value, Failure>, Vec<RecordedSignal>) {
    let mut text_acc = String::new();
    let mut reasoning_acc = String::new();
    let mut tool_states: Vec<ToolCallState> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut usage = UsageTokens::default();
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
            process_zai_sse_line(
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
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => return (Err(err), signals),
        }
    }

    let result =
        assemble_zai_streaming_response(&text_acc, &reasoning_acc, tool_states, stop_reason, usage);
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
        assemble_zai_streaming_response, build_provider_request, error_payload,
        parse_driver_config, process_zai_sse_line, status_failure,
        translate_zai_response_to_murmur, MurmurRequest, ToolCallSignal, ToolCallState,
        UsageTokens,
    };
    use murmur_driver_stream::Failure;
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

    pub struct ZaiDriver;

    impl exports::murmur::tool::run::Guest for ZaiDriver {
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

        // Model and output-cap validation happen inside here, pre-flight: a rejected request
        // costs no HTTP call.
        let provider_request = build_provider_request(&murmur_request, &config)?;

        let body = serde_json::to_vec(&provider_request)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));

        let headers = vec![
            ("content-type", "application/json".to_string()),
            ("content-length", body.len().to_string()),
        ];

        let response = dispatch_request(&url, headers, &body)?;
        let status = response.status();

        if status >= 400 {
            let text = consume_body_as_string(response, status)?;
            return Err(status_failure(status, &text));
        }

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
                .map_err(|err| format!("driver: failed to parse Z.ai response JSON: {err}"))?;
            translate_zai_response_to_murmur(&json)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut text_acc = String::new();
            let mut reasoning_acc = String::new();
            let mut tool_states: Vec<ToolCallState> = Vec::new();
            let mut stop_reason: Option<String> = None;
            let mut usage = UsageTokens::default();
            let mut done = false;

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
                process_zai_sse_line(
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
                    done = handle_line(
                        line,
                        &mut tool_states,
                        &mut stop_reason,
                        &mut usage,
                        &mut text_acc,
                        &mut reasoning_acc,
                    )?;
                    line_buf.clear();
                    if done {
                        break;
                    }
                } else {
                    line_buf.push(b);
                }
            }

            // Continue reading remaining chunks.
            if !done {
                'outer: loop {
                    let chunk = read_chunk(&stream, status)?;
                    if chunk.is_empty() {
                        break;
                    }
                    for &b in &chunk {
                        if b == b'\n' {
                            let line = String::from_utf8_lossy(&line_buf);
                            let line = line.trim_end_matches('\r');
                            done = handle_line(
                                line,
                                &mut tool_states,
                                &mut stop_reason,
                                &mut usage,
                                &mut text_acc,
                                &mut reasoning_acc,
                            )?;
                            line_buf.clear();
                            if done {
                                break 'outer;
                            }
                        } else {
                            line_buf.push(b);
                        }
                    }
                }
            }

            drop(stream);
            let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
            assemble_zai_streaming_response(
                &text_acc,
                &reasoning_acc,
                tool_states,
                stop_reason,
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
            Err(BodyWrite::Failed(message)) => return Err(message),
        }

        await_response(future)
    }

    /// Why a request body was not written whole.
    enum BodyWrite {
        /// The host closed the body stream.
        Closed,
        /// Any other failure, as its error message.
        Failed(String),
    }

    /// Stream `body` into `outgoing_body`, after `outgoing-handler.handle`: nothing drains the
    /// host's bounded body buffer before then. The output stream is a child of `outgoing_body`
    /// and drops when this returns, before the caller finishes or drops its parent.
    fn write_body(
        outgoing_body: &wasip2::http::types::OutgoingBody,
        body: &[u8],
    ) -> Result<(), BodyWrite> {
        let stream = outgoing_body.write().map_err(|()| {
            BodyWrite::Failed("driver: failed to open request body stream".to_string())
        })?;
        let mut remaining: &[u8] = body;
        while !remaining.is_empty() {
            let budget = stream
                .check_write()
                .map_err(|e| body_write(e, |e| format!("driver: check-write failed: {e:?}")))?
                as usize;
            if budget == 0 {
                stream.subscribe().block();
                continue;
            }
            let n = budget.min(remaining.len());
            stream
                .write(&remaining[..n])
                .map_err(|e| body_write(e, |e| format!("driver: write failed: {e:?}")))?;
            remaining = &remaining[n..];
        }
        stream
            .flush()
            .map_err(|e| body_write(e, |e| format!("driver: flush failed: {e:?}")))?;
        stream.subscribe().block();
        Ok(())
    }

    /// [`BodyWrite::Closed`] for a closed stream, otherwise `failed`'s message.
    fn body_write(
        err: wasip2::io::streams::StreamError,
        failed: fn(wasip2::io::streams::StreamError) -> String,
    ) -> BodyWrite {
        match err {
            wasip2::io::streams::StreamError::Closed => BodyWrite::Closed,
            err => BodyWrite::Failed(failed(err)),
        }
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

    export!(ZaiDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Nothing here talks to the provider. Every assertion is driven by a fixture and the host-side
// whole-stream parser.

#[cfg(test)]
mod tests {
    use super::{
        build_provider_request, error_payload, map_finish_reason, parse_driver_config,
        parse_zai_sse_body, parse_zai_sse_body_with_signals, status_failure,
        translate_murmur_request_to_zai, translate_zai_response_to_murmur, DriverConfig,
        MurmurRequest, RecordedSignal, SUPPORTED_MODELS,
    };
    use murmur_driver_stream::{Failure, Retry};
    use serde_json::{json, Value};

    // ── Fixtures and helpers ──────────────────────────────────────────────────

    fn request_from(value: Value) -> MurmurRequest {
        serde_json::from_value(value).expect("fixture must deserialize as a murmur request")
    }

    /// The canonical envelope the host builds, with the reserved `prompt_cache_key` present.
    fn simple_request() -> MurmurRequest {
        request_from(json!({
            "model": "glm-5.3",
            "max_tokens": 8192,
            "system": "You are a careful assistant.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Why is the sky blue?"}]}
            ],
            "tools": [],
            "params": {},
            "prompt_cache_key": "capsule:demo:0.1.0"
        }))
    }

    fn config(raw: &str) -> DriverConfig {
        parse_driver_config(Some(raw)).expect("fixture config must parse")
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

    fn parse_quiet(body: &str) -> Result<Value, Failure> {
        parse_zai_sse_body(body, &mut |_: &str| {}, &mut |_: &str| {})
    }

    fn request_for_model(model: &str, max_tokens: u64) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }))
    }

    // ── Scenario 1 — model check ──────────────────────────────────────────────

    #[test]
    fn only_the_exact_model_id_glm_5_3_is_accepted() {
        assert!(build_provider_request(
            &request_for_model("glm-5.3", 8192),
            &DriverConfig::default()
        )
        .is_ok());

        for rejected in ["glm-5.2", "GLM-5.3", "glm-5.3-flash", "glm-4.6"] {
            let err = build_provider_request(
                &request_for_model(rejected, 8192),
                &DriverConfig::default(),
            )
            .expect_err("only glm-5.3 is served, matched exactly");
            assert_eq!(
                err,
                format!(
                    "driver: model '{rejected}' is not supported by driver-zai. \
                     Supported models: glm-5.3."
                )
            );
        }
    }

    #[test]
    fn an_unsupported_model_is_refused_before_any_other_request_check() {
        // An oversized cap, an image and an orphan tool message would each be refused too; the
        // model check is the one that answers, so it runs before a body exists.
        let request = request_from(json!({
            "model": "glm-4.6",
            "max_tokens": 10_000_000u64,
            "messages": [
                {"role": "tool", "content": []},
                {"role": "user", "content": [{"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}}]}
            ]
        }));
        let err = build_provider_request(&request, &DriverConfig::default()).unwrap_err();
        assert!(err.contains("is not supported by driver-zai"), "got {err}");
    }

    // ── Scenario 2 — output cap ───────────────────────────────────────────────

    #[test]
    fn output_cap_is_written_as_max_tokens_up_to_the_131072_ceiling() {
        let body = body_for(
            &request_for_model("glm-5.3", 131_072),
            &DriverConfig::default(),
        );
        assert_eq!(body["max_tokens"], json!(131_072));
        assert!(
            !contains_key_anywhere(&body, "max_completion_tokens"),
            "Z.ai ignores max_completion_tokens: {body}"
        );

        let err = build_provider_request(
            &request_for_model("glm-5.3", 131_073),
            &DriverConfig::default(),
        )
        .expect_err("a cap above the ceiling must be refused pre-flight");
        assert_eq!(
            err,
            "driver: inference.max_tokens 131073 exceeds the Z.ai max_tokens ceiling of 131072 \
             for glm-5.3"
        );
    }

    // ── Scenario 3 — each dial value is accepted or refused by name ───────────

    #[test]
    fn absent_empty_whitespace_and_empty_object_configs_stamp_the_defaults() {
        let expected = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(expected["reasoning_effort"], json!("max"));
        assert_eq!(
            expected["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
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
        for (raw, sent) in [("low", "low"), (" HIGH ", "high"), ("max", "max")] {
            let body = body_for(
                &simple_request(),
                &config(&json!({"reasoning_effort": raw}).to_string()),
            );
            assert_eq!(body["reasoning_effort"], json!(sent), "effort {raw:?}");
            assert!(
                body.get("reasoning").is_none(),
                "effort is top-level: {body}"
            );
        }
    }

    #[test]
    fn effort_tiers_glm_5_3_does_not_have_are_refused_with_the_raw_value_quoted() {
        for raw in ["medium", "xhigh", "minimal", "none"] {
            let err = parse_driver_config(Some(&json!({"reasoning_effort": raw}).to_string()))
                .expect_err("only low, high and max exist on glm-5.3");
            assert_eq!(
                err,
                format!(
                    "driver: inference.driver.config 'reasoning_effort' must be one of low, \
                     high, max, got \"{raw}\""
                )
            );
        }
    }

    #[test]
    fn a_non_string_reasoning_effort_is_refused() {
        assert_eq!(
            parse_driver_config(Some(r#"{"reasoning_effort":3}"#)).unwrap_err(),
            "driver: inference.driver.config 'reasoning_effort' must be a string, got 3"
        );
    }

    #[test]
    fn thinking_enabled_is_accepted_and_leaves_the_body_as_the_default() {
        let default_body = body_for(&simple_request(), &DriverConfig::default());
        for raw in ["enabled", " Enabled "] {
            let config = config(&json!({"thinking": raw}).to_string());
            assert_eq!(config, DriverConfig::default());
            assert_eq!(body_for(&simple_request(), &config), default_body);
        }
    }

    #[test]
    fn thinking_disabled_gets_its_own_refusal_naming_the_key_the_value_and_the_model() {
        let err = parse_driver_config(Some(r#"{"thinking":"disabled"}"#))
            .expect_err("glm-5.3 cannot have thinking turned off");
        assert_eq!(
            err,
            "driver: inference.driver.config 'thinking' cannot be \"disabled\" on driver-zai: \
             glm-5.3 always reasons and Z.ai refuses to turn it off. Remove the key or set it to \
             \"enabled\"."
        );
        for named in ["'thinking'", "disabled", "glm-5.3"] {
            assert!(err.contains(named), "the refusal must name {named}: {err}");
        }
    }

    #[test]
    fn any_other_thinking_value_is_refused() {
        assert_eq!(
            parse_driver_config(Some(r#"{"thinking":"on"}"#)).unwrap_err(),
            "driver: inference.driver.config 'thinking' must be \"enabled\", got \"on\""
        );
        assert_eq!(
            parse_driver_config(Some(r#"{"thinking":true}"#)).unwrap_err(),
            "driver: inference.driver.config 'thinking' must be a string, got true"
        );
    }

    #[test]
    fn unrecognised_config_keys_are_reported_sorted_with_the_accepted_set() {
        assert_eq!(
            parse_driver_config(Some(r#"{"response_format":{},"clear_thinking":false}"#))
                .unwrap_err(),
            "driver: unrecognised inference.driver.config key(s): clear_thinking, \
             response_format. Accepted keys: reasoning_effort, replay_reasoning, thinking"
        );
    }

    #[test]
    fn a_config_that_is_not_a_json_object_is_refused() {
        let not_json = parse_driver_config(Some("not json")).unwrap_err();
        assert!(
            not_json.starts_with("driver: inference.driver.config is not valid JSON: "),
            "got {not_json}"
        );
        assert_eq!(
            parse_driver_config(Some("[]")).unwrap_err(),
            "driver: inference.driver.config must be a JSON object"
        );
    }

    // ── Scenario 4 — request translation with tools ───────────────────────────

    fn tools_request() -> MurmurRequest {
        request_from(json!({
            "model": "glm-5.3",
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
            "params": {"temperature": 0.6, "stream": false},
            "prompt_cache_key": "capsule:demo:0.1.0"
        }))
    }

    #[test]
    fn a_request_with_tools_becomes_the_chat_completions_body() {
        let body = body_for(&tools_request(), &config(r#"{"reasoning_effort":"high"}"#));

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
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(
            body["stream"],
            json!(true),
            "params cannot turn streaming off"
        );
        assert_eq!(body["tool_stream"], json!(true));
        assert_eq!(
            body["temperature"],
            json!(0.6),
            "an unclaimed param passes through"
        );

        for absent in [
            "prompt_cache_key",
            "stream_options",
            "max_completion_tokens",
        ] {
            assert!(
                !contains_key_anywhere(&body, absent),
                "'{absent}' must not appear at any nesting level: {body}"
            );
        }
        assert!(
            !body.to_string().contains("capsule:demo:0.1.0"),
            "the routing hint must not reach the provider: {body}"
        );
    }

    #[test]
    fn a_params_key_the_body_already_holds_does_not_override_it() {
        let request = request_from(json!({
            "model": "glm-5.3",
            "max_tokens": 1000,
            "messages": [],
            "params": {
                "model": "glm-4.6",
                "max_tokens": 5,
                "reasoning_effort": "medium",
                "thinking": {"type": "disabled"},
                "tool_stream": false
            }
        }));
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(body["model"], json!("glm-5.3"));
        assert_eq!(body["max_tokens"], json!(1000));
        assert_eq!(body["reasoning_effort"], json!("max"));
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
        assert_eq!(body["tool_stream"], json!(true));
    }

    // ── Scenario 5 — without tools, with thinking on ──────────────────────────

    #[test]
    fn a_hook_shaped_request_without_tools_keeps_thinking_on() {
        let request = request_from(json!({
            "model": "glm-5.3",
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
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("max"));
        assert_eq!(body["tool_stream"], json!(true));
        assert_eq!(
            body["messages"][2],
            json!({"role": "assistant", "content": "Done."})
        );
    }

    // ── Scenario 6 — reasoning restored on the next turn after a tool call ────

    const REASONING: &str = "Two independent calls: Paris, then Tokyo. \"Quote\" — ü.";

    /// Shaped like a live `tool_stream: true` stream: reasoning deltas, a content delta, two calls
    /// in fragments, and usage on the `finish_reason` chunk.
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
                json!({
                    "choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {"role": "assistant", "content": ""}}],
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
    fn reasoning_and_text_are_restored_on_the_next_turn_after_a_tool_call() {
        let response = parse_quiet(&live_tool_call_stream()).unwrap();

        // The runtime replays that content array verbatim as the next turn's assistant message.
        let follow_up = request_from(json!({
            "model": "glm-5.3",
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
            "model": "glm-5.3",
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
            "model": "glm-5.3",
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
            "model": "glm-5.3",
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

    // ── Scenario 7 — streaming ────────────────────────────────────────────────

    const INTERLEAVED_STREAM: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"The sky \"}}]}\n",
        "\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Rayleigh \"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"scatters blue.\"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"scattering.\"},\"finish_reason\":\"stop\"}]}\n",
        "data: [DONE]\n",
    );

    #[test]
    fn streamed_reasoning_and_text_go_to_separate_emitters_in_order() {
        let mut text_seen: Vec<String> = Vec::new();
        let mut thinking_seen: Vec<String> = Vec::new();

        let response = parse_zai_sse_body(
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
    fn a_stream_cut_before_its_finish_reason_fails_transiently() {
        let cut_off = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"The answer \"},\"finish_reason\":null}]}\n",
        );
        let blank_reason = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"\"}]}\n",
            "data: [DONE]\n",
        );
        let partial_tool_call = tool_delta(0, Some("call_1"), Some("bash"), Some("{\"cmd\":"));
        for body in [cut_off, blank_reason, partial_tool_call.as_str(), ""] {
            let failure = parse_quiet(body).unwrap_err();
            assert_eq!(
                failure,
                Failure::transient("driver: Z.ai stream ended with no finish_reason"),
                "body = {body:?}"
            );
            assert_eq!(
                failure.metadata(),
                vec![("transient".to_string(), "true".to_string())]
            );
        }
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

    // ── Scenario 8 — usage mapping with cached tokens ─────────────────────────

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
        translate_zai_response_to_murmur(&response).unwrap()
    }

    /// Usage on the `finish_reason` chunk, beside `choices`, as Z.ai sends it.
    fn streamed_with_usage(usage: Value) -> Value {
        let chunk = json!({
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "hi"}}],
            "usage": usage
        });
        parse_quiet(&format!("data: {chunk}\ndata: [DONE]\n")).unwrap()
    }

    fn both_paths(usage: Value) -> [Value; 2] {
        [
            buffered_with_usage(usage.clone()),
            streamed_with_usage(usage),
        ]
    }

    const LIVE_USAGE: &str = r#"{"prompt_tokens":4166,"completion_tokens":4,"total_tokens":4170,"prompt_tokens_details":{"cached_tokens":4160},"completion_tokens_details":{"reasoning_tokens":0}}"#;

    #[test]
    fn cached_tokens_sit_beside_input_tokens_and_sum_to_the_prompt_total() {
        let live: Value = serde_json::from_str(LIVE_USAGE).unwrap();
        for response in both_paths(live) {
            let usage = &response["usage"];
            assert_eq!(
                usage,
                &json!({"input_tokens": 6, "output_tokens": 4, "cached_tokens": 4160, "thinking_tokens": 0})
            );
            assert_eq!(
                reported_request(usage),
                4166,
                "input + cached is prompt_tokens"
            );
            assert!(
                usage.get("cache_write_tokens").is_none(),
                "Z.ai reports no cache write"
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
        for cached in [json!("4160"), json!(-1), json!(1.5), Value::Null] {
            for response in both_paths(json!({
                "prompt_tokens": 4166,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": cached.clone()}
            })) {
                assert_eq!(
                    response["usage"],
                    json!({"input_tokens": 4166, "output_tokens": 4}),
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
        assert!(translate_zai_response_to_murmur(&plain)
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
        let live: Value = serde_json::from_str(LIVE_USAGE).unwrap();
        assert_eq!(
            streamed_with_usage(live)["usage"]["cached_tokens"],
            json!(4160)
        );

        // Counts split across chunks are merged member by member before input is derived.
        let split = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":4166}}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":4160}}}\n",
            "data: [DONE]\n",
        );
        assert_eq!(
            parse_quiet(split).unwrap()["usage"],
            json!({"input_tokens": 6, "output_tokens": 4, "cached_tokens": 4160})
        );
    }

    // ── Scenario 9 — stop reasons ─────────────────────────────────────────────

    #[test]
    fn stop_tool_calls_and_length_map_to_the_runtime_stop_reasons() {
        for (finish, stop) in [
            ("stop", "end_turn"),
            ("tool_calls", "tool_call"),
            ("length", "max_tokens"),
        ] {
            assert_eq!(map_finish_reason(finish), Ok(stop));
            let response =
                json!({"choices": [{"finish_reason": finish, "message": {"content": "x"}}]});
            assert_eq!(
                translate_zai_response_to_murmur(&response).unwrap()["stop_reason"],
                json!(stop)
            );
        }
    }

    #[test]
    fn every_other_finish_reason_is_a_plain_error_naming_it_on_both_paths() {
        for reason in [
            "sensitive",
            "model_context_window_exceeded",
            "content_filter",
        ] {
            let expected =
                Failure::plain(format!("driver: unsupported Z.ai finish_reason '{reason}'"));
            let buffered =
                json!({"choices": [{"finish_reason": reason, "message": {"content": ""}}]});
            assert_eq!(
                translate_zai_response_to_murmur(&buffered).unwrap_err(),
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
                translate_zai_response_to_murmur(&json!({"choices": [choice]})).unwrap_err(),
                Failure::plain("driver: Z.ai response has no finish_reason"),
                "finish_reason = {finish_reason:?}"
            );
        }
    }

    // ── Scenario 10 — failure paths never panic ───────────────────────────────

    #[test]
    fn an_image_block_is_refused_before_any_body_is_built() {
        let request = request_from(json!({
            "model": "glm-5.3",
            "max_tokens": 8192,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is this?"},
                    {"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}}
                ]
            }]
        }));
        assert_eq!(
            build_provider_request(&request, &DriverConfig::default()).unwrap_err(),
            "driver: model 'glm-5.3' is text-only, so driver-zai refuses an image block"
        );
    }

    #[test]
    fn malformed_requests_are_refused_by_name() {
        let orphan_tool = request_from(json!({
            "model": "glm-5.3",
            "max_tokens": 8192,
            "messages": [{"role": "tool", "content": [{"type": "text", "text": "out"}]}]
        }));
        assert_eq!(
            translate_murmur_request_to_zai(&orphan_tool, &DriverConfig::default()).unwrap_err(),
            "driver: tool message is missing required field 'tool_call_id'"
        );

        let bad_role = request_from(json!({
            "model": "glm-5.3",
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
        let err = translate_zai_response_to_murmur(&buffered).unwrap_err();
        assert!(
            err.message
                .starts_with("driver: failed to parse Z.ai tool call arguments JSON: "),
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
                .starts_with("driver: failed to parse Z.ai tool call arguments JSON: "),
            "got {err}"
        );
        assert_eq!(err.retry, None);
    }

    #[test]
    fn a_response_with_no_choices_is_refused() {
        for response in [json!({}), json!({"choices": []}), json!({"choices": "x"})] {
            assert_eq!(
                translate_zai_response_to_murmur(&response).unwrap_err(),
                Failure::plain("driver: Z.ai response missing choices[0]")
            );
        }
    }

    #[test]
    fn a_provider_error_status_carries_the_status_and_body() {
        let body = r#"{"error":{"code":"1210","message":"This model always engages in thinking and cannot be disabled; please use low, high, or max"}}"#;
        let failure = status_failure(400, body);
        assert_eq!(
            error_payload(&failure.message),
            json!({"stop_reason": "error", "error": format!("HTTP 400: {body}")})
        );
        assert_eq!(failure.retry, None);
    }

    // ── Transient failures ────────────────────────────────────────────────────

    fn error_chunk(error: Value) -> String {
        format!("data: {}\n", json!({ "error": error }))
    }

    const TEXT_DELTA: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n";

    #[test]
    fn a_network_error_finish_reason_fails_transiently_on_both_paths() {
        let expected = Failure::transient("Z.ai stream failed: finish_reason 'network_error'");
        let buffered =
            json!({"choices": [{"finish_reason": "network_error", "message": {"content": "x"}}]});
        assert_eq!(
            translate_zai_response_to_murmur(&buffered).unwrap_err(),
            expected
        );
        let stream = format!(
            "{TEXT_DELTA}data: {}\ndata: [DONE]\n",
            json!({"choices": [{"delta": {}, "finish_reason": "network_error"}]})
        );
        assert_eq!(parse_quiet(&stream).unwrap_err(), expected);
        assert_eq!(map_finish_reason("network_error"), Err(expected));
    }

    #[test]
    fn each_transient_error_code_in_a_stream_fails_transiently_naming_it() {
        for (code, message) in [
            ("1200", "API Call Error"),
            ("1230", "API call process error"),
            (
                "1234",
                "Network error, error id: abc, please try again later",
            ),
            ("1302", "Rate limit reached for requests"),
            (
                "1305",
                "The service may be temporarily overloaded, please try again later",
            ),
        ] {
            let expected = Failure::transient(format!("Z.ai error: {code}: {message}"));
            let as_string = error_chunk(json!({"code": code, "message": message}));
            let as_number =
                error_chunk(json!({"code": code.parse::<u64>().unwrap(), "message": message}));
            for body in [
                as_string.clone(),
                as_number,
                format!("{TEXT_DELTA}{as_string}"),
                format!("{as_string}data: [DONE]\n"),
            ] {
                assert_eq!(parse_quiet(&body).unwrap_err(), expected, "body = {body:?}");
            }
        }
    }

    #[test]
    fn a_non_transient_error_chunk_then_eof_stays_plain_with_its_own_message() {
        for (error, message) in [
            (
                json!({"code": "1301", "message": "System detected potentially unsafe or sensitive content"}),
                "Z.ai error: 1301: System detected potentially unsafe or sensitive content",
            ),
            (
                json!({"code": 1214, "message": "Parameter `x` is invalid."}),
                "Z.ai error: 1214: Parameter `x` is invalid.",
            ),
            (
                json!({"code": "9999", "message": "something new"}),
                "Z.ai error: 9999: something new",
            ),
            (json!({}), "Z.ai error: unknown: unknown"),
            (
                json!({"message": "no code"}),
                "Z.ai error: unknown: no code",
            ),
        ] {
            let body = format!("{TEXT_DELTA}{}", error_chunk(error));
            assert_eq!(
                parse_quiet(&body).unwrap_err(),
                Failure::plain(message),
                "body = {body:?}"
            );
        }
    }

    #[test]
    fn an_error_chunk_takes_precedence_over_a_later_finish_reason() {
        let body = format!(
            "{}data: {}\n",
            error_chunk(json!({"code": "1234", "message": "Network error"})),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        );
        assert_eq!(
            parse_quiet(&body).unwrap_err(),
            Failure::transient("Z.ai error: 1234: Network error")
        );
    }

    #[test]
    fn a_pre_flight_refusal_is_a_plain_failure() {
        let request = request_for_model("glm-4.6", 8192);
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

    // ── Statuses ──────────────────────────────────────────────────────────────

    fn quota_body(code: &str) -> String {
        json!({"error": {"code": code, "message": "limit"}}).to_string()
    }

    #[test]
    fn every_status_but_a_quota_429_is_left_to_murmur() {
        let rate_limited = quota_body("1302");
        let overloaded = quota_body("1305");
        for (status, body) in [
            (400, quota_body("1210")),
            (401, quota_body("1000")),
            (403, quota_body("1220")),
            (404, "not found".to_string()),
            (408, String::new()),
            (413, String::new()),
            (422, quota_body("1214")),
            (429, rate_limited),
            (429, overloaded),
            (429, "not json".to_string()),
            (429, quota_body("1113").replace("error", "err")),
            (500, quota_body("1234")),
            (502, String::new()),
            (503, String::new()),
            (504, String::new()),
            (529, String::new()),
        ] {
            assert_eq!(
                status_failure(status, &body),
                Failure::plain(format!("HTTP {status}: {body}")),
                "status = {status}, body = {body}"
            );
        }
    }

    #[test]
    fn a_quota_or_plan_429_vetoes_the_retry() {
        for code in [
            "1113", "1308", "1309", "1310", "1311", "1313", "1314", "1315", "1316", "1317", "1318",
            "1319", "1320", "1321",
        ] {
            let body = quota_body(code);
            let failure = status_failure(429, &body);
            assert_eq!(failure, Failure::permanent(format!("HTTP 429: {body}")));
            assert_eq!(failure.retry, Some(Retry::Permanent));
        }
        let numeric = r#"{"error":{"code":1113,"message":"Insufficient balance or no resource package. Please recharge."}}"#;
        assert_eq!(
            status_failure(429, numeric),
            Failure::permanent(format!("HTTP 429: {numeric}"))
        );
    }

    #[test]
    fn a_quota_code_on_any_status_but_429_is_left_to_murmur() {
        for status in [400, 402, 500, 503] {
            let body = quota_body("1113");
            assert_eq!(status_failure(status, &body).retry, None);
        }
    }

    // ── Response content order on the JSON fallback ───────────────────────────

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
            translate_zai_response_to_murmur(&buffered).unwrap()["content"],
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
        let response = translate_zai_response_to_murmur(&buffered).unwrap();
        assert_eq!(
            response["content"],
            json!([{"type": "text", "text": "Rayleigh scattering."}])
        );
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    // ── Scenario 11 — source and manifest shape ───────────────────────────────

    /// The source of the `#[cfg(target_arch = "wasm32")]` module, which a host test cannot
    /// compile but can read.
    fn wasm_module_source() -> &'static str {
        let source = include_str!("lib.rs");
        let start = source
            .find("mod wasm_driver {")
            .expect("the wasm driver module must exist");
        &source[start..]
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
    fn config_parse_precedes_every_other_environment_read_in_run_inner() {
        // A rejected config must fail before an HTTP request can be dispatched. `run_inner` is
        // wasm-only, so its statement order is asserted from the source.
        let module = wasm_module_source();
        let run_inner = &module[module.find("fn run_inner(").expect("run_inner must exist")..];
        let parse_at = run_inner
            .find("parse_driver_config(")
            .expect("run_inner must parse the driver config");
        for later in [
            "INFERENCE_ENDPOINT_VAR",
            "build_provider_request(",
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
    fn no_reasoning_sentinel_scanner_anywhere_in_the_crate() {
        // Z.ai delivers reasoning on its own delta field, so an inline reasoning-tag scanner
        // here would be a second, silently-wrong reasoning path. The needle is assembled rather
        // than written out so that this assertion does not itself become the only occurrence.
        let sentinel = concat!("<", "think");
        assert!(!include_str!("lib.rs").contains(sentinel));
    }

    #[test]
    fn no_provider_url_is_hardcoded_anywhere_in_the_crate() {
        // Where inference goes is the host's fact. A literal provider URL in a driver is a
        // value that would become silently effective the moment the host stopped supplying
        // one, and it would carry the capsule's credentials with it. The needle is assembled
        // rather than written out so that this assertion does not itself become the only
        // occurrence of what it forbids.
        let provider_url = concat!("https://", "api.");
        assert!(
            !include_str!("lib.rs").contains(provider_url),
            "the driver must not name a provider URL; the endpoint comes from the host"
        );
    }

    #[test]
    fn run_inner_resolves_the_endpoint_through_the_shared_contract() {
        // One implementation of "a missing endpoint is a refusal" exists, in
        // murmur-driver-env. An inline `std::env::var` read here would be a second one, free
        // to disagree with it. `run_inner` is wasm-only, so this is asserted from the source;
        // the needle is assembled so that this test is not its own evidence.
        let source = include_str!("lib.rs");
        let run_inner = &source[source.find("fn run_inner(").expect("run_inner must exist")..];
        assert!(
            run_inner.contains(concat!("require_", "endpoint(")),
            "run_inner must resolve the endpoint through murmur_driver_env::require_endpoint"
        );
    }

    #[test]
    fn manifest_declares_the_upstream_auth_scheme_the_gateway_will_read() {
        // The runtime builds the auth header from these two fields, so a dropped quote pair
        // (`value: {key}` is a YAML flow mapping, not a string) or a renamed key leaves the
        // gateway with no scheme to apply and every request unauthenticated.
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "drivers/driver-zai/murmur.yaml must contain verbatim:\n{BLOCK}"
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
            "models:\n  glm-5.3:\n    max_output_tokens: 131072\n    context_window: 1048576\n"
        );
        assert!(
            include_str!("../murmur.yaml").contains(&block),
            "drivers/driver-zai/murmur.yaml must contain verbatim:\n{block}"
        );
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
                "driver-zai: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn wasm_driver_reports_tool_calls_on_murmur_stream_events_only() {
        let source = include_str!("lib.rs");
        // Ends at `mod tests` so the needles below cannot match themselves.
        let start = source
            .find("\nmod wasm_driver {")
            .expect("mod wasm_driver must exist");
        let end = source.find("\nmod tests {").expect("mod tests must exist");
        let wasm = &source[start..end];
        for call in [
            "murmur::stream::events::tool_call_started(",
            "murmur::stream::events::tool_call_input_bytes(",
        ] {
            assert!(wasm.contains(call), "mod wasm_driver must call {call}");
        }
        for retired in [concat!("murmur::", "text"), concat!("murmur:", "text")] {
            assert!(!source.contains(retired), "lib.rs must not name {retired}");
        }
    }

    // ── Tool-call signals ─────────────────────────────────────────────────────

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

    fn parse_with_signals(body: &str) -> (Value, Vec<RecordedSignal>) {
        let (result, signals) = parse_zai_sse_body_with_signals(body, &mut |_| {}, &mut |_| {});
        (result.unwrap(), signals)
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
    fn a_tool_call_index_past_the_limit_fails_the_turn_without_allocating() {
        use super::{process_zai_sse_line, UsageTokens};

        for index in [1024, 4_294_967_296, u64::MAX] {
            let line = tool_delta(index, Some("call_x"), Some("bash"), Some(""));
            let mut tool_states = Vec::new();
            let mut signals = Vec::new();
            let result = process_zai_sse_line(
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
            tool_delta(4_294_967_296, Some("call_x"), Some("bash"), Some("")),
            TOOL_CALLS_FINISH.to_string(),
        ]
        .concat();
        let (result, _) = parse_zai_sse_body_with_signals(&body, &mut |_| {}, &mut |_| {});
        assert_eq!(
            result,
            Err(Failure::plain(
                "driver: tool call index 4294967296 is past the limit of 1024 per turn"
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

    #[test]
    fn tool_call_whose_id_arrives_late_is_reported_once_it_does() {
        let head = [
            tool_delta(0, None, Some("bash"), Some(r#"{"cmd":"#)),
            tool_delta(0, None, None, Some(r#""ZQXJ"}"#)),
        ]
        .concat();
        let (_, before_the_id) = parse_zai_sse_body_with_signals(&head, &mut |_| {}, &mut |_| {});
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
    fn tool_call_that_never_gets_an_id_signals_nothing_and_is_returned_as_before() {
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
        let (result, signals) =
            parse_zai_sse_body_with_signals(&body, &mut |c| text.push(c.to_string()), &mut |c| {
                thinking.push(c.to_string())
            });

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

    // ── replay_reasoning — earlier turns' reasoning on or off ─────────────────

    /// System, user, a tool-call turn with reasoning and text, its tool result, a plain turn with
    /// reasoning, then the next user turn.
    fn replay_request() -> MurmurRequest {
        request_from(json!({
            "model": "glm-5.3",
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

    /// The S2 fixture's body as driver-zai 0.4.0 built it, captured from `fc72bb4`.
    const BODY_AT_0_4_0: &str = r#"{"max_tokens":8192,"messages":[{"content":"You are a careful assistant.","role":"system"},{"content":"How long is notes.txt?","role":"user"},{"content":"Reading it.","reasoning_content":"Read the file first. \"Quoted\" — ü.","role":"assistant","tool_calls":[{"function":{"arguments":"{\"path\":\"notes.txt\"}","name":"read_file"},"id":"call_1","type":"function"}]},{"content":"a\nb\nc","role":"tool","tool_call_id":"call_1"},{"content":"It has three lines.","reasoning_content":"Three lines.","role":"assistant"},{"content":"And how many words?","role":"user"}],"model":"glm-5.3","reasoning_effort":"max","stream":true,"thinking":{"clear_thinking":false,"type":"enabled"},"tool_stream":true,"tools":[{"function":{"name":"read_file","parameters":{"properties":{"path":{"type":"string"}},"type":"object"}},"type":"function"}]}"#;

    /// Drop every `reasoning_content` member from the body's messages.
    fn without_reasoning_content(mut body: Value) -> Value {
        for message in body["messages"].as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("reasoning_content");
        }
        body
    }

    #[test]
    fn the_default_body_is_byte_identical_to_0_4_0_s() {
        let body = body_for(&replay_request(), &DriverConfig::default());
        assert_eq!(body.to_string(), BODY_AT_0_4_0);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
    }

    #[test]
    fn absent_empty_whitespace_empty_object_and_true_configs_build_the_same_body() {
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
                BODY_AT_0_4_0,
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
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": true})
        );

        let mut expected =
            without_reasoning_content(body_for(&replay_request(), &DriverConfig::default()));
        expected["thinking"]["clear_thinking"] = json!(true);
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
            "model": "glm-5.3",
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
    fn replay_reasoning_false_combines_with_reasoning_effort_and_thinking_enabled() {
        let config =
            config(r#"{"reasoning_effort":"high","replay_reasoning":false,"thinking":"enabled"}"#);
        assert_eq!(
            config,
            DriverConfig {
                reasoning_effort: "high".to_string(),
                replay_reasoning: false,
            }
        );
        let body = body_for(&replay_request(), &config);
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": true})
        );
        assert!(!contains_key_anywhere(&body, "reasoning_content"), "{body}");
    }

    #[test]
    fn a_params_thinking_cannot_override_the_replay_stamp() {
        let mut request = json!({
            "model": "glm-5.3",
            "max_tokens": 1000,
            "messages": [],
            "params": {"thinking": {"type": "enabled", "clear_thinking": false}}
        });
        let body = body_for(
            &request_from(request.clone()),
            &config(r#"{"replay_reasoning":false}"#),
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": true})
        );
        request["params"]["thinking"]["clear_thinking"] = json!(true);
        let body = body_for(&request_from(request), &DriverConfig::default());
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
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
        let fallback = translate_zai_response_to_murmur(&json!({
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
