// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use serde::Deserialize;
use serde_json::{json, Map, Value};

// ── Model table ───────────────────────────────────────────────────────────────

/// The limits of one model, from Moonshot's documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelLimits {
    /// The largest `max_completion_tokens` this driver sends for the model. A larger value is
    /// refused here rather than clamped silently or spent on a provider 400.
    max_output_tokens: u64,
    /// The model's context window in tokens. No request path reads it: it is the number
    /// `murmur.yaml` declares under `models:`, and a test keeps the two in step.
    #[cfg_attr(not(test), allow(dead_code))]
    context_window: u64,
}

/// How Moonshot lets a request steer one model's reasoning, as its model parameter reference
/// documents it. Each kind names the request fields the model takes, so the body and the
/// config checks follow from the kind rather than from the model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingControl {
    /// Always reasons. Takes top-level `reasoning_effort` and has no `thinking` parameter.
    ReasoningEffort,
    /// Thinking and Preserved Thinking are always on. Takes neither `reasoning_effort` nor a
    /// `thinking` switch; an omitted `thinking` is read as `{"type":"enabled","keep":"all"}`.
    AlwaysOn,
    /// Takes `thinking` enabled or disabled, and no `reasoning_effort`.
    Switchable,
}

/// Every model this driver serves, with its limits and its thinking control. This table is the
/// one place the crate branches on a model id: everything else reads the row `validate_model`
/// returns. `murmur.yaml`'s `models:` block must declare the same numbers for each entry, and
/// README.md's "Model limits" table the same ids.
const SUPPORTED_MODELS: &[(&str, ModelLimits, ThinkingControl)] = &[
    ("kimi-k3.1-pro", K3_LIMITS, ThinkingControl::ReasoningEffort),
    (
        "kimi-k3.1-pro-highspeed",
        K3_LIMITS,
        ThinkingControl::ReasoningEffort,
    ),
    ("kimi-k3", K3_LIMITS, ThinkingControl::ReasoningEffort),
    ("kimi-k2.7-code", K2_LIMITS, ThinkingControl::AlwaysOn),
    (
        "kimi-k2.7-code-highspeed",
        K2_LIMITS,
        ThinkingControl::AlwaysOn,
    ),
    ("kimi-k2.6", K2_LIMITS, ThinkingControl::Switchable),
];

/// Moonshot documents `max_completion_tokens` "can be set up to 1048576" on the K3 models, and
/// a 1,048,576-token window for them.
const K3_LIMITS: ModelLimits = ModelLimits {
    max_output_tokens: 1_048_576,
    context_window: 1_048_576,
};

/// Moonshot documents a 262,144-token window for the K2 models and no `max_completion_tokens`
/// ceiling. A cap that does not fit beside the prompt is lowered by the API to fit, so the
/// window is the largest cap a request can carry.
const K2_LIMITS: ModelLimits = ModelLimits {
    max_output_tokens: 262_144,
    context_window: 262_144,
};

/// Look the model up by its exact id before a request body is built, so an unsupported model
/// costs no HTTP call.
fn validate_model(model: &str) -> Result<(&'static ModelLimits, ThinkingControl), String> {
    SUPPORTED_MODELS
        .iter()
        .find(|(id, _, _)| *id == model)
        .map(|(_, limits, control)| (limits, *control))
        .ok_or_else(|| {
            format!(
                "driver: model '{model}' is not supported by driver-moonshotai. \
                 Supported models: {}.",
                SUPPORTED_MODELS
                    .iter()
                    .map(|(id, _, _)| *id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Refuse an output cap above the model's ceiling rather than clamping it silently.
fn validate_max_tokens(limits: &ModelLimits, max_tokens: u64) -> Result<(), String> {
    if max_tokens > limits.max_output_tokens {
        return Err(format!(
            "driver: inference.max_tokens {max_tokens} exceeds the Moonshot \
             max_completion_tokens ceiling of {}",
            limits.max_output_tokens
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
const ACCEPTED_CONFIG_KEYS: &[&str] = &["reasoning_effort", "response_format", "thinking"];

/// Moonshot's own effort vocabulary, in ascending order of spend. `medium` is OpenAI's spelling
/// and is not accepted here: each driver owns its provider's vocabulary rather than a
/// murmur-wide normalisation across providers' effort dials.
const REASONING_EFFORT_VALUES: &[&str] = &["low", "high", "max"];

/// Moonshot's own default for the models that take `reasoning_effort`. Kept rather than
/// substituted with a cheaper tier, and stamped on every request to such a model so the
/// effective value is visible in a recorded request rather than left to drift with the
/// provider's default.
const DEFAULT_REASONING_EFFORT: &str = "max";

/// `strict: false` is refused rather than silently upgraded to `true`.
const STRICT_FALSE_REFUSAL: &str =
    "driver: inference.driver.config 'response_format.json_schema.strict' must be true — \
     strict: false is refused rather than silently upgraded, because a schema the provider does \
     not enforce is not the guarantee this key offers";

/// The dials read from `inference.driver.config`. Parsing knows nothing of the model: whether a
/// dial applies is checked against the model's [`ThinkingControl`] in `preflight_request`.
#[derive(Debug, Clone, Default, PartialEq)]
struct DriverConfig {
    /// `reasoning_effort` — one of [`REASONING_EFFORT_VALUES`], lowercased. `None` when the key
    /// is absent; a model that takes the dial then gets [`DEFAULT_REASONING_EFFORT`].
    reasoning_effort: Option<String>,
    /// `thinking` — `Some(true)` for `"enabled"`, `Some(false)` for `"disabled"`, `None` when
    /// the key is absent. A switchable model treats `None` as enabled.
    thinking: Option<bool>,
    /// `response_format` — validated and normalised (`strict` defaulted to `true`), then
    /// forwarded to the provider body under the same name.
    response_format: Option<Value>,
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

    Ok(DriverConfig {
        reasoning_effort: config
            .get("reasoning_effort")
            .map(parse_reasoning_effort)
            .transpose()?,
        thinking: config.get("thinking").map(parse_thinking).transpose()?,
        response_format: config
            .get("response_format")
            .map(validate_response_format)
            .transpose()?,
    })
}

/// Trimmed and matched case-insensitively; sent lowercased. The rejected value appears verbatim
/// in the error alongside the accepted set, so a manifest holding OpenAI's `medium` says so.
fn parse_reasoning_effort(value: &Value) -> Result<String, String> {
    match value {
        Value::String(raw) => {
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
        other => Err(format!(
            "driver: inference.driver.config 'reasoning_effort' must be a string, got {other}"
        )),
    }
}

/// `"enabled"` or `"disabled"`, trimmed and matched case-insensitively — the spelling
/// driver-openai, driver-anthropic and driver-deepseek read the same key with.
fn parse_thinking(value: &Value) -> Result<bool, String> {
    match value {
        Value::String(raw) if raw.trim().eq_ignore_ascii_case("enabled") => Ok(true),
        Value::String(raw) if raw.trim().eq_ignore_ascii_case("disabled") => Ok(false),
        other => Err(format!(
            "driver: inference.driver.config 'thinking' must be \"enabled\" or \"disabled\", got {other}"
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
    Image {
        source: MurmurImageSource,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
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

/// Every check that can be made without building a body or touching the network: the model,
/// then the output cap, then each configured dial against the model's thinking control. A dial
/// the model cannot honour is refused at any value. Returns the model's thinking control, which
/// decides the reasoning fields on the body.
fn preflight_request(
    request: &MurmurRequest,
    config: &DriverConfig,
) -> Result<ThinkingControl, String> {
    let model = request.model.as_str();
    let (limits, control) = validate_model(model)?;
    validate_max_tokens(limits, request.max_tokens)?;

    if config.reasoning_effort.is_some() && control != ThinkingControl::ReasoningEffort {
        return Err(format!(
            "driver: inference.driver.config 'reasoning_effort' is not accepted for model \
             '{model}': Moonshot documents reasoning_effort as not supported on it. Remove the key."
        ));
    }
    if config.thinking.is_some() {
        match control {
            ThinkingControl::ReasoningEffort => {
                return Err(format!(
                    "driver: inference.driver.config 'thinking' is not accepted for model \
                     '{model}': the K3 models always reason and Moonshot exposes no switch to \
                     turn it off, at any value. Remove the key."
                ))
            }
            ThinkingControl::AlwaysOn => {
                return Err(format!(
                    "driver: inference.driver.config 'thinking' is not accepted for model \
                     '{model}': its thinking is always on and cannot be switched, at any value. \
                     Remove the key."
                ))
            }
            ThinkingControl::Switchable => {}
        }
    }
    Ok(control)
}

/// The body `run_inner` puts on the wire: pre-flight, translate, stamp the streaming flags.
/// Tests assert on this rather than on a bare translation.
fn build_provider_request(request: &MurmurRequest, config: &DriverConfig) -> Result<Value, String> {
    let control = preflight_request(request, config)?;
    let mut body = translate_murmur_request_to_moonshot(request, config, control)?;
    stamp_streaming_flags(&mut body);
    Ok(body)
}

fn translate_murmur_request_to_moonshot(
    request: &MurmurRequest,
    config: &DriverConfig,
    control: ThinkingControl,
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

    match control {
        // Top-level, not nested under a `reasoning` object, and stamped on every request
        // including the unconfigured one.
        ThinkingControl::ReasoningEffort => {
            let effort = config
                .reasoning_effort
                .as_deref()
                .unwrap_or(DEFAULT_REASONING_EFFORT);
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort.to_string()),
            );
        }
        // Moonshot reads an omitted `thinking` as enabled with `keep: "all"`.
        ThinkingControl::AlwaysOn => {}
        // Enabled asks for `keep: "all"`, so the `reasoning_content` this driver replays on
        // every assistant turn is kept, as it is on the other two kinds.
        ThinkingControl::Switchable => {
            let thinking = if config.thinking.unwrap_or(true) {
                json!({"type": "enabled", "keep": "all"})
            } else {
                json!({"type": "disabled"})
            };
            body.insert("thinking".to_string(), thinking);
        }
    }

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
///
/// Moonshot reports the request total with the cache reads and writes inside it, while the
/// usage contract puts `cached_tokens` and `cache_write_tokens` beside `input_tokens`. So the
/// raw total is held here as `request_total`, and the contract's `input_tokens` is derived from
/// it only in `to_value`, after every streamed chunk has been merged — a total and a cache
/// count arriving in different chunks still subtract correctly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTokens {
    request_total: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

impl UsageTokens {
    /// Take every member `later` reports, keeping the current value for the rest. Streaming
    /// usage arrives across more than one chunk, each carrying only part of the picture.
    fn merge_from(&mut self, later: UsageTokens) {
        if later.request_total.is_some() {
            self.request_total = later.request_total;
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

    /// The contract `usage` object. `input_tokens` is the request tokens neither read from nor
    /// written to the cache — `request_total` minus both cache counts, an unreported cache
    /// member counting as `0` — so `input_tokens + cached_tokens + cache_write_tokens` is the
    /// provider's total. It is omitted when the total is unreported or the cache counts exceed
    /// it: such a report is inconsistent, and no value is invented for it. The other members
    /// are passed through as reported. `None` when no member survived, in which case the
    /// response carries no `usage` key at all.
    fn to_value(self) -> Option<Value> {
        let input_tokens = self
            .request_total
            .and_then(|total| total.checked_sub(self.cached_tokens.unwrap_or(0)))
            .and_then(|rest| rest.checked_sub(self.cache_write_tokens.unwrap_or(0)));
        let mut obj = Map::new();
        for (key, count) in [
            ("input_tokens", input_tokens),
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

/// Read a Moonshot `usage` object: the request total from `prompt_tokens`, the cache read and
/// write counts that are parts of it from `prompt_tokens_details`, the completion count from
/// `completion_tokens`. The top-level `usage.cached_tokens` some responses also carry is not
/// read — `prompt_tokens_details` is the documented home of the cache counts. A member that is
/// absent, null, or not a non-negative integer is dropped; its siblings are kept.
fn extract_moonshot_usage(usage: &Value) -> UsageTokens {
    UsageTokens {
        request_total: usage.get("prompt_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
        cached_tokens: usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        cache_write_tokens: usage
            .pointer("/prompt_tokens_details/cache_write_tokens")
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

/// Map a Moonshot `finish_reason` to a murmur `stop_reason`. Each caller refuses a missing reason
/// with its own path's message; an unmapped one is an error naming it rather than a guess.
fn map_finish_reason(finish_reason: &str) -> Result<&'static str, String> {
    match finish_reason {
        "stop" => Ok("end_turn"),
        "tool_calls" => Ok("tool_call"),
        "length" => Ok("max_tokens"),
        other => Err(format!(
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

    // Absent, `null`, non-string or empty: the turn did not finish.
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| "driver: Moonshot response has no finish_reason".to_string())?;
    let stop_reason = map_finish_reason(finish_reason)?;

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
/// and `Err` for a tool call index past [`murmur_driver_stream::MAX_SLOTS_PER_TURN`].
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
    signal: &mut impl FnMut(ToolCallSignal<'_>),
) -> Result<bool, String> {
    if line == "data: [DONE]" {
        return Ok(true);
    }
    let Some(json_str) = line.strip_prefix("data: ") else {
        return Ok(false);
    };
    let Ok(data) = serde_json::from_str::<Value>(json_str) else {
        return Ok(false);
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
                .map_err(|err| format!("driver: tool call {err}"))?;
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

/// Turn the accumulated stream state into the murmur response. `content` is `thinking` first,
/// then either the `tool_call` blocks or the `text` block.
fn assemble_moonshot_streaming_response(
    text_acc: &str,
    reasoning_acc: &str,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
    usage: UsageTokens,
) -> Result<Value, String> {
    // No chunk carried a finish_reason, so the stream ended before the turn did.
    let finish_reason = stop_reason
        .as_deref()
        .ok_or_else(|| "driver: Moonshot stream ended with no finish_reason".to_string())?;
    let stop_reason_str = map_finish_reason(finish_reason)?;

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
    parse_moonshot_sse_body_with_signals(body, emit_text, emit_thinking).0
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
fn parse_moonshot_sse_body_with_signals<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> (Result<Value, String>, Vec<RecordedSignal>) {
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
            process_moonshot_sse_line(
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

    let result = assemble_moonshot_streaming_response(
        &text_acc,
        &reasoning_acc,
        tool_states,
        stop_reason,
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
        translate_moonshot_response_to_murmur, MurmurRequest, ToolCallSignal, ToolCallState,
        UsageTokens,
    };
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
             -> Result<bool, String> {
                let mut et = |t: &str| {
                    murmur::stream::events::emit_chunk(t);
                    text_acc.push_str(t);
                };
                let mut eth = |t: &str| {
                    murmur::stream::events::emit_thinking_chunk(t);
                    reasoning_acc.push_str(t);
                };
                process_moonshot_sse_line(
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
        validate_model, DriverConfig, MurmurRequest, ThinkingControl, SUPPORTED_MODELS,
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

    /// A one-turn request for `model` with the given output cap.
    fn request_for(model: &str, max_tokens: u64) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }))
    }

    /// Every supported id of one thinking-control kind.
    fn models_of(control: ThinkingControl) -> Vec<&'static str> {
        SUPPORTED_MODELS
            .iter()
            .filter(|(_, _, kind)| *kind == control)
            .map(|(id, _, _)| *id)
            .collect()
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
    fn every_supported_model_builds_a_body_naming_it() {
        let ids: Vec<&str> = SUPPORTED_MODELS.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "kimi-k3.1-pro",
                "kimi-k3.1-pro-highspeed",
                "kimi-k3",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k2.6"
            ]
        );
        for id in ids {
            let body = body_for(&request_for(id, 8192), &DriverConfig::default());
            assert_eq!(body["model"], json!(id));
        }
    }

    #[test]
    fn unsupported_model_is_refused_before_any_request_is_built() {
        for rejected in [
            "kimi-k2.5",
            "moonshot-v1-8k",
            "kimi-k2-thinking",
            "gpt-5",
            "",
        ] {
            let err = validate_model(rejected).expect_err("the id is outside SUPPORTED_MODELS");
            assert_eq!(
                err,
                format!(
                    "driver: model '{rejected}' is not supported by driver-moonshotai. \
                     Supported models: kimi-k3.1-pro, kimi-k3.1-pro-highspeed, kimi-k3, \
                     kimi-k2.7-code, kimi-k2.7-code-highspeed, kimi-k2.6."
                )
            );
        }

        // The rejection reaches an operator through the ordinary request path too, with no
        // provider body built.
        let err = build_provider_request(&request_for("kimi-k2.5", 8192), &DriverConfig::default())
            .expect_err("kimi-k2.5 is deprecated by Moonshot and refused");
        assert!(err.contains("'kimi-k2.5'"), "got {err}");
    }

    // ── Scenario 4 — the reasoning fields follow the model's thinking control ─

    #[test]
    fn a_k3_model_gets_reasoning_effort_and_no_thinking() {
        for id in models_of(ThinkingControl::ReasoningEffort) {
            let body = body_for(&request_for(id, 8192), &DriverConfig::default());
            assert_eq!(body["reasoning_effort"], json!("max"), "{id}: {body}");
            assert!(body.get("thinking").is_none(), "{id}: {body}");

            let low = body_for(
                &request_for(id, 8192),
                &config(r#"{"reasoning_effort":"low"}"#),
            );
            assert_eq!(low["reasoning_effort"], json!("low"), "{id}: {low}");
        }
    }

    #[test]
    fn a_k2_7_code_model_gets_neither_reasoning_field() {
        let ids = models_of(ThinkingControl::AlwaysOn);
        assert_eq!(ids, ["kimi-k2.7-code", "kimi-k2.7-code-highspeed"]);
        for id in ids {
            let body = body_for(&request_for(id, 8192), &DriverConfig::default());
            assert!(body.get("reasoning_effort").is_none(), "{id}: {body}");
            assert!(body.get("thinking").is_none(), "{id}: {body}");
        }
    }

    #[test]
    fn kimi_k2_6_gets_a_thinking_switch_that_keeps_reasoning_when_enabled() {
        assert_eq!(models_of(ThinkingControl::Switchable), ["kimi-k2.6"]);

        for raw in [
            "{}",
            r#"{"thinking":"enabled"}"#,
            r#"{"thinking":" Enabled "}"#,
        ] {
            let body = body_for(&request_for("kimi-k2.6", 8192), &config(raw));
            assert_eq!(
                body["thinking"],
                json!({"type": "enabled", "keep": "all"}),
                "{raw}: {body}"
            );
            assert!(body.get("reasoning_effort").is_none(), "{raw}: {body}");
        }

        let disabled = body_for(
            &request_for("kimi-k2.6", 8192),
            &config(r#"{"thinking":"disabled"}"#),
        );
        assert_eq!(disabled["thinking"], json!({"type": "disabled"}));
        assert!(disabled.get("reasoning_effort").is_none(), "{disabled}");
    }

    #[test]
    fn reasoning_effort_on_a_k2_model_is_refused_naming_the_key_and_the_model() {
        for id in ["kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"] {
            let err = build_provider_request(
                &request_for(id, 8192),
                &config(r#"{"reasoning_effort":"high"}"#),
            )
            .expect_err("Moonshot documents reasoning_effort as not supported on K2");
            assert_eq!(
                err,
                format!(
                    "driver: inference.driver.config 'reasoning_effort' is not accepted for \
                     model '{id}': Moonshot documents reasoning_effort as not supported on it. \
                     Remove the key."
                )
            );
        }
    }

    #[test]
    fn thinking_on_a_k3_model_is_refused_at_any_value_with_the_k3_reason() {
        for id in models_of(ThinkingControl::ReasoningEffort) {
            for raw in [r#"{"thinking":"enabled"}"#, r#"{"thinking":"disabled"}"#] {
                let err = build_provider_request(&request_for(id, 8192), &config(raw))
                    .expect_err("the K3 models have no thinking switch");
                assert_eq!(
                    err,
                    format!(
                        "driver: inference.driver.config 'thinking' is not accepted for model \
                         '{id}': the K3 models always reason and Moonshot exposes no switch to \
                         turn it off, at any value. Remove the key."
                    )
                );
            }
        }
    }

    #[test]
    fn thinking_on_a_k2_7_code_model_is_refused_at_any_value() {
        for id in models_of(ThinkingControl::AlwaysOn) {
            for raw in [r#"{"thinking":"enabled"}"#, r#"{"thinking":"disabled"}"#] {
                let err = build_provider_request(&request_for(id, 8192), &config(raw))
                    .expect_err("K2.7 Code's thinking cannot be switched");
                assert_eq!(
                    err,
                    format!(
                        "driver: inference.driver.config 'thinking' is not accepted for model \
                         '{id}': its thinking is always on and cannot be switched, at any \
                         value. Remove the key."
                    )
                );
            }
        }
    }

    #[test]
    fn an_unknown_thinking_value_is_refused_naming_the_value_and_the_accepted_pair() {
        for (raw, shown) in [
            (r#"{"thinking":"auto"}"#, r#""auto""#),
            (r#"{"thinking":true}"#, "true"),
        ] {
            let err = parse_driver_config(Some(raw)).expect_err("only enabled or disabled");
            assert_eq!(
                err,
                format!(
                    "driver: inference.driver.config 'thinking' must be \"enabled\" or \
                     \"disabled\", got {shown}"
                )
            );
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

        assert_eq!(
            config(r#"{"reasoning_effort":"low"}"#).reasoning_effort,
            Some("low".to_string())
        );
        assert_eq!(
            config(r#"{"reasoning_effort":"HIGH"}"#).reasoning_effort,
            Some("high".to_string())
        );
        assert_eq!(
            config(r#"{"reasoning_effort":" max "}"#).reasoning_effort,
            Some("max".to_string())
        );

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
             Accepted keys: reasoning_effort, response_format, thinking"
        );
    }

    #[test]
    fn absent_empty_and_whitespace_configs_all_parse_to_the_default() {
        assert_eq!(parse_driver_config(None).unwrap(), DriverConfig::default());
        assert_eq!(
            parse_driver_config(Some("{}")).unwrap(),
            DriverConfig::default()
        );
        assert_eq!(
            parse_driver_config(Some("   ")).unwrap(),
            DriverConfig::default()
        );
        assert!(DriverConfig::default().reasoning_effort.is_none());
        assert!(DriverConfig::default().thinking.is_none());
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
        assert_eq!(
            body["seed"],
            json!(7),
            "an unfixed param still passes through"
        );
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

        for (id, ceiling) in [("kimi-k3.1-pro", 1_048_576u64), ("kimi-k2.6", 262_144)] {
            let err =
                build_provider_request(&request_for(id, ceiling + 1), &DriverConfig::default())
                    .expect_err("a cap above the ceiling must be refused pre-flight");
            assert_eq!(
                err,
                format!(
                    "driver: inference.max_tokens {} exceeds the Moonshot \
                     max_completion_tokens ceiling of {ceiling}",
                    ceiling + 1
                )
            );

            // The ceiling itself is accepted.
            let at_ceiling = body_for(&request_for(id, ceiling), &DriverConfig::default());
            assert_eq!(at_ceiling["max_completion_tokens"], json!(ceiling));
        }
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
    fn a_missing_finish_reason_is_refused() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\ndata: [DONE]\n";
        let err =
            parse_moonshot_sse_body(stream, &mut |_: &str| {}, &mut |_: &str| {}).unwrap_err();
        assert_eq!(err, "driver: Moonshot stream ended with no finish_reason");
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
        for later in ["INFERENCE_ENDPOINT_VAR", "dispatch_request("] {
            let at = run_inner
                .find(later)
                .unwrap_or_else(|| panic!("run_inner must reference {later}"));
            assert!(
                parse_at < at,
                "the driver config must be parsed before {later}"
            );
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
        let messages = body["messages"]
            .as_array()
            .expect("messages must be an array");

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
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            json!("plan")
        );
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

        let wrong_type = parse_driver_config(Some(r#"{"response_format":{"type":"text"}}"#))
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
            json!({"input_tokens": 60, "output_tokens": 20, "cached_tokens": 40})
        );
        assert!(
            !contains_key_anywhere(&response, "cache_write_tokens"),
            "an unreported cache-write count is omitted, not sent as 0"
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

    /// `input_tokens + cached_tokens + cache_write_tokens`, an absent member counted as `0` —
    /// the request count the runtime derives from a `usage` object.
    fn reported_request(usage: &Value) -> u64 {
        ["input_tokens", "cached_tokens", "cache_write_tokens"]
            .iter()
            .map(|key| usage.get(*key).and_then(Value::as_u64).unwrap_or(0))
            .sum()
    }

    fn buffered_with_usage(usage: Value) -> Value {
        json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "hi"}}],
            "usage": usage
        })
    }

    fn streamed_with_usage(usage: Value) -> Value {
        let chunk = json!({"choices": [], "usage": usage});
        let stream = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hi\"}},\"finish_reason\":\"stop\"}}]}}\n\
             data: {chunk}\n\
             data: [DONE]\n"
        );
        parse_moonshot_sse_body(&stream, &mut |_: &str| {}, &mut |_: &str| {}).unwrap()
    }

    #[test]
    fn usage_members_sum_to_the_provider_request_total() {
        // Moonshot's cached, cache-write and uncached counts are disjoint parts of
        // `prompt_tokens`: 10 + 4 + 5 = 19.
        let usage = json!({
            "prompt_tokens": 19,
            "completion_tokens": 3,
            "cached_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 10, "cache_write_tokens": 4}
        });
        let expected = json!({
            "input_tokens": 5,
            "output_tokens": 3,
            "cached_tokens": 10,
            "cache_write_tokens": 4
        });

        let response =
            translate_moonshot_response_to_murmur(&buffered_with_usage(usage.clone())).unwrap();
        assert_eq!(response["usage"], expected);
        assert_eq!(reported_request(&response["usage"]), 19);

        let streamed = streamed_with_usage(usage);
        assert_eq!(streamed["usage"], expected);
        assert_eq!(reported_request(&streamed["usage"]), 19);

        // The total and the cache details split across two chunks still subtract correctly.
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":19}}\n",
            "data: {\"choices\":[],\"usage\":{\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":10,\"cache_write_tokens\":4}}}\n",
            "data: [DONE]\n",
        );
        let split = parse_moonshot_sse_body(stream, &mut |_: &str| {}, &mut |_: &str| {}).unwrap();
        assert_eq!(split["usage"], expected);
    }

    #[test]
    fn the_top_level_cached_tokens_duplicate_is_never_read() {
        let usage = json!({
            "prompt_tokens": 19,
            "completion_tokens": 3,
            "cached_tokens": 17,
            "prompt_tokens_details": {"cached_tokens": 10, "cache_write_tokens": 4}
        });
        let expected = json!({
            "input_tokens": 5,
            "output_tokens": 3,
            "cached_tokens": 10,
            "cache_write_tokens": 4
        });

        let response =
            translate_moonshot_response_to_murmur(&buffered_with_usage(usage.clone())).unwrap();
        assert_eq!(response["usage"], expected);
        assert_eq!(streamed_with_usage(usage)["usage"], expected);

        // With no details at all the duplicate still supplies nothing.
        let response = translate_moonshot_response_to_murmur(&buffered_with_usage(
            json!({"prompt_tokens": 19, "cached_tokens": 10}),
        ))
        .unwrap();
        assert_eq!(response["usage"], json!({"input_tokens": 19}));
    }

    #[test]
    fn partial_or_inconsistent_usage_never_reports_a_negative_or_invented_input_count() {
        for (case, usage, expected) in [
            (
                "cache counts exceed the total",
                json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 40,
                    "prompt_tokens_details": {"cached_tokens": 90, "cache_write_tokens": 20}
                }),
                json!({"output_tokens": 40, "cached_tokens": 90, "cache_write_tokens": 20}),
            ),
            (
                "total with no cache details",
                json!({"prompt_tokens": 100, "completion_tokens": 40}),
                json!({"input_tokens": 100, "output_tokens": 40}),
            ),
            (
                "cache details with no total",
                json!({
                    "completion_tokens": 40,
                    "prompt_tokens_details": {"cached_tokens": 90, "cache_write_tokens": 20}
                }),
                json!({"output_tokens": 40, "cached_tokens": 90, "cache_write_tokens": 20}),
            ),
            (
                "malformed cache members beside a valid total",
                json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 40,
                    "prompt_tokens_details": {"cached_tokens": "90", "cache_write_tokens": -20}
                }),
                json!({"input_tokens": 100, "output_tokens": 40}),
            ),
            (
                "fractional cache member beside a valid total",
                json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 40,
                    "prompt_tokens_details": {"cached_tokens": 1.5, "cache_write_tokens": 20}
                }),
                json!({"input_tokens": 80, "output_tokens": 40, "cache_write_tokens": 20}),
            ),
        ] {
            let response =
                translate_moonshot_response_to_murmur(&buffered_with_usage(usage.clone())).unwrap();
            assert_eq!(response["usage"], expected, "buffered, {case}");
            assert_eq!(
                streamed_with_usage(usage)["usage"],
                expected,
                "streamed, {case}"
            );
        }
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
        assert_eq!(
            body_for(&request, &DriverConfig::default())["stream"],
            json!(true)
        );
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
        let err = translate_murmur_request_to_moonshot(
            &orphan_tool,
            &DriverConfig::default(),
            ThinkingControl::ReasoningEffort,
        )
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
        let err = translate_murmur_request_to_moonshot(
            &bad_role,
            &DriverConfig::default(),
            ThinkingControl::ReasoningEffort,
        )
        .expect_err("an unknown role must be named, not dropped");
        assert_eq!(err, "driver: unsupported message role 'developer'");

        // Every message opens with the driver prefix.
        for message in [
            translate_moonshot_response_to_murmur(&filtered).unwrap_err(),
            translate_murmur_request_to_moonshot(
                &orphan_tool,
                &DriverConfig::default(),
                ThinkingControl::ReasoningEffort,
            )
            .unwrap_err(),
            translate_murmur_request_to_moonshot(
                &bad_role,
                &DriverConfig::default(),
                ThinkingControl::ReasoningEffort,
            )
            .unwrap_err(),
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
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "hi"})
        );
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

    // ── A missing stop signal is refused ──────────────────────────────────────

    #[test]
    fn response_with_no_finish_reason_is_refused() {
        for finish_reason in [None, Some(Value::Null), Some(json!("")), Some(json!(1))] {
            let mut choice = json!({"message": {"role": "assistant", "content": "partial"}});
            if let Some(finish_reason) = finish_reason.clone() {
                choice["finish_reason"] = finish_reason;
            }
            let response = json!({"choices": [choice]});
            assert_eq!(
                translate_moonshot_response_to_murmur(&response).unwrap_err(),
                "driver: Moonshot response has no finish_reason",
                "finish_reason = {finish_reason:?}"
            );
        }
    }

    #[test]
    fn stream_that_ends_with_no_finish_reason_is_refused() {
        let cut_off = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"The answer \"},\"finish_reason\":null}]}\n",
        );
        let blank_reason = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"\"}]}\n",
            "data: [DONE]\n",
        );
        for body in [cut_off, blank_reason, ""] {
            assert_eq!(
                parse_moonshot_sse_body(body, &mut |_: &str| {}, &mut |_: &str| {}).unwrap_err(),
                "driver: Moonshot stream ended with no finish_reason",
                "body = {body:?}"
            );
        }
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
            "drivers/driver-moonshotai/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[test]
    fn manifest_models_cover_every_accepted_model() {
        use std::collections::BTreeMap;

        #[derive(serde::Deserialize)]
        struct Manifest {
            models: BTreeMap<String, Limits>,
        }
        #[derive(Debug, PartialEq, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Limits {
            max_output_tokens: u32,
            context_window: u32,
            #[serde(default)]
            max_input_tokens: Option<u32>,
        }

        // Parsed with the crate murmur parses the bundled manifest with.
        let models = serde_norway::from_str::<Manifest>(include_str!("../murmur.yaml"))
            .expect("murmur.yaml's models: block must parse")
            .models;
        assert!(!models.is_empty(), "murmur.yaml's models: block is empty");
        for (id, limits) in &models {
            assert!(
                limits.max_output_tokens >= 1
                    && limits.context_window >= 1
                    && limits.max_output_tokens <= limits.context_window,
                "models.{id}: {limits:?} must satisfy 1 <= max_output_tokens <= context_window"
            );
            if let Some(ceiling) = limits.max_input_tokens {
                assert!(
                    ceiling >= 1 && ceiling < limits.context_window,
                    "models.{id}: {limits:?} must satisfy 1 <= max_input_tokens < context_window"
                );
            }
        }

        // README.md's "Model limits" table:
        // | `<id>` | <max_output_tokens> | <context_window> | <max_input_tokens or none> | …
        let readme = include_str!("../README.md");
        let section = readme
            .split("\n## ")
            .find(|section| section.starts_with("Model limits\n"))
            .expect("README.md must have a ## Model limits section");
        let mut documented = BTreeMap::new();
        for row in section.lines() {
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            let [_, id, out, ctx, input, ..] = cells[..] else {
                continue;
            };
            let Some(id) = id.strip_prefix('`').and_then(|id| id.strip_suffix('`')) else {
                continue;
            };
            let number = |cell: &str| -> u32 {
                cell.parse()
                    .unwrap_or_else(|_| panic!("README.md row '{id}': '{cell}' is not an integer"))
            };
            let ceiling = match input {
                "none" => None,
                cell => Some(cell.parse().unwrap_or_else(|_| {
                    panic!("README.md row '{id}': '{cell}' is not an integer or none")
                })),
            };
            let limits = Limits {
                max_output_tokens: number(out),
                context_window: number(ctx),
                max_input_tokens: ceiling,
            };
            assert!(
                documented.insert(id.to_string(), limits).is_none(),
                "README.md's Model limits table lists '{id}' twice"
            );
        }

        // The accepted set is SUPPORTED_MODELS, and the manifest repeats its numbers.
        let accepted: Vec<&str> = super::SUPPORTED_MODELS
            .iter()
            .map(|(id, _, _)| *id)
            .collect();
        for (id, source, _) in super::SUPPORTED_MODELS {
            if let Some(limits) = models.get(*id) {
                assert_eq!(
                    (
                        u64::from(limits.max_output_tokens),
                        u64::from(limits.context_window)
                    ),
                    (source.max_output_tokens, source.context_window),
                    "murmur.yaml's models.{id} disagrees with SUPPORTED_MODELS"
                );
            }
        }

        for id in &accepted {
            assert!(
                models.contains_key(*id),
                "murmur.yaml's models: block lacks '{id}'"
            );
        }
        for id in models.keys() {
            assert!(
                accepted.contains(&id.as_str()),
                "murmur.yaml's models: block declares '{id}', which is outside the accepted set"
            );
        }
        for (id, limits) in &models {
            assert_eq!(
                documented.get(id),
                Some(limits),
                "README.md's Model limits table disagrees with murmur.yaml on '{id}'"
            );
        }
        for id in documented.keys() {
            assert!(
                models.contains_key(id),
                "README.md's Model limits table lists '{id}', which murmur.yaml does not declare"
            );
        }
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
                "driver-moonshotai: non-test source must not contain {needle}"
            );
        }
    }

    // ── Tool-call signals ─────────────────────────────────────────────────────

    use super::{parse_moonshot_sse_body_with_signals, RecordedSignal};

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
        let chunk = json!({"choices": [{"delta": delta, "finish_reason": null}]});
        format!("data: {chunk}\n")
    }

    /// One streamed chunk carrying a `tool_calls` fragment for `index`.
    fn tool_delta(
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> String {
        let mut call = json!({"index": index, "function": {}});
        if let Some(id) = id {
            call["id"] = json!(id);
            call["type"] = json!("function");
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
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\ndata: [DONE]\n";

    fn parse_with_signals(body: &str) -> (Value, Vec<RecordedSignal>) {
        let (result, signals) =
            parse_moonshot_sse_body_with_signals(body, &mut |_| {}, &mut |_| {});
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
        use super::{process_moonshot_sse_line, UsageTokens};

        for index in [1024, 4_294_967_296, u64::MAX] {
            let line = tool_delta(index, Some("call_x"), Some("bash"), Some(""));
            let mut tool_states = Vec::new();
            let mut signals = Vec::new();
            let result = process_moonshot_sse_line(
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
                Err(format!(
                    "driver: tool call index {index} is past the limit of 1024 per turn"
                ))
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
        let (result, _) = parse_moonshot_sse_body_with_signals(&body, &mut |_| {}, &mut |_| {});
        assert_eq!(
            result,
            Err(
                "driver: tool call index 4294967296 is past the limit of 1024 per turn".to_string()
            )
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
        let (_, before_the_id) =
            parse_moonshot_sse_body_with_signals(&head, &mut |_| {}, &mut |_| {});
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
        let (result, signals) = parse_moonshot_sse_body_with_signals(
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
}
