// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use std::collections::BTreeMap;

use murmur_driver_stream::Failure;
use serde::Deserialize;
use serde_json::{json, Map, Value};

// ── Model table ───────────────────────────────────────────────────────────────

/// The limits of one model, from xAI's documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelLimits {
    /// The largest `max_output_tokens` this driver sends for the model. A larger value is
    /// refused here rather than clamped silently or spent on a provider 400.
    max_output_tokens: u64,
    /// The model's context window in tokens. No request path reads it: it is the number
    /// `murmur.yaml` declares under `models:`, and a test keeps the two in step.
    #[cfg_attr(not(test), allow(dead_code))]
    context_window: u64,
}

/// Whether a model takes `reasoning.effort`, as its xAI model page lists it. The body and the
/// config check follow from the rule rather than from the model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortRule {
    /// The page lists these effort values and this default. The effort in force, configured or
    /// the default, is sent as `reasoning: {"effort": …}` on every request.
    Effort {
        values: &'static [&'static str],
        default: &'static str,
    },
    /// The page lists no effort values. No `reasoning` member is sent, and a configured
    /// `reasoning_effort` is refused.
    NoEffort,
}

/// The effort set and default xAI's grok-4.7 and grok-4.6 pages list.
const GROK_4_EFFORT: EffortRule = EffortRule::Effort {
    values: &["low", "medium", "high", "xhigh"],
    default: "high",
};

/// xAI documents no output ceiling. `max_output_tokens` defaults to 128,000 when unset, so that
/// default is the cap declared and the largest one sent; declaring the window instead would
/// leave murmur no input budget (`E-RUN-055`).
const DEFAULT_OUTPUT_CAP: u64 = 128_000;

/// Every model this driver serves, with its limits and its effort rule. This table is the one
/// place the crate branches on a model id: everything else reads the row `validate_model`
/// returns. `murmur.yaml`'s `models:` block must declare the same numbers for each entry, and
/// README.md's "Model limits" table the same ids.
const SUPPORTED_MODELS: &[(&str, ModelLimits, EffortRule)] = &[
    (
        "grok-4.7",
        ModelLimits {
            max_output_tokens: DEFAULT_OUTPUT_CAP,
            context_window: 500_000,
        },
        GROK_4_EFFORT,
    ),
    (
        "grok-4.6",
        ModelLimits {
            max_output_tokens: DEFAULT_OUTPUT_CAP,
            context_window: 500_000,
        },
        GROK_4_EFFORT,
    ),
    (
        "grok-build-0.1",
        ModelLimits {
            max_output_tokens: DEFAULT_OUTPUT_CAP,
            context_window: 256_000,
        },
        EffortRule::NoEffort,
    ),
];

/// Look the model up by its exact id before a request body is built, so an unsupported model
/// costs no HTTP call.
fn validate_model(model: &str) -> Result<(&'static ModelLimits, EffortRule), String> {
    SUPPORTED_MODELS
        .iter()
        .find(|(id, _, _)| *id == model)
        .map(|(_, limits, rule)| (limits, *rule))
        .ok_or_else(|| {
            format!(
                "driver: model '{model}' is not supported by driver-xai. Supported models: {}.",
                SUPPORTED_MODELS
                    .iter()
                    .map(|(id, _, _)| *id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Refuse an output cap above the model's ceiling rather than clamping it silently.
fn validate_max_tokens(model: &str, limits: &ModelLimits, max_tokens: u64) -> Result<(), String> {
    if max_tokens > limits.max_output_tokens {
        return Err(format!(
            "driver: inference.max_tokens {max_tokens} exceeds driver-xai's max_output_tokens \
             ceiling of {} for model '{model}'",
            limits.max_output_tokens
        ));
    }
    Ok(())
}

// ── Parameters never sent ─────────────────────────────────────────────────────

/// `params` members never forwarded, sorted. The one strip list, consulted by the one `params`
/// loop:
///
/// - `background`: a background response is stored at xAI and polled, and this driver is
///   stateless with `store: false`;
/// - `frequency_penalty`, `presence_penalty`, `stop`: xAI's reasoning guide says they return an
///   error on reasoning models, and every model here reasons;
/// - `max_completion_tokens`, `max_tokens`: the cap goes out once, as `max_output_tokens`;
/// - `previous_response_id`: needs a stored response, and cannot be combined with
///   `instructions`;
/// - `reasoning`, `reasoning_effort`: the effort comes from `inference.driver.config` and the
///   model's effort rule only;
/// - `search_parameters`: xAI's legacy server-side live search, a surface this driver declines.
const PROVIDER_REFUSED_PARAMS: &[&str] = &[
    "background",
    "frequency_penalty",
    "max_completion_tokens",
    "max_tokens",
    "presence_penalty",
    "previous_response_id",
    "reasoning",
    "reasoning_effort",
    "search_parameters",
    "stop",
];

/// What every request asks xAI to return: each reasoning item's encrypted content, which the
/// driver keeps on the turn's `thinking` block and sends back on every later request.
const INCLUDE: &str = "reasoning.encrypted_content";

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a hard error on the first inference call, before
// any HTTP request is dispatched — a declared setting that is parsed and ignored is a defect,
// not a convenience.

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &["reasoning_effort", "response_format"];

/// The members `response_format.json_schema` may carry, sorted. Each is placed on `text.format`;
/// any other would be dropped there, so it is refused instead.
const JSON_SCHEMA_MEMBERS: &[&str] = &["name", "schema", "strict"];

/// The members `response_format` itself may carry, sorted.
const RESPONSE_FORMAT_MEMBERS: &[&str] = &["json_schema", "type"];

/// `strict: false` is refused rather than silently upgraded to `true`.
const STRICT_FALSE_REFUSAL: &str =
    "driver: inference.driver.config 'response_format.json_schema.strict' must be true — \
     strict: false is refused rather than silently upgraded, because a schema the provider does \
     not enforce is not the guarantee this key offers";

/// A configured `reasoning_effort`: the value as written, for the refusal, and the trimmed,
/// lowercased value that is checked against the model and sent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EffortSetting {
    raw: String,
    normalised: String,
}

/// The dials read from `inference.driver.config`. Parsing knows nothing of the model: whether a
/// dial applies is checked against the model's [`EffortRule`] in `preflight_request`.
#[derive(Debug, Clone, Default, PartialEq)]
struct DriverConfig {
    /// `reasoning_effort`, a string. `None` when the key is absent; a model with an
    /// [`EffortRule::Effort`] rule then gets its default.
    reasoning_effort: Option<EffortSetting>,
    /// `response_format`, validated and already in the shape of xAI's `text.format`:
    /// `{"type":"json_schema","name","schema","strict":true}`.
    text_format: Option<Value>,
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

    let unrecognised = unrecognised_members(config, ACCEPTED_CONFIG_KEYS);
    if !unrecognised.is_empty() {
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
        text_format: config
            .get("response_format")
            .map(validate_response_format)
            .transpose()?,
    })
}

/// The keys of `object` outside `accepted`. `object` is a serde_json Map, i.e. sorted, so they
/// come out in a stable order and one manifest fix clears every one of them.
fn unrecognised_members(object: &Map<String, Value>, accepted: &[&str]) -> Vec<String> {
    object
        .keys()
        .filter(|key| !accepted.contains(&key.as_str()))
        .cloned()
        .collect()
}

/// A string, trimmed and lowercased. Whether the value is one the model takes is checked in
/// `preflight_request`, against the model's effort rule.
fn parse_reasoning_effort(value: &Value) -> Result<EffortSetting, String> {
    match value {
        Value::String(raw) => Ok(EffortSetting {
            raw: raw.clone(),
            normalised: raw.trim().to_ascii_lowercase(),
        }),
        other => Err(format!(
            "driver: inference.driver.config 'reasoning_effort' must be a string, got {other}"
        )),
    }
}

/// Validate `response_format` and return the `text.format` object it becomes.
///
/// It takes `{"type":"json_schema","json_schema":{"name","schema","strict"?}}`, the spelling the
/// other drivers here read. `name` must be a non-blank string, because xAI's Responses
/// `text.format` carries one; `schema` must be an object; `strict` is absent or `true` and is
/// always sent as `true`.
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

    let unrecognised = unrecognised_members(object, RESPONSE_FORMAT_MEMBERS);
    if !unrecognised.is_empty() {
        return Err(format!(
            "driver: inference.driver.config 'response_format' has unrecognised member(s): {}. \
             Accepted members: {}",
            unrecognised.join(", "),
            RESPONSE_FORMAT_MEMBERS.join(", ")
        ));
    }

    let Some(schema_block) = object.get("json_schema").and_then(Value::as_object) else {
        return Err(
            "driver: inference.driver.config 'response_format.json_schema' must be an object"
                .to_string(),
        );
    };

    let unrecognised = unrecognised_members(schema_block, JSON_SCHEMA_MEMBERS);
    if !unrecognised.is_empty() {
        return Err(format!(
            "driver: inference.driver.config 'response_format.json_schema' has unrecognised \
             member(s): {}. Accepted members: {}",
            unrecognised.join(", "),
            JSON_SCHEMA_MEMBERS.join(", ")
        ));
    }

    let Some(name) = schema_block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
    else {
        return Err(
            "driver: inference.driver.config 'response_format.json_schema.name' is required \
             and must be a non-blank string"
                .to_string(),
        );
    };

    let Some(schema) = schema_block
        .get("schema")
        .filter(|schema| schema.is_object())
    else {
        return Err(
            "driver: inference.driver.config 'response_format.json_schema.schema' is required \
             and must be an object"
                .to_string(),
        );
    };

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

    Ok(json!({
        "type": "json_schema",
        "name": name,
        "schema": schema,
        "strict": true,
    }))
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
    /// Prompt-cache routing hint, injected by the host as a reserved top-level
    /// `prompt_cache_key` member and constant for every turn of one task. xAI plumbs it to
    /// `x-grok-conv-id`, which its caching guide recommends for cache hits. Held as a raw `Value`
    /// so a null or non-string does not fail the request parse: anything but a non-blank string
    /// means "no key" (see [`prompt_cache_key`]).
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
    /// Reasoning from a turn this driver returned. murmur stores a driver's content blocks as
    /// returned and replays them to the same driver and model, so the reasoning item xAI sent
    /// rides here, in `xai_reasoning`, and goes back verbatim (see [`replayable_reasoning`]).
    /// The block's `text`, the readable summary, is not sent back: xAI takes the item, which
    /// carries its own summary.
    Thinking {
        #[serde(default)]
        xai_reasoning: Option<Value>,
    },
    /// Refused by `preflight_request`; its source is never read.
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

/// The routing hint to forward, or `None` when the host sent no usable one. An absent, null,
/// non-string, empty or whitespace-only value all mean "no key" and are never an error.
fn prompt_cache_key(request: &MurmurRequest) -> Option<&str> {
    request
        .prompt_cache_key
        .as_str()
        .map(str::trim)
        .filter(|key| !key.is_empty())
}

/// The reasoning item a `thinking` block carries, when it is one xAI can take back: an object
/// with `type: "reasoning"` and a non-empty string `encrypted_content`. Anything else is
/// dropped without an error. A compacted history has no `thinking` blocks at all, and a block
/// another release wrote may lack the member.
fn replayable_reasoning(item: Option<&Value>) -> Option<&Value> {
    item.filter(|item| {
        item.get("type").and_then(Value::as_str) == Some("reasoning")
            && item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|content| !content.is_empty())
    })
}

/// The text blocks of a message, joined with a newline.
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

/// Every check that can be made without building a body or touching the network: the model,
/// then the output cap, then the configured effort against the model's effort rule, then the
/// content's image blocks. Returns the model's effort rule, which decides the `reasoning`
/// member of the body.
fn preflight_request(request: &MurmurRequest, config: &DriverConfig) -> Result<EffortRule, String> {
    let model = request.model.as_str();
    let (limits, rule) = validate_model(model)?;
    validate_max_tokens(model, limits, request.max_tokens)?;

    if let Some(effort) = &config.reasoning_effort {
        match rule {
            EffortRule::NoEffort => {
                return Err(format!(
                    "driver: inference.driver.config 'reasoning_effort' is not accepted for \
                     model '{model}': xAI documents no reasoning effort for it. Remove the key."
                ))
            }
            EffortRule::Effort { values, .. } => {
                if !values.contains(&effort.normalised.as_str()) {
                    return Err(format!(
                        "driver: inference.driver.config 'reasoning_effort' must be one of {} \
                         for model '{model}', got \"{}\"",
                        values.join(", "),
                        effort.raw
                    ));
                }
            }
        }
    }

    let has_image = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, MurmurContentBlock::Image { .. }))
    });
    if has_image {
        return Err("driver: driver-xai sends text only and refuses an image block".to_string());
    }
    Ok(rule)
}

/// The body `run_inner` puts on the wire: pre-flight, translate, stamp the streaming flag.
/// Tests assert on this rather than on a bare translation.
fn build_provider_request(request: &MurmurRequest, config: &DriverConfig) -> Result<Value, String> {
    let rule = preflight_request(request, config)?;
    let mut body = translate_murmur_request_to_xai(request, config, rule)?;
    stamp_streaming_flags(&mut body);
    Ok(body)
}

fn translate_murmur_request_to_xai(
    request: &MurmurRequest,
    config: &DriverConfig,
    rule: EffortRule,
) -> Result<Value, String> {
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role.as_str() {
            "user" => input.push(translate_user_message(message)),
            "assistant" => input.extend(translate_assistant_message(message)?),
            "tool" => input.push(translate_tool_message(message)?),
            other => return Err(format!("driver: unsupported message role '{other}'")),
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("type".to_string(), Value::String("function".to_string()));
            function.insert("name".to_string(), Value::String(tool.name.clone()));
            if let Some(desc) = tool
                .description
                .as_deref()
                .map(str::trim)
                .filter(|desc| !desc.is_empty())
            {
                function.insert("description".to_string(), Value::String(desc.to_string()));
            }
            function.insert("parameters".to_string(), tool.parameters.clone());
            Value::Object(function)
        })
        .collect::<Vec<_>>();

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("input".to_string(), Value::Array(input));
    if let Some(system) = request
        .system
        .as_deref()
        .map(str::trim)
        .filter(|system| !system.is_empty())
    {
        body.insert(
            "instructions".to_string(),
            Value::String(system.to_string()),
        );
    }
    // Responses spells the output cap `max_output_tokens`; no `max_tokens` member is emitted.
    body.insert(
        "max_output_tokens".to_string(),
        Value::from(request.max_tokens),
    );
    // Stateless: nothing is kept at xAI, the whole history goes out every time, and each
    // reasoning item comes back encrypted so the next request can carry it.
    body.insert("store".to_string(), Value::Bool(false));
    body.insert("include".to_string(), json!([INCLUDE]));

    if let EffortRule::Effort { default, .. } = rule {
        let effort = config
            .reasoning_effort
            .as_ref()
            .map_or(default, |effort| effort.normalised.as_str());
        body.insert("reasoning".to_string(), json!({"effort": effort}));
    }

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    if let Some(format) = config.text_format.as_ref() {
        body.insert("text".to_string(), json!({"format": format}));
    }

    if let Some(key) = prompt_cache_key(request) {
        body.insert(
            "prompt_cache_key".to_string(),
            Value::String(key.to_string()),
        );
    }

    // The one params pass-through loop, consulting the one strip list.
    for (key, value) in &request.params {
        if body.contains_key(key) || PROVIDER_REFUSED_PARAMS.contains(&key.as_str()) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }

    Ok(Value::Object(body))
}

fn translate_user_message(message: &MurmurMessage) -> Value {
    json!({
        "role": "user",
        "content": [{"type": "input_text", "text": joined_text(&message.content)}],
    })
}

/// An assistant turn as Responses input items, in order: each reasoning item its `thinking`
/// blocks carry that xAI can take back, the `message` when the turn has text, then one
/// `function_call` per tool call.
fn translate_assistant_message(message: &MurmurMessage) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    for block in &message.content {
        if let MurmurContentBlock::Thinking { xai_reasoning, .. } = block {
            if let Some(item) = replayable_reasoning(xai_reasoning.as_ref()) {
                items.push(item.clone());
            }
        }
    }

    let text = joined_text(&message.content);
    if !text.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }));
    }

    for block in &message.content {
        if let MurmurContentBlock::ToolCall { id, name, input } = block {
            let arguments = serde_json::to_string(input)
                .map_err(|err| format!("driver: failed to serialize tool_call.input: {err}"))?;
            items.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": arguments,
            }));
        }
    }
    Ok(items)
}

fn translate_tool_message(message: &MurmurMessage) -> Result<Value, String> {
    let call_id = message.tool_call_id.clone().ok_or_else(|| {
        "driver: tool message is missing required field 'tool_call_id'".to_string()
    })?;
    Ok(json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": joined_text(&message.content),
    }))
}

/// Force streaming on, overriding any `stream` key from `params`. Responses streams its usage
/// on the terminal event, so no second flag is needed.
fn stamp_streaming_flags(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
    }
}

// ── Provider token usage ──────────────────────────────────────────────────────

/// Provider-reported token counts, carried on the reserved top-level `usage` object of the
/// translated response. Every member is independently optional: a count the provider did not
/// report is omitted rather than sent as `0`.
///
/// xAI reports the request total with the cached tokens inside it, and its reference example
/// counts reasoning beside `output_tokens` (`output 9, reasoning 110, total 151` on an input of
/// 32). The raw members are held here and the contract's members are derived only in
/// `to_value`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTokens {
    /// `usage.input_tokens`, cached tokens included.
    request_total: Option<u64>,
    /// `usage.output_tokens`.
    output_tokens: Option<u64>,
    /// `usage.input_tokens_details.cached_tokens`.
    cached_tokens: Option<u64>,
    /// `usage.output_tokens_details.reasoning_tokens`.
    reasoning_tokens: Option<u64>,
    /// `usage.total_tokens`.
    total_tokens: Option<u64>,
}

impl UsageTokens {
    /// Take every member `later` reports, keeping the current value for the rest.
    fn merge_from(&mut self, later: UsageTokens) {
        let take = |current: &mut Option<u64>, later: Option<u64>| {
            if later.is_some() {
                *current = later;
            }
        };
        take(&mut self.request_total, later.request_total);
        take(&mut self.output_tokens, later.output_tokens);
        take(&mut self.cached_tokens, later.cached_tokens);
        take(&mut self.reasoning_tokens, later.reasoning_tokens);
        take(&mut self.total_tokens, later.total_tokens);
    }

    /// The completion as xAI bills it: `total_tokens − input_tokens` when both are reported
    /// and the difference does not underflow, which counts reasoning in it whether xAI reports
    /// reasoning inside `output_tokens` or beside it; otherwise `output_tokens` as reported.
    fn billed_output(self) -> Option<u64> {
        match (self.total_tokens, self.request_total) {
            (Some(total), Some(request)) => total.checked_sub(request).or(self.output_tokens),
            _ => self.output_tokens,
        }
    }

    /// The contract `usage` object:
    ///
    /// - `input_tokens` = request total − cached tokens, omitted when the total is unreported or
    ///   the subtraction would go below zero;
    /// - `cached_tokens` as reported;
    /// - `output_tokens` from [`UsageTokens::billed_output`];
    /// - `thinking_tokens` = `reasoning_tokens`, kept only when it is at most that
    ///   `output_tokens`, as the contract makes it a subset.
    ///
    /// `None` when no member survived, in which case the response carries no `usage` key.
    fn to_value(self) -> Option<Value> {
        let input_tokens = self
            .request_total
            .and_then(|total| total.checked_sub(self.cached_tokens.unwrap_or(0)));
        let output_tokens = self.billed_output();
        let thinking_tokens = self
            .reasoning_tokens
            .filter(|reasoning| output_tokens.is_some_and(|output| *reasoning <= output));
        let mut obj = Map::new();
        for (key, count) in [
            ("input_tokens", input_tokens),
            ("cached_tokens", self.cached_tokens),
            ("output_tokens", output_tokens),
            ("thinking_tokens", thinking_tokens),
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

/// Read an xAI Responses `usage` object. A member that is absent, null, or not a non-negative
/// integer is dropped; its siblings are kept. `cost_in_usd_ticks` is never read.
fn extract_xai_usage(usage: &Value) -> UsageTokens {
    UsageTokens {
        request_total: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cached_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
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

// ── Stream state ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
    /// A `function_call` event named this slot's `output_index`. Slots below the highest one
    /// that hold another kind of item stay unset and are skipped.
    present: bool,
    /// `Started` has been signalled for this call.
    started: bool,
    /// The last `InputBytes` total signalled for this call.
    reported_bytes: u64,
}

impl ToolCallState {
    /// Signal what this call's state newly supports, after an event has been merged into it:
    /// its start, once both its call id and name are known, then its argument text's byte
    /// length whenever that has grown past the last total.
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
/// is the running UTF-8 byte length of the call's argument text.
enum ToolCallSignal<'a> {
    Started { id: &'a str, name: &'a str },
    InputBytes { id: &'a str, bytes: u64 },
}

/// One reasoning output item of a turn, keyed by its `output_index`.
#[derive(Debug, Default)]
struct ReasoningSlot {
    /// The summary or reasoning-text deltas streamed for the item, concatenated.
    streamed: String,
    /// The item exactly as xAI sent it, from `response.output_item.done` or the terminal
    /// event's `response.output`.
    item: Option<Value>,
}

impl ReasoningSlot {
    /// The `thinking` block for this item: the streamed text, else the item's `summary[].text`
    /// joined, else `""`; with `xai_reasoning` when the item arrived. `None` when there is
    /// neither text nor an item.
    fn thinking_block(self) -> Option<Value> {
        let text = if self.streamed.is_empty() {
            self.item.as_ref().map(summary_text).unwrap_or_default()
        } else {
            self.streamed
        };
        match self.item {
            Some(item) => Some(json!({"type": "thinking", "text": text, "xai_reasoning": item})),
            None if !text.is_empty() => Some(json!({"type": "thinking", "text": text})),
            None => None,
        }
    }
}

/// A reasoning item's `summary[].text`, joined with a blank line.
fn summary_text(item: &Value) -> String {
    item.get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// What a turn has said so far, on either path.
#[derive(Default)]
struct StreamState {
    /// The `output_text` deltas, concatenated.
    text: String,
    /// Function calls by `output_index`, grown with [`murmur_driver_stream::slot_mut`].
    tool_states: Vec<ToolCallState>,
    /// Reasoning items by `output_index`, so output order is key order.
    reasoning: BTreeMap<u64, ReasoningSlot>,
    /// A reasoning item arrived on `response.output_item.done`. When none did, the terminal
    /// event's `response.output` supplies them.
    reasoning_items_seen: bool,
    /// The terminal status: `response.status`, else the terminal event's name.
    status: Option<String>,
    /// `response.incomplete_details.reason`.
    incomplete_reason: Option<String>,
    /// A provider error from an `error` event or a terminal `error`, already classified.
    provider_error: Option<Failure>,
    usage: UsageTokens,
    /// Set by a complete line that is not SSE, or a `data:` payload that is not JSON. Such a
    /// body is not an xAI stream that was cut, so ending it with no terminal event is not a
    /// transient failure.
    malformed: bool,
}

impl StreamState {
    /// The tool slot at `index`, refused at or past the per-turn limit before anything is
    /// stored for it.
    fn tool_slot(&mut self, index: u64) -> Result<&mut ToolCallState, Failure> {
        let state = murmur_driver_stream::slot_mut(&mut self.tool_states, index)
            .map_err(|err| Failure::plain(format!("driver: tool call {err}")))?;
        state.present = true;
        Ok(state)
    }

    /// Record a reasoning item, filling the slot's text from its summary when no delta
    /// streamed for it, and return that text so the caller can emit it.
    fn record_reasoning_item(&mut self, index: u64, item: &Value) -> Option<String> {
        let slot = self.reasoning.entry(index).or_default();
        slot.item = Some(item.clone());
        if slot.streamed.is_empty() {
            Some(summary_text(item)).filter(|text| !text.is_empty())
        } else {
            None
        }
    }

    /// Fill a function call from a whole `function_call` item: any missing call id, name or
    /// arguments.
    fn fill_function_call(
        &mut self,
        index: u64,
        item: &Value,
        signal: &mut impl FnMut(ToolCallSignal<'_>),
    ) -> Result<(), Failure> {
        let state = self.tool_slot(index)?;
        let field = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or_default();
        if state.id.is_empty() {
            state.id = field("call_id").to_string();
        }
        if state.name.is_empty() {
            state.name = field("name").to_string();
        }
        if state.arguments.is_empty() {
            state.arguments = field("arguments").to_string();
        }
        state.signal_due(signal);
        Ok(())
    }
}

/// Whether `line` is SSE syntax: blank, a `:` comment, or one of the four SSE fields.
fn is_sse_line(line: &str) -> bool {
    line.is_empty()
        || line.starts_with(':')
        || ["data", "event", "id", "retry"].iter().any(|field| {
            line.strip_prefix(field)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
        })
}

/// An event's `output_index`, `0` when absent.
fn output_index(data: &Value) -> u64 {
    data.get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Process one complete SSE line of xAI Responses events. Returns `Ok(true)` when the stream is
/// done (`data: [DONE]`), and `Err` for a function call at an `output_index` past
/// [`murmur_driver_stream::MAX_SLOTS_PER_TURN`].
///
/// xAI publishes no list of its Responses events. These are OpenAI's names, which the Grok
/// Build CLI reads from xAI's `/v1/responses`:
///
/// - `response.output_text.delta` goes to `emit_text`;
/// - `response.reasoning_summary_text.delta` and `response.reasoning_text.delta` go to
///   `emit_thinking`, accumulated under their `output_index`;
/// - `response.output_item.added` and `.done`, and `response.function_call_arguments.delta`
///   and `.done`, build the function calls;
/// - `response.output_item.done` with a reasoning item keeps that item verbatim;
/// - `response.completed`, `.incomplete` and `.failed` end the turn;
/// - `error` records a provider failure.
///
/// Nothing here scans text for reasoning tags.
fn process_xai_sse_line(
    line: &str,
    state: &mut StreamState,
    emit_text: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
    signal: &mut impl FnMut(ToolCallSignal<'_>),
) -> Result<bool, Failure> {
    let Some(payload) = line.strip_prefix("data:") else {
        state.malformed |= !is_sse_line(line);
        return Ok(false);
    };
    let payload = payload.strip_prefix(' ').unwrap_or(payload);
    if payload == "[DONE]" {
        return Ok(true);
    }
    let Ok(data) = serde_json::from_str::<Value>(payload) else {
        state.malformed = true;
        return Ok(false);
    };
    let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");
    let delta = || {
        data.get("delta")
            .and_then(Value::as_str)
            .filter(|delta| !delta.is_empty())
    };

    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = delta() {
                emit_text(delta);
                state.text.push_str(delta);
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = delta() {
                emit_thinking(delta);
                state
                    .reasoning
                    .entry(output_index(&data))
                    .or_default()
                    .streamed
                    .push_str(delta);
            }
        }
        "response.output_item.added" => {
            let item = data.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let tool = state.tool_slot(output_index(&data))?;
                let field = |name: &str| item.get(name).and_then(Value::as_str);
                if let Some(id) = field("call_id").filter(|_| tool.id.is_empty()) {
                    tool.id = id.to_string();
                }
                if let Some(name) = field("name").filter(|_| tool.name.is_empty()) {
                    tool.name = name.to_string();
                }
                tool.signal_due(signal);
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = delta() {
                let tool = state.tool_slot(output_index(&data))?;
                tool.arguments.push_str(delta);
                tool.signal_due(signal);
            }
        }
        "response.function_call_arguments.done" => {
            if let Some(arguments) = data.get("arguments").and_then(Value::as_str) {
                let tool = state.tool_slot(output_index(&data))?;
                // Authoritative over the deltas; it reports only when longer than their total.
                tool.arguments = arguments.to_string();
                tool.signal_due(signal);
            }
        }
        "response.output_item.done" => {
            let item = data.get("item").unwrap_or(&Value::Null);
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    state.reasoning_items_seen = true;
                    if let Some(text) = state.record_reasoning_item(output_index(&data), item) {
                        emit_thinking(&text);
                    }
                }
                Some("function_call") => {
                    state.fill_function_call(output_index(&data), item, signal)?;
                }
                _ => {}
            }
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            let response = data.get("response").unwrap_or(&Value::Null);
            state.status = response
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| !status.is_empty())
                .unwrap_or_else(|| event_type.trim_start_matches("response."))
                .to_string()
                .into();
            state.incomplete_reason = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .map(String::from);
            let error = response
                .get("error")
                .filter(|error| !error.is_null())
                .or_else(|| data.get("error").filter(|error| !error.is_null()));
            if let Some(error) = error {
                state.provider_error = Some(provider_error_failure(error));
            }
            if let Some(reported) = response.get("usage").filter(|usage| !usage.is_null()) {
                state.usage.merge_from(extract_xai_usage(reported));
            }
            if !state.reasoning_items_seen {
                let output = response.get("output").and_then(Value::as_array);
                for (index, item) in output.into_iter().flatten().enumerate() {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        if let Some(text) = state.record_reasoning_item(index as u64, item) {
                            emit_thinking(&text);
                        }
                    }
                }
            }
        }
        "error" => {
            let error = data
                .get("error")
                .filter(|error| error.is_object())
                .unwrap_or(&data);
            state.provider_error = Some(provider_error_failure(error));
        }
        _ => {}
    }

    Ok(false)
}

/// Turn the accumulated state into the murmur response. `content` is the `thinking` blocks in
/// output order, then the `text` block when non-empty, then the `tool_call` blocks.
fn assemble_xai_streaming_response(state: StreamState) -> Result<Value, Failure> {
    // A provider error outranks a missing terminal event: an `error` event and then EOF is
    // the provider's error, not a cut stream.
    if let Some(failure) = state.provider_error {
        return Err(failure);
    }

    // No terminal event, so the stream ended before the turn did. After a `2xx` head that is a
    // broken connection, which the same request may not meet again — unless the body was never
    // a well-formed xAI stream.
    let Some(status) = state.status.as_deref() else {
        let message = "driver: xAI Responses stream ended with no terminal event";
        return Err(if state.malformed {
            Failure::plain(message)
        } else {
            Failure::transient(message)
        });
    };

    let calls: Vec<ToolCallState> = state
        .tool_states
        .into_iter()
        .filter(|call| call.present)
        .collect();
    let stop_reason = match status {
        "completed" if calls.is_empty() => "end_turn",
        "completed" => "tool_call",
        "incomplete" => match state.incomplete_reason.as_deref() {
            Some("max_output_tokens") => "max_tokens",
            other => {
                return Err(Failure::plain(format!(
                    "driver: xAI Responses turn ended incomplete: {}",
                    other.unwrap_or("no reason given")
                )))
            }
        },
        "failed" => {
            return Err(Failure::plain(
                "driver: xAI Responses turn failed with no error",
            ))
        }
        other => {
            return Err(Failure::plain(format!(
                "driver: unsupported xAI Responses status '{other}'"
            )))
        }
    };
    let capped = stop_reason == "max_tokens";

    let mut tool_content = Vec::new();
    for call in calls {
        let arguments = if call.arguments.is_empty() {
            "{}"
        } else {
            call.arguments.as_str()
        };
        let input: Value = match serde_json::from_str(arguments) {
            Ok(input) => input,
            Err(_) if capped => return Err(Failure::plain(truncated_tool_call_at_cap(&call.name))),
            Err(err) => {
                return Err(Failure::plain(format!(
                    "driver: failed to parse xAI function_call arguments JSON for tool '{}': {err}",
                    call.name
                )))
            }
        };
        tool_content.push(json!({
            "type": "tool_call",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }

    let mut content: Vec<Value> = state
        .reasoning
        .into_values()
        .filter_map(ReasoningSlot::thinking_block)
        .collect();
    if !state.text.is_empty() {
        content.push(json!({"type": "text", "text": state.text}));
    }
    content.extend(tool_content);

    Ok(murmur_response(stop_reason, content, state.usage))
}

/// The message for a tool call whose arguments the output cap cut off. The arguments are left
/// out: the cap, not their syntax, is what the operator has to act on.
fn truncated_tool_call_at_cap(name: &str) -> String {
    let name = if name.is_empty() { "<unnamed>" } else { name };
    format!(
        "driver: xAI Responses turn stopped at the inference.max_tokens output cap; tool call \
         '{name}' was cut off mid-arguments and cannot be run — raise the cap and re-run"
    )
}

// ── Response translation (non-streaming fallback) ─────────────────────────────

/// The non-streaming JSON path, reached when the response body's first non-whitespace byte is
/// `{` or `[` — scripted test servers, and a provider that ignores `stream: true`. The body's
/// `output` items fill the same state the stream does, and the same assembly reads it.
fn translate_xai_response_to_murmur(response: &Value) -> Result<Value, Failure> {
    let mut state = StreamState::default();
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        state.provider_error = Some(provider_error_failure(error));
        return assemble_xai_streaming_response(state);
    }
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| !status.is_empty())
        .ok_or_else(|| Failure::plain("driver: xAI Responses response has no status"))?;
    state.status = Some(status.to_string());
    state.incomplete_reason = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .map(String::from);
    if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
        state.usage = extract_xai_usage(usage);
    }

    let output = response.get("output").and_then(Value::as_array);
    for (index, item) in output.into_iter().flatten().enumerate() {
        let index = index as u64;
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                state.record_reasoning_item(index, item);
            }
            Some("function_call") => state.fill_function_call(index, item, &mut |_| {})?,
            Some("message") => {
                let parts = item.get("content").and_then(Value::as_array);
                for part in parts.into_iter().flatten() {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            state.text.push_str(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    assemble_xai_streaming_response(state)
}

/// Parse a complete SSE body string, driving the same line processor the wasm module drives.
/// This is how the streaming assertions run host-side, with no network and no provider account.
#[cfg(test)]
fn parse_xai_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, Failure> {
    parse_xai_sse_body_with_signals(body, emit_text, emit_thinking).0
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
fn parse_xai_sse_body_with_signals<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> (Result<Value, Failure>, Vec<RecordedSignal>) {
    let mut state = StreamState::default();
    let mut signals = Vec::new();

    // Only newline-terminated lines, as the wasm module reads them: an unterminated tail is a
    // line the connection cut, never processed.
    let terminated = &body[..body.rfind('\n').map_or(0, |at| at + 1)];
    for line in terminated.lines() {
        let line = line.trim_end_matches('\r');
        let done = process_xai_sse_line(
            line,
            &mut state,
            &mut |t: &str| emit_text(t),
            &mut |t: &str| emit_thinking(t),
            &mut |s| signals.push(RecordedSignal::from(s)),
        );
        match done {
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => return (Err(err), signals),
        }
    }

    (assemble_xai_streaming_response(state), signals)
}

// ── Error payloads ────────────────────────────────────────────────────────────

fn error_payload(message: &str) -> Value {
    json!({
        "stop_reason": "error",
        "error": message,
    })
}

/// The `data` payload and `tool-result.metadata` a failed call returns: the same error payload
/// every failure has, plus the failure's `transient` tuple when it carries one.
fn failure_result(failure: &Failure) -> (Value, Vec<(String, String)>) {
    (error_payload(&failure.message), failure.metadata())
}

// ── Failures murmur may retry ─────────────────────────────────────────────────

/// Error `code`s and `type`s that mark a provider error reported inside a `2xx` stream as
/// transient. xAI documents no in-stream error codes, so this is the OpenAI-compatible
/// vocabulary for server load, a rate limit and a server fault.
const TRANSIENT_ERRORS: &[&str] = &[
    "overloaded",
    "rate_limit_error",
    "rate_limit_exceeded",
    "server_error",
    "service_unavailable",
];

/// The failure for an error object xAI sent after a `2xx` head, from an `error` event or a
/// terminal event's `error`: `xAI error: <code-or-type>: <message>`, `unknown` for a missing
/// part. Transient when its code or type is in [`TRANSIENT_ERRORS`], plain otherwise.
fn provider_error_failure(error: &Value) -> Failure {
    let field = |name: &str| {
        error
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let code = field("code");
    // An `error` event read whole has `type: "error"`, which names the event, not the error.
    let kind = field("type").filter(|kind| *kind != "error");
    let message = field("message").or_else(|| error.as_str());
    let text = format!(
        "xAI error: {}: {}",
        code.or(kind).unwrap_or("unknown"),
        message.unwrap_or("unknown")
    );
    if [code, kind]
        .into_iter()
        .flatten()
        .any(|name| TRANSIENT_ERRORS.contains(&name))
    {
        Failure::transient(text)
    } else {
        Failure::plain(text)
    }
}

/// The failure for a provider status `>= 400`: `HTTP <status>: <body>`, carrying xAI's own body
/// so the operator sees what it said. murmur classifies a status itself — it retries a `429`
/// and a `5xx` — and xAI documents no quota body, so nothing here marks one permanent.
fn status_failure(status: u16, body: &str) -> Failure {
    Failure::plain(format!("HTTP {status}: {body}"))
}

// ── WASM driver module ────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_driver {
    use super::{
        assemble_xai_streaming_response, build_provider_request, failure_result,
        parse_driver_config, process_xai_sse_line, status_failure,
        translate_xai_response_to_murmur, MurmurRequest, StreamState, ToolCallSignal,
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

    pub struct XaiDriver;

    impl exports::murmur::tool::run::Guest for XaiDriver {
        fn run(
            input: exports::murmur::tool::run::ToolInput,
        ) -> exports::murmur::tool::run::ToolResult {
            let (response, metadata) = match run_inner(input) {
                Ok(value) => (value, vec![]),
                Err(failure) => failure_result(&failure),
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

    /// Feed one complete line to the processor, emitting text and thinking chunks to the host
    /// as they arrive.
    fn handle_line(line: &[u8], state: &mut StreamState) -> Result<bool, Failure> {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches('\r');
        process_xai_sse_line(
            line,
            state,
            &mut |t: &str| murmur::stream::events::emit_chunk(t),
            &mut |t: &str| murmur::stream::events::emit_thinking_chunk(t),
            &mut signal_tool_call,
        )
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

        // Every pre-flight refusal happens inside here: a rejected request costs no HTTP call.
        let provider_request = build_provider_request(&murmur_request, &config)?;

        let body = serde_json::to_vec(&provider_request)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}/responses", endpoint.trim_end_matches('/'));

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
                .map_err(|err| format!("driver: failed to parse xAI response JSON: {err}"))?;
            translate_xai_response_to_murmur(&json)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut state = StreamState::default();
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

            drop(stream);
            let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
            assemble_xai_streaming_response(state)?
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

    export!(XaiDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// No xAI key is configured where this was built, so nothing here talks to the provider. Every
// assertion is driven by a fixture and the host-side whole-stream parser.

#[cfg(test)]
mod tests {
    use super::{
        build_provider_request, failure_result, parse_driver_config, parse_xai_sse_body,
        parse_xai_sse_body_with_signals, status_failure, translate_xai_response_to_murmur,
        validate_model, DriverConfig, EffortRule, MurmurRequest, RecordedSignal, UsageTokens,
        PROVIDER_REFUSED_PARAMS, SUPPORTED_MODELS,
    };
    use murmur_driver_stream::{Failure, TRANSIENT_METADATA_KEY};
    use serde_json::{json, Value};

    // ── Fixtures and helpers ──────────────────────────────────────────────────

    const SUPPORTED: &str = "Supported models: grok-4.7, grok-4.6, grok-build-0.1.";
    const CUT: &str = "driver: xAI Responses stream ended with no terminal event";
    const ENCRYPTED: &str = "gAAAAABk-encrypted-reasoning==";

    fn request_from(value: Value) -> MurmurRequest {
        serde_json::from_value(value).expect("fixture must deserialize as a murmur request")
    }

    /// The canonical envelope the host builds, with the reserved `prompt_cache_key` present.
    fn simple_request() -> MurmurRequest {
        request_from(json!({
            "model": "grok-4.7",
            "max_tokens": 8192,
            "system": "  You are a careful assistant.  ",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Why is the sky blue?"}]}
            ],
            "tools": [],
            "params": {},
            "prompt_cache_key": "xai-proof:0.1.0:task-1"
        }))
    }

    /// A one-turn request for `model` with the given output cap.
    fn request_for(model: &str, max_tokens: u64) -> MurmurRequest {
        request_from(json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }))
    }

    fn config(raw: &str) -> DriverConfig {
        parse_driver_config(Some(raw)).expect("fixture config must parse")
    }

    fn body_for(request: &MurmurRequest, config: &DriverConfig) -> Value {
        build_provider_request(request, config).expect("fixture request must translate")
    }

    fn refusal(request: &MurmurRequest, config: &DriverConfig) -> String {
        build_provider_request(request, config).expect_err("the request must be refused")
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

    /// One SSE event line followed by the blank separator line.
    fn event(data: Value) -> String {
        format!("data: {data}\n\n")
    }

    fn text_delta(text: &str) -> String {
        event(json!({"type": "response.output_text.delta", "output_index": 1, "delta": text}))
    }

    fn summary_delta(index: u64, text: &str) -> String {
        event(json!({"type": "response.reasoning_summary_text.delta",
                     "output_index": index, "summary_index": 0, "delta": text}))
    }

    fn reasoning_item(id: &str, summary: &[&str], encrypted: &str) -> Value {
        json!({
            "id": id,
            "type": "reasoning",
            "summary": summary
                .iter()
                .map(|text| json!({"type": "summary_text", "text": text}))
                .collect::<Vec<_>>(),
            "status": "completed",
            "encrypted_content": encrypted,
        })
    }

    fn item_added(index: u64, item: Value) -> String {
        event(json!({"type": "response.output_item.added", "output_index": index, "item": item}))
    }

    fn item_done(index: u64, item: Value) -> String {
        event(json!({"type": "response.output_item.done", "output_index": index, "item": item}))
    }

    fn function_call_added(index: u64, call_id: &str, name: &str) -> String {
        item_added(
            index,
            json!({"type": "function_call", "id": "fc_1", "call_id": call_id, "name": name,
                   "arguments": "", "status": "in_progress"}),
        )
    }

    fn arguments_delta(index: u64, delta: &str) -> String {
        event(json!({"type": "response.function_call_arguments.delta",
                     "output_index": index, "delta": delta}))
    }

    fn arguments_done(index: u64, arguments: &str) -> String {
        event(json!({"type": "response.function_call_arguments.done",
                     "output_index": index, "arguments": arguments}))
    }

    fn terminal(kind: &str, response: Value) -> String {
        event(json!({"type": format!("response.{kind}"), "response": response}))
    }

    fn completed() -> String {
        terminal("completed", json!({"status": "completed"}))
    }

    fn completed_with_usage(usage: Value) -> String {
        terminal("completed", json!({"status": "completed", "usage": usage}))
    }

    fn parse(body: &str) -> Result<Value, Failure> {
        parse_xai_sse_body(body, &mut |_: &str| {}, &mut |_: &str| {})
    }

    fn parse_with_signals(body: &str) -> (Value, Vec<RecordedSignal>) {
        let (result, signals) =
            parse_xai_sse_body_with_signals(body, &mut |_: &str| {}, &mut |_: &str| {});
        (result.expect("the fixture stream must parse"), signals)
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

    fn usage_of(usage: Value) -> Option<Value> {
        let body = format!("{}{}", text_delta("x"), completed_with_usage(usage));
        parse(&body).unwrap().get("usage").cloned()
    }

    fn reported(input: u64, cached: u64, output: u64, reasoning: u64, total: u64) -> Value {
        json!({
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": cached},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": reasoning},
            "total_tokens": total,
            "cost_in_usd_ticks": 28_896_000u64,
        })
    }

    /// The non-test source of the crate: everything before `mod tests`.
    fn non_test_source() -> &'static str {
        let source = include_str!("lib.rs");
        &source[..source.find("\nmod tests {").expect("mod tests must exist")]
    }

    /// The source of the `#[cfg(target_arch = "wasm32")]` module, which a host test cannot
    /// compile but can read. Ends at `mod tests` so a needle below cannot match itself.
    fn wasm_module_source() -> &'static str {
        let source = non_test_source();
        &source[source
            .find("\nmod wasm_driver {")
            .expect("the wasm driver module must exist")..]
    }

    // ── The model table ───────────────────────────────────────────────────────

    #[test]
    fn the_table_holds_the_three_confirmed_entries_with_their_limits_and_effort_rules() {
        let effort = EffortRule::Effort {
            values: &["low", "medium", "high", "xhigh"],
            default: "high",
        };
        let rows: Vec<(&str, u64, u64, EffortRule)> = SUPPORTED_MODELS
            .iter()
            .map(|(id, limits, rule)| (*id, limits.max_output_tokens, limits.context_window, *rule))
            .collect();
        assert_eq!(
            rows,
            [
                ("grok-4.7", 128_000, 500_000, effort),
                ("grok-4.6", 128_000, 500_000, effort),
                ("grok-build-0.1", 128_000, 256_000, EffortRule::NoEffort),
            ]
        );
    }

    #[test]
    fn every_supported_model_builds_a_body_naming_it() {
        for (id, _, _) in SUPPORTED_MODELS {
            let body = body_for(&request_for(id, 8192), &DriverConfig::default());
            assert_eq!(body["model"], json!(id));
        }
    }

    #[test]
    fn an_unsupported_model_is_refused_by_exact_string_before_any_body() {
        for rejected in [
            "grok-4.5",
            "grok-code-fast-1",
            "Grok-4.7",
            "grok-4.7 ",
            "grok-4.20-multi-agent-0309",
            "gpt-5.6-luna",
        ] {
            let expected =
                format!("driver: model '{rejected}' is not supported by driver-xai. {SUPPORTED}");
            assert_eq!(validate_model(rejected).unwrap_err(), expected);
            assert_eq!(
                refusal(&request_for(rejected, 8192), &DriverConfig::default()),
                expected
            );
        }
    }

    #[test]
    fn manifest_models_cover_every_accepted_model() {
        use std::collections::BTreeMap;

        #[derive(serde::Deserialize)]
        struct Manifest {
            models: BTreeMap<String, Limits>,
        }
        // `deny_unknown_fields` with no `vision` member: the table declares none.
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
                limits.max_output_tokens >= 1 && limits.max_output_tokens < limits.context_window,
                "models.{id}: {limits:?} must leave an input budget below the window"
            );
            assert_eq!(
                limits.max_input_tokens, None,
                "models.{id}: xAI documents no input ceiling"
            );
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
                cell => Some(number(cell)),
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

        let accepted: Vec<&str> = SUPPORTED_MODELS.iter().map(|(id, _, _)| *id).collect();
        for (id, source, _) in SUPPORTED_MODELS {
            let limits = models
                .get(*id)
                .unwrap_or_else(|| panic!("murmur.yaml's models: block lacks '{id}'"));
            assert_eq!(
                (
                    u64::from(limits.max_output_tokens),
                    u64::from(limits.context_window)
                ),
                (source.max_output_tokens, source.context_window),
                "murmur.yaml's models.{id} disagrees with SUPPORTED_MODELS"
            );
        }
        for id in models.keys() {
            assert!(
                accepted.contains(&id.as_str()),
                "murmur.yaml's models: block declares '{id}', which is outside the accepted set"
            );
        }
        assert_eq!(
            documented, models,
            "README.md's Model limits table must equal murmur.yaml's models:"
        );
    }

    // ── The output cap ────────────────────────────────────────────────────────

    #[test]
    fn the_cap_is_sent_as_max_output_tokens_and_refused_above_the_ceiling() {
        let body = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(body["max_output_tokens"], json!(8192));

        for (id, _, _) in SUPPORTED_MODELS {
            let at_ceiling = body_for(&request_for(id, 128_000), &DriverConfig::default());
            assert_eq!(at_ceiling["max_output_tokens"], json!(128_000));
            assert_eq!(
                refusal(&request_for(id, 128_001), &DriverConfig::default()),
                format!(
                    "driver: inference.max_tokens 128001 exceeds driver-xai's max_output_tokens \
                     ceiling of 128000 for model '{id}'"
                )
            );
        }
    }

    #[test]
    fn no_body_carries_max_tokens_or_max_completion_tokens() {
        let request = request_from(json!({
            "model": "grok-4.6",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "params": {"max_tokens": 1, "max_completion_tokens": 2}
        }));
        for body in [
            body_for(&simple_request(), &DriverConfig::default()),
            body_for(&request, &DriverConfig::default()),
        ] {
            for key in ["max_tokens", "max_completion_tokens"] {
                assert!(!contains_key_anywhere(&body, key), "{key} in {body}");
            }
        }
    }

    // ── Config ────────────────────────────────────────────────────────────────

    #[test]
    fn absent_empty_and_whitespace_configs_all_parse_to_the_default() {
        for raw in [None, Some("{}"), Some("   "), Some("")] {
            assert_eq!(parse_driver_config(raw).unwrap(), DriverConfig::default());
        }
        assert!(parse_driver_config(Some("not json"))
            .unwrap_err()
            .starts_with("driver: inference.driver.config is not valid JSON: "));
        assert_eq!(
            parse_driver_config(Some("[]")).unwrap_err(),
            "driver: inference.driver.config must be a JSON object"
        );
    }

    #[test]
    fn unrecognised_config_keys_are_refused_by_name_with_the_accepted_set() {
        for key in ["thinking", "store", "replay_reasoning"] {
            let err = parse_driver_config(Some(&format!(r#"{{"{key}":true}}"#))).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "driver: unrecognised inference.driver.config key(s): {key}. Accepted keys: \
                     reasoning_effort, response_format"
                )
            );
        }
        let err = parse_driver_config(Some(r#"{"store":false,"thinking":"enabled"}"#)).unwrap_err();
        assert_eq!(
            err,
            "driver: unrecognised inference.driver.config key(s): store, thinking. Accepted \
             keys: reasoning_effort, response_format"
        );
    }

    #[test]
    fn the_effort_in_force_is_on_every_grok_4_body_and_absent_on_grok_build() {
        for id in ["grok-4.7", "grok-4.6"] {
            let body = body_for(&request_for(id, 8192), &DriverConfig::default());
            assert_eq!(body["reasoning"], json!({"effort": "high"}), "{id}");
            for (raw, sent) in [
                ("low", "low"),
                ("medium", "medium"),
                ("high", "high"),
                ("xhigh", "xhigh"),
                (" XHigh ", "xhigh"),
            ] {
                let body = body_for(
                    &request_for(id, 8192),
                    &config(&json!({"reasoning_effort": raw}).to_string()),
                );
                assert_eq!(body["reasoning"], json!({"effort": sent}), "{id} {raw}");
                assert!(body.get("reasoning_effort").is_none(), "{body}");
            }
        }
        let body = body_for(
            &request_for("grok-build-0.1", 8192),
            &DriverConfig::default(),
        );
        assert!(!contains_key_anywhere(&body, "reasoning"), "{body}");
        assert!(!contains_key_anywhere(&body, "effort"), "{body}");
    }

    #[test]
    fn an_effort_the_model_does_not_take_is_refused_naming_the_values_and_the_raw_value() {
        for id in ["grok-4.7", "grok-4.6"] {
            for raw in ["max", "none", "minimal", " Ultra "] {
                assert_eq!(
                    refusal(
                        &request_for(id, 8192),
                        &config(&json!({"reasoning_effort": raw}).to_string())
                    ),
                    format!(
                        "driver: inference.driver.config 'reasoning_effort' must be one of low, \
                         medium, high, xhigh for model '{id}', got \"{raw}\""
                    )
                );
            }
        }
    }

    #[test]
    fn any_effort_on_grok_build_is_refused() {
        for raw in ["high", "low", "max"] {
            assert_eq!(
                refusal(
                    &request_for("grok-build-0.1", 8192),
                    &config(&json!({"reasoning_effort": raw}).to_string())
                ),
                "driver: inference.driver.config 'reasoning_effort' is not accepted for model \
                 'grok-build-0.1': xAI documents no reasoning effort for it. Remove the key."
            );
        }
    }

    #[test]
    fn a_non_string_effort_is_refused() {
        for (raw, shown) in [("3", "3"), ("true", "true"), ("null", "null")] {
            assert_eq!(
                parse_driver_config(Some(&format!(r#"{{"reasoning_effort":{raw}}}"#))).unwrap_err(),
                format!(
                    "driver: inference.driver.config 'reasoning_effort' must be a string, got {shown}"
                )
            );
        }
    }

    const PLAN_FORMAT: &str = r#"{"response_format":{"type":"json_schema","json_schema":{"name":"plan","schema":{"type":"object","properties":{"steps":{"type":"array"}}}}}}"#;

    #[test]
    fn response_format_becomes_text_format_with_strict_true() {
        let body = body_for(&simple_request(), &config(PLAN_FORMAT));
        assert_eq!(
            body["text"],
            json!({"format": {
                "type": "json_schema",
                "name": "plan",
                "schema": {"type": "object", "properties": {"steps": {"type": "array"}}},
                "strict": true
            }})
        );
        assert!(!contains_key_anywhere(&body, "response_format"), "{body}");

        let strict = PLAN_FORMAT.replace(r#""name":"plan","#, r#""name":"plan","strict":true,"#);
        assert_eq!(
            body_for(&simple_request(), &config(&strict))["text"],
            body["text"]
        );

        let unset = body_for(&simple_request(), &DriverConfig::default());
        assert!(unset.get("text").is_none(), "{unset}");
    }

    #[test]
    fn a_response_format_xai_cannot_honour_is_refused_by_name() {
        let schema = r#"{"type":"object"}"#;
        for (raw, expected) in [
            (
                r#""text""#.to_string(),
                "driver: inference.driver.config 'response_format' must be a JSON object, got \"text\"",
            ),
            (
                r#"{"type":"json_object"}"#.to_string(),
                "driver: inference.driver.config 'response_format.type' must be \"json_schema\", got \"json_object\"",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"name":"p","schema":{schema}}},"extra":1}}"#),
                "driver: inference.driver.config 'response_format' has unrecognised member(s): extra. Accepted members: json_schema, type",
            ),
            (
                r#"{"type":"json_schema"}"#.to_string(),
                "driver: inference.driver.config 'response_format.json_schema' must be an object",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"name":"p","schema":{schema},"description":"d"}}}}"#),
                "driver: inference.driver.config 'response_format.json_schema' has unrecognised member(s): description. Accepted members: name, schema, strict",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"schema":{schema}}}}}"#),
                "driver: inference.driver.config 'response_format.json_schema.name' is required and must be a non-blank string",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"name":"  ","schema":{schema}}}}}"#),
                "driver: inference.driver.config 'response_format.json_schema.name' is required and must be a non-blank string",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"name":7,"schema":{schema}}}}}"#),
                "driver: inference.driver.config 'response_format.json_schema.name' is required and must be a non-blank string",
            ),
            (
                r#"{"type":"json_schema","json_schema":{"name":"p"}}"#.to_string(),
                "driver: inference.driver.config 'response_format.json_schema.schema' is required and must be an object",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"name":"p","schema":{schema},"strict":"yes"}}}}"#),
                "driver: inference.driver.config 'response_format.json_schema.strict' must be a boolean, got \"yes\"",
            ),
            (
                format!(r#"{{"type":"json_schema","json_schema":{{"name":"p","schema":{schema},"strict":false}}}}"#),
                super::STRICT_FALSE_REFUSAL,
            ),
        ] {
            let err = parse_driver_config(Some(&format!(r#"{{"response_format":{raw}}}"#)))
                .unwrap_err();
            assert_eq!(err, expected, "{raw}");
        }
    }

    // ── Preflight order ───────────────────────────────────────────────────────

    #[test]
    fn preflight_checks_the_model_then_the_cap_then_the_effort_then_images() {
        let image_request = |model: &str, max_tokens: u64| {
            request_from(json!({
                "model": model,
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]}]
            }))
        };
        let effort = config(r#"{"reasoning_effort":"high"}"#);

        assert!(refusal(&image_request("grok-9", 999_999), &effort)
            .starts_with("driver: model 'grok-9' is not supported"));
        assert!(refusal(&image_request("grok-build-0.1", 999_999), &effort)
            .starts_with("driver: inference.max_tokens 999999 exceeds"));
        assert!(refusal(&image_request("grok-build-0.1", 8192), &effort)
            .starts_with("driver: inference.driver.config 'reasoning_effort' is not accepted"));
        assert_eq!(
            refusal(
                &image_request("grok-build-0.1", 8192),
                &DriverConfig::default()
            ),
            "driver: driver-xai sends text only and refuses an image block"
        );
    }

    #[test]
    fn an_image_block_anywhere_is_refused() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}});
        for messages in [
            json!([{"role": "user", "content": [image.clone()]}]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "shoot"}]},
                {"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "screenshot", "input": {}}]},
                {"role": "tool", "tool_call_id": "c1", "content": [{"type": "text", "text": "here"}, image.clone()]}
            ]),
        ] {
            let request = request_from(
                json!({"model": "grok-4.7", "max_tokens": 8192, "messages": messages}),
            );
            assert_eq!(
                refusal(&request, &DriverConfig::default()),
                "driver: driver-xai sends text only and refuses an image block"
            );
        }
    }

    // ── The body ──────────────────────────────────────────────────────────────

    #[test]
    fn the_body_carries_exactly_the_documented_members() {
        let body = body_for(&simple_request(), &DriverConfig::default());
        assert_eq!(
            body,
            json!({
                "model": "grok-4.7",
                "input": [{"role": "user", "content": [{"type": "input_text", "text": "Why is the sky blue?"}]}],
                "instructions": "You are a careful assistant.",
                "max_output_tokens": 8192,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "stream": true,
                "reasoning": {"effort": "high"},
                "prompt_cache_key": "xai-proof:0.1.0:task-1"
            })
        );
    }

    #[test]
    fn a_blank_system_prompt_sends_no_instructions() {
        for system in [json!(null), json!(""), json!("   \n")] {
            let request = request_from(json!({
                "model": "grok-4.7", "max_tokens": 64, "system": system,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
            }));
            let body = body_for(&request, &DriverConfig::default());
            assert!(body.get("instructions").is_none(), "{body}");
        }
    }

    #[test]
    fn every_body_is_stateless_whatever_the_request_says() {
        let params = json!({
            "store": true,
            "include": [],
            "stream": false,
            "previous_response_id": "resp_123",
            "background": true,
            "reasoning": {"effort": "low"},
            "reasoning_effort": "low",
            "frequency_penalty": 1,
            "presence_penalty": 1,
            "stop": ["\n"],
            "search_parameters": {"mode": "on"},
            "seed": 7
        });
        for model in ["grok-4.7", "grok-4.6", "grok-build-0.1"] {
            let request = request_from(json!({
                "model": model, "max_tokens": 64,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "params": params,
                "previous_response_id": "resp_456",
                "continuation_id": "resp_789"
            }));
            let body = body_for(&request, &DriverConfig::default());
            assert_eq!(body["store"], json!(false), "{body}");
            assert_eq!(
                body["include"],
                json!(["reasoning.encrypted_content"]),
                "{body}"
            );
            assert_eq!(body["stream"], json!(true), "{body}");
            assert_eq!(
                body["seed"],
                json!(7),
                "an unlisted param passes through: {body}"
            );
            for refused in PROVIDER_REFUSED_PARAMS {
                if *refused == "reasoning" && model != "grok-build-0.1" {
                    assert_eq!(body["reasoning"], json!({"effort": "high"}), "{body}");
                    continue;
                }
                assert!(
                    !contains_key_anywhere(&body, refused),
                    "{refused} in {body}"
                );
            }
        }
        for body in [
            body_for(&simple_request(), &DriverConfig::default()),
            body_for(&simple_request(), &config(PLAN_FORMAT)),
        ] {
            assert!(!contains_key_anywhere(&body, "previous_response_id"));
            assert_eq!(body["store"], json!(false));
        }
    }

    #[test]
    fn a_param_never_overrides_a_member_the_driver_sets() {
        let request = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "params": {"model": "grok-4.5", "max_output_tokens": 999_999, "input": "x"}
        }));
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(body["model"], json!("grok-4.7"));
        assert_eq!(body["max_output_tokens"], json!(64));
        assert!(body["input"].is_array());
    }

    #[test]
    fn tools_go_out_as_responses_functions_without_strict_or_choice() {
        let request = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [
                {"name": "read", "description": "  Read a file.  ", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}},
                {"name": "noop", "description": "   ", "parameters": {"type": "object"}},
                {"name": "bare"}
            ]
        }));
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(
            body["tools"],
            json!([
                {"type": "function", "name": "read", "description": "Read a file.",
                 "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}},
                {"type": "function", "name": "noop", "parameters": {"type": "object"}},
                {"type": "function", "name": "bare", "parameters": {}}
            ])
        );
        for absent in ["strict", "tool_choice", "parallel_tool_calls"] {
            assert!(!contains_key_anywhere(&body, absent), "{absent} in {body}");
        }
    }

    #[test]
    fn a_no_tools_request_sends_no_tools_member() {
        // hook-compact asks for a summary with no tools.
        let body = body_for(&simple_request(), &DriverConfig::default());
        assert!(body.get("tools").is_none(), "{body}");
        let response = parse(&format!(
            "{}{}{}",
            text_delta("Summary: "),
            text_delta("the user asked about the sky."),
            completed()
        ))
        .unwrap();
        assert_eq!(
            response,
            json!({"stop_reason": "end_turn", "content": [
                {"type": "text", "text": "Summary: the user asked about the sky."}
            ]})
        );
    }

    #[test]
    fn the_prompt_cache_key_is_forwarded_only_when_it_is_a_non_blank_string() {
        let with_key = |key: Value| {
            let request = request_from(json!({
                "model": "grok-4.7", "max_tokens": 64,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "prompt_cache_key": key
            }));
            body_for(&request, &DriverConfig::default())
        };
        assert_eq!(
            with_key(json!(" cap:0.1.0:t ")).get("prompt_cache_key"),
            Some(&json!("cap:0.1.0:t"))
        );
        for no_key in [
            json!(null),
            json!(""),
            json!("  "),
            json!(7),
            json!({"k": "v"}),
        ] {
            assert!(
                with_key(no_key.clone()).get("prompt_cache_key").is_none(),
                "{no_key}"
            );
        }
        assert!(
            body_for(&request_for("grok-4.7", 64), &DriverConfig::default())
                .get("prompt_cache_key")
                .is_none()
        );
    }

    #[test]
    fn each_role_becomes_its_input_item() {
        let request = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading it."},
                    {"type": "tool_call", "id": "call_1", "name": "read", "input": {"path": "x"}},
                    {"type": "tool_call", "id": "call_2", "name": "read", "input": {"path": "y"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "X"}]},
                {"role": "tool", "tool_call_id": "call_2", "content": [{"type": "text", "text": "Y"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
            ]
        }));
        let body = body_for(&request, &DriverConfig::default());
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "a\nb"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading it."}]},
                {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"x\"}"},
                {"type": "function_call", "call_id": "call_2", "name": "read", "arguments": "{\"path\":\"y\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "X"},
                {"type": "function_call_output", "call_id": "call_2", "output": "Y"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}
            ])
        );
    }

    #[test]
    fn a_tool_turn_without_its_call_id_and_an_unknown_role_are_refused_by_name() {
        let tool = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [{"role": "tool", "content": [{"type": "text", "text": "X"}]}]
        }));
        assert_eq!(
            refusal(&tool, &DriverConfig::default()),
            "driver: tool message is missing required field 'tool_call_id'"
        );
        let system = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [{"role": "system", "content": [{"type": "text", "text": "X"}]}]
        }));
        assert_eq!(
            refusal(&system, &DriverConfig::default()),
            "driver: unsupported message role 'system'"
        );
    }

    // ── Encrypted reasoning, kept and sent back ───────────────────────────────

    /// Turn 1 of a tool round trip: two summary deltas, the reasoning item, one function call
    /// whose arguments arrive in two deltas, then `response.completed` with usage.
    fn tool_turn_stream() -> String {
        let item = reasoning_item("rs_1", &["I should read the file."], ENCRYPTED);
        [
            item_added(0, json!({"id": "rs_1", "type": "reasoning", "summary": []})),
            summary_delta(0, "I should read "),
            summary_delta(0, "the file."),
            item_done(0, item),
            function_call_added(1, "call_abc", "read"),
            arguments_delta(1, "{\"path\":"),
            arguments_delta(1, "\"README.md\"}"),
            arguments_done(1, "{\"path\":\"README.md\"}"),
            item_done(
                1,
                json!({"type": "function_call", "id": "fc_1", "call_id": "call_abc",
                       "name": "read", "arguments": "{\"path\":\"README.md\"}", "status": "completed"}),
            ),
            completed_with_usage(reported(32, 8, 9, 110, 151)),
        ]
        .concat()
    }

    #[test]
    fn a_tool_round_trip_sends_the_reasoning_item_back_verbatim() {
        let mut thinking = Vec::new();
        let turn = parse_xai_sse_body(&tool_turn_stream(), &mut |_: &str| {}, &mut |t: &str| {
            thinking.push(t.to_string())
        })
        .unwrap();
        assert_eq!(thinking, ["I should read ", "the file."]);
        let item = reasoning_item("rs_1", &["I should read the file."], ENCRYPTED);
        assert_eq!(
            turn,
            json!({
                "stop_reason": "tool_call",
                "content": [
                    {"type": "thinking", "text": "I should read the file.", "xai_reasoning": item},
                    {"type": "tool_call", "id": "call_abc", "name": "read", "input": {"path": "README.md"}}
                ],
                "usage": {"input_tokens": 24, "cached_tokens": 8, "output_tokens": 119, "thinking_tokens": 110}
            })
        );

        // murmur stores the content as returned and sends it back on the next request.
        let next = request_from(json!({
            "model": "grok-4.7", "max_tokens": 8192,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read the README."}]},
                {"role": "assistant", "content": turn["content"]},
                {"role": "tool", "tool_call_id": "call_abc", "content": [{"type": "text", "text": "# Title"}]}
            ]
        }));
        let body = body_for(&next, &DriverConfig::default());
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "Read the README."}]},
                item,
                {"type": "function_call", "call_id": "call_abc", "name": "read", "arguments": "{\"path\":\"README.md\"}"},
                {"type": "function_call_output", "call_id": "call_abc", "output": "# Title"}
            ])
        );
        assert_eq!(
            serde_json::to_string(&body["input"][1]).unwrap(),
            serde_json::to_string(&turn["content"][0]["xai_reasoning"]).unwrap(),
            "byte for byte"
        );
        assert!(!contains_key_anywhere(&body, "previous_response_id"));
    }

    #[test]
    fn two_reasoning_items_come_back_in_output_order() {
        let first = reasoning_item("rs_a", &["First."], "enc-a");
        let second = reasoning_item("rs_b", &[], "enc-b");
        let body = [
            item_done(0, first.clone()),
            text_delta("Middle."),
            item_done(2, second.clone()),
            completed(),
        ]
        .concat();
        let turn = parse(&body).unwrap();
        assert_eq!(
            turn["content"],
            json!([
                {"type": "thinking", "text": "First.", "xai_reasoning": first},
                {"type": "thinking", "text": "", "xai_reasoning": second},
                {"type": "text", "text": "Middle."}
            ])
        );
        let next = request_from(json!({
            "model": "grok-4.6", "max_tokens": 64,
            "messages": [{"role": "assistant", "content": turn["content"]}]
        }));
        let input = &body_for(&next, &DriverConfig::default())["input"];
        assert_eq!(input[0], first);
        assert_eq!(input[1], second);
        assert_eq!(input[2]["type"], "message");
    }

    #[test]
    fn a_reasoning_item_with_no_summary_is_kept_with_empty_text_and_replayed() {
        let item = reasoning_item("rs_quiet", &[], ENCRYPTED);
        let mut thinking = Vec::new();
        let turn = parse_xai_sse_body(
            &[item_done(0, item.clone()), text_delta("ok"), completed()].concat(),
            &mut |_: &str| {},
            &mut |t: &str| thinking.push(t.to_string()),
        )
        .unwrap();
        assert!(thinking.is_empty(), "nothing to show: {thinking:?}");
        assert_eq!(
            turn["content"][0],
            json!({"type": "thinking", "text": "", "xai_reasoning": item})
        );
        let next = request_from(json!({
            "model": "grok-4.6", "max_tokens": 64,
            "messages": [{"role": "assistant", "content": turn["content"]}]
        }));
        assert_eq!(body_for(&next, &DriverConfig::default())["input"][0], item);
    }

    #[test]
    fn a_summary_that_only_arrives_on_the_item_becomes_the_thinking_text_and_is_emitted() {
        let item = reasoning_item("rs_s", &["Part one.", "Part two."], ENCRYPTED);
        let mut thinking = Vec::new();
        let turn = parse_xai_sse_body(
            &[item_done(0, item.clone()), completed()].concat(),
            &mut |_: &str| {},
            &mut |t: &str| thinking.push(t.to_string()),
        )
        .unwrap();
        assert_eq!(thinking, ["Part one.\n\nPart two."]);
        assert_eq!(turn["content"][0]["text"], "Part one.\n\nPart two.");
    }

    #[test]
    fn reasoning_text_deltas_stream_as_thinking_too() {
        let body = [
            event(json!({"type": "response.reasoning_text.delta", "output_index": 0, "delta": "raw "})),
            event(json!({"type": "response.reasoning_text.delta", "output_index": 0, "delta": ""})),
            event(json!({"type": "response.reasoning_text.delta", "output_index": 0, "delta": "trace"})),
            item_done(0, reasoning_item("rs_r", &["ignored, deltas streamed"], ENCRYPTED)),
            completed(),
        ]
        .concat();
        let mut thinking = Vec::new();
        let turn = parse_xai_sse_body(&body, &mut |_: &str| {}, &mut |t: &str| {
            thinking.push(t.to_string())
        })
        .unwrap();
        assert_eq!(thinking, ["raw ", "trace"]);
        assert_eq!(turn["content"][0]["text"], "raw trace");
    }

    #[test]
    fn the_reasoning_item_is_taken_from_the_terminal_event_when_no_item_done_carried_it() {
        let item = reasoning_item("rs_t", &["From the end."], ENCRYPTED);
        let body = [
            summary_delta(0, "Streamed."),
            text_delta("Answer."),
            terminal(
                "completed",
                json!({"status": "completed", "output": [
                    item.clone(),
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Answer."}]}
                ]}),
            ),
        ]
        .concat();
        let turn = parse(&body).unwrap();
        assert_eq!(
            turn["content"],
            json!([
                {"type": "thinking", "text": "Streamed.", "xai_reasoning": item},
                {"type": "text", "text": "Answer."}
            ])
        );

        // An item already taken from `output_item.done` is not duplicated by the terminal one.
        let body = [
            item_done(0, item.clone()),
            terminal(
                "completed",
                json!({"status": "completed", "output": [item.clone()]}),
            ),
        ]
        .concat();
        assert_eq!(
            parse(&body).unwrap()["content"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn a_thinking_block_without_a_replayable_item_sends_nothing_back() {
        for xai_reasoning in [
            json!(null),
            json!("a string"),
            json!({"type": "reasoning", "id": "rs_x"}),
            json!({"type": "reasoning", "encrypted_content": ""}),
            json!({"type": "reasoning", "encrypted_content": 7}),
            json!({"type": "message", "encrypted_content": ENCRYPTED}),
        ] {
            let request = request_from(json!({
                "model": "grok-4.7", "max_tokens": 64,
                "messages": [{"role": "assistant", "content": [
                    {"type": "thinking", "text": "t", "xai_reasoning": xai_reasoning},
                    {"type": "text", "text": "Hi."}
                ]}]
            }));
            let input = body_for(&request, &DriverConfig::default())["input"].clone();
            assert_eq!(
                input,
                json!([{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi."}]}]),
                "{xai_reasoning}"
            );
        }
        // A plain thinking block, such as a compacted or older history might hold.
        let request = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "text": "older reasoning"},
                {"type": "thinking"},
                {"type": "tool_call", "id": "c", "name": "n", "input": {}}
            ]}]
        }));
        assert_eq!(
            body_for(&request, &DriverConfig::default())["input"],
            json!([{"type": "function_call", "call_id": "c", "name": "n", "arguments": "{}"}])
        );
    }

    // ── Streaming ─────────────────────────────────────────────────────────────

    #[test]
    fn text_and_thinking_stream_separately_and_empty_deltas_are_dropped() {
        let body = [
            summary_delta(0, "Think."),
            summary_delta(0, ""),
            text_delta("Hel"),
            text_delta(""),
            text_delta("lo."),
            completed(),
        ]
        .concat();
        let (mut texts, mut thoughts) = (Vec::new(), Vec::new());
        let turn = parse_xai_sse_body(
            &body,
            &mut |t: &str| texts.push(t.to_string()),
            &mut |t: &str| thoughts.push(t.to_string()),
        )
        .unwrap();
        assert_eq!(texts, ["Hel", "lo."]);
        assert_eq!(thoughts, ["Think."]);
        assert_eq!(
            turn["content"],
            json!([{"type": "thinking", "text": "Think."}, {"type": "text", "text": "Hello."}])
        );
    }

    #[test]
    fn done_ends_the_stream_and_data_without_a_space_is_read() {
        let body = format!(
            "data:{}\n{}data: [DONE]\n{}",
            json!({"type": "response.output_text.delta", "delta": "a"}),
            completed(),
            text_delta("never read")
        );
        assert_eq!(
            parse(&body).unwrap()["content"],
            json!([{"type": "text", "text": "a"}])
        );
    }

    #[test]
    fn a_tool_call_signals_its_start_then_rising_byte_totals_and_no_argument_text() {
        let (turn, signals) = parse_with_signals(&tool_turn_stream());
        assert_eq!(
            signals,
            [
                started("call_abc", "read"),
                input_bytes("call_abc", 8),
                input_bytes("call_abc", 20),
            ]
        );
        assert_eq!(turn["stop_reason"], "tool_call");
    }

    #[test]
    fn arguments_done_is_authoritative_and_output_item_done_fills_what_is_missing() {
        let body = [
            item_added(3, json!({"type": "function_call", "name": "bash"})),
            arguments_delta(3, "{\"cmd\":\"l"),
            arguments_done(3, "{\"cmd\":\"ls -la\"}"),
            item_done(
                3,
                json!({"type": "function_call", "call_id": "call_late", "name": "other",
                       "arguments": "{\"ignored\":true}"}),
            ),
            completed(),
        ]
        .concat();
        let (turn, signals) = parse_with_signals(&body);
        assert_eq!(
            turn["content"],
            json!([{"type": "tool_call", "id": "call_late", "name": "bash", "input": {"cmd": "ls -la"}}])
        );
        assert_eq!(
            signals,
            [started("call_late", "bash"), input_bytes("call_late", 16)]
        );

        // A call announced only by its finished item.
        let body = [
            item_done(
                0,
                json!({"type": "function_call", "call_id": "c0", "name": "probe", "arguments": "{}"}),
            ),
            completed(),
        ]
        .concat();
        let (turn, signals) = parse_with_signals(&body);
        assert_eq!(
            turn["content"],
            json!([{"type": "tool_call", "id": "c0", "name": "probe", "input": {}}])
        );
        assert_eq!(signals, [started("c0", "probe"), input_bytes("c0", 2)]);
    }

    #[test]
    fn two_calls_keep_separate_ids_and_totals() {
        let body = [
            function_call_added(0, "c_a", "read"),
            function_call_added(1, "c_b", "write"),
            arguments_delta(1, "{}"),
            arguments_delta(0, "{\"p\":1}"),
            completed(),
        ]
        .concat();
        let (turn, signals) = parse_with_signals(&body);
        assert_eq!(
            signals,
            [
                started("c_a", "read"),
                started("c_b", "write"),
                input_bytes("c_b", 2),
                input_bytes("c_a", 7),
            ]
        );
        assert_eq!(turn["content"][0]["id"], "c_a");
        assert_eq!(turn["content"][1]["id"], "c_b");
    }

    #[test]
    fn an_output_index_past_the_limit_fails_the_turn_and_the_last_one_under_it_is_kept() {
        for event in [
            function_call_added(1024, "c", "n"),
            arguments_delta(1024, "{}"),
            arguments_done(1024, "{}"),
            item_done(
                1024,
                json!({"type": "function_call", "call_id": "c", "name": "n"}),
            ),
        ] {
            let err = parse(&format!("{event}{}", completed())).unwrap_err();
            assert_eq!(
                err,
                Failure::plain("driver: tool call index 1024 is past the limit of 1024 per turn")
            );
        }
        let err = parse(&function_call_added(4_294_967_296, "c", "n")).unwrap_err();
        assert_eq!(
            err,
            Failure::plain("driver: tool call index 4294967296 is past the limit of 1024 per turn")
        );
        let turn = parse(&[function_call_added(1023, "c", "n"), completed()].concat()).unwrap();
        assert_eq!(
            turn["content"],
            json!([{"type": "tool_call", "id": "c", "name": "n", "input": {}}])
        );
    }

    #[test]
    fn text_written_beside_tool_calls_is_kept_and_replayed() {
        let body = [
            item_done(0, reasoning_item("rs", &["R."], ENCRYPTED)),
            text_delta("Let me look."),
            function_call_added(2, "c1", "read"),
            arguments_done(2, "{}"),
            completed(),
        ]
        .concat();
        let turn = parse(&body).unwrap();
        let kinds: Vec<&str> = turn["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["thinking", "text", "tool_call"]);

        let next = request_from(json!({
            "model": "grok-4.7", "max_tokens": 64,
            "messages": [{"role": "assistant", "content": turn["content"]}]
        }));
        let types: Vec<String> = body_for(&next, &DriverConfig::default())["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(types, ["reasoning", "message", "function_call"]);
    }

    // ── Stop reasons ──────────────────────────────────────────────────────────

    #[test]
    fn an_incomplete_turn_at_the_cap_is_max_tokens() {
        let body = [
            text_delta("Cut"),
            terminal(
                "incomplete",
                json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}),
            ),
        ]
        .concat();
        let turn = parse(&body).unwrap();
        assert_eq!(turn["stop_reason"], "max_tokens");
        assert_eq!(turn["content"], json!([{"type": "text", "text": "Cut"}]));
    }

    #[test]
    fn a_tool_call_cut_at_the_cap_names_the_tool_and_the_cap() {
        let body = [
            function_call_added(0, "c1", "write_file"),
            arguments_delta(0, "{\"content\":\"lorem ip"),
            terminal(
                "incomplete",
                json!({"incomplete_details": {"reason": "max_output_tokens"}}),
            ),
        ]
        .concat();
        let err = parse(&body).unwrap_err();
        assert_eq!(
            err,
            Failure::plain(
                "driver: xAI Responses turn stopped at the inference.max_tokens output cap; tool \
                 call 'write_file' was cut off mid-arguments and cannot be run — raise the cap \
                 and re-run"
            )
        );
        assert!(!err.message.contains("lorem"));
    }

    #[test]
    fn other_endings_are_plain_failures_naming_what_arrived() {
        for (body, message) in [
            (
                terminal(
                    "incomplete",
                    json!({"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}),
                ),
                "driver: xAI Responses turn ended incomplete: content_filter",
            ),
            (
                terminal("incomplete", json!({"status": "incomplete"})),
                "driver: xAI Responses turn ended incomplete: no reason given",
            ),
            (
                terminal("failed", json!({"status": "failed", "error": null})),
                "driver: xAI Responses turn failed with no error",
            ),
            (
                terminal("completed", json!({"status": "queued"})),
                "driver: unsupported xAI Responses status 'queued'",
            ),
        ] {
            assert_eq!(parse(&body), Err(Failure::plain(message)), "{body}");
        }
        let bad = [
            function_call_added(0, "c1", "bash"),
            arguments_done(0, "{not json"),
            completed(),
        ]
        .concat();
        let err = parse(&bad).unwrap_err();
        assert!(
            err.message.starts_with(
                "driver: failed to parse xAI function_call arguments JSON for tool 'bash': "
            ),
            "{err}"
        );
        assert_eq!(err.retry, None);
    }

    #[test]
    fn the_status_falls_back_to_the_terminal_event_name() {
        assert_eq!(
            parse(&[text_delta("x"), terminal("completed", json!({}))].concat()).unwrap()
                ["stop_reason"],
            "end_turn"
        );
        assert_eq!(
            parse(&terminal("failed", json!({}))),
            Err(Failure::plain(
                "driver: xAI Responses turn failed with no error"
            ))
        );
    }

    // ── The buffered JSON path ────────────────────────────────────────────────

    #[test]
    fn a_buffered_response_translates_like_the_stream() {
        let item = reasoning_item("rs_1", &["Read it."], ENCRYPTED);
        let response = json!({
            "id": "resp_1", "object": "response", "status": "completed", "store": false,
            "output": [
                item,
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading."}]},
                {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"}
            ],
            "usage": reported(125, 98, 48, 0, 173)
        });
        assert_eq!(
            translate_xai_response_to_murmur(&response).unwrap(),
            json!({
                "stop_reason": "tool_call",
                "content": [
                    {"type": "thinking", "text": "Read it.", "xai_reasoning": item},
                    {"type": "text", "text": "Reading."},
                    {"type": "tool_call", "id": "call_1", "name": "read", "input": {"path": "a"}}
                ],
                "usage": {"input_tokens": 27, "cached_tokens": 98, "output_tokens": 48, "thinking_tokens": 0}
            })
        );
    }

    #[test]
    fn a_buffered_response_without_a_status_or_with_an_error_fails() {
        assert_eq!(
            translate_xai_response_to_murmur(&json!({"output": []})),
            Err(Failure::plain(
                "driver: xAI Responses response has no status"
            ))
        );
        assert_eq!(
            translate_xai_response_to_murmur(
                &json!({"status": "failed", "error": {"code": "server_error", "message": "boom"}})
            ),
            Err(Failure::transient("xAI error: server_error: boom"))
        );
    }

    // ── Usage ─────────────────────────────────────────────────────────────────

    #[test]
    fn the_reference_example_counts_reasoning_in_the_completion() {
        assert_eq!(
            usage_of(reported(32, 8, 9, 110, 151)),
            Some(
                json!({"input_tokens": 24, "cached_tokens": 8, "output_tokens": 119, "thinking_tokens": 110})
            )
        );
    }

    #[test]
    fn the_caching_guide_example_keeps_output_as_reported() {
        assert_eq!(
            usage_of(reported(125, 98, 48, 0, 173)),
            Some(
                json!({"input_tokens": 27, "cached_tokens": 98, "output_tokens": 48, "thinking_tokens": 0})
            )
        );
    }

    #[test]
    fn no_total_gives_output_as_reported() {
        assert_eq!(
            usage_of(json!({"input_tokens": 50, "output_tokens": 30,
                            "output_tokens_details": {"reasoning_tokens": 20}})),
            Some(json!({"input_tokens": 50, "output_tokens": 30, "thinking_tokens": 20}))
        );
        // Reasoning beside an output with no total cannot be placed inside it, so it is dropped.
        assert_eq!(
            usage_of(
                json!({"output_tokens": 9, "output_tokens_details": {"reasoning_tokens": 110}})
            ),
            Some(json!({"output_tokens": 9}))
        );
        // A total below the input is inconsistent; output falls back to the reported member.
        assert_eq!(
            usage_of(json!({"input_tokens": 50, "output_tokens": 5, "total_tokens": 10})),
            Some(json!({"input_tokens": 50, "output_tokens": 5}))
        );
    }

    #[test]
    fn cached_above_the_input_omits_input_tokens() {
        assert_eq!(
            usage_of(
                json!({"input_tokens": 5, "input_tokens_details": {"cached_tokens": 8},
                            "output_tokens": 3})
            ),
            Some(json!({"cached_tokens": 8, "output_tokens": 3}))
        );
    }

    #[test]
    fn no_usage_gives_no_usage_key_and_cost_is_never_reported() {
        let turn = parse(&[text_delta("x"), completed()].concat()).unwrap();
        assert!(turn.get("usage").is_none(), "{turn}");
        assert_eq!(usage_of(json!({"cost_in_usd_ticks": 5})), None);
        assert_eq!(UsageTokens::default().to_value(), None);
        let usage = usage_of(reported(32, 8, 9, 110, 151)).unwrap();
        assert!(usage.get("cost_in_usd_ticks").is_none());
    }

    #[test]
    fn reported_zeros_are_kept() {
        assert_eq!(
            usage_of(reported(10, 0, 0, 0, 10)),
            Some(
                json!({"input_tokens": 10, "cached_tokens": 0, "output_tokens": 0, "thinking_tokens": 0})
            )
        );
    }

    // ── Failures ──────────────────────────────────────────────────────────────

    #[test]
    fn a_stream_with_no_terminal_event_is_transient() {
        for body in [
            text_delta("Hel"),
            summary_delta(0, "Let me"),
            [
                function_call_added(0, "c", "bash"),
                arguments_delta(0, "{\"cmd\":\"l"),
            ]
            .concat(),
            ": keep-alive\nevent: response.output_text.delta\nid: 7\nretry: 1000\n\n".to_string(),
            format!("{}data: {{\"type\":\"response.outp", text_delta("x")),
            String::new(),
        ] {
            assert_eq!(parse(&body), Err(Failure::transient(CUT)), "{body:?}");
        }
    }

    #[test]
    fn a_malformed_body_with_no_terminal_event_stays_plain() {
        for body in [
            "<html>\n<body>502 Bad Gateway</body>\n</html>\n".to_string(),
            format!("{}data: {{not json\n", text_delta("x")),
        ] {
            assert_eq!(parse(&body), Err(Failure::plain(CUT)), "{body:?}");
        }
    }

    #[test]
    fn each_transient_error_code_or_type_is_transient() {
        for name in [
            "overloaded",
            "rate_limit_error",
            "rate_limit_exceeded",
            "server_error",
            "service_unavailable",
        ] {
            let by_code = event(json!({"type": "error", "code": name, "message": "try later"}));
            assert_eq!(
                parse(&format!("{}{by_code}", text_delta("partial"))),
                Err(Failure::transient(format!("xAI error: {name}: try later")))
            );
            let by_type = event(json!({"type": "error", "error": {"type": name, "message": "m"}}));
            assert_eq!(
                parse(&by_type),
                Err(Failure::transient(format!("xAI error: {name}: m")))
            );
        }
    }

    #[test]
    fn other_provider_errors_stay_plain_with_the_provider_message() {
        for (error, message) in [
            (
                json!({"type": "error", "code": "invalid_request_error", "message": "bad"}),
                "xAI error: invalid_request_error: bad",
            ),
            (
                json!({"type": "error", "message": "no code"}),
                "xAI error: unknown: no code",
            ),
            (
                json!({"type": "error", "code": "brand_new"}),
                "xAI error: brand_new: unknown",
            ),
            (
                json!({"type": "error", "error": {"code": "context_length_exceeded", "type": "invalid_request_error", "message": "too long"}}),
                "xAI error: context_length_exceeded: too long",
            ),
        ] {
            assert_eq!(
                parse(&event(error.clone())),
                Err(Failure::plain(message)),
                "{error}"
            );
        }
    }

    #[test]
    fn a_terminal_error_is_the_provider_failure() {
        let body = terminal(
            "failed",
            json!({"status": "failed", "error": {"code": "server_error", "message": "internal"}}),
        );
        assert_eq!(
            parse(&body),
            Err(Failure::transient("xAI error: server_error: internal"))
        );
        let body = terminal(
            "failed",
            json!({"status": "failed", "error": {"code": "invalid_prompt", "message": "nope"}}),
        );
        assert_eq!(
            parse(&body),
            Err(Failure::plain("xAI error: invalid_prompt: nope"))
        );
    }

    #[test]
    fn a_provider_error_outranks_a_missing_or_later_terminal_event() {
        let error = event(json!({"type": "error", "code": "overloaded", "message": "busy"}));
        assert_eq!(
            parse(&format!("{}{error}", text_delta("x"))),
            Err(Failure::transient("xAI error: overloaded: busy"))
        );
        assert_eq!(
            parse(&format!("{error}{}", completed())),
            Err(Failure::transient("xAI error: overloaded: busy"))
        );
        assert_eq!(
            parse(&format!("{error}<html>\n")),
            Err(Failure::transient("xAI error: overloaded: busy"))
        );
    }

    #[test]
    fn every_status_is_plain_and_left_to_murmur() {
        for (status, body) in [
            (
                400,
                r#"{"code":"Client specified an invalid argument","error":"bad"}"#,
            ),
            (401, "unauthorized"),
            (403, "forbidden"),
            (404, r#"{"error":"model not found"}"#),
            (413, "too large"),
            (422, "unprocessable"),
            (429, r#"{"error":"rate limit"}"#),
            (429, r#"{"error":{"type":"insufficient_quota"}}"#),
            (500, "internal"),
            (502, "<html>bad gateway</html>"),
            (503, ""),
            (504, "timeout"),
        ] {
            assert_eq!(
                status_failure(status, body),
                Failure::plain(format!("HTTP {status}: {body}")),
                "status {status}"
            );
        }
    }

    #[test]
    fn a_failure_renders_the_error_payload_plus_at_most_one_transient_tuple() {
        let (data, metadata) = failure_result(&Failure::transient(CUT));
        assert_eq!(data, json!({"stop_reason": "error", "error": CUT}));
        assert_eq!(
            metadata,
            vec![(TRANSIENT_METADATA_KEY.to_string(), "true".to_string())]
        );
        let (data, metadata) = failure_result(&status_failure(429, "slow down"));
        assert_eq!(
            data,
            json!({"stop_reason": "error", "error": "HTTP 429: slow down"})
        );
        assert!(metadata.is_empty());
    }

    #[test]
    fn every_refusal_opens_with_driver() {
        let cases: Vec<String> = vec![
            refusal(&request_for("grok-4.5", 1), &DriverConfig::default()),
            refusal(&request_for("grok-4.7", 200_000), &DriverConfig::default()),
            refusal(
                &request_for("grok-4.7", 1),
                &config(r#"{"reasoning_effort":"max"}"#),
            ),
            refusal(
                &request_for("grok-build-0.1", 1),
                &config(r#"{"reasoning_effort":"low"}"#),
            ),
            parse_driver_config(Some(r#"{"thinking":"on"}"#)).unwrap_err(),
            parse_driver_config(Some("nope")).unwrap_err(),
            parse_driver_config(Some(r#"{"reasoning_effort":1}"#)).unwrap_err(),
            parse_driver_config(Some(r#"{"response_format":{}}"#)).unwrap_err(),
            parse(&text_delta("x")).unwrap_err().message,
            parse(&function_call_added(2048, "c", "n"))
                .unwrap_err()
                .message,
        ];
        for message in cases {
            assert!(message.starts_with("driver: "), "{message}");
        }
    }

    // ── Source contracts ──────────────────────────────────────────────────────

    #[test]
    fn no_provider_url_is_hardcoded_anywhere_in_the_crate() {
        // Where inference goes is the host's fact. The needles are assembled rather than
        // written out so that this assertion does not itself become an occurrence.
        let source = non_test_source();
        for needle in [
            concat!("x", ".ai"),
            concat!("https://", "api."),
            concat!("api", ".x"),
        ] {
            assert!(
                !source.contains(needle),
                "the driver must not name a provider URL ({needle}); the endpoint comes from the host"
            );
        }
    }

    #[test]
    fn run_inner_resolves_the_endpoint_through_the_shared_contract() {
        let module = wasm_module_source();
        let run_inner = &module[module.find("fn run_inner(").expect("run_inner must exist")..];
        assert!(
            run_inner.contains(concat!("require_", "endpoint(")),
            "run_inner must resolve the endpoint through murmur_driver_env::require_endpoint"
        );
        assert!(
            run_inner.contains(concat!("\"{}/", "responses\"")),
            "requests go to <endpoint>/responses"
        );
    }

    #[test]
    fn config_parse_precedes_every_other_environment_read_in_run_inner() {
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
    fn the_driver_neither_reads_a_credential_nor_builds_an_auth_header() {
        let non_test = non_test_source().to_lowercase();
        for needle in [
            concat!("murmur_inference_", "api_key"),
            concat!("\"author", "ization\""),
            concat!("\"x-api", "-key\""),
            concat!("bear", "er "),
        ] {
            assert!(
                !non_test.contains(needle),
                "driver-xai: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn the_request_carries_only_the_content_headers() {
        let module = wasm_module_source();
        let run_inner = &module[module.find("fn run_inner(").expect("run_inner must exist")..];
        let start = run_inner
            .find("let headers = vec![")
            .expect("run_inner builds headers");
        let headers = &run_inner[start..start + run_inner[start..].find("];").unwrap()];
        let names: Vec<&str> = headers
            .lines()
            .filter_map(|line| line.trim().strip_prefix("(\""))
            .filter_map(|line| line.split('"').next())
            .collect();
        assert_eq!(names, ["content-type", "content-length"]);
    }

    #[test]
    fn manifest_declares_the_upstream_auth_scheme_the_gateway_will_read() {
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "drivers/driver-xai/murmur.yaml must contain verbatim:\n{BLOCK}"
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
            "the streaming loop must emit text chunks"
        );
        assert_eq!(text_sites.len(), thinking_sites.len());
        for (text_at, thinking_at) in text_sites.iter().zip(thinking_sites.iter()) {
            assert!(text_at.abs_diff(*thinking_at) < 400);
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
        for retired in [concat!("murmur::", "text"), concat!("murmur:", "text")] {
            assert!(
                !include_str!("lib.rs").contains(retired),
                "lib.rs must not name {retired}"
            );
        }
    }

    #[test]
    fn no_reasoning_sentinel_scanner_anywhere_in_the_crate() {
        let sentinel = concat!("<", "think");
        assert!(!include_str!("lib.rs").contains(sentinel));
    }

    #[test]
    fn the_run_wrapper_attaches_metadata_only_to_a_failure() {
        let module = wasm_module_source();
        let run = &module[module.find("fn run(").expect("run must exist")..];
        let run = &run[..run
            .find("fn handle_line(")
            .expect("handle_line must follow run")];
        assert!(run.contains("Ok(value) => (value, vec![])"));
        assert!(run.contains("Err(failure) => failure_result(&failure)"));
        assert!(!run.contains(TRANSIENT_METADATA_KEY));
    }

    #[test]
    fn dispatch_request_sends_the_request_before_it_writes_the_body() {
        let module = wasm_module_source();
        let start = module
            .find("fn dispatch_request(")
            .expect("lib.rs defines dispatch_request");
        let rest = &module[start..];
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
        assert!(!dispatch[..handle].contains("check_write"));
        assert!(write < finish);
        assert!(finish < awaited);
    }
}
