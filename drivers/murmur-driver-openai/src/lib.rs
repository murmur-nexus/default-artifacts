use serde::Deserialize;
use serde_json::{json, Map, Value};

/// Reserved key carrying the driver-continuation id, verbatim from the v0.5.12
/// host/WIT protocol ("Stateful driver continuation, part 1"). Used in two places:
///   • request-side: an optional top-level field of the incoming murmur request
///     JSON (`tool-input.data`), injected by the host when it holds an id;
///   • response-side: the `tool-result.metadata` key this driver returns the
///     provider response `id` under, so the host can persist and re-supply it.
/// Both directions must use this exact string — the host reads/writes it verbatim.
const CONTINUATION_ID_KEY: &str = "continuation_id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    GptClassic,
    OSeriesReasoning,
}

// Parameters that cause a 400 error on o-series reasoning models.
const O_SERIES_UNSUPPORTED_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "n",
];

fn classify_model(model: &str) -> ModelFamily {
    // o-series: model string starts with 'o' followed immediately by a digit.
    // Covers o1, o1-mini, o3, o3-mini, o4-mini, etc.
    let bytes = model.trim().as_bytes();
    if matches!(bytes.first(), Some(b'o'))
        && matches!(bytes.get(1), Some(b) if b.is_ascii_digit())
    {
        return ModelFamily::OSeriesReasoning;
    }
    ModelFamily::GptClassic
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiSurface {
    ChatCompletions,
    Responses,
}

/// Major version following a `gpt-` prefix, e.g. "gpt-5.2-codex" -> Some(5),
/// "gpt-4.1" -> Some(4), "o3" -> None (no `gpt-` prefix), "gpt-turbo" -> None.
fn gpt_major_version(model: &str) -> Option<u32> {
    let rest = model.trim().strip_prefix("gpt-")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Selects which OpenAI HTTP API/JSON shape to target.
///
/// o-series reasoning models always use Chat Completions, independent of any
/// version-like number in the name. Otherwise, gpt-<N> with N >= 5 uses the
/// Responses API; everything else (gpt-4.x, gpt-3.5-*, unknown/non-numbered
/// names) uses Chat Completions.
fn classify_api_surface(model: &str, family: ModelFamily) -> ApiSurface {
    if family == ModelFamily::OSeriesReasoning {
        return ApiSurface::ChatCompletions;
    }
    match gpt_major_version(model) {
        Some(major) if major >= 5 => ApiSurface::Responses,
        _ => ApiSurface::ChatCompletions,
    }
}

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
    /// Held continuation id, injected by the host (reserved `continuation_id`
    /// key). Present only when the host holds a stored provider response id for
    /// this context; when present, `messages` is already the host-sliced
    /// incremental tail (see `translate_murmur_request_to_responses`). Absent on
    /// a first call or right after the host drops the id on a replace-context
    /// commit, in which case `messages` is the full context and we full-resend.
    #[serde(default)]
    continuation_id: Option<String>,
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
    // Reasoning persisted by a prior driver response. OpenAI chat-completions has no
    // request-side slot for reasoning, so these blocks are ignored when translating
    // history back into a request (no round-trip requirement, unlike Anthropic/DeepSeek).
    Thinking {
        #[allow(dead_code)]
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

// ── Server-side response retention (`store`) governance ───────────────────────
//
// Setting `store: true` on a Responses request makes OpenAI retain the response
// server-side (retrievable by `id`, ~30 days) — a data-handling implication in
// the same category as this project's shell / network / filesystem grants, which
// are always *explicit* opt-ins rather than driver-internal defaults. So retention
// is NOT an unconditional default here: it is gated behind an explicit capsule-level
// grant, supplied through the existing `inference.driver.config` manifest object,
// which the host serializes to the `MURMUR_INFERENCE_DRIVER_CONFIG` env var and never
// puts on the wire. Opt in with:
//
//   inference:
//     driver:
//       artifact: murmur-driver-openai
//       config: { store: true }
//
// When unset (the default), `store` stays `false`, no continuation id is returned,
// and no `previous_response_id` is ever sent — byte-identical to pre-v0.5.16 behavior.
// Because a first call must be stored for its id to be chainable on the *next* call,
// retention can't be conditioned on "a continuation is already active"; the capsule
// author's grant is the gate instead. Enabling `store` is what unlocks the whole
// continuation feature (the host only ever re-supplies an id this driver first
// returned), so this single flag governs both retention and continuation.

/// Whether the capsule opted into server-side response retention (`store: true`).
/// Parses the `MURMUR_INFERENCE_DRIVER_CONFIG` JSON object for a boolean `store`
/// field; anything missing, malformed, or non-boolean is treated as opt-out.
fn store_opt_in(driver_config: Option<&str>) -> bool {
    driver_config
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|cfg| cfg.get("store").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn translate_murmur_request_to_openai(
    request: &MurmurRequest,
    family: ModelFamily,
) -> Result<Value, String> {
    let mut messages = Vec::new();

    if let Some(system) = request
        .system
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
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
            if let Some(description) = tool
                .description
                .as_ref()
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())
            {
                function.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }

            json!({
                "type": "function",
                "function": function,
            })
        })
        .collect::<Vec<_>>();

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("max_completion_tokens".to_string(), Value::from(request.max_tokens));
    body.insert("messages".to_string(), Value::Array(messages));

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    for (key, value) in &request.params {
        if body.contains_key(key) {
            continue;
        }
        if family == ModelFamily::OSeriesReasoning
            && O_SERIES_UNSUPPORTED_PARAMS.contains(&key.as_str())
        {
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
                MurmurContentBlock::Text { text } => Some(json!({
                    "type": "text",
                    "text": text,
                })),
                MurmurContentBlock::Image { source } => Some(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", source.media_type, source.data),
                    }
                })),
                _ => None,
            })
            .collect::<Vec<_>>();

        json!({
            "role": "user",
            "content": parts,
        })
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

        json!({
            "role": "user",
            "content": text,
        })
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
                "function": {
                    "name": name,
                    "arguments": arguments,
                }
            }));
        }
    }

    if tool_calls.is_empty() {
        Ok(json!({
            "role": "assistant",
            "content": text,
        }))
    } else {
        Ok(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": tool_calls,
        }))
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

// ── Responses API: request translation ───────────────────────────────────────

/// Translate a murmur request into an OpenAI Responses API body.
///
/// `store` is the capsule's retention grant (see `store_opt_in`). A continuation
/// is *active* only when `store` is granted AND the host supplied a non-empty
/// `continuation_id`; the host guarantees it only ever re-supplies an id this
/// driver previously returned, so this can't fire without a prior opt-in.
///
/// On the continuation path we send `previous_response_id` and DROP assistant
/// messages from the input: the host slices `messages` to the incremental tail,
/// which after a tool-call turn is `[assistant(function_call…), tool_result…]`,
/// and that assistant turn (its text, function_calls, AND its reasoning items)
/// already lives server-side under the referenced response. Re-sending it would
/// duplicate server-side items; sending only the new user/tool items is the
/// canonical Responses continuation shape — and it is exactly what lets reasoning
/// items round-trip (they stay server-side and are never re-serialized here,
/// which the Chat Completions path structurally cannot do).
fn translate_murmur_request_to_responses(
    request: &MurmurRequest,
    store: bool,
) -> Result<Value, String> {
    let mut input = Vec::new();

    // Continuation is gated on the retention grant; an id without the grant is ignored.
    let continuation_id = if store {
        request
            .continuation_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
    } else {
        None
    };
    let continuation_active = continuation_id.is_some();

    for message in &request.messages {
        match message.role.as_str() {
            "user" => input.push(translate_user_message_responses(message)),
            // On the continuation path the assistant turn is already retained
            // server-side under `previous_response_id`; skip it to avoid
            // duplicating items (and to let its reasoning items round-trip).
            "assistant" if continuation_active => {}
            "assistant" => input.extend(translate_assistant_message_responses(message)?),
            "tool" => input.push(translate_tool_message_responses(message)?),
            other => return Err(format!("driver: unsupported message role '{other}'")),
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut obj = Map::new();
            obj.insert("type".to_string(), Value::String("function".to_string()));
            obj.insert("name".to_string(), Value::String(tool.name.clone()));
            obj.insert("parameters".to_string(), tool.parameters.clone());
            if let Some(description) = tool
                .description
                .as_ref()
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())
            {
                obj.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            Value::Object(obj)
        })
        .collect::<Vec<_>>();

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("input".to_string(), Value::Array(input));
    body.insert(
        "max_output_tokens".to_string(),
        Value::from(request.max_tokens),
    );
    body.insert("store".to_string(), Value::Bool(store));

    if let Some(id) = continuation_id {
        body.insert(
            "previous_response_id".to_string(),
            Value::String(id.to_string()),
        );
    }

    if let Some(system) = request
        .system
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        body.insert("instructions".to_string(), Value::String(system.to_string()));
    }

    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    for (key, value) in &request.params {
        if body.contains_key(key) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }

    Ok(Value::Object(body))
}

fn translate_user_message_responses(message: &MurmurMessage) -> Value {
    let parts = message
        .content
        .iter()
        .filter_map(|block| match block {
            MurmurContentBlock::Text { text } => Some(json!({
                "type": "input_text",
                "text": text,
            })),
            MurmurContentBlock::Image { source } => Some(json!({
                "type": "input_image",
                "image_url": format!("data:{};base64,{}", source.media_type, source.data),
            })),
            _ => None,
        })
        .collect::<Vec<_>>();

    json!({
        "role": "user",
        "content": parts,
    })
}

fn translate_assistant_message_responses(message: &MurmurMessage) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();

    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            MurmurContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

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

fn translate_tool_message_responses(message: &MurmurMessage) -> Result<Value, String> {
    let call_id = message.tool_call_id.clone().ok_or_else(|| {
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
        "type": "function_call_output",
        "call_id": call_id,
        "output": text,
    }))
}

fn translate_openai_response_to_murmur(response: &Value) -> Result<Value, String> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "driver: OpenAI response missing choices[0]".to_string())?;

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");

    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_call",
        "length" => "max_tokens",
        "content_filter" => {
            return Ok(json!({
                "stop_reason": "error",
                "error": "OpenAI response blocked by content_filter",
            }));
        }
        other => {
            return Err(format!(
                "driver: unsupported OpenAI finish_reason '{other}'"
            ));
        }
    };

    let message = choice.get("message").unwrap_or(&Value::Null);

    let mut content = Vec::new();
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let arguments_raw = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");

            let arguments: Value = serde_json::from_str(arguments_raw).map_err(|err| {
                format!("driver: failed to parse OpenAI tool call arguments JSON: {err}")
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
    }

    if content.is_empty() {
        match message.get("content") {
            Some(Value::String(text)) => {
                content.push(json!({"type": "text", "text": text}));
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            content.push(json!({"type": "text", "text": text}));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(json!({
        "stop_reason": stop_reason,
        "content": content,
    }))
}

// ── Responses API: non-streaming response translation ────────────────────────

/// Result of mapping a Responses `status` (+ incomplete/error details) to a
/// murmur stop_reason. `Error` carries a ready-to-return error payload so both
/// the non-streaming and streaming call sites short-circuit identically.
enum ResponsesStopReason {
    EndTurn,
    ToolCall,
    MaxTokens,
    Error(Value),
}

impl ResponsesStopReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::ToolCall => "tool_call",
            Self::MaxTokens => "max_tokens",
            Self::Error(_) => "error",
        }
    }

    fn into_error(self) -> Option<Value> {
        match self {
            Self::Error(v) => Some(v),
            _ => None,
        }
    }
}

fn responses_stop_reason(
    status: &str,
    has_function_call: bool,
    incomplete_reason: Option<&str>,
) -> Result<ResponsesStopReason, String> {
    match status {
        "completed" => Ok(if has_function_call {
            ResponsesStopReason::ToolCall
        } else {
            ResponsesStopReason::EndTurn
        }),
        "incomplete" => match incomplete_reason {
            Some("max_output_tokens") => Ok(ResponsesStopReason::MaxTokens),
            other => Ok(ResponsesStopReason::Error(error_payload(&format!(
                "OpenAI Responses incomplete: {}",
                other.unwrap_or("unknown reason")
            )))),
        },
        "failed" => Ok(ResponsesStopReason::Error(error_payload(
            "OpenAI Responses request failed",
        ))),
        other => Err(format!(
            "driver: unsupported OpenAI Responses status '{other}'"
        )),
    }
}

fn translate_responses_to_murmur(response: &Value) -> Result<Value, String> {
    if let Some(err) = response.get("error").filter(|e| !e.is_null()) {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Ok(error_payload(&format!("OpenAI Responses error: {msg}")));
    }

    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let has_function_call = output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));

    let incomplete_reason = response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);

    let stop_reason = responses_stop_reason(status, has_function_call, incomplete_reason)?;
    let stop_reason_str = stop_reason.as_str();
    if let Some(err_payload) = stop_reason.into_error() {
        return Ok(err_payload);
    }

    let mut content = Vec::new();
    for item in &output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    content.push(json!({"type": "text", "text": text}));
                                }
                            }
                            Some("refusal") => {
                                let msg = part
                                    .get("refusal")
                                    .and_then(Value::as_str)
                                    .unwrap_or("response refused");
                                return Ok(error_payload(&format!(
                                    "OpenAI response refused: {msg}"
                                )));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("function_call") => {
                let arguments_raw = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                let arguments: Value = serde_json::from_str(arguments_raw).map_err(|err| {
                    format!("driver: failed to parse Responses function_call arguments JSON: {err}")
                })?;
                content.push(json!({
                    "type": "tool_call",
                    "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                    "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "input": arguments,
                }));
            }
            // "reasoning" items and anything else are intentionally dropped — no
            // request-side round-trip slot for reasoning in this driver (see
            // MurmurContentBlock::Thinking comment above).
            _ => {}
        }
    }

    Ok(json!({
        "stop_reason": stop_reason_str,
        "content": content,
    }))
}

// ── SSE streaming types and parsing ──────────────────────────────────────────

/// State machine for routing `<think>…</think>` content to a separate emit callback.
/// Handles tags that are split across SSE chunk boundaries.
struct ThinkingState {
    in_thinking: bool,
    /// Bytes that may be a partial `<think>` or `</think>` tag, held from the previous chunk.
    partial_tag: String,
}

impl ThinkingState {
    fn new() -> Self {
        Self { in_thinking: false, partial_tag: String::new() }
    }

    /// Process `content`, routing text to `emit_text` and thinking to `emit_thinking`.
    fn process(
        &mut self,
        content: &str,
        emit_text: &mut impl FnMut(&str),
        emit_thinking: &mut impl FnMut(&str),
    ) {
        if self.partial_tag.is_empty() {
            self.scan(content, emit_text, emit_thinking);
        } else {
            let mut combined = std::mem::take(&mut self.partial_tag);
            combined.push_str(content);
            self.scan(&combined, emit_text, emit_thinking);
        }
    }

    fn scan(
        &mut self,
        s: &str,
        emit_text: &mut impl FnMut(&str),
        emit_thinking: &mut impl FnMut(&str),
    ) {
        let target = if self.in_thinking { "</think>" } else { "<think>" };
        if let Some(pos) = s.find(target) {
            if pos > 0 {
                if self.in_thinking {
                    emit_thinking(&s[..pos]);
                } else {
                    emit_text(&s[..pos]);
                }
            }
            self.in_thinking = !self.in_thinking;
            self.scan(&s[pos + target.len()..], emit_text, emit_thinking);
        } else {
            // No complete tag. Find the longest suffix of `s` that is a strict prefix of
            // `target`, buffer it for the next chunk, and emit the rest now.
            let tag_b = target.as_bytes();
            let s_b = s.as_bytes();
            let max_partial = tag_b.len().min(s_b.len());
            let partial_len = (1..=max_partial)
                .rev()
                .find(|&n| s_b.ends_with(&tag_b[..n]))
                .unwrap_or(0);
            let emit_end = s_b.len() - partial_len;
            if emit_end > 0 {
                if self.in_thinking {
                    emit_thinking(&s[..emit_end]);
                } else {
                    emit_text(&s[..emit_end]);
                }
            }
            if partial_len > 0 {
                self.partial_tag = s[emit_end..].to_owned();
            }
        }
    }

    /// Flush any partial tag buffer after the stream ends. Emits in the current mode.
    fn flush(
        &mut self,
        emit_text: &mut impl FnMut(&str),
        emit_thinking: &mut impl FnMut(&str),
    ) {
        if !self.partial_tag.is_empty() {
            let partial = std::mem::take(&mut self.partial_tag);
            if self.in_thinking {
                emit_thinking(&partial);
            } else {
                emit_text(&partial);
            }
        }
    }
}

struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

/// Process one complete SSE line. Returns `true` when the stream is done (`[DONE]`).
///
/// Text content is routed through `thinking` and split between `emit_text` (which the
/// caller also uses to accumulate `text_acc`) and `emit_thinking`. Structured thinking
/// blocks in array-format content are dispatched directly to `emit_thinking`.
fn process_openai_sse_line(
    line: &str,
    tool_states: &mut Vec<ToolCallState>,
    stop_reason: &mut Option<String>,
    thinking: &mut ThinkingState,
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

    // Capture finish_reason (non-null only in the final delta line).
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

    // Text delta — handle both string and array content forms.
    match delta.get("content") {
        Some(Value::String(content)) if !content.is_empty() => {
            thinking.process(content, emit_text, emit_thinking);
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                thinking.process(t, emit_text, emit_thinking);
                            }
                        }
                    }
                    Some("thinking") | Some("reasoning") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                emit_thinking(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // Tool call fragments.
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

/// Parse a complete SSE body string (used in tests).
#[cfg(test)]
fn parse_openai_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, String> {
    let mut text_acc = String::new();
    let mut thinking_acc = String::new();
    let mut tool_states: Vec<ToolCallState> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut thinking = ThinkingState::new();

    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        let done = {
            let mut combined_text = |t: &str| { emit_text(t); text_acc.push_str(t); };
            let mut combined_thinking = |t: &str| { emit_thinking(t); thinking_acc.push_str(t); };
            process_openai_sse_line(line, &mut tool_states, &mut stop_reason, &mut thinking, &mut combined_text, &mut combined_thinking)
        };
        if done { break; }
    }
    {
        let mut combined_text = |t: &str| { emit_text(t); text_acc.push_str(t); };
        let mut combined_thinking = |t: &str| { emit_thinking(t); thinking_acc.push_str(t); };
        thinking.flush(&mut combined_text, &mut combined_thinking);
    }

    assemble_openai_streaming_response(&text_acc, &thinking_acc, tool_states, stop_reason)
}

/// Parse a complete Responses API SSE body string (used in tests).
#[cfg(test)]
fn parse_responses_sse_body<F: FnMut(&str), G: FnMut(&str)>(
    body: &str,
    emit_text: &mut F,
    emit_thinking: &mut G,
) -> Result<Value, String> {
    let mut text_acc = String::new();
    let mut thinking_acc = String::new();
    let mut tool_states: Vec<ToolCallState> = Vec::new();
    let mut tool_index_by_output_index = std::collections::HashMap::new();
    let mut status: Option<String> = None;
    let mut incomplete_reason: Option<String> = None;
    let mut error_message: Option<String> = None;
    let mut response_id: Option<String> = None;

    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        let done = {
            let mut combined_text = |t: &str| { emit_text(t); text_acc.push_str(t); };
            let mut combined_thinking = |t: &str| { emit_thinking(t); thinking_acc.push_str(t); };
            process_responses_sse_line(
                line,
                &mut tool_states,
                &mut tool_index_by_output_index,
                &mut status,
                &mut incomplete_reason,
                &mut error_message,
                &mut response_id,
                &mut combined_text,
                &mut combined_thinking,
            )
        };
        if done { break; }
    }

    assemble_responses_streaming_response(
        &text_acc,
        &thinking_acc,
        tool_states,
        status,
        incomplete_reason,
        error_message,
    )
}

fn assemble_openai_streaming_response(
    text_acc: &str,
    thinking_acc: &str,
    tool_states: Vec<ToolCallState>,
    stop_reason: Option<String>,
) -> Result<Value, String> {
    let stop_reason_str = match stop_reason.as_deref() {
        Some("stop") | None => "end_turn",
        Some("tool_calls") => "tool_call",
        Some("length") => "max_tokens",
        Some("content_filter") => {
            return Ok(error_payload("OpenAI response blocked by content_filter"));
        }
        Some(other) => {
            return Err(format!("driver: unsupported OpenAI finish_reason '{other}'"));
        }
    };

    let mut tool_content = Vec::new();
    for state in tool_states {
        if state.id.is_empty() && state.name.is_empty() {
            continue;
        }
        let input: Value = serde_json::from_str(&state.arguments).map_err(|err| {
            format!("driver: failed to parse OpenAI tool call arguments JSON: {err}")
        })?;
        tool_content.push(json!({
            "type": "tool_call",
            "id": state.id,
            "name": state.name,
            "input": input,
        }));
    }

    Ok(json!({
        "stop_reason": stop_reason_str,
        "content": assemble_content_tail(thinking_acc, tool_content, text_acc),
    }))
}

fn error_payload(message: &str) -> Value {
    json!({
        "stop_reason": "error",
        "error": message,
    })
}

/// Shared tail: orders content blocks as thinking (if any) first, then either
/// tool_call blocks or a single text block. Used by both Chat Completions and
/// Responses streaming assembly.
fn assemble_content_tail(thinking_acc: &str, tool_content: Vec<Value>, text_acc: &str) -> Vec<Value> {
    let mut content = Vec::new();
    if !thinking_acc.is_empty() {
        content.push(json!({"type": "thinking", "text": thinking_acc}));
    }
    if !tool_content.is_empty() {
        content.extend(tool_content);
    } else if !text_acc.is_empty() {
        content.push(json!({"type": "text", "text": text_acc}));
    }
    content
}

// ── Responses API: streaming ──────────────────────────────────────────────────

/// Process one complete SSE line of Responses API typed events. Returns `true`
/// when the stream is done (`data: [DONE]`, if the provider sends one).
#[allow(clippy::too_many_arguments)]
fn process_responses_sse_line(
    line: &str,
    tool_states: &mut Vec<ToolCallState>,
    tool_index_by_output_index: &mut std::collections::HashMap<u64, usize>,
    status: &mut Option<String>,
    incomplete_reason: &mut Option<String>,
    error_message: &mut Option<String>,
    response_id: &mut Option<String>,
    emit_text: &mut impl FnMut(&str),
    emit_thinking: &mut impl FnMut(&str),
) -> bool {
    let Some(json_str) = line.strip_prefix("data: ") else {
        return false;
    };
    if json_str == "[DONE]" {
        return true;
    }
    let Ok(data) = serde_json::from_str::<Value>(json_str) else {
        return false;
    };
    let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");

    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit_text(delta);
                }
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit_thinking(delta);
                }
            }
        }
        "response.output_item.added" => {
            let item = data.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let output_index = data.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                let idx = tool_states.len();
                tool_states.push(ToolCallState {
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: String::new(),
                });
                tool_index_by_output_index.insert(output_index, idx);
            }
        }
        "response.function_call_arguments.delta" => {
            let output_index = data.get("output_index").and_then(Value::as_u64).unwrap_or(0);
            if let (Some(&idx), Some(delta)) = (
                tool_index_by_output_index.get(&output_index),
                data.get("delta").and_then(Value::as_str),
            ) {
                tool_states[idx].arguments.push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            let output_index = data.get("output_index").and_then(Value::as_u64).unwrap_or(0);
            if let (Some(&idx), Some(arguments)) = (
                tool_index_by_output_index.get(&output_index),
                data.get("arguments").and_then(Value::as_str),
            ) {
                tool_states[idx].arguments = arguments.to_string();
            }
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            let response_obj = data.get("response").unwrap_or(&Value::Null);
            *status = response_obj
                .get("status")
                .and_then(Value::as_str)
                .or_else(|| Some(event_type.trim_start_matches("response.")))
                .map(String::from);
            if let Some(id) = response_obj.get("id").and_then(Value::as_str) {
                *response_id = Some(id.to_string());
            }
            *incomplete_reason = response_obj
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(String::from);
            let err = response_obj
                .get("error")
                .filter(|e| !e.is_null())
                .or_else(|| data.get("error").filter(|e| !e.is_null()));
            if let Some(err) = err {
                *error_message = Some(
                    err.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                );
            }
        }
        _ => {}
    }

    false
}

fn assemble_responses_streaming_response(
    text_acc: &str,
    thinking_acc: &str,
    tool_states: Vec<ToolCallState>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    error_message: Option<String>,
) -> Result<Value, String> {
    if let Some(msg) = error_message {
        return Ok(error_payload(&format!("OpenAI Responses error: {msg}")));
    }

    let has_function_call = !tool_states.is_empty();
    let stop_reason = responses_stop_reason(
        status.as_deref().unwrap_or("completed"),
        has_function_call,
        incomplete_reason.as_deref(),
    )?;
    let stop_reason_str = stop_reason.as_str();
    if let Some(err_payload) = stop_reason.into_error() {
        return Ok(err_payload);
    }

    let mut tool_content = Vec::new();
    for state in tool_states {
        let input: Value = serde_json::from_str(&state.arguments).map_err(|err| {
            format!("driver: failed to parse Responses function_call arguments JSON: {err}")
        })?;
        tool_content.push(json!({
            "type": "tool_call",
            "id": state.id,
            "name": state.name,
            "input": input,
        }));
    }

    Ok(json!({
        "stop_reason": stop_reason_str,
        "content": assemble_content_tail(thinking_acc, tool_content, text_acc),
    }))
}

#[cfg(target_arch = "wasm32")]
mod wasm_driver {
    use super::{
        assemble_openai_streaming_response, assemble_responses_streaming_response,
        classify_api_surface, classify_model, error_payload, process_openai_sse_line,
        process_responses_sse_line, store_opt_in, translate_murmur_request_to_openai,
        translate_murmur_request_to_responses, translate_openai_response_to_murmur,
        translate_responses_to_murmur, ApiSurface, MurmurRequest, ThinkingState, ToolCallState,
        CONTINUATION_ID_KEY,
    };
    use std::collections::HashMap;
    use serde_json::{json, Value};

    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "driver",
        generate_all,
    });

    pub struct OpenAiDriver;

    impl exports::murmur::tool::run::Guest for OpenAiDriver {
        fn run(
            input: exports::murmur::tool::run::ToolInput,
        ) -> exports::murmur::tool::run::ToolResult {
            let (response, continuation_id) = match run_inner(input) {
                Ok(pair) => pair,
                Err(err) => (error_payload(&err), None),
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

            // Hand the provider response id back to the host on the reserved
            // `continuation_id` metadata key so it can persist it and chain the
            // next call. Only populated when retention (`store`) was granted and
            // the response succeeded (see `run_inner`); otherwise the list is
            // empty and the host holds no id — byte-identical to pre-v0.5.16.
            let metadata = match continuation_id {
                Some(id) => vec![(CONTINUATION_ID_KEY.to_string(), id)],
                None => vec![],
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

    /// Returns the murmur response plus, on the Responses surface with retention
    /// granted, the provider response `id` to hand back on the `continuation_id`
    /// metadata key. The id is `None` on the Chat Completions surface, without a
    /// `store` grant, or on an error response.
    fn run_inner(
        input: exports::murmur::tool::run::ToolInput,
    ) -> Result<(Value, Option<String>), String> {
        let endpoint = std::env::var("MURMUR_INFERENCE_ENDPOINT")
            .map_err(|_| "driver: missing MURMUR_INFERENCE_ENDPOINT".to_string())?;
        let api_key = std::env::var("MURMUR_INFERENCE_API_KEY").ok();
        // Capsule-level server-side retention grant (see `store_opt_in`). Gates
        // `store: true` and, transitively, the whole continuation feature.
        let store = store_opt_in(std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").ok().as_deref());

        let raw = input
            .data
            .ok_or_else(|| "driver: missing tool-input.data".to_string())?;

        let murmur_request: MurmurRequest = serde_json::from_str(&raw)
            .map_err(|err| format!("driver: failed to parse tool-input.data: {err}"))?;

        let family = classify_model(&murmur_request.model);
        let surface = classify_api_surface(&murmur_request.model, family);

        let (mut provider_request, url_suffix) = match surface {
            ApiSurface::ChatCompletions => (
                translate_murmur_request_to_openai(&murmur_request, family)?,
                "chat/completions",
            ),
            ApiSurface::Responses => (
                translate_murmur_request_to_responses(&murmur_request, store)?,
                "responses",
            ),
        };

        // Force streaming on; overrides any 'stream' key from params.
        if let Some(obj) = provider_request.as_object_mut() {
            obj.insert("stream".to_string(), json!(true));
        }

        let body = serde_json::to_vec(&provider_request)
            .map_err(|err| format!("driver: failed to encode request body: {err}"))?;

        let url = format!("{}/{}", endpoint.trim_end_matches('/'), url_suffix);

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
            return Ok((error_payload(&format!("HTTP {status}: {text}")), None));
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

        // `provider_response_id` is the raw Responses `id` (streaming or not);
        // `result` is the translated murmur response.
        let (result, provider_response_id): (Value, Option<String>) = if is_json {
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
                .map_err(|err| format!("driver: failed to parse OpenAI response JSON: {err}"))?;
            match surface {
                ApiSurface::ChatCompletions => {
                    (translate_openai_response_to_murmur(&json)?, None)
                }
                ApiSurface::Responses => (
                    translate_responses_to_murmur(&json)?,
                    json.get("id").and_then(Value::as_str).map(String::from),
                ),
            }
        } else {
            match surface {
                ApiSurface::ChatCompletions => {
                    (run_chat_completions_sse(stream, &first, incoming_body)?, None)
                }
                ApiSurface::Responses => run_responses_sse(stream, &first, incoming_body)?,
            }
        };

        // Only surface a continuation id when retention was granted and the
        // response succeeded — an errored/empty response is not chainable.
        let continuation_id = if store
            && surface == ApiSurface::Responses
            && result.get("stop_reason").and_then(Value::as_str) != Some("error")
        {
            provider_response_id.filter(|id| !id.is_empty())
        } else {
            None
        };

        Ok((result, continuation_id))
    }

    /// SSE streaming for the Chat Completions surface: process lines incrementally,
    /// emitting chunks as they arrive.
    fn run_chat_completions_sse(
        stream: wasip2::io::streams::InputStream,
        first: &[u8],
        incoming_body: wasip2::http::types::IncomingBody,
    ) -> Result<Value, String> {
        let mut line_buf: Vec<u8> = Vec::new();
        let mut text_acc = String::new();
        let mut thinking_acc = String::new();
        let mut tool_states: Vec<ToolCallState> = Vec::new();
        let mut stop_reason: Option<String> = None;
        let mut thinking = ThinkingState::new();
        let mut done = false;

        // Process bytes already read.
        for &b in first {
            if b == b'\n' {
                let line = String::from_utf8_lossy(&line_buf);
                let line = line.trim_end_matches('\r');
                {
                    let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
                    let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                    done = process_openai_sse_line(
                        line,
                        &mut tool_states,
                        &mut stop_reason,
                        &mut thinking,
                        &mut emit_t,
                        &mut emit_think,
                    );
                }
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
                        {
                            let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
                            let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                            done = process_openai_sse_line(
                                line,
                                &mut tool_states,
                                &mut stop_reason,
                                &mut thinking,
                                &mut emit_t,
                                &mut emit_think,
                            );
                        }
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
        {
            let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
            let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
            thinking.flush(&mut emit_t, &mut emit_think);
        }
        assemble_openai_streaming_response(&text_acc, &thinking_acc, tool_states, stop_reason)
    }

    /// SSE streaming for the Responses surface: process lines incrementally,
    /// emitting chunks as they arrive.
    fn run_responses_sse(
        stream: wasip2::io::streams::InputStream,
        first: &[u8],
        incoming_body: wasip2::http::types::IncomingBody,
    ) -> Result<(Value, Option<String>), String> {
        let mut line_buf: Vec<u8> = Vec::new();
        let mut text_acc = String::new();
        let mut thinking_acc = String::new();
        let mut tool_states: Vec<ToolCallState> = Vec::new();
        let mut tool_index_by_output_index: HashMap<u64, usize> = HashMap::new();
        let mut status: Option<String> = None;
        let mut incomplete_reason: Option<String> = None;
        let mut error_message: Option<String> = None;
        let mut response_id: Option<String> = None;
        let mut done = false;

        // Process bytes already read.
        for &b in first {
            if b == b'\n' {
                let line = String::from_utf8_lossy(&line_buf);
                let line = line.trim_end_matches('\r');
                {
                    let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
                    let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                    done = process_responses_sse_line(
                        line,
                        &mut tool_states,
                        &mut tool_index_by_output_index,
                        &mut status,
                        &mut incomplete_reason,
                        &mut error_message,
                        &mut response_id,
                        &mut emit_t,
                        &mut emit_think,
                    );
                }
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
                        {
                            let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
                            let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                            done = process_responses_sse_line(
                                line,
                                &mut tool_states,
                                &mut tool_index_by_output_index,
                                &mut status,
                                &mut incomplete_reason,
                                &mut error_message,
                                &mut response_id,
                                &mut emit_t,
                                &mut emit_think,
                            );
                        }
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
        let result = assemble_responses_streaming_response(
            &text_acc,
            &thinking_acc,
            tool_states,
            status,
            incomplete_reason,
            error_message,
        )?;
        Ok((result, response_id))
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

    fn read_chunk(
        stream: &wasip2::io::streams::InputStream,
    ) -> Result<Vec<u8>, String> {
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
                Some(Err(())) => return Err("driver: response future already consumed".to_string()),
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

    export!(OpenAiDriver);
}

#[cfg(test)]
mod tests {
    use super::{
        classify_api_surface, classify_model, gpt_major_version, parse_openai_sse_body,
        parse_responses_sse_body, process_responses_sse_line, store_opt_in,
        translate_murmur_request_to_openai, translate_murmur_request_to_responses,
        translate_openai_response_to_murmur, translate_responses_to_murmur, ApiSurface,
        ModelFamily, MurmurRequest, ToolCallState,
    };
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn translates_murmur_request_to_openai_chat_completions() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-4.1",
            "max_tokens": 512,
            "system": "You are helpful",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
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
                    "parameters": {"type": "object"}
                }
            ]
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_openai(&request, ModelFamily::GptClassic).unwrap();

        assert_eq!(translated["messages"][0]["role"], "system");
        assert_eq!(translated["messages"][1]["role"], "assistant");
        assert_eq!(
            translated["messages"][1]["content"],
            serde_json::Value::Null
        );
        assert_eq!(translated["messages"][1]["tool_calls"][0]["id"], "tc-1");
        assert_eq!(
            translated["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert_eq!(translated["messages"][2]["role"], "tool");
        assert_eq!(translated["messages"][2]["tool_call_id"], "tc-1");

        assert_eq!(translated["tools"][0]["type"], "function");
        assert_eq!(translated["tools"][0]["function"]["name"], "calc");
        assert_eq!(
            translated["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn translates_openai_response_to_murmur_format() {
        let openai = json!({
            "choices": [
                {
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "tc-1",
                                "type": "function",
                                "function": {
                                    "name": "calc",
                                    "arguments": "{\"x\":2}"
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let translated = translate_openai_response_to_murmur(&openai).unwrap();

        assert_eq!(translated["stop_reason"], "tool_call");
        assert_eq!(translated["content"][0]["type"], "tool_call");
        assert_eq!(translated["content"][0]["id"], "tc-1");
        assert_eq!(translated["content"][0]["name"], "calc");
        assert_eq!(translated["content"][0]["input"], json!({"x": 2}));
    }

    #[test]
    fn maps_content_filter_to_error_stop_reason() {
        let openai = json!({
            "choices": [
                {
                    "finish_reason": "content_filter",
                    "message": {"role": "assistant", "content": null}
                }
            ]
        });

        let translated = translate_openai_response_to_murmur(&openai).unwrap();
        assert_eq!(translated["stop_reason"], "error");
        assert!(translated["error"]
            .as_str()
            .unwrap()
            .contains("content_filter"));
    }

    #[test]
    fn classifies_gpt_models_as_classic() {
        for model in &[
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-4-turbo",
            "gpt-3.5-turbo",
            "gpt-5",
        ] {
            assert_eq!(
                classify_model(model),
                ModelFamily::GptClassic,
                "expected GptClassic for {model}"
            );
        }
    }

    #[test]
    fn classifies_o_series_as_reasoning() {
        for model in &[
            "o1",
            "o1-mini",
            "o1-preview",
            "o3",
            "o3-mini",
            "o4-mini",
        ] {
            assert_eq!(
                classify_model(model),
                ModelFamily::OSeriesReasoning,
                "expected OSeriesReasoning for {model}"
            );
        }
    }

    #[test]
    fn o_series_strips_unsupported_params() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "o3-mini",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "params": {
                "temperature": 0.7,
                "top_p": 0.9,
                "presence_penalty": 0.5,
                "frequency_penalty": 0.3,
                "reasoning_effort": "high",
                "stream": false
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_openai(&request, ModelFamily::OSeriesReasoning).unwrap();

        assert!(translated.get("temperature").is_none(), "temperature must be stripped");
        assert!(translated.get("top_p").is_none(), "top_p must be stripped");
        assert!(translated.get("presence_penalty").is_none(), "presence_penalty must be stripped");
        assert!(translated.get("frequency_penalty").is_none(), "frequency_penalty must be stripped");
        assert_eq!(translated["reasoning_effort"], "high");
        assert_eq!(translated["stream"], false);
    }

    #[test]
    fn gpt_classic_passes_temperature_through() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "max_tokens": 512,
            "messages": [],
            "params": {
                "temperature": 0.5,
                "top_p": 0.9
            }
        }))
        .unwrap();

        let translated =
            translate_murmur_request_to_openai(&request, ModelFamily::GptClassic).unwrap();

        assert_eq!(translated["temperature"], 0.5);
        assert_eq!(translated["top_p"], 0.9);
    }

    #[test]
    fn openai_streaming_text_response() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"He\"},\"finish_reason\":null}]}\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );

        let mut emitted: Vec<String> = Vec::new();
        let result = parse_openai_sse_body(body, &mut |chunk| emitted.push(chunk.to_string()), &mut |_| {})
            .unwrap();

        assert_eq!(emitted, vec!["He", "llo", " world"]);
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "Hello world");
    }

    #[test]
    fn openai_streaming_tool_call() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\"}}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        );

        let mut emitted: Vec<String> = Vec::new();
        let result = parse_openai_sse_body(body, &mut |chunk| emitted.push(chunk.to_string()), &mut |_| {})
            .unwrap();

        assert!(emitted.is_empty(), "no text chunks expected for tool calls");
        assert_eq!(result["stop_reason"], "tool_call");
        assert_eq!(result["content"][0]["type"], "tool_call");
        assert_eq!(result["content"][0]["id"], "call_abc");
        assert_eq!(result["content"][0]["name"], "bash");
        assert_eq!(result["content"][0]["input"], json!({"cmd": "ls"}));
    }

    #[test]
    fn thinking_tags_routed_to_thinking_callback() {
        // Typical DeepSeek R1 / Qwen pattern: response starts with <think>…</think>
        // then the actual answer follows.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>Let me\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" reason.\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"</think>\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"The answer is 42.\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );

        let mut text_chunks: Vec<String> = Vec::new();
        let mut thinking_chunks: Vec<String> = Vec::new();
        let result = parse_openai_sse_body(
            body,
            &mut |t| text_chunks.push(t.to_string()),
            &mut |t| thinking_chunks.push(t.to_string()),
        ).unwrap();

        assert_eq!(thinking_chunks.join(""), "Let me reason.");
        assert_eq!(text_chunks, vec!["The answer is 42."]);
        // Reasoning is persisted as a first-class thinking block, ahead of the text block.
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "Let me reason.");
        assert_eq!(result["content"][1]["type"], "text");
        assert_eq!(result["content"][1]["text"], "The answer is 42.");
        assert_eq!(result["stop_reason"], "end_turn");
    }

    #[test]
    fn thinking_tag_split_across_sse_chunks() {
        // The closing </think> tag is split: </thi in one chunk, nk> in the next.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>thoughts</thi\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk>answer\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );

        let mut text_chunks: Vec<String> = Vec::new();
        let mut thinking_chunks: Vec<String> = Vec::new();
        let result = parse_openai_sse_body(
            body,
            &mut |t| text_chunks.push(t.to_string()),
            &mut |t| thinking_chunks.push(t.to_string()),
        ).unwrap();

        assert_eq!(thinking_chunks.join(""), "thoughts");
        assert_eq!(text_chunks.join(""), "answer");
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "thoughts");
        assert_eq!(result["content"][1]["type"], "text");
        assert_eq!(result["content"][1]["text"], "answer");
    }

    #[test]
    fn thinking_tag_absent_passes_text_through_unchanged() {
        // Models that don't emit <think> tags should behave exactly as before.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );

        let mut text_chunks: Vec<String> = Vec::new();
        let mut thinking_chunks: Vec<String> = Vec::new();
        let result = parse_openai_sse_body(
            body,
            &mut |t| text_chunks.push(t.to_string()),
            &mut |t| thinking_chunks.push(t.to_string()),
        ).unwrap();

        assert!(thinking_chunks.is_empty());
        assert_eq!(text_chunks, vec!["Hello ", "world"]);
        assert_eq!(result["content"][0]["text"], "Hello world");
    }

    #[test]
    fn array_content_with_structured_thinking_block() {
        // Future OpenAI / provider format: delta.content is an array with typed blocks.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"text\":\"inner thought\"}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );

        let mut text_chunks: Vec<String> = Vec::new();
        let mut thinking_chunks: Vec<String> = Vec::new();
        let result = parse_openai_sse_body(
            body,
            &mut |t| text_chunks.push(t.to_string()),
            &mut |t| thinking_chunks.push(t.to_string()),
        ).unwrap();

        assert_eq!(thinking_chunks, vec!["inner thought"]);
        assert_eq!(text_chunks, vec!["reply"]);
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][0]["text"], "inner thought");
        assert_eq!(result["content"][1]["type"], "text");
        assert_eq!(result["content"][1]["text"], "reply");
    }

    // ── Responses API: model routing ─────────────────────────────────────────

    #[test]
    fn gpt_major_version_extracts_leading_digits() {
        assert_eq!(gpt_major_version("gpt-5"), Some(5));
        assert_eq!(gpt_major_version("gpt-5-mini"), Some(5));
        assert_eq!(gpt_major_version("gpt-5.1"), Some(5));
        assert_eq!(gpt_major_version("gpt-5.2-codex"), Some(5));
        assert_eq!(gpt_major_version("gpt-4.1"), Some(4));
        assert_eq!(gpt_major_version("gpt-4o"), Some(4));
        assert_eq!(gpt_major_version("gpt-3.5-turbo"), Some(3));
        assert_eq!(gpt_major_version("gpt-turbo"), None);
        assert_eq!(gpt_major_version("gpt-"), None);
        assert_eq!(gpt_major_version("o3"), None);
        assert_eq!(gpt_major_version(""), None);
    }

    #[test]
    fn classifies_gpt5_and_above_as_responses() {
        for model in &["gpt-5", "gpt-5-mini", "gpt-5.1", "gpt-5.2-codex"] {
            assert_eq!(
                classify_api_surface(model, ModelFamily::GptClassic),
                ApiSurface::Responses,
                "expected Responses for {model}"
            );
        }
    }

    #[test]
    fn classifies_below_gpt5_as_chat_completions() {
        for model in &["gpt-4.1", "gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"] {
            assert_eq!(
                classify_api_surface(model, ModelFamily::GptClassic),
                ApiSurface::ChatCompletions,
                "expected ChatCompletions for {model}"
            );
        }
    }

    #[test]
    fn o_series_always_chat_completions_even_if_version_like() {
        for model in &["o1", "o3", "o4-mini"] {
            assert_eq!(
                classify_api_surface(model, ModelFamily::OSeriesReasoning),
                ApiSurface::ChatCompletions,
                "expected ChatCompletions for {model}"
            );
        }
    }

    #[test]
    fn unknown_model_name_defaults_to_chat_completions() {
        for model in &["custom-model-x", ""] {
            assert_eq!(
                classify_api_surface(model, ModelFamily::GptClassic),
                ApiSurface::ChatCompletions,
                "expected ChatCompletions for {model}"
            );
        }
    }

    // ── Responses API: request translation ───────────────────────────────────

    #[test]
    fn translates_system_to_instructions() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "system": "You are helpful",
            "messages": []
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();
        assert_eq!(translated["instructions"], "You are helpful");

        let request_empty: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "system": "   ",
            "messages": []
        }))
        .unwrap();
        let translated_empty = translate_murmur_request_to_responses(&request_empty, false).unwrap();
        assert!(translated_empty.get("instructions").is_none());

        let request_absent: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": []
        }))
        .unwrap();
        let translated_absent = translate_murmur_request_to_responses(&request_absent, false).unwrap();
        assert!(translated_absent.get("instructions").is_none());
    }

    #[test]
    fn translates_tools_to_flat_responses_shape() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [],
            "tools": [
                {"name": "calc", "description": "calculate", "parameters": {"type": "object"}}
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();
        assert_eq!(translated["tools"][0]["type"], "function");
        assert_eq!(translated["tools"][0]["name"], "calc");
        assert_eq!(translated["tools"][0]["description"], "calculate");
        assert_eq!(translated["tools"][0]["parameters"], json!({"type": "object"}));
        assert!(translated["tools"][0].get("function").is_none());
    }

    #[test]
    fn translates_multiturn_history_with_tool_call_round_trip() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_call", "id": "tc-1", "name": "calc", "input": {"x": 1}}
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "tc-1",
                    "content": [{"type": "text", "text": "2"}]
                }
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();

        assert_eq!(translated["input"][0]["type"], "function_call");
        assert_eq!(translated["input"][0]["call_id"], "tc-1");
        assert_eq!(translated["input"][0]["name"], "calc");
        assert_eq!(translated["input"][0]["arguments"], "{\"x\":1}");

        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(translated["input"][1]["call_id"], "tc-1");
        assert_eq!(translated["input"][1]["output"], "2");
    }

    #[test]
    fn assistant_message_with_text_and_tool_call_produces_two_items() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me check that."},
                        {"type": "tool_call", "id": "tc-1", "name": "calc", "input": {"x": 1}}
                    ]
                }
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();

        assert_eq!(translated["input"].as_array().unwrap().len(), 2);
        assert_eq!(translated["input"][0]["type"], "message");
        assert_eq!(translated["input"][0]["role"], "assistant");
        assert_eq!(translated["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(translated["input"][0]["content"][0]["text"], "Let me check that.");
        assert_eq!(translated["input"][1]["type"], "function_call");
        assert_eq!(translated["input"][1]["call_id"], "tc-1");
    }

    #[test]
    fn translates_user_image_content_to_input_image_part() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "What is this?"},
                        {"type": "image", "source": {"media_type": "image/png", "data": "abc123"}}
                    ]
                }
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();

        assert_eq!(translated["input"][0]["role"], "user");
        assert_eq!(translated["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(translated["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            translated["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,abc123"
        );
    }

    #[test]
    fn max_output_tokens_set_and_store_defaults_false_when_not_opted_in() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 777,
            "messages": []
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();
        assert_eq!(translated["max_output_tokens"], 777);
        assert_eq!(translated["store"], false);
        assert!(translated.get("previous_response_id").is_none());
    }

    // ── Server-side retention (`store`) governance + continuation wiring ───────

    #[test]
    fn store_opt_in_parses_driver_config() {
        assert!(store_opt_in(Some(r#"{"store":true}"#)));
        assert!(!store_opt_in(Some(r#"{"store":false}"#)));
        // Missing key, empty object, malformed JSON, non-bool, and absent config
        // all mean opt-out — retention is never an accidental default.
        assert!(!store_opt_in(Some(r#"{}"#)));
        assert!(!store_opt_in(Some(r#"{"store":"true"}"#)));
        assert!(!store_opt_in(Some("not json")));
        assert!(!store_opt_in(None));
    }

    #[test]
    fn store_opt_in_sets_store_true_without_continuation_on_first_call() {
        // Opted in, but no continuation id yet (first call / post-drop full resend):
        // store is true so the response id becomes retrievable, full messages are
        // sent, and no previous_response_id is present.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, true).unwrap();
        assert_eq!(translated["store"], true);
        assert!(translated.get("previous_response_id").is_none());
        assert_eq!(translated["input"].as_array().unwrap().len(), 1);
        assert_eq!(translated["input"][0]["role"], "user");
    }

    #[test]
    fn continuation_sends_previous_response_id_and_drops_assistant_items() {
        // The host-sliced incremental tail after a tool-call turn:
        // [assistant(function_call), tool_result]. With a continuation active the
        // assistant turn is already server-side, so only the new tool result is
        // wired, plus previous_response_id.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "continuation_id": "resp_prev_123",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "let me compute"},
                        {"type": "tool_call", "id": "tc-1", "name": "calc", "input": {"x": 1}}
                    ]
                },
                {"role": "tool", "tool_call_id": "tc-1", "content": [{"type": "text", "text": "2"}]}
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, true).unwrap();

        assert_eq!(translated["store"], true);
        assert_eq!(translated["previous_response_id"], "resp_prev_123");
        let input = translated["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "assistant items must be dropped on continuation");
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "tc-1");
        assert_eq!(input[0]["output"], "2");
    }

    #[test]
    fn continuation_with_new_user_message_keeps_only_user_item() {
        // Same-context next Task (Scenario 7): tail is [assistant, user]; the
        // assistant is already retained, so only the new user message is wired.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "continuation_id": "resp_prev_456",
            "messages": [
                {"role": "assistant", "content": [{"type": "text", "text": "prior answer"}]},
                {"role": "user", "content": [{"type": "text", "text": "follow-up"}]}
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, true).unwrap();
        let input = translated["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "follow-up");
        assert_eq!(translated["previous_response_id"], "resp_prev_456");
    }

    #[test]
    fn continuation_id_ignored_when_store_not_opted_in() {
        // Defense in depth: a continuation id without the retention grant is
        // ignored — full translation, store false, no previous_response_id.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "continuation_id": "resp_prev_789",
            "messages": [
                {"role": "assistant", "content": [{"type": "text", "text": "prior"}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]}
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, false).unwrap();
        assert_eq!(translated["store"], false);
        assert!(translated.get("previous_response_id").is_none());
        // Full translation — assistant item is NOT dropped.
        assert_eq!(translated["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_continuation_id_is_treated_as_no_continuation() {
        // An empty/whitespace id is not a real continuation (matches the host's
        // empty-value-drops semantics) → full resend, no previous_response_id.
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "continuation_id": "  ",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]}
            ]
        }))
        .unwrap();
        let translated = translate_murmur_request_to_responses(&request, true).unwrap();
        assert!(translated.get("previous_response_id").is_none());
        assert_eq!(translated["store"], true);
        assert_eq!(translated["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn reasoning_items_round_trip_via_continuation_not_re_serialized() {
        // Reasoning continuity: turn 1 returns reasoning + a function_call; those
        // items live server-side under the response id. Turn 2's incremental tail
        // carries the assistant(function_call) + tool result, but on the
        // continuation path the assistant (and thus any reasoning the driver would
        // otherwise have to re-serialize, which it cannot) is dropped — the model
        // still sees the reasoning because previous_response_id chains it. This is
        // exactly what the Chat Completions path cannot do.
        let turn1: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "think hard"}]}
            ]
        }))
        .unwrap();
        let t1 = translate_murmur_request_to_responses(&turn1, true).unwrap();
        assert_eq!(t1["store"], true, "turn 1 must be stored for reasoning to persist");
        assert!(t1.get("previous_response_id").is_none());

        // Turn 2 chains via previous_response_id; the reasoning-bearing assistant
        // turn is not re-sent, only the new tool result is.
        let turn2: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "continuation_id": "resp_turn1",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "text": "some server-side reasoning"},
                        {"type": "tool_call", "id": "tc-9", "name": "calc", "input": {"x": 2}}
                    ]
                },
                {"role": "tool", "tool_call_id": "tc-9", "content": [{"type": "text", "text": "4"}]}
            ]
        }))
        .unwrap();
        let t2 = translate_murmur_request_to_responses(&turn2, true).unwrap();
        assert_eq!(t2["previous_response_id"], "resp_turn1");
        let input = t2["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "tc-9");
    }

    #[test]
    fn responses_streaming_captures_response_id_from_completed_event() {
        // The provider response id (returned to the host on the continuation
        // metadata key) is captured from the terminal streaming event.
        let mut tool_states: Vec<ToolCallState> = Vec::new();
        let mut idx: HashMap<u64, usize> = HashMap::new();
        let mut status: Option<String> = None;
        let mut incomplete_reason: Option<String> = None;
        let mut error_message: Option<String> = None;
        let mut response_id: Option<String> = None;
        let line = r#"data: {"type":"response.completed","response":{"id":"resp_stream_42","status":"completed"}}"#;
        let done = process_responses_sse_line(
            line,
            &mut tool_states,
            &mut idx,
            &mut status,
            &mut incomplete_reason,
            &mut error_message,
            &mut response_id,
            &mut |_| {},
            &mut |_| {},
        );
        assert!(!done);
        assert_eq!(response_id.as_deref(), Some("resp_stream_42"));
    }

    #[test]
    fn responses_tool_message_without_tool_call_id_errors() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {"role": "tool", "content": [{"type": "text", "text": "2"}]}
            ]
        }))
        .unwrap();
        assert!(translate_murmur_request_to_responses(&request, false).is_err());
    }

    #[test]
    fn responses_unsupported_message_role_errors() {
        let request: MurmurRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "max_tokens": 256,
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "hi"}]}
            ]
        }))
        .unwrap();
        assert!(translate_murmur_request_to_responses(&request, false).is_err());
    }

    // ── Responses API: non-streaming response translation ─────────────────────

    #[test]
    fn translates_responses_completed_text_to_murmur() {
        let response = json!({
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello!"}]}
            ]
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "end_turn");
        assert_eq!(translated["content"][0]["type"], "text");
        assert_eq!(translated["content"][0]["text"], "Hello!");
    }

    #[test]
    fn translates_responses_completed_function_call_to_tool_call_stop_reason() {
        let response = json!({
            "status": "completed",
            "output": [
                {"type": "function_call", "call_id": "call_abc", "name": "calc", "arguments": "{\"x\":2}"}
            ]
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "tool_call");
        assert_eq!(translated["content"][0]["type"], "tool_call");
        assert_eq!(translated["content"][0]["id"], "call_abc");
        assert_eq!(translated["content"][0]["name"], "calc");
        assert_eq!(translated["content"][0]["input"], json!({"x": 2}));
    }

    #[test]
    fn translates_responses_incomplete_max_output_tokens_to_max_tokens() {
        let response = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": []
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "max_tokens");
    }

    #[test]
    fn translates_responses_incomplete_unknown_reason_to_error() {
        let response = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "something_else"},
            "output": []
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "error");
    }

    #[test]
    fn translates_responses_failed_to_error() {
        let response = json!({"status": "failed", "output": []});
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "error");
    }

    #[test]
    fn translates_responses_top_level_error_object_to_error() {
        let response = json!({
            "error": {"message": "rate limited"},
            "output": []
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "error");
        assert!(translated["error"].as_str().unwrap().contains("rate limited"));
    }

    #[test]
    fn ignores_reasoning_items_in_responses_output() {
        let response = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "r1", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}
            ]
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["content"].as_array().unwrap().len(), 1);
        assert_eq!(translated["content"][0]["type"], "text");
        assert_eq!(translated["content"][0]["text"], "answer");
    }

    #[test]
    fn refusal_content_part_maps_to_error() {
        let response = json!({
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "refusal", "refusal": "cannot help with that"}]}
            ]
        });
        let translated = translate_responses_to_murmur(&response).unwrap();
        assert_eq!(translated["stop_reason"], "error");
        assert!(translated["error"].as_str().unwrap().contains("cannot help with that"));
    }

    // ── Responses API: streaming ────────────────────────────────────────────

    #[test]
    fn responses_streaming_output_text_delta_assembly() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"He\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let mut emitted: Vec<String> = Vec::new();
        let result = parse_responses_sse_body(body, &mut |t| emitted.push(t.to_string()), &mut |_| {})
            .unwrap();
        assert_eq!(emitted, vec!["He", "llo"]);
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "Hello");
    }

    #[test]
    fn responses_streaming_function_call_arguments_delta_assembles_tool_call() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\"}}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"cmd\\\":\"}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"ls\\\"}\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let result = parse_responses_sse_body(body, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(result["stop_reason"], "tool_call");
        assert_eq!(result["content"][0]["type"], "tool_call");
        assert_eq!(result["content"][0]["id"], "call_1");
        assert_eq!(result["content"][0]["name"], "bash");
        assert_eq!(result["content"][0]["input"], json!({"cmd": "ls"}));
    }

    #[test]
    fn responses_streaming_function_call_arguments_done_overwrites_partial_deltas() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\"}}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"cmd\\\":\"}\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let result = parse_responses_sse_body(body, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(result["content"][0]["input"], json!({"cmd": "ls"}));
    }

    #[test]
    fn responses_streaming_reasoning_summary_delta_routes_to_thinking() {
        let body = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Thinking...\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Answer.\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let mut text_chunks: Vec<String> = Vec::new();
        let mut thinking_chunks: Vec<String> = Vec::new();
        let result = parse_responses_sse_body(
            body,
            &mut |t| text_chunks.push(t.to_string()),
            &mut |t| thinking_chunks.push(t.to_string()),
        )
        .unwrap();
        assert_eq!(thinking_chunks, vec!["Thinking..."]);
        assert_eq!(text_chunks, vec!["Answer."]);
        assert_eq!(result["content"][0]["type"], "thinking");
        assert_eq!(result["content"][1]["type"], "text");
    }

    #[test]
    fn responses_streaming_incomplete_max_output_tokens() {
        let body = "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n";
        let result = parse_responses_sse_body(body, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(result["stop_reason"], "max_tokens");
    }

    #[test]
    fn responses_streaming_error_event_handling() {
        let body = "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"boom\"}}}\n";
        let result = parse_responses_sse_body(body, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(result["stop_reason"], "error");
        assert!(result["error"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn responses_streaming_multiple_tool_calls_keyed_by_output_index() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"a\"}}\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"b\"}}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"y\\\":2}\"}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"x\\\":1}\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let result = parse_responses_sse_body(body, &mut |_| {}, &mut |_| {}).unwrap();
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["id"], "call_1");
        assert_eq!(content[0]["name"], "a");
        assert_eq!(content[0]["input"], json!({"x": 1}));
        assert_eq!(content[1]["id"], "call_2");
        assert_eq!(content[1]["name"], "b");
        assert_eq!(content[1]["input"], json!({"y": 2}));
    }
}
