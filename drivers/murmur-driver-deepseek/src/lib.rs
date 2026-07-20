// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};

// ── Model validation ──────────────────────────────────────────────────────────

const SUPPORTED_MODELS: &[&str] = &["deepseek-v4-flash", "deepseek-v4-pro"];
const DEPRECATED_MODELS: &[&str] = &["deepseek-chat", "deepseek-reasoner"];

fn validate_model(model: &str) -> Result<(), String> {
    if DEPRECATED_MODELS.contains(&model) {
        return Err(format!(
            "model '{model}' is deprecated and not supported by murmur-driver-deepseek \
             (to be removed by DeepSeek on 2026-07-24). \
             Use 'deepseek-v4-flash' or 'deepseek-v4-pro'."
        ));
    }
    if !SUPPORTED_MODELS.contains(&model) {
        return Err(format!(
            "model '{model}' is not supported by murmur-driver-deepseek. \
             Supported models: deepseek-v4-flash, deepseek-v4-pro."
        ));
    }
    Ok(())
}

// ── Thinking mode config ──────────────────────────────────────────────────────

struct ThinkingConfig {
    /// "enabled" | "disabled" — default "enabled"
    thinking: String,
    /// "high" | "max" — default "high"
    reasoning_effort: String,
}

impl ThinkingConfig {
    fn from_config(config: &HashMap<String, String>) -> Self {
        ThinkingConfig {
            thinking: config
                .get("thinking")
                .cloned()
                .unwrap_or_else(|| "enabled".to_string()),
            reasoning_effort: config
                .get("reasoning_effort")
                .cloned()
                .unwrap_or_else(|| "high".to_string()),
        }
    }

    fn is_thinking_enabled(&self) -> bool {
        self.thinking == "enabled"
    }
}

// Parameters stripped from the request when thinking is enabled.
// DeepSeek ignores them silently, but we omit them explicitly.
const THINKING_STRIPPED_PARAMS: &[&str] =
    &["temperature", "top_p", "presence_penalty", "frequency_penalty"];

// ── Murmur request types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MurmurRequest {
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
    Text { text: String },
    // Reasoning persisted from a prior driver response. DeepSeek requires `reasoning_content`
    // to be reattached to the assistant message on tool-call turns or it returns 400, so this
    // block round-trips through conversation history.
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
fn thinking_text(content: &[MurmurContentBlock]) -> Option<String> {
    content.iter().find_map(|block| match block {
        MurmurContentBlock::Thinking { text } if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
}

// ── Request translation ───────────────────────────────────────────────────────

fn translate_murmur_request_to_deepseek(
    request: &MurmurRequest,
    thinking: &ThinkingConfig,
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
    body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    // DeepSeek thinking extension
    body.insert(
        "thinking".to_string(),
        json!({"type": thinking.thinking}),
    );

    if thinking.is_thinking_enabled() {
        body.insert(
            "reasoning_effort".to_string(),
            Value::String(thinking.reasoning_effort.clone()),
        );
    }

    // Pass through inference params, stripping thinking-incompatible fields when enabled.
    for (key, value) in &request.params {
        if body.contains_key(key) {
            continue;
        }
        if thinking.is_thinking_enabled() && THINKING_STRIPPED_PARAMS.contains(&key.as_str()) {
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

    if tool_calls.is_empty() {
        Ok(json!({"role": "assistant", "content": text}))
    } else {
        // Reattach reasoning_content from the persisted Thinking block. Only assistant messages
        // with tool_calls carry reasoning_content — DeepSeek requires it on those turns or 400s.
        let reasoning_content = thinking_text(&message.content);

        let mut msg = json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": tool_calls,
        });
        if let Some(rc) = reasoning_content {
            msg["reasoning_content"] = json!(rc);
        }
        Ok(msg)
    }
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

// ── Response translation ──────────────────────────────────────────────────────

fn translate_deepseek_response_to_murmur(response: &Value) -> Result<Value, String> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: DeepSeek response missing choices[0]".to_string())?;

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");

    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_call",
        "length" => "max_tokens",
        other => {
            return Err(format!(
                "driver: unsupported DeepSeek finish_reason '{other}'"
            ));
        }
    };

    let message = choice.get("message").unwrap_or(&Value::Null);

    let reasoning_content = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let mut content: Vec<Value> = Vec::new();

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        // Tool-call turn: store reasoning_content as a first-class thinking block so the agent
        // loop preserves it in the assistant message. On the next call the driver reads it back
        // and reattaches it as reasoning_content on the outgoing request (preventing a 400).
        if let Some(rc) = reasoning_content {
            content.push(json!({
                "type": "thinking",
                "text": rc,
            }));
        }

        for call in tool_calls {
            let arguments_raw = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");

            let arguments: Value = serde_json::from_str(arguments_raw).map_err(|err| {
                format!("driver: failed to parse DeepSeek tool call arguments JSON: {err}")
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
        // Final-answer turn: reasoning becomes a first-class thinking block (rendered separately
        // by the UI, excluded from result.txt), followed by the visible answer text.
        if let Some(rc) = reasoning_content {
            content.push(json!({"type": "thinking", "text": rc}));
        }

        let text = match message.get("content") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }

    Ok(json!({
        "stop_reason": stop_reason,
        "content": content,
    }))
}

// ── SSE streaming ──────────────────────────────────────────────────────────────

struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

/// Map a DeepSeek `finish_reason` to a murmur `stop_reason`. `None` → "end_turn".
fn map_finish_reason(finish_reason: Option<&str>) -> Result<&'static str, String> {
    match finish_reason {
        Some("stop") | None => Ok("end_turn"),
        Some("tool_calls") => Ok("tool_call"),
        Some("length") => Ok("max_tokens"),
        Some(other) => Err(format!("driver: unsupported DeepSeek finish_reason '{other}'")),
    }
}

/// Process one complete SSE line. Returns `true` when the stream is done (`[DONE]`).
///
/// DeepSeek streams OpenAI-compatible chat-completions chunks plus a `reasoning_content`
/// delta field. `reasoning_content` is routed to `emit_thinking`, `content` to `emit_text`.
fn process_deepseek_sse_line(
    line: &str,
    tool_states: &mut Vec<ToolCallState>,
    stop_reason: &mut Option<String>,
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

fn assemble_deepseek_streaming_response(
    text_acc: &str,
    reasoning_acc: &str,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
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
            format!("driver: failed to parse DeepSeek tool call arguments JSON: {err}")
        })?;
        tool_content.push(json!({
            "type": "tool_call",
            "id": state.id,
            "name": state.name,
            "input": input,
        }));
    }

    let mut content = Vec::new();
    // Thinking first — required for the tool-call-turn round-trip and rendered separately by UI.
    if !reasoning_acc.is_empty() {
        content.push(json!({"type": "thinking", "text": reasoning_acc}));
    }
    if !tool_content.is_empty() {
        content.extend(tool_content);
    } else if !text_acc.is_empty() {
        content.push(json!({"type": "text", "text": text_acc}));
    }

    Ok(json!({
        "stop_reason": stop_reason_str,
        "content": content,
    }))
}

/// Parse a complete SSE body string (used in tests).
#[cfg(test)]
fn parse_deepseek_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, String> {
    let mut text_acc = String::new();
    let mut reasoning_acc = String::new();
    let mut tool_states: Vec<ToolCallState> = Vec::new();
    let mut stop_reason: Option<String> = None;

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
            process_deepseek_sse_line(line, &mut tool_states, &mut stop_reason, &mut et, &mut eth)
        };
        if done {
            break;
        }
    }

    assemble_deepseek_streaming_response(&text_acc, &reasoning_acc, tool_states, stop_reason)
}

#[allow(dead_code)]
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
        assemble_deepseek_streaming_response, error_payload, process_deepseek_sse_line,
        translate_deepseek_response_to_murmur, translate_murmur_request_to_deepseek, validate_model,
        MurmurRequest, ThinkingConfig, ToolCallState,
    };
    use std::collections::HashMap;
    use serde_json::{json, Value};

    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "driver",
        generate_all,
    });

    pub struct DeepSeekDriver;

    impl exports::murmur::tool::run::Guest for DeepSeekDriver {
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
            .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
        let api_key = std::env::var("MURMUR_INFERENCE_API_KEY").ok();

        let driver_config_str =
            std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").unwrap_or_default();
        let driver_config: HashMap<String, String> =
            serde_json::from_str::<serde_json::Map<String, Value>>(&driver_config_str)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect();
        let thinking = ThinkingConfig::from_config(&driver_config);

        let raw = input
            .data
            .ok_or_else(|| "driver: missing tool-input.data".to_string())?;

        let murmur_request: MurmurRequest = serde_json::from_str(&raw)
            .map_err(|err| format!("driver: failed to parse tool-input.data: {err}"))?;

        // Model validation — pre-flight, no HTTP call made if rejected.
        validate_model(&murmur_request.model)?;

        let mut provider_request =
            translate_murmur_request_to_deepseek(&murmur_request, &thinking)?;

        // Force streaming on; overrides any 'stream' key from params.
        if let Some(obj) = provider_request.as_object_mut() {
            obj.insert("stream".to_string(), json!(true));
        }

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
            let json: Value = serde_json::from_str(&text)
                .map_err(|err| format!("driver: failed to parse DeepSeek response JSON: {err}"))?;
            translate_deepseek_response_to_murmur(&json)?
        } else {
            // SSE streaming: process lines incrementally, emitting chunks as they arrive.
            let mut line_buf: Vec<u8> = Vec::new();
            let mut text_acc = String::new();
            let mut reasoning_acc = String::new();
            let mut tool_states: Vec<ToolCallState> = Vec::new();
            let mut stop_reason: Option<String> = None;
            let mut done = false;

            let handle_line = |line: &str,
                                   tool_states: &mut Vec<ToolCallState>,
                                   stop_reason: &mut Option<String>,
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
                process_deepseek_sse_line(line, tool_states, stop_reason, &mut et, &mut eth)
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
            assemble_deepseek_streaming_response(&text_acc, &reasoning_acc, tool_states, stop_reason)?
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

    export!(DeepSeekDriver);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        parse_deepseek_sse_body, translate_deepseek_response_to_murmur,
        translate_murmur_request_to_deepseek, validate_model, MurmurRequest, ThinkingConfig,
    };
    use serde_json::json;
    use std::collections::HashMap;

    fn thinking_enabled() -> ThinkingConfig {
        ThinkingConfig {
            thinking: "enabled".to_string(),
            reasoning_effort: "high".to_string(),
        }
    }

    fn thinking_disabled() -> ThinkingConfig {
        ThinkingConfig {
            thinking: "disabled".to_string(),
            reasoning_effort: "high".to_string(),
        }
    }

    // ── validate_model ────────────────────────────────────────────────────────

    #[test]
    fn validate_model_rejects_deprecated() {
        let err_chat = validate_model("deepseek-chat").unwrap_err();
        assert!(
            err_chat.contains("deprecated"),
            "expected 'deprecated' in: {err_chat}"
        );
        assert!(
            err_chat.contains("deepseek-v4-flash"),
            "expected alternative model name in: {err_chat}"
        );

        let err_reasoner = validate_model("deepseek-reasoner").unwrap_err();
        assert!(
            err_reasoner.contains("deprecated"),
            "expected 'deprecated' in: {err_reasoner}"
        );
        assert!(
            err_reasoner.contains("deepseek-v4-pro"),
            "expected alternative model name in: {err_reasoner}"
        );
    }

    #[test]
    fn validate_model_rejects_unknown() {
        let err = validate_model("gpt-4o").unwrap_err();
        assert!(
            err.contains("not supported"),
            "expected 'not supported' in: {err}"
        );
        assert!(
            !err.contains("deprecated"),
            "unknown model should not say deprecated: {err}"
        );
    }

    #[test]
    fn validate_model_accepts_supported() {
        assert!(validate_model("deepseek-v4-flash").is_ok());
        assert!(validate_model("deepseek-v4-pro").is_ok());
    }

    // ── ThinkingConfig ────────────────────────────────────────────────────────

    #[test]
    fn thinking_config_defaults() {
        let cfg = ThinkingConfig::from_config(&HashMap::new());
        assert_eq!(cfg.thinking, "enabled", "thinking should default to 'enabled'");
        assert_eq!(
            cfg.reasoning_effort, "high",
            "reasoning_effort should default to 'high'"
        );
        assert!(cfg.is_thinking_enabled());
    }

    #[test]
    fn thinking_config_explicit_disabled() {
        let mut map = HashMap::new();
        map.insert("thinking".to_string(), "disabled".to_string());
        map.insert("reasoning_effort".to_string(), "max".to_string());
        let cfg = ThinkingConfig::from_config(&map);
        assert_eq!(cfg.thinking, "disabled");
        assert_eq!(cfg.reasoning_effort, "max");
        assert!(!cfg.is_thinking_enabled());
    }

    // ── Request serialisation ─────────────────────────────────────────────────

    #[test]
    fn request_strips_temperature_when_thinking_enabled() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "deepseek-v4-flash",
            "max_tokens": 512,
            "messages": [],
            "params": {
                "temperature": 0.7,
                "top_p": 0.9,
                "presence_penalty": 0.1,
                "frequency_penalty": 0.2,
            }
        }))
        .unwrap();

        let body =
            translate_murmur_request_to_deepseek(&request, &thinking_enabled()).unwrap();

        // Verify stripped fields are absent
        assert!(
            body.get("temperature").is_none(),
            "temperature must be absent when thinking is enabled"
        );
        assert!(
            body.get("top_p").is_none(),
            "top_p must be absent when thinking is enabled"
        );
        assert!(
            body.get("presence_penalty").is_none(),
            "presence_penalty must be absent when thinking is enabled"
        );
        assert!(
            body.get("frequency_penalty").is_none(),
            "frequency_penalty must be absent when thinking is enabled"
        );

        // Verify thinking fields are present
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn request_includes_temperature_when_thinking_disabled() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "deepseek-v4-flash",
            "max_tokens": 512,
            "messages": [],
            "params": {
                "temperature": 0.7,
                "top_p": 0.9,
            }
        }))
        .unwrap();

        let body =
            translate_murmur_request_to_deepseek(&request, &thinking_disabled()).unwrap();

        // Temperature should pass through when thinking is disabled
        assert_eq!(
            body["temperature"], 0.7,
            "temperature must be present when thinking is disabled"
        );
        assert_eq!(body["top_p"], 0.9);

        // Thinking block present but no reasoning_effort
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must be absent when thinking is disabled"
        );
    }

    #[test]
    fn request_contains_thinking_block_always() {
        let request: MurmurRequest =
            serde_json::from_value(json!({"model": "deepseek-v4-pro", "max_tokens": 1024, "messages": []}))
                .unwrap();

        let enabled = translate_murmur_request_to_deepseek(&request, &thinking_enabled()).unwrap();
        assert_eq!(enabled["thinking"]["type"], "enabled");

        let disabled =
            translate_murmur_request_to_deepseek(&request, &thinking_disabled()).unwrap();
        assert_eq!(disabled["thinking"]["type"], "disabled");
    }

    // ── Response translation ──────────────────────────────────────────────────

    #[test]
    fn response_tool_call_stores_reasoning_as_thinking_block() {
        let response = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "Let me call the tool.",
                    "tool_calls": [{
                        "id": "tc-1",
                        "type": "function",
                        "function": {"name": "echo", "arguments": "{\"msg\":\"hi\"}"}
                    }]
                }
            }]
        });

        let result = translate_deepseek_response_to_murmur(&response).unwrap();
        assert_eq!(result["stop_reason"], "tool_call");

        // First content block is a first-class thinking block (no <thinking> sentinel).
        let first = &result["content"][0];
        assert_eq!(first["type"], "thinking");
        assert_eq!(first["text"], "Let me call the tool.");

        // Second block should be the tool_call
        let second = &result["content"][1];
        assert_eq!(second["type"], "tool_call");
        assert_eq!(second["id"], "tc-1");
        assert_eq!(second["name"], "echo");
    }

    #[test]
    fn response_tool_call_no_reasoning_content() {
        let response = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "tc-1",
                        "type": "function",
                        "function": {"name": "echo", "arguments": "{}"}
                    }]
                }
            }]
        });

        let result = translate_deepseek_response_to_murmur(&response).unwrap();
        assert_eq!(result["stop_reason"], "tool_call");
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        assert_eq!(result["content"][0]["type"], "tool_call");
    }

    #[test]
    fn response_final_answer_with_reasoning() {
        let response = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning_content": "Let me think..."
                }
            }]
        });

        let result = translate_deepseek_response_to_murmur(&response).unwrap();
        assert_eq!(result["stop_reason"], "end_turn");
        // Reasoning and answer are separate blocks — thinking first, then text.
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "Let me think...");
        assert_eq!(result["content"][1]["type"], "text");
        assert_eq!(result["content"][1]["text"], "The answer is 42.");
    }

    // ── reasoning_content round-trip ──────────────────────────────────────────

    #[test]
    fn assistant_tool_call_message_reattaches_reasoning_content() {
        // Simulate a MurmurMessage that was stored by a prior driver response:
        // content includes a first-class thinking block AND a tool_call block.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "deepseek-v4-pro",
            "max_tokens": 1024,
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "please call echo"}]
                },
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "text": "I should call echo."},
                        {"type": "tool_call", "id": "tc-1", "name": "echo", "input": {"msg": "hi"}}
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "tc-1",
                    "content": [{"type": "text", "text": "hi"}]
                }
            ]
        }))
        .unwrap();

        let body = translate_murmur_request_to_deepseek(&request, &thinking_enabled()).unwrap();
        let messages = body["messages"].as_array().unwrap();

        // messages[0] = user, messages[1] = assistant (tool-call), messages[2] = tool
        let asst = &messages[1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["reasoning_content"], "I should call echo.");
        assert!(asst["tool_calls"].is_array());
        assert!(
            asst["content"].is_null(),
            "content should be null for tool-call assistant message"
        );
    }

    // ── Streaming ──────────────────────────────────────────────────────────────

    #[test]
    fn streaming_reasoning_then_text() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think.\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"The answer \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"is 42.\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );

        let mut text: Vec<String> = Vec::new();
        let mut think: Vec<String> = Vec::new();
        let result = parse_deepseek_sse_body(
            body,
            &mut |t| text.push(t.to_string()),
            &mut |t| think.push(t.to_string()),
        )
        .unwrap();

        assert_eq!(think.join(""), "Let me think.");
        assert_eq!(text.join(""), "The answer is 42.");
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "Let me think.");
        assert_eq!(result["content"][1]["type"], "text");
        assert_eq!(result["content"][1]["text"], "The answer is 42.");
        assert_eq!(result["stop_reason"], "end_turn");
    }

    #[test]
    fn streaming_tool_call_keeps_reasoning_block() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Need echo.\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tc-1\",\"type\":\"function\",\"function\":{\"name\":\"echo\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"msg\\\":\\\"hi\\\"}\"}}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        );

        let mut text: Vec<String> = Vec::new();
        let mut think: Vec<String> = Vec::new();
        let result = parse_deepseek_sse_body(
            body,
            &mut |t| text.push(t.to_string()),
            &mut |t| think.push(t.to_string()),
        )
        .unwrap();

        assert_eq!(think.join(""), "Need echo.");
        assert!(text.is_empty(), "no visible text on a tool-call turn");
        assert_eq!(result["stop_reason"], "tool_call");
        // Reasoning preserved as a thinking block (round-trips to reasoning_content next turn).
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "Need echo.");
        assert_eq!(result["content"][1]["type"], "tool_call");
        assert_eq!(result["content"][1]["id"], "tc-1");
        assert_eq!(result["content"][1]["name"], "echo");
        assert_eq!(result["content"][1]["input"], json!({"msg": "hi"}));
    }
}
