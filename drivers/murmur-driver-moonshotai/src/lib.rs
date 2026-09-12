// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use serde::Deserialize;
use serde_json::{json, Map, Value};

// ── Model validation ──────────────────────────────────────────────────────────

/// The one model family this driver speaks. Moonshot also ships `kimi-k2.7-code`,
/// `kimi-k2.7-code-highspeed` and `kimi-k2.6`; none of them are implemented here. This constant
/// is the classification seam a second family would be added at — nothing else in the crate
/// branches on the model name.
const SUPPORTED_MODELS: &[&str] = &["kimi-k3"];

/// Reject an unsupported model before a request body is built, so it costs no HTTP call.
fn validate_model(model: &str) -> Result<(), String> {
    if !SUPPORTED_MODELS.contains(&model) {
        return Err(format!(
            "driver: model '{model}' is not supported by murmur-driver-moonshotai. \
             Supported models: {}.",
            SUPPORTED_MODELS.join(", ")
        ));
    }
    Ok(())
}

// ── Output-token ceiling ──────────────────────────────────────────────────────

/// Moonshot's ceiling for `max_completion_tokens`. A larger value is refused here rather than
/// clamped silently or spent on a provider 400.
const MAX_COMPLETION_TOKENS_CEILING: u64 = 1_048_576;

fn validate_max_tokens(max_tokens: u64) -> Result<(), String> {
    if max_tokens > MAX_COMPLETION_TOKENS_CEILING {
        return Err(format!(
            "driver: inference.max_tokens {max_tokens} exceeds the Moonshot \
             max_completion_tokens ceiling of {MAX_COMPLETION_TOKENS_CEILING}"
        ));
    }
    Ok(())
}

// ── Fixed sampling parameters ─────────────────────────────────────────────────

/// Sampling parameters Moonshot fixes server-side — `temperature=1.0`, `top_p=0.95`, `n=1`,
/// `presence_penalty=0`, `frequency_penalty=0`. They are omitted from every request instead of
/// forwarded and ignored. This is the one strip list, consulted by the one `params` loop; a
/// second strip mechanism alongside it would be two things to keep in step.
const PROVIDER_FIXED_PARAMS: &[&str] = &[
    "frequency_penalty",
    "n",
    "presence_penalty",
    "temperature",
    "top_p",
];

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a hard error on the first inference call, before
// any HTTP request is dispatched — a declared setting that is parsed and ignored is a defect,
// not a convenience.

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &["reasoning_effort", "response_format"];

/// Moonshot's own effort vocabulary, in ascending order of spend. `medium` is OpenAI's spelling
/// and is not accepted here: each driver owns its provider's vocabulary rather than a
/// murmur-wide normalisation across providers' effort dials.
const REASONING_EFFORT_VALUES: &[&str] = &["low", "high", "max"];

/// Moonshot's own default. Kept rather than substituted with a cheaper tier, and stamped on
/// every request so the effective value is visible in a recorded request rather than left to
/// drift with the provider's default.
const DEFAULT_REASONING_EFFORT: &str = "max";

/// `thinking` is refused with its own message rather than the generic unrecognised-key one:
/// `kimi-k3` always reasons and the provider exposes no switch, so there is nothing behind the
/// key to honour at any value. If `kimi-k2.6` — the one Moonshot model with a genuine
/// thinking/non-thinking toggle — is ever added, the key becomes meaningful and must keep this
/// spelling, matching murmur-driver-anthropic and murmur-driver-deepseek.
const THINKING_REFUSAL: &str = "driver: inference.driver.config 'thinking' is not accepted by \
     murmur-driver-moonshotai: kimi-k3 always reasons and Moonshot exposes no switch to turn it \
     off, at any value. Remove the key.";

/// `strict: false` is refused rather than silently upgraded to `true`.
const STRICT_FALSE_REFUSAL: &str =
    "driver: inference.driver.config 'response_format.json_schema.strict' must be true — \
     strict: false is refused rather than silently upgraded, because a schema the provider does \
     not enforce is not the guarantee this key offers";

/// The two dials read from `inference.driver.config`.
#[derive(Debug, Clone, PartialEq)]
struct DriverConfig {
    /// `reasoning_effort` — one of [`REASONING_EFFORT_VALUES`], lowercased. Always Some; an
    /// unconfigured capsule gets [`DEFAULT_REASONING_EFFORT`].
    reasoning_effort: String,
    /// `response_format` — validated and normalised (`strict` defaulted to `true`), then
    /// forwarded to the provider body under the same name.
    response_format: Option<Value>,
}

impl Default for DriverConfig {
    fn default() -> Self {
        DriverConfig {
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
            response_format: None,
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

    // Checked before the generic sweep so the operator is told *why* the key cannot exist here,
    // rather than being told it was merely misspelled.
    if config.contains_key("thinking") {
        return Err(THINKING_REFUSAL.to_string());
    }

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
        reasoning_effort: parse_reasoning_effort(config.get("reasoning_effort"))?,
        response_format: config
            .get("response_format")
            .map(validate_response_format)
            .transpose()?,
    })
}

/// Trimmed and matched case-insensitively; sent lowercased. The rejected value appears verbatim
/// in the error alongside the accepted set, so a manifest holding OpenAI's `medium` says so.
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

/// Validate loudly and minimally, then return the object to forward under `response_format`.
///
/// The constraint applies to the final `content` only. `reasoning_content` is never parsed as
/// the structured payload: it becomes a `thinking` block exactly as it does without a schema,
/// and the JSON payload lands in the `text` block.
fn validate_response_format(value: &Value) -> Result<Value, String> {
    let Some(object) = value.as_object() else {
        return Err(format!(
            "driver: inference.driver.config 'response_format' must be a JSON object, got {value}"
        ));
    };

    let declared_type = object.get("type");
    if declared_type.and_then(Value::as_str) != Some("json_schema") {
        return Err(format!(
            "driver: inference.driver.config 'response_format.type' must be \"json_schema\", got {}",
            declared_type
                .map(Value::to_string)
                .unwrap_or_else(|| "no value".to_string())
        ));
    }

    let Some(schema_block) = object.get("json_schema").and_then(Value::as_object) else {
        return Err(
            "driver: inference.driver.config 'response_format.json_schema' must be an object"
                .to_string(),
        );
    };

    if !schema_block.get("schema").is_some_and(Value::is_object) {
        return Err(
            "driver: inference.driver.config 'response_format.json_schema.schema' is required \
             and must be an object"
                .to_string(),
        );
    }

    match schema_block.get("strict") {
        None | Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => return Err(STRICT_FALSE_REFUSAL.to_string()),
        Some(other) => {
            return Err(format!(
                "driver: inference.driver.config 'response_format.json_schema.strict' must be a \
                 boolean, got {other}"
            ))
        }
    }

    // `strict` defaults to true when absent, and is normalised onto the body so the effective
    // value is visible in a recorded request.
    let mut normalised_schema = schema_block.clone();
    normalised_schema.insert("strict".to_string(), Value::Bool(true));
    let mut normalised = object.clone();
    normalised.insert("json_schema".to_string(), Value::Object(normalised_schema));
    Ok(Value::Object(normalised))
}

// ── Murmur request types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MurmurRequest {
    // The host sends a prompt-cache routing hint as a reserved top-level `prompt_cache_key`
    // member. It is deliberately not declared here: Moonshot's context caching is automatic and
    // its Chat Completions API defines no cache-key field. An undeclared member is dropped by
    // serde, so it cannot reach the provider body at any nesting level — do not add a field and
    // filter it out later.
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
    Text { text: String },
    // Reasoning persisted from a prior driver response. The runtime stores and replays whatever
    // content blocks a driver emits without inspecting them, so this block — emitted by this
    // driver and read back by it — is where preserved reasoning lives. No runtime concept and no
    // interface widening is involved.
    Thinking { text: String },
    Image { source: MurmurImageSource },
    ToolCall { id: String, name: String, input: Value },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct MurmurImageSource {
    media_type: String,
    data: String,
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

// ── Request translation ───────────────────────────────────────────────────────

/// Every check that can be made without building a body or touching the network.
fn preflight_request(request: &MurmurRequest) -> Result<(), String> {
    validate_model(&request.model)?;
    validate_max_tokens(request.max_tokens)?;
    Ok(())
}

/// The body `run_inner` puts on the wire: pre-flight, translate, stamp the streaming flags.
/// Tests assert on this rather than on a bare translation.
fn build_provider_request(
    request: &MurmurRequest,
    config: &DriverConfig,
) -> Result<Value, String> {
    preflight_request(request)?;
    let mut body = translate_murmur_request_to_moonshot(request, config)?;
    stamp_streaming_flags(&mut body);
    Ok(body)
}

fn translate_murmur_request_to_moonshot(
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
    body.insert("model".to_string(), Value::String(request.model.clone()));
    // Moonshot's Chat Completions surface spells the output cap `max_completion_tokens`; it
    // defines no `max_tokens` member, so none is emitted.
    body.insert(
        "max_completion_tokens".to_string(),
        Value::from(request.max_tokens),
    );
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

    if let Some(response_format) = config.response_format.as_ref() {
        body.insert("response_format".to_string(), response_format.clone());
    }

    // The one params pass-through loop, consulting the one strip list.
    for (key, value) in &request.params {
        if body.contains_key(key) {
            continue;
        }
        if PROVIDER_FIXED_PARAMS.contains(&key.as_str()) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }

    Ok(Value::Object(body))
}

fn translate_user_message(message: &MurmurMessage) -> Value {
    let has_image = message
        .content
        .iter()
        .any(|block| matches!(block, MurmurContentBlock::Image { .. }));

    if has_image {
        let parts = message
            .content
            .iter()
            .filter_map(|block| match block {
                MurmurContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                MurmurContentBlock::Image { source } => Some(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", source.media_type, source.data),
                    }
                })),
                _ => None,
            })
            .collect::<Vec<_>>();

        json!({"role": "user", "content": parts})
    } else {
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                MurmurContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        json!({"role": "user", "content": text})
    }
}

/// Rebuild an assistant turn, reattaching the reasoning this driver preserved as a `thinking`
/// block. Moonshot's Preserved Thinking asks for the assistant message to come back complete on
/// multi-turn conversations and tool calls, so `reasoning_content` is restored on both the
/// tool-call turn and the plain-text turn.
///
/// When no `thinking` block is present the member is omitted entirely — never sent as an empty
/// string — because that is the ordinary state of a history that has been compacted.
fn translate_assistant_message(message: &MurmurMessage) -> Result<Value, String> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            MurmurContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

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
        json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": tool_calls,
        })
    };

    if let Some(reasoning_content) = thinking_text(&message.content) {
        msg["reasoning_content"] = json!(reasoning_content);
    }

    Ok(msg)
}

fn translate_tool_message(message: &MurmurMessage) -> Result<Value, String> {
    let tool_call_id = message.tool_call_id.clone().ok_or_else(|| {
        "driver: tool message is missing required field 'tool_call_id'".to_string()
    })?;

    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            MurmurContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": text,
    }))
}

// ── Provider token usage ──────────────────────────────────────────────────────

/// Provider-reported token counts, carried on the reserved top-level `usage` object of the
/// translated response. Every member is independently optional: a count the provider did not
/// report is omitted rather than sent as `0`, because the host records a reported `0` as a real
/// zero and a fabricated one reads as a cache miss on the `inference` trace event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTokens {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

impl UsageTokens {
    /// Take every member `later` reports, keeping the current value for the rest. Streaming
    /// usage arrives across more than one chunk, each carrying only part of the picture.
    fn merge_from(&mut self, later: UsageTokens) {
        if later.input_tokens.is_some() {
            self.input_tokens = later.input_tokens;
        }
        if later.output_tokens.is_some() {
            self.output_tokens = later.output_tokens;
        }
        if later.cached_tokens.is_some() {
            self.cached_tokens = later.cached_tokens;
        }
        if later.cache_write_tokens.is_some() {
            self.cache_write_tokens = later.cache_write_tokens;
        }
    }

    /// `None` when no member survived, in which case the response carries no `usage` key at all.
    fn to_value(self) -> Option<Value> {
        let mut obj = Map::new();
        for (key, count) in [
            ("input_tokens", self.input_tokens),
            ("output_tokens", self.output_tokens),
            ("cached_tokens", self.cached_tokens),
            ("cache_write_tokens", self.cache_write_tokens),
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

/// Read the contract members out of a Moonshot `usage` object. `cache_write_tokens` is always
/// omitted: Moonshot's caching is automatic and it reports no cache-write count, so any value
/// put there would be invented. A member that is absent, null, or not a non-negative integer is
/// dropped; its siblings are kept.
fn extract_moonshot_usage(usage: &Value) -> UsageTokens {
    UsageTokens {
        input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
        cached_tokens: usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        cache_write_tokens: None,
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

/// Force streaming on, overriding any `stream` key from `params`, and opt into the usage-bearing
/// final chunk. `stream_options` is only valid alongside `stream: true`, so the two are stamped
/// together; without it Moonshot streams no token counts at all.
fn stamp_streaming_flags(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
        obj.insert("stream_options".to_string(), json!({"include_usage": true}));
    }
}

// ── Stop-reason mapping ───────────────────────────────────────────────────────

/// Map a Moonshot `finish_reason` to a murmur `stop_reason`. A missing reason is `end_turn`; an
/// unmapped one is an error naming it rather than a guess.
fn map_finish_reason(finish_reason: Option<&str>) -> Result<&'static str, String> {
    match finish_reason {
        Some("stop") | None => Ok("end_turn"),
        Some("tool_calls") => Ok("tool_call"),
        Some("length") => Ok("max_tokens"),
        Some(other) => Err(format!(
            "driver: unsupported Moonshot finish_reason '{other}'"
        )),
    }
}

// ── Response translation (non-streaming fallback) ─────────────────────────────

/// The non-streaming JSON path, reached when the response body's first non-whitespace byte is
/// `{` or `[` — scripted test servers, and providers that ignore `stream: true`.
fn translate_moonshot_response_to_murmur(response: &Value) -> Result<Value, String> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: Moonshot response missing choices[0]".to_string())?;

    let stop_reason = map_finish_reason(
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
    )?;

    let message = choice.get("message").unwrap_or(&Value::Null);

    // Never parsed as the structured payload, whatever it looks like: it is the model's
    // reasoning, and it becomes a thinking block byte for byte.
    let reasoning_content = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let mut content: Vec<Value> = Vec::new();

    // Thinking first — it is what makes the tool-call round trip work and what the UI renders
    // separately.
    if let Some(rc) = reasoning_content {
        content.push(json!({"type": "thinking", "text": rc}));
    }

    // An OpenAI-compatible server may report `tool_calls: []` on a plain answer turn. Taking the
    // tool-call branch on an empty array would drop the answer text, so the branch is chosen on
    // a call actually being present rather than on the member being present.
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|calls| !calls.is_empty());

    if let Some(tool_calls) = tool_calls {
        for call in tool_calls {
            let arguments_raw = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");

            let arguments: Value = serde_json::from_str(arguments_raw).map_err(|err| {
                format!("driver: failed to parse Moonshot tool call arguments JSON: {err}")
            })?;

            content.push(json!({
                "type": "tool_call",
                "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                "name": call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "input": arguments,
            }));
        }
    } else {
        let text = match message.get("content") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }

    let usage = response
        .get("usage")
        .map(extract_moonshot_usage)
        .unwrap_or_default();

    Ok(murmur_response(stop_reason, content, usage))
}

// ── SSE streaming ─────────────────────────────────────────────────────────────

struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

/// Process one complete SSE line. Returns `true` when the stream is done (`data: [DONE]`).
///
/// Moonshot streams OpenAI-compatible chat-completions chunks plus a separate
/// `reasoning_content` delta. `delta.reasoning_content` is routed to `emit_thinking`,
/// `delta.content` to `emit_text`; the two never mix, and nothing in this crate scans the text
/// for reasoning sentinels.
fn process_moonshot_sse_line(
    line: &str,
    tool_states: &mut Vec<ToolCallState>,
    stop_reason: &mut Option<String>,
    usage: &mut UsageTokens,
    emit_text: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
) -> bool {
    if line == "data: [DONE]" {
        return true;
    }
    let Some(json_str) = line.strip_prefix("data: ") else {
        return false;
    };
    let Ok(data) = serde_json::from_str::<Value>(json_str) else {
        return false;
    };

    // Read before the `choices`-shaped early returns below: the usage-bearing final chunk has an
    // empty `choices` array and would otherwise be discarded.
    if let Some(reported) = data.get("usage").filter(|value| !value.is_null()) {
        usage.merge_from(extract_moonshot_usage(reported));
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
        return false;
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
            let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            while tool_states.len() <= idx {
                tool_states.push(ToolCallState {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
            }
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                if tool_states[idx].id.is_empty() {
                    tool_states[idx].id = id.to_string();
                }
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(Value::as_str) {
                    if tool_states[idx].name.is_empty() {
                        tool_states[idx].name = name.to_string();
                    }
                }
                if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                    tool_states[idx].arguments.push_str(args);
                }
            }
        }
    }

    false
}

/// Turn the accumulated stream state into the murmur response. `content` is `thinking` first,
/// then either the `tool_call` blocks or the `text` block.
fn assemble_moonshot_streaming_response(
    text_acc: &str,
    reasoning_acc: &str,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
    usage: UsageTokens,
) -> Result<Value, String> {
    let stop_reason_str = map_finish_reason(stop_reason.as_deref())?;

    let mut tool_content = Vec::new();
    for state in tool_states {
        if state.id.is_empty() && state.name.is_empty() {
            continue;
        }
        let arguments = if state.arguments.is_empty() {
            "{}"
        } else {
            state.arguments.as_str()
        };
        let input: Value = serde_json::from_str(arguments).map_err(|err| {
            format!("driver: failed to parse Moonshot tool call arguments JSON: {err}")
        })?;
        tool_content.push(json!({
            "type": "tool_call",
            "id": state.id,
            "name": state.name,
            "input": input,
        }));
    }

    let mut content = Vec::new();
    if !reasoning_acc.is_empty() {
        content.push(json!({"type": "thinking", "text": reasoning_acc}));
    }
    if !tool_content.is_empty() {
        content.extend(tool_content);
    } else if !text_acc.is_empty() {
        content.push(json!({"type": "text", "text": text_acc}));
    }

    Ok(murmur_response(stop_reason_str, content, usage))
}

/// Parse a complete SSE body string, driving the same line processor the wasm module drives.
/// This is how the streaming assertions run host-side, with no network and no provider account.
#[cfg(test)]
fn parse_moonshot_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, String> {
    let mut text_acc = String::new();
    let mut reasoning_acc = String::new();
    let mut tool_states: Vec<ToolCallState> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut usage = UsageTokens::default();

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
            process_moonshot_sse_line(
                line,
                &mut tool_states,
                &mut stop_reason,
                &mut usage,
                &mut et,
                &mut eth,
            )
        };
        if done {
            break;
        }
    }

    assemble_moonshot_streaming_response(&text_acc, &reasoning_acc, tool_states, stop_reason, usage)
}

// ── Error payloads ────────────────────────────────────────────────────────────

fn error_payload(message: &str) -> Value {
    json!({
        "stop_reason": "error",
        "error": message,
    })
}

/// The payload for a provider status `>= 400`. Same shape as any other driver error, carrying
/// the status and the provider's own body so the operator sees what Moonshot said.
fn http_error_payload(status: u16, body: &str) -> Value {
    error_payload(&format!("HTTP {status}: {body}"))
}

// ── WASM driver module ────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_driver {
    use super::{
        assemble_moonshot_streaming_response, build_provider_request, error_payload,
        http_error_payload, parse_driver_config, process_moonshot_sse_line,
        translate_moonshot_response_to_murmur, MurmurRequest, ToolCallState, UsageTokens,
    };
    use serde_json::Value;

    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "driver",
        generate_all,
    });

    pub struct MoonshotDriver;

    impl exports::murmur::tool::run::Guest for MoonshotDriver {
        fn run(
            input: exports::murmur::tool::run::ToolInput,
        ) -> exports::murmur::tool::run::ToolResult {
            let response = match run_inner(input) {
                Ok(value) => value,
                Err(err) => error_payload(&err),
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
                metadata: vec![],
            }
        }
    }

    fn run_inner(input: exports::murmur::tool::run::ToolInput) -> Result<Value, String> {
        // First fallible step: a config the driver cannot honour fails here, before any other
        // environment read and before any HTTP request is dispatched.
        let driver_config = std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").ok();
        let config = parse_driver_config(driver_config.as_deref())?;

        let endpoint = murmur_driver_env::require_endpoint(
            std::env::var(murmur_driver_env::INFERENCE_ENDPOINT_VAR)
                .ok()
                .as_deref(),
        )?;
        // The only place a provider key is read. There is no MOONSHOT_API_KEY variable in the
        // driver environment; a capsule author writes `api_key: ${MOONSHOT_API_KEY}` under
        // `inference:` and the runtime delivers the resolved value here.
        let api_key = std::env::var("MURMUR_INFERENCE_API_KEY").ok();

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

        let mut headers = vec![
            ("content-type", "application/json".to_string()),
            ("content-length", body.len().to_string()),
        ];

        if let Some(key) = api_key.as_ref().map(|k| k.trim()).filter(|k| !k.is_empty()) {
            headers.push(("authorization", format!("Bearer {key}")));
        }

        let response = dispatch_request(&url, headers, &body)?;
        let status = response.status();

        if status >= 400 {
            let text = consume_body_as_string(response)?;
            return Ok(http_error_payload(status, &text));
        }

        let incoming_body = response
            .consume()
            .map_err(|()| "driver: failed to consume response body".to_string())?;
        let stream = incoming_body
            .stream()
            .map_err(|()| "driver: failed to stream response body".to_string())?;

        // Detect format from first read: JSON bodies start with '{' or '['.
        let first = read_chunk(&stream)?;
        let is_json = first
            .iter()
            .find(|&&b| !b.is_ascii_whitespace())
            .map(|&b| b == b'{' || b == b'[')
            .unwrap_or(false);

        let result = if is_json {
            // Non-streaming fallback (test servers, providers ignoring stream:true).
            let mut all = first;
            loop {
                let chunk = read_chunk(&stream)?;
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
                .map_err(|err| format!("driver: failed to parse Moonshot response JSON: {err}"))?;
            translate_moonshot_response_to_murmur(&json)?
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
             -> bool {
                let mut et = |t: &str| {
                    murmur::text::chunks::emit_chunk(t);
                    text_acc.push_str(t);
                };
                let mut eth = |t: &str| {
                    murmur::text::chunks::emit_thinking_chunk(t);
                    reasoning_acc.push_str(t);
                };
                process_moonshot_sse_line(line, tool_states, stop_reason, usage, &mut et, &mut eth)
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
                    );
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
                    let chunk = read_chunk(&stream)?;
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
                            );
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
            assemble_moonshot_streaming_response(
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
        {
            let stream = outgoing_body
                .write()
                .map_err(|()| "driver: failed to open request body stream".to_string())?;
            let mut remaining: &[u8] = body;
            while !remaining.is_empty() {
                let budget = stream
                    .check_write()
                    .map_err(|e| format!("driver: check-write failed: {e:?}"))?
                    as usize;
                if budget == 0 {
                    stream.subscribe().block();
                    continue;
                }
                let n = budget.min(remaining.len());
                stream
                    .write(&remaining[..n])
                    .map_err(|e| format!("driver: write failed: {e:?}"))?;
                remaining = &remaining[n..];
            }
            stream
                .flush()
                .map_err(|e| format!("driver: flush failed: {e:?}"))?;
            stream.subscribe().block();
        }
        wasip2::http::types::OutgoingBody::finish(outgoing_body, None)
            .map_err(|err| format!("driver: failed to finalize request body: {err:?}"))?;

        let future = wasip2::http::outgoing_handler::handle(request, None)
            .map_err(|err| format!("driver: failed to dispatch HTTP request: {err:?}"))?;

        await_response(future)
    }

    fn consume_body_as_string(
        response: wasip2::http::types::IncomingResponse,
    ) -> Result<String, String> {
        let incoming_body = response
            .consume()
            .map_err(|()| "driver: failed to consume error response body".to_string())?;
        let stream = incoming_body
            .stream()
            .map_err(|()| "driver: failed to stream error response body".to_string())?;
        let mut bytes = Vec::new();
        loop {
            let chunk = read_chunk(&stream)?;
            if chunk.is_empty() {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        drop(stream);
        let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
        String::from_utf8(bytes)
            .map_err(|err| format!("driver: error response body is not UTF-8: {err}"))
    }

    fn read_chunk(stream: &wasip2::io::streams::InputStream) -> Result<Vec<u8>, String> {
        match stream.blocking_read(16 * 1024) {
            Ok(chunk) => Ok(chunk),
            Err(wasip2::io::streams::StreamError::Closed) => Ok(Vec::new()),
            Err(err) => Err(format!("driver: failed to read response stream: {err:?}")),
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

    export!(MoonshotDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Access to `kimi-k3` is gated on a funded Moonshot account, so nothing here talks to the
// provider. Every assertion is driven by a fixture and the host-side whole-stream parser.

#[cfg(test)]
mod tests {
    use super::{
        build_provider_request, http_error_payload, parse_driver_config, parse_moonshot_sse_body,
        translate_moonshot_response_to_murmur, translate_murmur_request_to_moonshot,
        validate_model, DriverConfig, MurmurRequest,
    };
    use serde_json::{json, Value};

    // ── Fixtures and helpers ──────────────────────────────────────────────────

    fn request_from(value: Value) -> MurmurRequest {
        serde_json::from_value(value).expect("fixture must deserialize as a murmur request")
    }

    /// The canonical envelope the host builds, with the reserved `prompt_cache_key` present.
    fn simple_request() -> MurmurRequest {
        request_from(json!({
            "model": "kimi-k3",
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

    // ── Scenario 3 — model scope ──────────────────────────────────────────────

    #[test]
    fn unsupported_model_is_refused_before_any_request_is_built() {
        assert!(validate_model("kimi-k3").is_ok());

        for rejected in ["kimi-k2.6", "gpt-5", "kimi-k2.7-code"] {
            let err = validate_model(rejected)
                .expect_err("only kimi-k3 is implemented by this driver");
            assert!(err.starts_with("driver: "), "got {err}");
            assert!(err.contains(rejected), "the model must be named: {err}");
            assert!(err.contains("kimi-k3"), "the supported set must be listed: {err}");
        }

        // The rejection reaches an operator through the ordinary request path too, with no
        // provider body built.
        let request = request_from(json!({
            "model": "kimi-k2.6",
            "max_tokens": 8192,
            "messages": []
        }));
        assert!(build_provider_request(&request, &DriverConfig::default()).is_err());
    }

    // ── Scenario 4 — `thinking` refused by name ───────────────────────────────

    #[test]
    fn thinking_config_key_is_refused_by_name_with_its_own_reason() {
        let generic = parse_driver_config(Some(r#"{"verbosity":"high"}"#))
            .expect_err("an unrecognised key must be refused");

        for raw in [r#"{"thinking":"enabled"}"#, r#"{"thinking":"disabled"}"#] {
            let err = parse_driver_config(Some(raw))
                .expect_err("kimi-k3 has no thinking switch, so the key cannot be honoured");
            assert_eq!(
                err,
                "driver: inference.driver.config 'thinking' is not accepted by \
                 murmur-driver-moonshotai: kimi-k3 always reasons and Moonshot exposes no switch \
                 to turn it off, at any value. Remove the key."
            );
            assert_ne!(err, generic, "the refusal must not be the generic message");
            assert!(!err.contains("unrecognised"), "got {err}");
        }
    }

    // ── Scenario 5 — `reasoning_effort` vocabulary ────────────────────────────

    #[test]
    fn reasoning_effort_medium_is_refused_naming_the_value_and_the_accepted_set() {
        let err = parse_driver_config(Some(r#"{"reasoning_effort":"medium"}"#))
            .expect_err("medium is OpenAI's vocabulary, not Moonshot's");
        assert_eq!(
            err,
            "driver: inference.driver.config 'reasoning_effort' must be one of low, high, max, \
             got \"medium\""
        );

        assert_eq!(config(r#"{"reasoning_effort":"low"}"#).reasoning_effort, "low");
        assert_eq!(config(r#"{"reasoning_effort":"HIGH"}"#).reasoning_effort, "high");
        assert_eq!(config(r#"{"reasoning_effort":" max "}"#).reasoning_effort, "max");

        let err = parse_driver_config(Some(r#"{"reasoning_effort":3}"#))
            .expect_err("a non-string effort must be refused");
        assert_eq!(
            err,
            "driver: inference.driver.config 'reasoning_effort' must be a string, got 3"
        );
    }

    // ── Scenario 6 — unrecognised keys ────────────────────────────────────────

    #[test]
    fn unrecognised_config_keys_are_reported_sorted_with_the_accepted_set() {
        let err = parse_driver_config(Some(r#"{"verbosity":"high","stroe":true}"#))
            .expect_err("neither key is implemented by this driver");
        assert_eq!(
            err,
            "driver: unrecognised inference.driver.config key(s): stroe, verbosity. \
             Accepted keys: reasoning_effort, response_format"
        );
    }

    #[test]
    fn absent_empty_and_whitespace_configs_all_parse_to_the_default() {
        assert_eq!(parse_driver_config(None).unwrap(), DriverConfig::default());
        assert_eq!(parse_driver_config(Some("{}")).unwrap(), DriverConfig::default());
        assert_eq!(parse_driver_config(Some("   ")).unwrap(), DriverConfig::default());
        assert_eq!(DriverConfig::default().reasoning_effort, "max");
        assert!(DriverConfig::default().response_format.is_none());

        assert!(parse_driver_config(Some("not json")).is_err());
        assert!(parse_driver_config(Some("[]")).is_err());
    }

    // ── Scenario 7 — effort on the body ───────────────────────────────────────

    #[test]
    fn reasoning_effort_reaches_the_body_top_level_in_the_provider_vocabulary() {
        let unconfigured = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(
            unconfigured["reasoning_effort"],
            json!("max"),
            "the provider default is stamped, not omitted"
        );
        assert!(
            unconfigured.get("reasoning").is_none(),
            "the effort dial is top-level on Chat Completions, not nested"
        );

        let low = body_for(&simple_request(), &config(r#"{"reasoning_effort":"low"}"#));
        assert_eq!(low["reasoning_effort"], json!("low"));
    }

    // ── Scenario 8 — fixed parameters ─────────────────────────────────────────

    #[test]
    fn provider_fixed_parameters_are_omitted_from_the_body() {
        let request = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "params": {
                "temperature": 0.2,
                "top_p": 0.5,
                "n": 2,
                "presence_penalty": 1,
                "frequency_penalty": 1,
                "seed": 7
            }
        }));
        let body = body_for(&request, &DriverConfig::default());

        for fixed in [
            "temperature",
            "top_p",
            "n",
            "presence_penalty",
            "frequency_penalty",
        ] {
            assert!(
                !contains_key_anywhere(&body, fixed),
                "'{fixed}' is fixed server-side and must not appear at any nesting level: {body}"
            );
        }
        assert_eq!(body["seed"], json!(7), "an unfixed param still passes through");
    }

    // ── Scenario 9 — the output cap ───────────────────────────────────────────

    #[test]
    fn output_cap_is_written_as_max_completion_tokens_and_bounded_by_the_ceiling() {
        let body = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(body["max_completion_tokens"], json!(8192));
        assert!(
            !contains_key_anywhere(&body, "max_tokens"),
            "Moonshot defines no max_tokens member: {body}"
        );

        let oversized = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 2_000_000u64,
            "messages": []
        }));
        let err = build_provider_request(&oversized, &DriverConfig::default())
            .expect_err("a cap above the ceiling must be refused pre-flight");
        assert_eq!(
            err,
            "driver: inference.max_tokens 2000000 exceeds the Moonshot max_completion_tokens \
             ceiling of 1048576"
        );

        // The ceiling itself is accepted.
        let at_ceiling = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 1_048_576u64,
            "messages": []
        }));
        assert!(build_provider_request(&at_ceiling, &DriverConfig::default()).is_ok());
    }

    // ── Scenario 10 — reasoning and text stay separated ───────────────────────

    const INTERLEAVED_STREAM: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"The sky \"}}]}\n",
        "\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Rayleigh \"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"scatters blue.\"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"scattering.\"},\"finish_reason\":\"stop\"}]}\n",
        "data: [DONE]\n",
    );

    #[test]
    fn streamed_reasoning_and_text_stay_separated_and_ordered() {
        let mut text_seen: Vec<String> = Vec::new();
        let mut thinking_seen: Vec<String> = Vec::new();

        let response = parse_moonshot_sse_body(
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
            ]),
            "thinking first, then text"
        );
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn a_missing_finish_reason_ends_the_turn() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\ndata: [DONE]\n";
        let response =
            parse_moonshot_sse_body(stream, &mut |_: &str| {}, &mut |_: &str| {}).unwrap();
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn a_length_finish_reason_maps_to_max_tokens() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"length\"}]}\n",
            "data: [DONE]\n",
        );
        let response =
            parse_moonshot_sse_body(stream, &mut |_: &str| {}, &mut |_: &str| {}).unwrap();
        assert_eq!(response["stop_reason"], json!("max_tokens"));
    }

    // ── Scenario 11 — the wasm module wires both emitters ─────────────────────

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
            .match_indices("murmur::text::chunks::emit_chunk(")
            .map(|(index, _)| index)
            .collect();
        let thinking_sites: Vec<usize> = module
            .match_indices("murmur::text::chunks::emit_thinking_chunk(")
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
            "MURMUR_INFERENCE_API_KEY",
            "dispatch_request(",
        ] {
            let at = run_inner
                .find(later)
                .unwrap_or_else(|| panic!("run_inner must reference {later}"));
            assert!(parse_at < at, "the driver config must be parsed before {later}");
        }
    }

    #[test]
    fn no_reasoning_sentinel_scanner_anywhere_in_the_crate() {
        // Moonshot delivers reasoning on its own delta field, so an inline reasoning-tag scanner
        // here would be a second, silently-wrong reasoning path. The needle is assembled rather
        // than written out so that this assertion does not itself become the only occurrence.
        let sentinel = concat!("<", "think");
        assert!(!include_str!("lib.rs").contains(sentinel));
    }

    // ── Scenario 12 — tool-call round trip ────────────────────────────────────

    const TOOL_CALL_STREAM: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"I should read \"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"the file.\"}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]}}]}\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
        "data: [DONE]\n",
    );

    #[test]
    fn tool_call_round_trip_preserves_reasoning_across_turns() {
        let response =
            parse_moonshot_sse_body(TOOL_CALL_STREAM, &mut |_: &str| {}, &mut |_: &str| {})
                .expect("the tool-call stream must parse");

        assert_eq!(response["stop_reason"], json!("tool_call"));
        assert_eq!(
            response["content"],
            json!([
                {"type": "thinking", "text": "I should read the file."},
                {
                    "type": "tool_call",
                    "id": "call_abc",
                    "name": "read_file",
                    "input": {"path": "README.md"}
                }
            ]),
            "thinking first, then the call assembled from the argument fragments"
        );

        // The runtime replays that content array verbatim as the next turn's assistant message.
        let follow_up = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read the README."}]},
                {"role": "assistant", "content": response["content"]},
                {
                    "role": "tool",
                    "tool_call_id": "call_abc",
                    "content": [{"type": "text", "text": "# Title"}]
                }
            ]
        }));
        let body = body_for(&follow_up, &DriverConfig::default());
        let messages = body["messages"].as_array().expect("messages must be an array");

        let assistant = &messages[1];
        assert_eq!(assistant["role"], json!("assistant"));
        assert_eq!(
            assistant["reasoning_content"],
            json!("I should read the file."),
            "preserved reasoning is reattached so the provider accepts the turn"
        );
        assert_eq!(assistant["tool_calls"][0]["id"], json!("call_abc"));
        assert_eq!(
            assistant["tool_calls"][0]["function"]["name"],
            json!("read_file")
        );
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            json!("{\"path\":\"README.md\"}")
        );

        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "call_abc", "content": "# Title"})
        );
    }

    // ── Scenario 13 — a compacted history has no reasoning to replay ──────────

    #[test]
    fn post_compaction_assistant_message_without_thinking_is_accepted() {
        let request = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read it."}]},
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_call",
                        "id": "call_abc",
                        "name": "read_file",
                        "input": {"path": "README.md"}
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc",
                    "content": [{"type": "text", "text": "# Title"}]
                }
            ]
        }));

        let body = build_provider_request(&request, &DriverConfig::default())
            .expect("a compacted history is the ordinary state, not an error");
        let assistant = &body["messages"][1];
        assert!(
            assistant.get("reasoning_content").is_none(),
            "the member is omitted entirely, never sent as an empty string: {assistant}"
        );
        assert_eq!(assistant["tool_calls"][0]["id"], json!("call_abc"));
    }

    #[test]
    fn a_plain_assistant_turn_replays_its_reasoning_too() {
        let request = request_from(json!({
            "model": "kimi-k3",
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
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": "Rayleigh scattering.",
                "reasoning_content": "Because of scattering."
            })
        );
    }

    // ── Scenario 14 — structured output ───────────────────────────────────────

    const PLAN_SCHEMA: &str = r#"{"response_format":{"type":"json_schema","json_schema":{"name":"plan","schema":{"type":"object","properties":{"steps":{"type":"array"}}}}}}"#;

    #[test]
    fn json_schema_constrains_final_content_only() {
        let body = body_for(&simple_request(), &config(PLAN_SCHEMA));
        assert_eq!(body["response_format"]["type"], json!("json_schema"));
        assert_eq!(body["response_format"]["json_schema"]["name"], json!("plan"));
        assert_eq!(
            body["response_format"]["json_schema"]["strict"],
            json!(true),
            "strict defaults to true when absent"
        );

        let strict_false = parse_driver_config(Some(
            r#"{"response_format":{"type":"json_schema","json_schema":{"name":"p","schema":{},"strict":false}}}"#,
        ))
        .expect_err("strict: false is refused rather than silently upgraded");
        assert_eq!(
            strict_false,
            "driver: inference.driver.config 'response_format.json_schema.strict' must be true \
             — strict: false is refused rather than silently upgraded, because a schema the \
             provider does not enforce is not the guarantee this key offers"
        );

        let wrong_type =
            parse_driver_config(Some(r#"{"response_format":{"type":"text"}}"#))
                .expect_err("only json_schema is offered by this key");
        assert_eq!(
            wrong_type,
            "driver: inference.driver.config 'response_format.type' must be \"json_schema\", \
             got \"text\""
        );

        let missing_schema = parse_driver_config(Some(
            r#"{"response_format":{"type":"json_schema","json_schema":{"name":"p"}}}"#,
        ))
        .expect_err("json_schema.schema is required");
        assert_eq!(
            missing_schema,
            "driver: inference.driver.config 'response_format.json_schema.schema' is required \
             and must be an object"
        );

        let not_an_object = parse_driver_config(Some(r#"{"response_format":"json_schema"}"#))
            .expect_err("the value must be an object");
        assert_eq!(
            not_an_object,
            "driver: inference.driver.config 'response_format' must be a JSON object, got \
             \"json_schema\""
        );
    }

    #[test]
    fn reasoning_content_is_never_parsed_as_the_structured_payload() {
        let payload = "{\"steps\":[\"read\",\"write\"]}";
        let reasoning = "{\"draft\": \"this looks like JSON but is reasoning\"}";
        let provider_response = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"reasoning_content": reasoning, "content": payload}
            }]
        });

        let response = translate_moonshot_response_to_murmur(&provider_response).unwrap();
        assert_eq!(
            response["content"],
            json!([
                {"type": "thinking", "text": reasoning},
                {"type": "text", "text": payload}
            ]),
            "the reasoning stays a thinking block; the payload lands byte-identical in text"
        );
    }

    #[test]
    fn an_empty_tool_calls_array_still_yields_the_answer_text() {
        // An OpenAI-compatible server may report `tool_calls: []` on a plain answer turn. The
        // buffered path must read that as "no tool call" rather than as a tool-call turn with
        // nothing in it, or the answer is dropped and the turn comes back empty.
        let buffered = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": "Rayleigh scattering.", "tool_calls": []}
            }]
        });
        let response = translate_moonshot_response_to_murmur(&buffered).unwrap();
        assert_eq!(
            response["content"],
            json!([{"type": "text", "text": "Rayleigh scattering."}])
        );
        assert_eq!(response["stop_reason"], json!("end_turn"));
    }

    // ── Scenario 15 — token usage ─────────────────────────────────────────────

    #[test]
    fn usage_round_trips_on_both_paths() {
        // Non-streaming.
        let buffered = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "hi"}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 40}
            }
        });
        let response = translate_moonshot_response_to_murmur(&buffered).unwrap();
        assert_eq!(
            response["usage"],
            json!({"input_tokens": 100, "output_tokens": 20, "cached_tokens": 40})
        );
        assert!(
            !contains_key_anywhere(&response, "cache_write_tokens"),
            "Moonshot reports no cache-write count, so the member is always absent"
        );

        // Streaming — the usage-bearing final chunk has an empty `choices` array.
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n",
            "data: [DONE]\n",
        );
        let streamed =
            parse_moonshot_sse_body(stream, &mut |_: &str| {}, &mut |_: &str| {}).unwrap();
        assert_eq!(
            streamed["usage"],
            json!({"input_tokens": 7, "output_tokens": 3, "cached_tokens": 0}),
            "a reported 0 is kept as 0"
        );

        // An unreported member is omitted rather than sent as 0.
        let partial = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 5}
        });
        assert_eq!(
            translate_moonshot_response_to_murmur(&partial).unwrap()["usage"],
            json!({"input_tokens": 5})
        );

        // No member survives — no `usage` key at all.
        let empty = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "hi"}}],
            "usage": {}
        });
        let response = translate_moonshot_response_to_murmur(&empty).unwrap();
        assert!(response.get("usage").is_none(), "got {response}");
    }

    #[test]
    fn streaming_flags_are_stamped_together_on_every_request() {
        let body = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));

        // `params` cannot turn streaming off.
        let request = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [],
            "params": {"stream": false}
        }));
        assert_eq!(body_for(&request, &DriverConfig::default())["stream"], json!(true));
    }

    // ── Scenario 16 — failure paths ───────────────────────────────────────────

    #[test]
    fn failure_paths_produce_an_error_payload_not_a_panic() {
        // An unmapped finish_reason, on both paths.
        let filtered = json!({
            "choices": [{"finish_reason": "content_filter", "message": {"content": ""}}]
        });
        let err = translate_moonshot_response_to_murmur(&filtered)
            .expect_err("an unmapped stop reason must not be guessed at");
        assert_eq!(
            err,
            "driver: unsupported Moonshot finish_reason 'content_filter'"
        );

        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"content_filter\"}]}\n",
            "data: [DONE]\n",
        );
        assert!(parse_moonshot_sse_body(stream, &mut |_: &str| {}, &mut |_: &str| {}).is_err());

        // A tool message without its correlation id.
        let orphan_tool = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [{"role": "tool", "content": [{"type": "text", "text": "out"}]}]
        }));
        let err = translate_murmur_request_to_moonshot(&orphan_tool, &DriverConfig::default())
            .expect_err("a tool result with no call to attach to must be refused");
        assert_eq!(
            err,
            "driver: tool message is missing required field 'tool_call_id'"
        );

        // An unsupported role.
        let bad_role = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [{"role": "developer", "content": []}]
        }));
        let err = translate_murmur_request_to_moonshot(&bad_role, &DriverConfig::default())
            .expect_err("an unknown role must be named, not dropped");
        assert_eq!(err, "driver: unsupported message role 'developer'");

        // Every message opens with the driver prefix.
        for message in [
            translate_moonshot_response_to_murmur(&filtered).unwrap_err(),
            translate_murmur_request_to_moonshot(&orphan_tool, &DriverConfig::default())
                .unwrap_err(),
            translate_murmur_request_to_moonshot(&bad_role, &DriverConfig::default()).unwrap_err(),
        ] {
            assert!(message.starts_with("driver: "), "got {message}");
        }

        // A provider status >= 400.
        assert_eq!(
            http_error_payload(429, "{\"error\":\"rate limit exceeded\"}"),
            json!({
                "stop_reason": "error",
                "error": "HTTP 429: {\"error\":\"rate limit exceeded\"}"
            })
        );
    }

    // ── The reserved prompt-cache hint is dropped ─────────────────────────────

    #[test]
    fn prompt_cache_key_never_reaches_the_provider_body() {
        let request = simple_request();
        let body = body_for(&request, &DriverConfig::default());
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            !serialized.contains("capsule:demo:0.1.0"),
            "the routing hint must not reach the provider at any nesting level: {serialized}"
        );
        assert!(!contains_key_anywhere(&body, "prompt_cache_key"));
    }

    // ── Request shaping ───────────────────────────────────────────────────────

    #[test]
    fn system_prompt_and_tools_reach_the_chat_completions_shape() {
        let request = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "system": "  Be brief.  ",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{
                "name": "read_file",
                "description": " Read a file. ",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }]
        }));
        let body = body_for(&request, &DriverConfig::default());

        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "Be brief."})
        );
        assert_eq!(body["messages"][1], json!({"role": "user", "content": "hi"}));
        assert_eq!(body["tools"][0]["type"], json!("function"));
        assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
        assert_eq!(
            body["tools"][0]["function"]["description"],
            json!("Read a file.")
        );

        // No tools declared means no `tools` member at all.
        let bare = body_for(&simple_request(), &DriverConfig::default());
        assert!(bare.get("tools").is_none());
    }

    #[test]
    fn an_image_block_becomes_an_openai_shaped_content_part() {
        let request = request_from(json!({
            "model": "kimi-k3",
            "max_tokens": 8192,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is this?"},
                    {"type": "image", "source": {"media_type": "image/png", "data": "QUJD"}}
                ]
            }]
        }));
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
            ])
        );
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
        // to disagree with it again. `run_inner` is wasm-only, so this is asserted from the
        // source; the needle is assembled so that this test is not its own evidence.
        let source = include_str!("lib.rs");
        let run_inner = &source[source
            .find("fn run_inner(")
            .expect("run_inner must exist")..];
        assert!(
            run_inner.contains(concat!("require_", "endpoint(")),
            "run_inner must resolve the endpoint through murmur_driver_env::require_endpoint"
        );
    }

    #[test]
    fn manifest_declares_the_inference_auth_scheme_the_gateway_will_read() {
        // The runtime builds the auth header from these two fields, so a dropped quote pair
        // (`value: {key}` is a YAML flow mapping, not a string) or a renamed key leaves the
        // gateway with no scheme to apply and every request unauthenticated.
        const BLOCK: &str = "inference_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "drivers/murmur-driver-moonshotai/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }
}
