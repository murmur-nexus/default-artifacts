// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    Claude3,
    Claude4Plus,
}

fn classify_model(model: &str) -> ModelFamily {
    // Anthropic uses two naming conventions:
    //   Claude 3.x: claude-{major}-{size}-{date}    e.g. claude-3-opus-20240229
    //   Claude 4.x: claude-{size}-{major}-{minor}   e.g. claude-opus-4-6
    // We extract the major version number from whichever position it appears in.
    if let Some(rest) = model.trim().strip_prefix("claude-") {
        let parts: Vec<&str> = rest.splitn(3, '-').collect();
        if let Some(first) = parts.first() {
            if let Ok(major) = first.parse::<u32>() {
                // claude-{N}-... format: major is the first segment
                return if major >= 4 {
                    ModelFamily::Claude4Plus
                } else {
                    ModelFamily::Claude3
                };
            }
            // claude-{size}-{N}-... format: major is the second segment
            if let Some(Ok(major)) = parts.get(1).map(|s| s.parse::<u32>()) {
                return if major >= 4 {
                    ModelFamily::Claude4Plus
                } else {
                    ModelFamily::Claude3
                };
            }
        }
    }
    ModelFamily::Claude3
}

// Reads `beta_features` from the driver config JSON and returns a comma-separated
// string suitable for the `anthropic-beta` header. Accepts a single string or a list.
fn parse_beta_features(config_json: &str) -> Option<String> {
    let config: Value = serde_json::from_str(config_json).ok()?;
    match config.get("beta_features")? {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(arr) => {
            let joined = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(",");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

// ── Extended-thinking config ──────────────────────────────────────────────────

// Parameters Anthropic rejects (or that must equal their default) when extended thinking
// is enabled. Stripped from the outgoing request body in that case.
const THINKING_STRIPPED_PARAMS: &[&str] = &["temperature", "top_p", "top_k"];

const DEFAULT_THINKING_BUDGET_TOKENS: u32 = 1024;

#[derive(Debug, Clone, Copy)]
struct ThinkingConfig {
    enabled: bool,
    budget_tokens: u32,
}

impl ThinkingConfig {
    fn disabled() -> Self {
        Self { enabled: false, budget_tokens: DEFAULT_THINKING_BUDGET_TOKENS }
    }
}

// Reads `thinking` ("enabled"/"disabled", default disabled) and `thinking_budget_tokens`
// (default 1024) from the driver config JSON.
fn parse_thinking_config(config_json: &str) -> ThinkingConfig {
    let Ok(config) = serde_json::from_str::<Value>(config_json) else {
        return ThinkingConfig::disabled();
    };
    let enabled = config
        .get("thinking")
        .and_then(Value::as_str)
        .map(|s| s.trim().eq_ignore_ascii_case("enabled"))
        .unwrap_or(false);
    let budget_tokens = config
        .get("thinking_budget_tokens")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_THINKING_BUDGET_TOKENS);
    ThinkingConfig { enabled, budget_tokens }
}

#[derive(Debug, Deserialize)]
struct MurmurRequest {
    // The host sends a prompt-cache routing hint as a reserved top-level `prompt_cache_key`
    // member. It is deliberately not declared here: the Messages API rejects a body carrying
    // any field it does not define, and it defines no cache-key field. An undeclared member is
    // dropped by serde, so it cannot reach the provider body — do not add a field and filter it.
    model: String,
    max_tokens: u32,
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
    // Extended-thinking block persisted from a prior driver response. Anthropic requires the
    // original `thinking` block (with its `signature`) to be replayed verbatim on the assistant
    // turn that precedes a tool_result, or it returns 400. Re-sent only when thinking is enabled.
    Thinking {
        #[serde(default)]
        text: String,
        #[serde(default)]
        signature: String,
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
    #[serde(default)]
    r#type: Option<String>,
    media_type: String,
    data: String,
}

fn default_object() -> Value {
    json!({})
}

fn translate_murmur_request_to_anthropic(
    request: &MurmurRequest,
    family: ModelFamily,
    thinking: &ThinkingConfig,
) -> Result<Value, String> {
    let mut messages = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        match message.role.as_str() {
            "user" | "assistant" => {
                messages.push(json!({
                    "role": message.role,
                    "content": translate_standard_content_blocks(&message.content, thinking.enabled),
                }));
            }
            "tool" => {
                let tool_use_id = message.tool_call_id.clone().ok_or_else(|| {
                    "driver: tool message is missing required field 'tool_call_id'".to_string()
                })?;

                let content = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        MurmurContentBlock::Text { text } => Some(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": [{"type": "text", "text": text}],
                        })),
                        _ => None,
                    })
                    .collect::<Vec<_>>();

                messages.push(json!({
                    "role": "user",
                    "content": content,
                }));
            }
            other => {
                return Err(format!("driver: unsupported message role '{other}'"));
            }
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut mapped = Map::new();
            mapped.insert("name".to_string(), Value::String(tool.name.clone()));
            mapped.insert("input_schema".to_string(), tool.parameters.clone());
            if let Some(description) = tool
                .description
                .as_ref()
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())
            {
                mapped.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            Value::Object(mapped)
        })
        .collect::<Vec<_>>();

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));

    if let Some(system) = request
        .system
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        body.insert("system".to_string(), Value::String(system.to_string()));
    }

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    if request.params.get("tool_choice").and_then(Value::as_str) == Some("required") {
        body.insert("tool_choice".to_string(), json!({"type": "any"}));
    }

    // Extended thinking: inject the `thinking` block and strip sampling params Anthropic
    // rejects while thinking is on (temperature/top_p/top_k). budget_tokens must be < max_tokens.
    if thinking.enabled {
        let budget = thinking
            .budget_tokens
            .min(request.max_tokens.saturating_sub(1))
            .max(1);
        body.insert(
            "thinking".to_string(),
            json!({"type": "enabled", "budget_tokens": budget}),
        );
    }

    // Claude 4+ rejects requests that send both temperature and top_p; drop top_p in that case.
    let drop_top_p = family == ModelFamily::Claude4Plus
        && request.params.contains_key("temperature")
        && request.params.contains_key("top_p");

    for (key, value) in &request.params {
        if body.contains_key(key) {
            continue;
        }
        if drop_top_p && key == "top_p" {
            continue;
        }
        if thinking.enabled && THINKING_STRIPPED_PARAMS.contains(&key.as_str()) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }

    Ok(Value::Object(body))
}

fn translate_standard_content_blocks(
    blocks: &[MurmurContentBlock],
    thinking_enabled: bool,
) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| match block {
            MurmurContentBlock::Text { text } => Some(json!({
                "type": "text",
                "text": text,
            })),
            // Replay the original signed thinking block so Anthropic can verify it on tool-use
            // continuation turns. Only when thinking is enabled and we have a signature — an
            // unsigned or stale block sent with thinking off is a 400.
            MurmurContentBlock::Thinking { text, signature } => {
                if thinking_enabled && !signature.is_empty() {
                    Some(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature,
                    }))
                } else {
                    None
                }
            }
            MurmurContentBlock::Image { source } => Some(json!({
                "type": "image",
                "source": {
                    "type": source.r#type.as_deref().unwrap_or("base64"),
                    "media_type": source.media_type,
                    "data": source.data,
                }
            })),
            MurmurContentBlock::ToolCall { id, name, input } => Some(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            MurmurContentBlock::Unknown => None,
        })
        .collect()
}

// ── Provider token usage ─────────────────────────────────────────────

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
    /// usage arrives across more than one event, each carrying only part of the picture.
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

/// Read the four contract members out of an Anthropic `usage` object. A member that is absent,
/// null, or not a non-negative integer is dropped; its siblings are kept. A non-object argument
/// yields no members at all.
fn extract_anthropic_usage(usage: &Value) -> UsageTokens {
    UsageTokens {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cached_tokens: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
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

/// Force streaming on, overriding any `stream` key from `params`. The Messages API reports usage
/// on the stream's own `message_start` and `message_delta` events, and rejects a body carrying
/// any field it does not define, so no usage opt-in is stamped alongside it.
fn stamp_streaming_flags(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
    }
}

fn translate_anthropic_response_to_murmur(response: &Value) -> Result<Value, String> {
    let anthropic_stop = response
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");

    let stop_reason = match anthropic_stop {
        "end_turn" => "end_turn",
        "tool_use" => "tool_call",
        "max_tokens" => "max_tokens",
        "stop_sequence" => "end_turn",
        other => {
            return Err(format!(
                "driver: unsupported Anthropic stop_reason '{other}'"
            ));
        }
    };

    let content_blocks = response
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let content = content_blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => Some(json!({
                "type": "text",
                "text": block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            })),
            Some("tool_use") => Some(json!({
                "type": "tool_call",
                "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
                "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                "input": block.get("input").cloned().unwrap_or_else(default_object),
            })),
            _ => None,
        })
        .collect::<Vec<_>>();

    let usage = response
        .get("usage")
        .map(extract_anthropic_usage)
        .unwrap_or_default();

    Ok(murmur_response(stop_reason, content, usage))
}

// ── SSE streaming types and parsing ──────────────────────────────────────────

enum AnthropicBlock {
    Text { text: String },
    Thinking { text: String, signature: String },
    ToolUse { id: String, name: String, input: String },
}

struct AnthropicSseState {
    blocks: Vec<Option<AnthropicBlock>>,
    stop_reason: Option<String>,
    usage: UsageTokens,
    current_event: String,
    current_data: String,
    done: bool,
}

impl AnthropicSseState {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            stop_reason: None,
            usage: UsageTokens::default(),
            current_event: String::new(),
            current_data: String::new(),
            done: false,
        }
    }
}

/// Process one SSE line, updating state and calling `emit` for text deltas.
/// Returns `true` when the stream is done (`message_stop` dispatched).
fn process_anthropic_sse_line(
    line: &str,
    state: &mut AnthropicSseState,
    emit: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
) -> bool {
    if let Some(event) = line.strip_prefix("event: ") {
        state.current_event = event.to_string();
    } else if let Some(data) = line.strip_prefix("data: ") {
        state.current_data = data.to_string();
    } else if line.starts_with(':') {
        // heartbeat comment — ignore
    } else if line.is_empty() && !state.current_event.is_empty() {
        let event = std::mem::take(&mut state.current_event);
        let data = std::mem::take(&mut state.current_data);
        dispatch_anthropic_sse_event(&event, &data, state, emit, emit_thinking);
    }
    state.done
}

fn dispatch_anthropic_sse_event(
    event: &str,
    data: &str,
    state: &mut AnthropicSseState,
    emit: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
) {
    let Ok(val) = serde_json::from_str::<Value>(data) else {
        return;
    };

    match event {
        "content_block_start" => {
            let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block_type = val
                .pointer("/content_block/type")
                .and_then(Value::as_str)
                .unwrap_or("text");

            while state.blocks.len() <= index {
                state.blocks.push(None);
            }

            state.blocks[index] = Some(match block_type {
                "tool_use" => {
                    let id = val
                        .pointer("/content_block/id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = val
                        .pointer("/content_block/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    AnthropicBlock::ToolUse { id, name, input: String::new() }
                }
                "thinking" => AnthropicBlock::Thinking {
                    text: String::new(),
                    signature: String::new(),
                },
                _ => AnthropicBlock::Text { text: String::new() },
            });
        }
        "content_block_delta" => {
            let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta_type = val
                .pointer("/delta/type")
                .and_then(Value::as_str)
                .unwrap_or("");

            if index < state.blocks.len() {
                match delta_type {
                    "text_delta" => {
                        let text = val
                            .pointer("/delta/text")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !text.is_empty() {
                            emit(text);
                            if let Some(Some(AnthropicBlock::Text { text: acc })) =
                                state.blocks.get_mut(index)
                            {
                                acc.push_str(text);
                            }
                        }
                    }
                    "input_json_delta" => {
                        let partial = val
                            .pointer("/delta/partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(Some(AnthropicBlock::ToolUse { input, .. })) =
                            state.blocks.get_mut(index)
                        {
                            input.push_str(partial);
                        }
                    }
                    "thinking_delta" => {
                        let thought = val
                            .pointer("/delta/thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !thought.is_empty() {
                            emit_thinking(thought);
                            if let Some(Some(AnthropicBlock::Thinking { text, .. })) =
                                state.blocks.get_mut(index)
                            {
                                text.push_str(thought);
                            }
                        }
                    }
                    "signature_delta" => {
                        let sig = val
                            .pointer("/delta/signature")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(Some(AnthropicBlock::Thinking { signature, .. })) =
                            state.blocks.get_mut(index)
                        {
                            signature.push_str(sig);
                        }
                    }
                    _ => {}
                }
            }
        }
        "message_start" => {
            // Carries the request-side counts plus a placeholder `output_tokens` that a later
            // `message_delta` supersedes.
            if let Some(usage) = val.pointer("/message/usage") {
                state.usage.merge_from(extract_anthropic_usage(usage));
            }
        }
        "message_delta" => {
            if let Some(reason) = val.pointer("/delta/stop_reason").and_then(Value::as_str) {
                if !reason.is_empty() {
                    state.stop_reason = Some(reason.to_string());
                }
            }
            // Cumulative counts for the completion so far; the last one dispatched wins.
            if let Some(usage) = val.get("usage") {
                state.usage.merge_from(extract_anthropic_usage(usage));
            }
        }
        "message_stop" => {
            state.done = true;
        }
        _ => {} // content_block_stop, ping — no-op
    }
}

/// Parse a complete SSE body string (used in tests).
#[cfg(test)]
fn parse_anthropic_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, String> {
    let mut state = AnthropicSseState::new();
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        process_anthropic_sse_line(line, &mut state, emit, emit_thinking);
        if state.done {
            break;
        }
    }
    assemble_anthropic_streaming_response(state)
}

fn assemble_anthropic_streaming_response(state: AnthropicSseState) -> Result<Value, String> {
    let anthropic_stop = state.stop_reason.as_deref().unwrap_or("end_turn");
    let stop_reason = match anthropic_stop {
        "end_turn" => "end_turn",
        "tool_use" => "tool_call",
        "max_tokens" => "max_tokens",
        "stop_sequence" => "end_turn",
        other => {
            return Err(format!(
                "driver: unsupported Anthropic stop_reason '{other}'"
            ));
        }
    };

    let usage = state.usage;
    let mut content = Vec::new();
    for block in state.blocks.into_iter().flatten() {
        match block {
            AnthropicBlock::Text { text } if !text.is_empty() => {
                content.push(json!({"type": "text", "text": text}));
            }
            AnthropicBlock::Thinking { text, signature } if !text.is_empty() => {
                content.push(json!({
                    "type": "thinking",
                    "text": text,
                    "signature": signature,
                }));
            }
            AnthropicBlock::ToolUse { id, name, input } => {
                let input_val: Value =
                    serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool_call",
                    "id": id,
                    "name": name,
                    "input": input_val,
                }));
            }
            _ => {}
        }
    }

    Ok(murmur_response(stop_reason, content, usage))
}

#[allow(dead_code)] // reachable only from the wasm32-gated driver module
fn error_payload(message: &str) -> Value {
    json!({
        "stop_reason": "error",
        "error": message,
    })
}

#[cfg(target_arch = "wasm32")]
mod wasm_driver {
    use super::{
        assemble_anthropic_streaming_response, classify_model, error_payload, parse_beta_features,
        parse_thinking_config, process_anthropic_sse_line, stamp_streaming_flags,
        translate_anthropic_response_to_murmur, translate_murmur_request_to_anthropic,
        AnthropicSseState, MurmurRequest, ThinkingConfig,
    };
    use serde_json::Value;

    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "driver",
        generate_all,
    });

    const ANTHROPIC_VERSION: &str = "2023-06-01";

    pub struct AnthropicDriver;

    impl exports::murmur::tool::run::Guest for AnthropicDriver {
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
        let endpoint = std::env::var("MURMUR_INFERENCE_ENDPOINT")
            .map_err(|_| "driver: missing MURMUR_INFERENCE_ENDPOINT".to_string())?;
        let api_key = std::env::var("MURMUR_INFERENCE_API_KEY").ok();
        let driver_config = std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").ok();

        let raw = input
            .data
            .ok_or_else(|| "driver: missing tool-input.data".to_string())?;

        let murmur_request: MurmurRequest = serde_json::from_str(&raw)
            .map_err(|err| format!("driver: failed to parse tool-input.data: {err}"))?;

        let family = classify_model(&murmur_request.model);
        let thinking = driver_config
            .as_deref()
            .map(parse_thinking_config)
            .unwrap_or_else(ThinkingConfig::disabled);
        let mut provider_request =
            translate_murmur_request_to_anthropic(&murmur_request, family, &thinking)?;

        stamp_streaming_flags(&mut provider_request);

        let body = serde_json::to_vec(&provider_request)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}/v1/messages", endpoint.trim_end_matches('/'));

        let beta_features = driver_config.as_deref().and_then(parse_beta_features);

        let mut headers = vec![
            ("content-type", "application/json".to_string()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
            ("content-length", body.len().to_string()),
        ];

        if let Some(key) = api_key.as_ref().map(|k| k.trim()).filter(|k| !k.is_empty()) {
            headers.push(("x-api-key", key.to_string()));
        }

        if let Some(beta) = beta_features {
            headers.push(("anthropic-beta", beta));
        }

        let response = dispatch_request(&url, headers, &body)?;
        let status = response.status();

        if status >= 400 {
            let text = consume_body_as_string(response)?;
            return Ok(error_payload(&format!("HTTP {status}: {text}")));
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
            let json: Value = serde_json::from_str(&text).map_err(|err| {
                format!("driver: failed to parse Anthropic response JSON: {err}")
            })?;
            translate_anthropic_response_to_murmur(&json)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut state = AnthropicSseState::new();
            let mut done = false;

            // Process bytes already read.
            for &b in &first {
                if b == b'\n' {
                    let line = String::from_utf8_lossy(&line_buf);
                    let line = line.trim_end_matches('\r');
                    done = process_anthropic_sse_line(
                        line,
                        &mut state,
                        &mut |chunk| murmur::text::chunks::emit_chunk(chunk),
                        &mut |chunk| murmur::text::chunks::emit_thinking_chunk(chunk),
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
                            done = process_anthropic_sse_line(
                                line,
                                &mut state,
                                &mut |chunk| murmur::text::chunks::emit_chunk(chunk),
                                &mut |chunk| murmur::text::chunks::emit_thinking_chunk(chunk),
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
            assemble_anthropic_streaming_response(state)?
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
                    .map_err(|e| format!("driver: check-write failed: {e:?}"))? as usize;
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
                    return Err("driver: response future already consumed".to_string())
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

    export!(AnthropicDriver);
}

#[cfg(test)]
mod tests {
    use super::{
        classify_model, parse_anthropic_sse_body, parse_beta_features, parse_thinking_config,
        stamp_streaming_flags, translate_anthropic_response_to_murmur,
        translate_murmur_request_to_anthropic, ModelFamily, MurmurRequest, ThinkingConfig,
    };
    use serde_json::{json, Value};

    fn thinking_off() -> ThinkingConfig {
        ThinkingConfig::disabled()
    }

    #[test]
    fn translates_murmur_request_to_anthropic_messages_api() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 2048,
            "system": "You are helpful",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "I'll call a tool."},
                        {"type": "tool_call", "id": "tc-1", "name": "calc", "input": {"x": 1}}
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "tc-1",
                    "content": [{"type": "text", "text": "2"}]
                }
            ],
            "tools": [
                {
                    "name": "calc",
                    "description": "calculate",
                    "parameters": {"type": "object", "properties": {"x": {"type": "number"}}}
                }
            ],
            "params": {
                "temperature": 0.2
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &thinking_off()).unwrap();
        let messages = translated["messages"].as_array().unwrap();

        assert_eq!(
            translated["model"],
            Value::String("claude-opus-4-6".to_string())
        );
        assert_eq!(messages[0]["content"][1]["type"], "tool_use");
        assert_eq!(messages[0]["content"][1]["id"], "tc-1");

        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "tc-1");

        assert_eq!(
            translated["tools"][0]["input_schema"],
            json!({
                "type": "object",
                "properties": {"x": {"type": "number"}}
            })
        );
        assert_eq!(translated["temperature"], json!(0.2));
    }

    #[test]
    fn maps_required_tool_choice_to_anthropic_any() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 2048,
            "messages": [],
            "tools": [
                {
                    "name": "calc",
                    "parameters": {"type": "object", "properties": {}}
                }
            ],
            "params": {
                "tool_choice": "required"
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &thinking_off()).unwrap();

        assert_eq!(translated["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn translates_anthropic_response_to_murmur_format() {
        let anthropic = json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Need tool"},
                {"type": "tool_use", "id": "tc-1", "name": "calc", "input": {"x": 2}}
            ]
        });

        let translated = translate_anthropic_response_to_murmur(&anthropic).unwrap();
        assert_eq!(translated["stop_reason"], "tool_call");

        let content = translated["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_call");
        assert_eq!(content[1]["id"], "tc-1");
        assert_eq!(content[1]["input"], json!({"x": 2}));
    }

    #[test]
    fn maps_stop_sequence_to_end_turn() {
        let anthropic = json!({
            "stop_reason": "stop_sequence",
            "content": [{"type": "text", "text": "done"}]
        });

        let translated = translate_anthropic_response_to_murmur(&anthropic).unwrap();
        assert_eq!(translated["stop_reason"], "end_turn");
    }

    #[test]
    fn classifies_claude3_models() {
        for model in &[
            "claude-3-opus-20240229",
            "claude-3-5-sonnet-20241022",
            "claude-3-haiku-20240307",
        ] {
            assert_eq!(
                classify_model(model),
                ModelFamily::Claude3,
                "expected Claude3 for {model}"
            );
        }
    }

    #[test]
    fn classifies_claude4_plus_models() {
        for model in &[
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-7",
        ] {
            assert_eq!(
                classify_model(model),
                ModelFamily::Claude4Plus,
                "expected Claude4Plus for {model}"
            );
        }
    }

    #[test]
    fn claude4_drops_top_p_when_temperature_also_present() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 1024,
            "messages": [],
            "params": {
                "temperature": 0.7,
                "top_p": 0.9
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &thinking_off()).unwrap();

        assert_eq!(translated["temperature"], 0.7);
        assert!(
            translated.get("top_p").is_none(),
            "top_p must be dropped when temperature is also set on Claude 4+"
        );
    }

    #[test]
    fn claude4_keeps_top_p_when_temperature_absent() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 1024,
            "messages": [],
            "params": {
                "top_p": 0.9
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &thinking_off()).unwrap();

        assert_eq!(translated["top_p"], 0.9);
    }

    #[test]
    fn claude3_allows_both_temperature_and_top_p() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 512,
            "messages": [],
            "params": {
                "temperature": 0.5,
                "top_p": 0.8
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude3, &thinking_off()).unwrap();

        assert_eq!(translated["temperature"], 0.5);
        assert_eq!(translated["top_p"], 0.8);
    }

    #[test]
    fn parse_beta_features_from_string() {
        let config = r#"{"beta_features": "interleaved-thinking-2025-05-14"}"#;
        assert_eq!(
            parse_beta_features(config),
            Some("interleaved-thinking-2025-05-14".to_string())
        );
    }

    #[test]
    fn parse_beta_features_from_list() {
        let config = r#"{"beta_features": ["interleaved-thinking-2025-05-14", "fine-tuning-2024-01-01"]}"#;
        assert_eq!(
            parse_beta_features(config),
            Some("interleaved-thinking-2025-05-14,fine-tuning-2024-01-01".to_string())
        );
    }

    #[test]
    fn parse_beta_features_absent_returns_none() {
        assert_eq!(parse_beta_features(r#"{"temperature": 0.5}"#), None);
        assert_eq!(parse_beta_features(r#"{}"#), None);
        assert_eq!(parse_beta_features(r#"{"beta_features": ""}"#), None);
        assert_eq!(parse_beta_features(r#"{"beta_features": []}"#), None);
    }

    #[test]
    fn anthropic_streaming_text_response() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-6\",\"stop_reason\":null,\"stop_sequence\":null}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let mut emitted: Vec<String> = Vec::new();
        let result =
            parse_anthropic_sse_body(body, &mut |chunk| emitted.push(chunk.to_string()), &mut |_| {}).unwrap();

        assert_eq!(emitted, vec!["Hello", " world"]);
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "Hello world");
    }

    #[test]
    fn anthropic_streaming_tool_use() {
        let body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"bash\",\"input\":{}}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"} }\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let mut emitted: Vec<String> = Vec::new();
        let result =
            parse_anthropic_sse_body(body, &mut |chunk| emitted.push(chunk.to_string()), &mut |_| {}).unwrap();

        assert!(emitted.is_empty(), "no text chunks expected for tool use");
        assert_eq!(result["stop_reason"], "tool_call");
        assert_eq!(result["content"][0]["type"], "tool_call");
        assert_eq!(result["content"][0]["id"], "toolu_01");
        assert_eq!(result["content"][0]["name"], "bash");
        assert_eq!(result["content"][0]["input"], json!({"cmd": "ls"}));
    }

    // ── Extended thinking ─────────────────────────────────────────────────────

    #[test]
    fn parse_thinking_config_reads_enabled_and_budget() {
        let cfg = parse_thinking_config(r#"{"thinking":"enabled","thinking_budget_tokens":4096}"#);
        assert!(cfg.enabled);
        assert_eq!(cfg.budget_tokens, 4096);

        let default_budget = parse_thinking_config(r#"{"thinking":"enabled"}"#);
        assert!(default_budget.enabled);
        assert_eq!(default_budget.budget_tokens, 1024);

        let off = parse_thinking_config(r#"{"thinking":"disabled"}"#);
        assert!(!off.enabled);
        assert!(!parse_thinking_config("{}").enabled);
        assert!(!parse_thinking_config("not json").enabled);
    }

    #[test]
    fn thinking_enabled_injects_block_and_strips_sampling_params() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 8192,
            "messages": [],
            "params": {"temperature": 0.7, "top_p": 0.9, "top_k": 40}
        }))
        .unwrap();

        let cfg = ThinkingConfig { enabled: true, budget_tokens: 2048 };
        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &cfg).unwrap();

        assert_eq!(translated["thinking"]["type"], "enabled");
        assert_eq!(translated["thinking"]["budget_tokens"], 2048);
        assert!(translated.get("temperature").is_none(), "temperature must be stripped");
        assert!(translated.get("top_p").is_none(), "top_p must be stripped");
        assert!(translated.get("top_k").is_none(), "top_k must be stripped");
    }

    #[test]
    fn thinking_budget_capped_below_max_tokens() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 1000,
            "messages": [],
        }))
        .unwrap();

        let cfg = ThinkingConfig { enabled: true, budget_tokens: 4096 };
        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &cfg).unwrap();

        assert_eq!(translated["thinking"]["budget_tokens"], 999);
    }

    #[test]
    fn thinking_block_replayed_only_when_enabled_and_signed() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 4096,
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "text": "step by step", "signature": "sig-abc"},
                    {"type": "text", "text": "the answer"}
                ]
            }]
        }))
        .unwrap();

        // Enabled → thinking block replayed first, verbatim, with signature.
        let on = ThinkingConfig { enabled: true, budget_tokens: 2048 };
        let with_thinking =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &on).unwrap();
        let content = &with_thinking["messages"][0]["content"];
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "step by step");
        assert_eq!(content[0]["signature"], "sig-abc");
        assert_eq!(content[1]["type"], "text");

        // Disabled → thinking block dropped (sending it would 400).
        let without =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &thinking_off())
                .unwrap();
        assert_eq!(without["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn anthropic_streaming_thinking_then_text() {
        let body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reason.\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer is 42.\"}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let mut text: Vec<String> = Vec::new();
        let mut think: Vec<String> = Vec::new();
        let result = parse_anthropic_sse_body(
            body,
            &mut |c| text.push(c.to_string()),
            &mut |c| think.push(c.to_string()),
        )
        .unwrap();

        assert_eq!(think.join(""), "Let me reason.");
        assert_eq!(text, vec!["The answer is 42."]);
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "Let me reason.");
        assert_eq!(result["content"][0]["signature"], "sig123");
        assert_eq!(result["content"][1]["type"], "text");
        assert_eq!(result["content"][1]["text"], "The answer is 42.");
        assert_eq!(result["stop_reason"], "end_turn");
    }

    // ── prompt_cache_key (never forwarded) ────────────────────────────────────

    #[test]
    fn prompt_cache_key_never_reaches_the_anthropic_body() {
        // The Messages API 400s on any body member it does not define, so the reserved
        // top-level hint must not survive translation at any nesting depth.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 1024,
            "prompt_cache_key": "demo-capsule:1.0.0:ctx-7",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "params": {"temperature": 0.2}
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_anthropic(&request, ModelFamily::Claude4Plus, &thinking_off())
                .unwrap();

        let serialized = serde_json::to_string(&translated).unwrap();
        assert!(
            !serialized.contains("prompt_cache_key"),
            "prompt_cache_key leaked into the Anthropic body: {serialized}"
        );
        assert!(!serialized.contains("demo-capsule:1.0.0:ctx-7"));
        // Unrelated params still pass through.
        assert_eq!(translated["temperature"], json!(0.2));
    }

    // ── usage ─────────────────────────────────────────────────────────────────

    #[test]
    fn non_streaming_usage_round_trip() {
        let response = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {
                "input_tokens": 12043,
                "output_tokens": 218,
                "cache_read_input_tokens": 11780,
                "cache_creation_input_tokens": 7
            }
        });

        let translated = translate_anthropic_response_to_murmur(&response).unwrap();

        assert_eq!(translated["stop_reason"], "end_turn");
        assert_eq!(translated["content"][0]["text"], "Hello");
        assert_eq!(
            translated["usage"],
            json!({
                "input_tokens": 12043,
                "output_tokens": 218,
                "cached_tokens": 11780,
                "cache_write_tokens": 7
            })
        );
    }

    #[test]
    fn streaming_usage_seeded_by_message_start_and_finished_by_message_delta() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_read_input_tokens\":90,\"cache_creation_input_tokens\":10}}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":55}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let result = parse_anthropic_sse_body(body, &mut |_| {}, &mut |_| {}).unwrap();

        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["content"][0]["text"], "Hello");
        assert_eq!(result["usage"]["input_tokens"], 100);
        // The cumulative delta count wins over the message_start placeholder.
        assert_eq!(result["usage"]["output_tokens"], 55);
        assert_eq!(result["usage"]["cached_tokens"], 90);
        assert_eq!(result["usage"]["cache_write_tokens"], 10);
    }

    #[test]
    fn missing_or_malformed_usage_omits_the_member() {
        for usage in [
            None,
            Some(json!("12043")),
            Some(json!({"input_tokens": "12043", "output_tokens": -4, "cached_tokens": 1.5})),
            Some(json!({})),
            Some(Value::Null),
        ] {
            let mut response = json!({
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "Hello"}]
            });
            if let Some(usage) = usage.clone() {
                response["usage"] = usage;
            }

            let translated = translate_anthropic_response_to_murmur(&response).unwrap();
            assert!(
                translated.get("usage").is_none(),
                "expected no usage key for {usage:?}, got {translated}"
            );
        }
    }

    #[test]
    fn reported_zero_is_kept_and_unreported_siblings_are_omitted() {
        let response = json!({
            "stop_reason": "end_turn",
            "content": [],
            "usage": {"input_tokens": 40, "cache_read_input_tokens": 0}
        });

        let translated = translate_anthropic_response_to_murmur(&response).unwrap();

        assert_eq!(
            translated["usage"],
            json!({"input_tokens": 40, "cached_tokens": 0})
        );
    }

    #[test]
    fn streaming_flags_carry_no_stream_options() {
        let mut body = json!({"model": "claude-opus-4-6", "stream": false});
        stamp_streaming_flags(&mut body);
        assert_eq!(body["stream"], json!(true));
        assert!(body.get("stream_options").is_none());
    }
}
