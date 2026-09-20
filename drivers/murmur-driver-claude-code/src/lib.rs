//! The process driver for Claude Code.
//!
//! Every fact about the `claude` CLI — its flags, the shape of its MCP config, how it spells a
//! bridged tool, the JSON line it reads off stdin, the control request that interrupts a turn,
//! and the variables it cannot run without — lives in this crate and nowhere else. The runtime
//! runs *a* process driver; it never knows it is running the Claude one.
//!
//! The driver is granted nothing: no environment, no filesystem, no network, no Murmur host
//! import. It is pure translation both ways — a `LaunchRequest` into the `LaunchPlan` that
//! spawns the harness, and the lines the harness prints into the events the runtime acts on.

use serde_json::{json, Value};

// ── What this driver drives ───────────────────────────────────────────────────

/// The harness name the runtime traces and reports this driver under.
const HARNESS: &str = "claude-code";

/// The executable the runtime resolves, unless the manifest overrides it with `command:`.
const BINARY: &str = "claude";

/// The arguments that make the harness print its version, for the runtime's version probe.
const VERSION_ARGS: &[&str] = &["--version"];

/// The `claude` releases this driver release was tested against. An untested version is a
/// warning at run time, never a refusal, so this list documents what was measured rather than
/// gating anything.
const TESTED_VERSIONS: &[&str] = &["2.1.277", "2.1.278"];

/// The variables `claude` cannot run without, each of which the capsule must declare in
/// `capabilities.env.allow` or be refused at load.
///
/// `HOME` locates the login store under `~/.claude` that a subscription run reads its credential
/// from. `PATH` is what an npm-shaped install — a launcher script that execs `node` — needs to
/// start at all, and what the runtime's own bare-name binary resolution walks.
const REQUIRED_ENV: &[&str] = &["HOME", "PATH"];

/// `parse` emits `text-delta` events: `--include-partial-messages` makes the harness stream them.
const STREAMS_TEXT: bool = true;

// ── Launch constants ──────────────────────────────────────────────────────────

/// The name of the MCP config file the driver asks the runtime to write, and points
/// `--mcp-config` at.
const MCP_CONFIG_FILE_NAME: &str = "mcp-config.json";

/// The token the runtime replaces with the path of the private `0700` directory it writes
/// `LaunchPlan::files` into.
const FILES_DIR_TOKEN: &str = "{files_dir}";

/// The prefix of the `request_id` on the interrupt control request. The id is derived from the
/// session id rather than generated: the driver is granted no randomness, stays host-testable,
/// and per-run uniqueness is the only scope the id has.
const INTERRUPT_REQUEST_ID_PREFIX: &str = "murmur-interrupt-";

// ── Mirrors of the WIT records ────────────────────────────────────────────────
//
// The `#[cfg(target_arch = "wasm32")]` adapter converts the generated WIT types to these and
// back. Every decision lives on this side of that gate, where `cargo test` reaches it.

/// How a turn in progress can be stopped gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMethod {
    StdinMessage,
    SignalInt,
    Unsupported,
}

/// What this driver drives, read once when the driver is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub harness: String,
    pub binary: String,
    pub version_args: Vec<String>,
    pub tested_versions: Vec<String>,
    pub interrupt: InterruptMethod,
    pub required_env: Vec<String>,
    pub streams_text: bool,
}

/// The endpoint through which the harness calls the capsule's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bridge {
    pub server_name: String,
    pub url: String,
    pub bearer_token: String,
    pub tool_names: Vec<String>,
}

/// Whether a turn starts a harness session or continues one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    New,
    Resume,
}

/// The harness session a turn runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub mode: SessionMode,
}

/// Everything the driver needs to plan one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub model: Option<String>,
    pub system_prompt: String,
    pub config: Option<String>,
    pub bridge: Option<Bridge>,
    pub session: Session,
    pub harness_version: Option<String>,
    pub task: String,
}

/// A file the runtime writes into the private files directory before spawning the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverFile {
    pub name: String,
    pub contents: String,
}

/// How to spawn the harness for one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub files: Vec<DriverFile>,
    pub stdin: Option<Vec<u8>>,
    pub keep_stdin_open: bool,
    pub interrupt_stdin: Option<Vec<u8>>,
}

/// The harness's report of the session it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub auth: String,
    pub model: Option<String>,
}

/// A tool the agent called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub input: String,
}

/// The result of a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultInfo {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

/// The harness is retrying a request to its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryInfo {
    pub attempt: u32,
    pub reason: String,
}

/// Why a turn failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Auth,
    Quota,
    MaxTurns,
    Canceled,
    HarnessError,
    Other,
}

/// A failed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    pub kind: FailureKind,
    pub message: String,
}

/// One thing the harness did, read out of its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    SessionStarted(SessionInfo),
    TextDelta(String),
    Text(String),
    ThinkingDelta(String),
    Thinking(String),
    ToolCall(ToolCallInfo),
    ToolResult(ToolResultInfo),
    Retry(RetryInfo),
    TurnEnd(String),
    TurnFailed(TurnFailure),
    Note(String),
}

/// How the harness exited, for a run whose output ended without a terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stderr_tail: String,
    pub interrupted: bool,
    pub saw_terminal: bool,
}

// ── describe ─────────────────────────────────────────────────────────────────

/// What this driver drives. Called once, when the driver is loaded.
pub fn describe() -> Description {
    Description {
        harness: HARNESS.to_string(),
        binary: BINARY.to_string(),
        version_args: VERSION_ARGS.iter().map(|s| s.to_string()).collect(),
        tested_versions: TESTED_VERSIONS.iter().map(|s| s.to_string()).collect(),
        interrupt: InterruptMethod::StdinMessage,
        required_env: REQUIRED_ENV.iter().map(|s| s.to_string()).collect(),
        streams_text: STREAMS_TEXT,
    }
}

// ── launch ───────────────────────────────────────────────────────────────────

/// Plans one turn. `claude` needs nothing this driver cannot supply from a request, so every
/// request is plannable and no branch here returns `Err`.
pub fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
    let args = build_args(&request);
    let files = match request.bridge.as_ref() {
        Some(bridge) => vec![DriverFile {
            name: MCP_CONFIG_FILE_NAME.to_string(),
            contents: mcp_config(bridge),
        }],
        None => Vec::new(),
    };

    Ok(LaunchPlan {
        args,
        // The harness gets exactly what `capabilities.env.allow` passes through: declaring a
        // variable here would put a value in the harness's environment that the operator never
        // asked for, and `claude` needs none.
        env_set: Vec::new(),
        files,
        stdin: Some(stdin_line(&request.task)),
        // `--input-format stream-json` reads messages until stdin closes, and closing it is how
        // the runtime ends the turn — so an interrupt written mid-turn still has somewhere to go.
        keep_stdin_open: true,
        interrupt_stdin: Some(interrupt_message(&request.session.id)),
    })
}

/// The full argument list after `claude`.
///
/// `--print` with `--output-format stream-json --verbose` is the non-interactive streaming mode;
/// `--input-format stream-json` is what makes the task a JSON line on stdin and makes the
/// interrupt control request possible. `--setting-sources ""` is what stops the operator's own
/// `CLAUDE.md`, settings and hooks loading into a capsule's turn — the capsule's manifest is the
/// only configuration a run answers to.
fn build_args(request: &LaunchRequest) -> Vec<String> {
    let mut args: Vec<String> = [
        "--print",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--include-partial-messages",
        "--setting-sources",
        "",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    if let Some(model) = model_arg(request.model.as_deref()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    args.push(
        match request.session.mode {
            SessionMode::New => "--session-id",
            SessionMode::Resume => "--resume",
        }
        .to_string(),
    );
    args.push(request.session.id.clone());

    match request.bridge.as_ref() {
        Some(bridge) => {
            let names: Vec<String> = bridge
                .tool_names
                .iter()
                .map(|name| harness_tool_name(&bridge.server_name, name))
                .collect();
            // One comma-joined argument rather than one argument per tool: `claude` 2.1.278
            // registers both spellings, and this keeps `--tools` exactly two argv entries in
            // every case, matching the no-bridge form below.
            args.push("--tools".to_string());
            args.push(names.join(","));
            args.push("--mcp-config".to_string());
            args.push(format!("{FILES_DIR_TOKEN}/{MCP_CONFIG_FILE_NAME}"));
            // Without this, `claude` merges the operator's own MCP servers into the capsule's.
            args.push("--strict-mcp-config".to_string());
            // The bridge already enforces the capsule's capabilities on every tool call, so a
            // second interactive approval inside the harness would only deadlock a headless run.
            args.push("--permission-mode".to_string());
            args.push("bypassPermissions".to_string());
        }
        None => {
            // An empty tool list, not an absent flag: omitting `--tools` leaves the harness its
            // own built-in tools, which a capsule exposing none did not ask for.
            args.push("--tools".to_string());
            args.push(String::new());
        }
    }

    args.push("--system-prompt".to_string());
    args.push(request.system_prompt.clone());

    args
}

/// The model to pass to `--model`, or `none` to leave the choice to the harness.
///
/// A manifest that writes `model: ""` reaches the driver as an empty or blank string, and
/// `--model ""` is an error the harness reports rather than a default — so a blank model is the
/// same as no model at all.
fn model_arg(model: Option<&str>) -> Option<&str> {
    model.map(str::trim).filter(|model| !model.is_empty())
}

/// The prefix `claude` addresses a tool from the MCP server `server_name` by.
///
/// This is the one place the harness's tool naming is built. `parse` strips this same prefix off
/// a `tool_use` block's name to recover the bare artifact name the runtime knows — the server
/// name always comes from the request's bridge, never from a constant here.
pub fn tool_name_prefix(server_name: &str) -> String {
    format!("mcp__{server_name}__")
}

/// The name `claude` addresses one bridged tool by.
fn harness_tool_name(server_name: &str, bare_name: &str) -> String {
    format!("{}{bare_name}", tool_name_prefix(server_name))
}

/// The contents of the MCP config file pointing `claude` at the capsule's tool bridge.
///
/// Built with `serde_json` rather than by concatenation, so a server name, URL or token
/// containing a quote or backslash escapes instead of producing a file `claude` cannot read.
fn mcp_config(bridge: &Bridge) -> String {
    json!({
        "mcpServers": {
            &bridge.server_name: {
                "type": "http",
                "url": &bridge.url,
                "headers": { "Authorization": format!("Bearer {}", bridge.bearer_token) },
            }
        }
    })
    .to_string()
}

/// The single newline-terminated JSON line `--input-format stream-json` reads the task from.
fn stdin_line(task: &str) -> Vec<u8> {
    let mut line = json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": task }] },
    })
    .to_string()
    .into_bytes();
    line.push(b'\n');
    line
}

/// The control request that interrupts a turn, written to the harness's still-open stdin.
fn interrupt_message(session_id: &str) -> Vec<u8> {
    let mut line = json!({
        "type": "control_request",
        "request_id": format!("{INTERRUPT_REQUEST_ID_PREFIX}{session_id}"),
        "request": { "subtype": "interrupt" },
    })
    .to_string()
    .into_bytes();
    line.push(b'\n');
    line
}

// ── parse ────────────────────────────────────────────────────────────────────

/// `session-info.auth` for a harness drawing on a Claude subscription.
pub const AUTH_SUBSCRIPTION: &str = "subscription";

/// `session-info.auth` for a harness billing an API key.
///
/// Every `apiKeySource` but the one below reads as this, absent included: the runtime warns on
/// anything that is not `subscription`, so a reading this driver does not recognise fails
/// towards being warned about rather than towards a quiet subscription claim.
pub const AUTH_API_KEY: &str = "api-key";

/// The `apiKeySource` `claude` prints when no API key is in play, which is the only reading
/// that means the turn is spending a subscription.
const NO_API_KEY_SOURCE: &str = "none";

/// How many characters of an unreadable line a `note` repeats before truncating it, so one
/// runaway line cannot fill a trace.
pub const NOTE_LINE_LIMIT: usize = 512;

/// What a `note` about an unreadable line opens with.
const NOTE_PREFIX: &str = "unreadable stdout line: ";

/// The `input` of a `tool_use` block that carries none.
const EMPTY_TOOL_INPUT: &str = "{}";

/// What a retry says when the harness names neither an error nor a status.
const UNKNOWN_RETRY_REASON: &str = "unknown";

/// The `terminal_reason` `claude` prints for a turn that was stopped mid-stream.
const ABORTED_STREAMING: &str = "aborted_streaming";

/// The `subtype` `claude` prints when it stopped at its turn limit. It is the only failure
/// `subtype` is the sole witness to, and the only one this driver reads it for.
const ERROR_MAX_TURNS: &str = "error_max_turns";

/// What a `turn-failed` says when the `result` line describes the failure in no field at all.
const UNDESCRIBED_FAILURE: &str = "the harness reported a failure with no message";

/// What `launch` learned that `parse` needs.
///
/// The interface keeps one driver instance for a whole run, which is what lets a batch of lines
/// be read against the bridge that same run was launched with. It is the only thing carried
/// from one call to the next; every line is otherwise read entirely on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseContext {
    /// The prefix `launch` told the harness to address the capsule's tools by, stripped off a
    /// `tool_use` name again here. `None` passes every tool name through untouched.
    pub tool_prefix: Option<String>,
}

impl ParseContext {
    /// Reads tool names exactly as the harness printed them.
    pub const fn none() -> Self {
        Self { tool_prefix: None }
    }

    /// Reads tool names with `prefix` stripped off.
    pub fn with_prefix(prefix: &str) -> Self {
        Self {
            tool_prefix: Some(prefix.to_string()),
        }
    }
}

/// The context a turn launched with `bridge` reads its output against.
pub fn parse_context_for(bridge: Option<&Bridge>) -> ParseContext {
    match bridge {
        Some(bridge) => ParseContext::with_prefix(&tool_name_prefix(&bridge.server_name)),
        None => ParseContext::none(),
    }
}

/// The bare artifact name behind a tool name the harness printed.
///
/// Only the prefix this driver added in `launch` comes off. Stripping a generic
/// `mcp__<anything>__` shape instead would also rename the tools of an MCP server this driver
/// never registered, handing the runtime a name it has no artifact for.
pub fn strip_tool_prefix<'a>(name: &'a str, context: &ParseContext) -> &'a str {
    match context.tool_prefix.as_deref() {
        Some(prefix) => name.strip_prefix(prefix).unwrap_or(name),
        None => name,
    }
}

/// Reads a batch of complete stdout lines into events.
///
/// Every line `claude` prints is self-contained — a `tool_result` carries its own `tool_use_id`
/// — so this holds no state across lines and the events of a batch are the events of its lines
/// in order. The same lines split into different batches therefore produce the same events.
pub fn parse_lines(lines: &[String], context: &ParseContext) -> Vec<Event> {
    lines
        .iter()
        .flat_map(|line| parse_line(line, context))
        .collect()
}

/// Reads one stdout line into the events it carries.
///
/// Never fails and never panics: a line that is not a JSON object of a shape this driver knows
/// becomes a single `note` for the trace, which is all the interface offers for saying so.
pub fn parse_line(line: &str, context: &ParseContext) -> Vec<Event> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let Some(value) = serde_json::from_str::<Value>(line)
        .ok()
        .filter(Value::is_object)
    else {
        return vec![note_for(line)];
    };

    match value.get("type").and_then(Value::as_str) {
        Some("system") => system_events(&value),
        Some("stream_event") => stream_events(&value),
        Some("assistant") => assistant_events(&value, context),
        Some("user") => user_events(&value),
        Some("result") => vec![result_event(&value)],
        // A control response answers the driver's own interrupt request; the `result` line that
        // follows is what says the turn ended.
        Some("control_response") => Vec::new(),
        _ => vec![note_for(line)],
    }
}

/// A `note` repeating a line the driver could not read, truncated at `NOTE_LINE_LIMIT`.
fn note_for(line: &str) -> Event {
    let mut note = String::from(NOTE_PREFIX);
    match line.char_indices().nth(NOTE_LINE_LIMIT) {
        Some((cut, _)) => {
            note.push_str(&line[..cut]);
            note.push('…');
        }
        None => note.push_str(line),
    }
    Event::Note(note)
}

/// Reads a `system` line. Every subtype but these two is the harness talking to itself.
fn system_events(value: &Value) -> Vec<Event> {
    match value.get("subtype").and_then(Value::as_str) {
        Some("init") => vec![Event::SessionStarted(SessionInfo {
            id: text_at(value, "session_id"),
            auth: match value.get("apiKeySource").and_then(Value::as_str) {
                Some(NO_API_KEY_SOURCE) => AUTH_SUBSCRIPTION,
                _ => AUTH_API_KEY,
            }
            .to_string(),
            model: value
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .map(str::to_string),
        })],
        Some("api_retry") => vec![Event::Retry(RetryInfo {
            attempt: value
                .get("attempt")
                .and_then(Value::as_u64)
                .unwrap_or_default() as u32,
            reason: retry_reason(value),
        })],
        _ => Vec::new(),
    }
}

/// Why the harness says the attempt before this one failed.
fn retry_reason(value: &Value) -> String {
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty());

    match (error, scalar_text(value.get("error_status"))) {
        (Some(error), Some(status)) => format!("{error} ({status})"),
        (Some(error), None) => error.to_string(),
        (None, Some(status)) => format!("HTTP {status}"),
        (None, None) => UNKNOWN_RETRY_REASON.to_string(),
    }
}

/// Reads a `stream_event` line. Only the two content deltas carry anything the runtime streams;
/// the message and block framing around them is the harness's own bookkeeping.
fn stream_events(value: &Value) -> Vec<Event> {
    let delta = &value["event"]["delta"];
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => vec![Event::TextDelta(text_at(delta, "text"))],
        Some("thinking_delta") => vec![Event::ThinkingDelta(text_at(delta, "thinking"))],
        _ => Vec::new(),
    }
}

/// Reads an `assistant` line into at most one `thinking`, at most one `text`, and one
/// `tool-call` per `tool_use` block.
///
/// One `text` per message is what makes the interface's rule — a `text` replaces the
/// `text-delta`s streamed for that message rather than being appended to them — well defined:
/// a message whose content is split across several text blocks would otherwise replace its own
/// text halfway through. A message with no text of its own emits no `text` event at all.
///
/// The `<synthetic>` `assistant` line `claude` prints for an API error is read like any other:
/// the `turn-failed` on the `result` line that follows is what tells the runtime the turn
/// failed, and the harness's own quirks are not the contract's to cater for.
fn assistant_events(value: &Value, context: &ParseContext) -> Vec<Event> {
    let mut thinking = String::new();
    let mut text = String::new();
    let mut calls = Vec::new();

    for block in content_blocks(value) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(str_at(block, "text")),
            Some("thinking") => thinking.push_str(str_at(block, "thinking")),
            Some("tool_use") => calls.push(Event::ToolCall(ToolCallInfo {
                id: text_at(block, "id"),
                name: strip_tool_prefix(str_at(block, "name"), context).to_string(),
                input: match block.get("input") {
                    Some(input) if !input.is_null() => input.to_string(),
                    _ => EMPTY_TOOL_INPUT.to_string(),
                },
            })),
            _ => {}
        }
    }

    let mut events = Vec::new();
    if !thinking.is_empty() {
        events.push(Event::Thinking(thinking));
    }
    if !text.is_empty() {
        events.push(Event::Text(text));
    }
    events.extend(calls);
    events
}

/// Reads a `user` line. The harness writes tool results back to itself on one, and also the
/// `[Request interrupted by user]` marker, which the `result` line already says.
fn user_events(value: &Value) -> Vec<Event> {
    content_blocks(value)
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| {
            Event::ToolResult(ToolResultInfo {
                id: text_at(block, "tool_use_id"),
                output: tool_result_output(block.get("content")),
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// A `tool_result` block's content as one string. `claude` writes either the output itself or a
/// list of blocks; a block that is not text keeps its JSON rather than being dropped.
fn tool_result_output(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(output)) => output.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => text_at(block, "text"),
                _ => block.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

/// Reads a `result` line, the one line that ends a turn.
///
/// `is_error` decides, never `subtype`: `claude` 2.1.278 reports an authentication or quota
/// failure as `subtype: "success"` alongside `is_error: true`, so a reader that trusts
/// `subtype` reports a 401 as the model's answer. A line with no readable `is_error` fails
/// closed — a turn nobody can confirm succeeded is not reported as one that did.
fn result_event(value: &Value) -> Event {
    if value.get("is_error").and_then(Value::as_bool) == Some(false) {
        return Event::TurnEnd(text_at(value, "result"));
    }
    Event::TurnFailed(TurnFailure {
        kind: failure_kind(value),
        message: failure_message(value),
    })
}

/// Why the turn failed, read in the order the fields can be trusted in.
fn failure_kind(value: &Value) -> FailureKind {
    if let Some(status) = value.get("api_error_status").and_then(Value::as_i64) {
        match status {
            401 | 403 => return FailureKind::Auth,
            429 => return FailureKind::Quota,
            _ => {}
        }
    }
    if value.get("terminal_reason").and_then(Value::as_str) == Some(ABORTED_STREAMING) {
        return FailureKind::Canceled;
    }
    if value.get("subtype").and_then(Value::as_str) == Some(ERROR_MAX_TURNS) {
        return FailureKind::MaxTurns;
    }
    FailureKind::HarnessError
}

/// The failure as the harness described it, or as much of it as the line does say. An
/// interrupted turn writes no `result` text, so the fields it did write are the message.
fn failure_message(value: &Value) -> String {
    if let Some(result) = value
        .get("result")
        .and_then(Value::as_str)
        .filter(|result| !result.is_empty())
    {
        return result.to_string();
    }

    let described: Vec<String> = [
        scalar_text(value.get("subtype")),
        scalar_text(value.get("terminal_reason")).map(|it| format!("terminal_reason: {it}")),
        scalar_text(value.get("api_error_status")).map(|it| format!("api_error_status: {it}")),
    ]
    .into_iter()
    .flatten()
    .collect();

    if described.is_empty() {
        return UNDESCRIBED_FAILURE.to_string();
    }
    described.join(", ")
}

/// The content blocks of a line's `message`, empty when it carries none.
fn content_blocks(value: &Value) -> &[Value] {
    value["message"]["content"]
        .as_array()
        .map_or(&[], Vec::as_slice)
}

/// The string at `key`, or `""` when the harness wrote none there.
fn str_at<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The owned string at `key`, or `""` when the harness wrote none there.
fn text_at(value: &Value, key: &str) -> String {
    str_at(value, key).to_string()
}

/// A JSON scalar as the text a message quotes it by; `None` for absent, null and empty, the
/// three ways a field says nothing.
fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(other.to_string()),
    }
}

// ── classify-exit ────────────────────────────────────────────────────────────

/// Classifies a run whose output ended without a terminal event.
///
/// It only ever answers `turn-failed`. A run that reaches here printed no `result` line, and a
/// turn whose result nobody read is not one that succeeded.
pub fn classify_exit(exit: ExitStatus) -> Event {
    // Before the exit code is read at all, which is the rule the interface holds every driver
    // to: `claude` exits `0` on a turn stopped by SIGINT, so the code cannot tell a canceled
    // turn from a finished one.
    if exit.interrupted && !exit.saw_terminal {
        return failed(
            FailureKind::Canceled,
            format!("{BINARY} was interrupted before it reported a result"),
        );
    }
    if exit.code == Some(0) && !exit.saw_terminal {
        return failed(
            FailureKind::HarnessError,
            format!("{BINARY} exited without a result"),
        );
    }

    let what = match (exit.code, exit.signal) {
        (Some(code), _) => format!("{BINARY} exited with code {code}"),
        (None, Some(signal)) => format!("{BINARY} was killed by signal {signal}"),
        (None, None) => format!("{BINARY} exited without a status"),
    };
    let tail = exit.stderr_tail.trim();
    failed(
        FailureKind::HarnessError,
        if tail.is_empty() {
            what
        } else {
            format!("{what}: {tail}")
        },
    )
}

/// A `turn-failed` event.
fn failed(kind: FailureKind, message: String) -> Event {
    Event::TurnFailed(TurnFailure { kind, message })
}

// ── the wasm adapter ─────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_driver {
    //! Converts the generated WIT records to the crate-root mirrors, calls the logic, and
    //! converts the answer back. It holds no flag, no string the harness reads, and no branch
    //! the runtime acts on — everything it would decide lives at the crate root, where the
    //! host-side tests reach it.

    wit_bindgen::generate!({
        path: "../../wit/process-driver",
        world: "process-driver",
        generate_all,
    });

    use std::cell::RefCell;

    thread_local! {
        /// What `launch` learned that `parse` needs. The interface keeps one driver instance
        /// for a whole run and a component instance runs on one thread, so the context the
        /// last `launch` stored is the one whose output `parse` is reading.
        static PARSE_CONTEXT: RefCell<super::ParseContext> =
            const { RefCell::new(super::ParseContext::none()) };
    }

    use exports::murmur::driver::process::{
        Bridge, Description, DriverFile, Event, ExitStatus, FailureKind, Guest, InterruptMethod,
        LaunchPlan, LaunchRequest, RetryInfo, Session, SessionInfo, SessionMode, ToolCallInfo,
        ToolResultInfo, TurnFailure,
    };

    pub struct ClaudeCodeDriver;

    impl Guest for ClaudeCodeDriver {
        fn describe() -> Description {
            from_description(super::describe())
        }

        fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
            let request = to_request(request);
            let context = super::parse_context_for(request.bridge.as_ref());
            PARSE_CONTEXT.with(|stored| *stored.borrow_mut() = context);
            super::launch(request).map(from_plan)
        }

        fn parse(lines: Vec<String>) -> Vec<Event> {
            PARSE_CONTEXT
                .with(|context| super::parse_lines(&lines, &context.borrow()))
                .into_iter()
                .map(from_event)
                .collect()
        }

        fn classify_exit(exit: ExitStatus) -> Event {
            from_event(super::classify_exit(to_exit(exit)))
        }
    }

    fn from_description(description: super::Description) -> Description {
        Description {
            harness: description.harness,
            binary: description.binary,
            version_args: description.version_args,
            tested_versions: description.tested_versions,
            interrupt: match description.interrupt {
                super::InterruptMethod::StdinMessage => InterruptMethod::StdinMessage,
                super::InterruptMethod::SignalInt => InterruptMethod::SignalInt,
                super::InterruptMethod::Unsupported => InterruptMethod::Unsupported,
            },
            required_env: description.required_env,
            streams_text: description.streams_text,
        }
    }

    fn to_request(request: LaunchRequest) -> super::LaunchRequest {
        super::LaunchRequest {
            model: request.model,
            system_prompt: request.system_prompt,
            config: request.config,
            bridge: request.bridge.map(to_bridge),
            session: to_session(request.session),
            harness_version: request.harness_version,
            task: request.task,
        }
    }

    fn to_bridge(bridge: Bridge) -> super::Bridge {
        super::Bridge {
            server_name: bridge.server_name,
            url: bridge.url,
            bearer_token: bridge.bearer_token,
            tool_names: bridge.tool_names,
        }
    }

    fn to_session(session: Session) -> super::Session {
        super::Session {
            id: session.id,
            mode: match session.mode {
                SessionMode::New => super::SessionMode::New,
                SessionMode::Resume => super::SessionMode::Resume,
            },
        }
    }

    fn from_plan(plan: super::LaunchPlan) -> LaunchPlan {
        LaunchPlan {
            args: plan.args,
            env_set: plan.env_set,
            files: plan
                .files
                .into_iter()
                .map(|file| DriverFile {
                    name: file.name,
                    contents: file.contents,
                })
                .collect(),
            stdin: plan.stdin,
            keep_stdin_open: plan.keep_stdin_open,
            interrupt_stdin: plan.interrupt_stdin,
        }
    }

    fn to_exit(exit: ExitStatus) -> super::ExitStatus {
        super::ExitStatus {
            code: exit.code,
            signal: exit.signal,
            stderr_tail: exit.stderr_tail,
            interrupted: exit.interrupted,
            saw_terminal: exit.saw_terminal,
        }
    }

    fn from_event(event: super::Event) -> Event {
        match event {
            super::Event::SessionStarted(info) => Event::SessionStarted(SessionInfo {
                id: info.id,
                auth: info.auth,
                model: info.model,
            }),
            super::Event::TextDelta(text) => Event::TextDelta(text),
            super::Event::Text(text) => Event::Text(text),
            super::Event::ThinkingDelta(text) => Event::ThinkingDelta(text),
            super::Event::Thinking(text) => Event::Thinking(text),
            super::Event::ToolCall(call) => Event::ToolCall(ToolCallInfo {
                id: call.id,
                name: call.name,
                input: call.input,
            }),
            super::Event::ToolResult(result) => Event::ToolResult(ToolResultInfo {
                id: result.id,
                output: result.output,
                is_error: result.is_error,
            }),
            super::Event::Retry(retry) => Event::Retry(RetryInfo {
                attempt: retry.attempt,
                reason: retry.reason,
            }),
            super::Event::TurnEnd(result) => Event::TurnEnd(result),
            super::Event::TurnFailed(failure) => Event::TurnFailed(TurnFailure {
                kind: match failure.kind {
                    super::FailureKind::Auth => FailureKind::Auth,
                    super::FailureKind::Quota => FailureKind::Quota,
                    super::FailureKind::MaxTurns => FailureKind::MaxTurns,
                    super::FailureKind::Canceled => FailureKind::Canceled,
                    super::FailureKind::HarnessError => FailureKind::HarnessError,
                    super::FailureKind::Other => FailureKind::Other,
                },
                message: failure.message,
            }),
            super::Event::Note(note) => Event::Note(note),
        }
    }

    export!(ClaudeCodeDriver);
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const SESSION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const SYSTEM_PROMPT: &str = "You are a Murmur capsule.";

    fn request() -> LaunchRequest {
        LaunchRequest {
            model: None,
            system_prompt: SYSTEM_PROMPT.to_string(),
            config: None,
            bridge: None,
            session: Session {
                id: SESSION_ID.to_string(),
                mode: SessionMode::New,
            },
            harness_version: None,
            task: "Do the thing.".to_string(),
        }
    }

    fn bridge() -> Bridge {
        Bridge {
            server_name: "claude_bridge".to_string(),
            url: "http://127.0.0.1:8931/mcp".to_string(),
            bearer_token: "tok-123".to_string(),
            tool_names: vec!["echo_tool".to_string(), "write_file".to_string()],
        }
    }

    fn args_of(request: LaunchRequest) -> Vec<String> {
        launch(request).expect("launch plans every request").args
    }

    /// The index of `flag` in `args`, or `None` when it is absent.
    fn index_of(args: &[String], flag: &str) -> Option<usize> {
        args.iter().position(|arg| arg == flag)
    }

    /// The single argument following `flag`.
    fn value_after(args: &[String], flag: &str) -> Option<String> {
        index_of(args, flag).and_then(|i| args.get(i + 1).cloned())
    }

    // ── Scenario 1 — describe ─────────────────────────────────────────────────

    #[test]
    fn describe_names_the_harness_and_what_it_needs() {
        let description = describe();
        assert_eq!(description.harness, "claude-code");
        assert_eq!(description.binary, "claude");
        assert_eq!(description.version_args, vec!["--version".to_string()]);
        assert_eq!(
            description.tested_versions,
            vec!["2.1.277".to_string(), "2.1.278".to_string()]
        );
        assert_eq!(description.interrupt, InterruptMethod::StdinMessage);
        assert_eq!(
            description.required_env,
            vec!["HOME".to_string(), "PATH".to_string()]
        );
        assert!(description.streams_text);
    }

    // ── Scenario 2 — the session flag ─────────────────────────────────────────

    #[test]
    fn a_new_session_is_started_by_id_and_a_resumed_one_is_resumed_by_id() {
        let new = args_of(request());
        assert_eq!(
            value_after(&new, "--session-id").as_deref(),
            Some(SESSION_ID)
        );
        assert_eq!(index_of(&new, "--resume"), None);

        let mut resumed = request();
        resumed.session.mode = SessionMode::Resume;
        let resumed = args_of(resumed);
        assert_eq!(
            value_after(&resumed, "--resume").as_deref(),
            Some(SESSION_ID)
        );
        assert_eq!(index_of(&resumed, "--session-id"), None);

        // The same slot either way: only the flag's spelling changes with the mode.
        assert_eq!(
            index_of(&new, "--session-id"),
            index_of(&resumed, "--resume")
        );
    }

    // ── Scenario 3 — the model flag ───────────────────────────────────────────

    #[test]
    fn a_model_is_passed_trimmed_and_a_blank_one_is_left_to_the_harness() {
        let mut with_model = request();
        with_model.model = Some("claude-opus-5".to_string());
        let args = args_of(with_model);
        assert_eq!(
            value_after(&args, "--model").as_deref(),
            Some("claude-opus-5")
        );

        let mut padded = request();
        padded.model = Some("  claude-opus-5  ".to_string());
        let args = args_of(padded);
        assert_eq!(
            value_after(&args, "--model").as_deref(),
            Some("claude-opus-5")
        );

        for blank in [None, Some(String::new()), Some("  ".to_string())] {
            let mut blank_request = request();
            blank_request.model = blank.clone();
            let args = args_of(blank_request);
            assert_eq!(
                index_of(&args, "--model"),
                None,
                "a blank model ({blank:?}) must emit no --model"
            );
        }
    }

    #[test]
    fn no_argument_following_model_is_ever_empty() {
        for model in [
            None,
            Some(String::new()),
            Some("  ".to_string()),
            Some("claude-opus-5".to_string()),
        ] {
            let mut candidate = request();
            candidate.model = model.clone();
            candidate.bridge = Some(bridge());
            let args = args_of(candidate);
            if let Some(value) = value_after(&args, "--model") {
                assert!(
                    !value.is_empty(),
                    "--model followed by an empty argument for {model:?}"
                );
            }
        }
    }

    // ── Scenario 4 — the bridge ───────────────────────────────────────────────

    #[test]
    fn a_bridge_names_its_tools_the_harness_way_and_is_configured_from_a_file() {
        let mut bridged = request();
        bridged.bridge = Some(bridge());
        let plan = launch(bridged).expect("launch plans every request");

        assert_eq!(
            value_after(&plan.args, "--tools").as_deref(),
            Some("mcp__claude_bridge__echo_tool,mcp__claude_bridge__write_file")
        );
        assert_eq!(
            value_after(&plan.args, "--mcp-config").as_deref(),
            Some("{files_dir}/mcp-config.json")
        );
        assert!(plan.args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert_eq!(
            value_after(&plan.args, "--permission-mode").as_deref(),
            Some("bypassPermissions")
        );

        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].name, "mcp-config.json");
        let config: Value =
            serde_json::from_str(&plan.files[0].contents).expect("the MCP config must be JSON");
        assert_eq!(
            config,
            json!({
                "mcpServers": {
                    "claude_bridge": {
                        "type": "http",
                        "url": "http://127.0.0.1:8931/mcp",
                        "headers": { "Authorization": "Bearer tok-123" },
                    }
                }
            })
        );
    }

    #[test]
    fn a_server_name_or_token_carrying_a_quote_escapes_rather_than_breaking_the_file() {
        let mut bridged = request();
        bridged.bridge = Some(Bridge {
            server_name: "odd\"name\\".to_string(),
            bearer_token: "tok\"\n123".to_string(),
            ..bridge()
        });
        let plan = launch(bridged).expect("launch plans every request");

        let config: Value = serde_json::from_str(&plan.files[0].contents)
            .expect("the MCP config must still be JSON");
        let server = &config["mcpServers"]["odd\"name\\"];
        assert_eq!(
            server["headers"]["Authorization"],
            json!("Bearer tok\"\n123")
        );
    }

    // ── Scenario 5 — no bridge ────────────────────────────────────────────────

    #[test]
    fn without_a_bridge_the_tool_list_is_empty_and_no_mcp_flag_appears() {
        let plan = launch(request()).expect("launch plans every request");

        assert_eq!(value_after(&plan.args, "--tools").as_deref(), Some(""));
        for absent in [
            "--mcp-config",
            "--strict-mcp-config",
            "--permission-mode",
            "bypassPermissions",
        ] {
            assert_eq!(
                index_of(&plan.args, absent),
                None,
                "an unbridged turn must not carry {absent}"
            );
        }
        assert!(plan.files.is_empty());
    }

    // ── Scenario 6 — the server name comes from the request ───────────────────

    #[test]
    fn tool_names_are_built_from_the_requests_server_name() {
        let mut bridged = request();
        bridged.bridge = Some(Bridge {
            server_name: "murmur".to_string(),
            tool_names: vec!["echo_tool".to_string()],
            ..bridge()
        });
        let args = args_of(bridged);
        assert_eq!(
            value_after(&args, "--tools").as_deref(),
            Some("mcp__murmur__echo_tool")
        );
    }

    #[test]
    fn the_driver_hardcodes_no_server_name() {
        // The runtime names the bridge server; a constant here would silently stop matching it,
        // and the harness would be told about tools under a name it cannot call.
        let source = include_str!("lib.rs");
        let non_test = &source[..source.find("\nmod tests {").expect("mod tests must exist")];
        for needle in [
            concat!("claude", "_bridge"),
            concat!("mcp__", "murmur", "__"),
        ] {
            assert!(
                !non_test.contains(needle),
                "murmur-driver-claude-code: non-test source must not contain {needle}"
            );
        }
    }

    // ── Scenario 7 — the recorded argv ────────────────────────────────────────

    #[test]
    fn the_recorded_case_produces_the_recorded_argv() {
        let mut recorded = request();
        recorded.bridge = Some(Bridge {
            server_name: "claude_bridge".to_string(),
            tool_names: vec!["echo_tool".to_string()],
            ..bridge()
        });

        assert_eq!(
            args_of(recorded),
            vec![
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--include-partial-messages",
                "--setting-sources",
                "",
                "--session-id",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "--tools",
                "mcp__claude_bridge__echo_tool",
                "--mcp-config",
                "{files_dir}/mcp-config.json",
                "--strict-mcp-config",
                "--permission-mode",
                "bypassPermissions",
                "--system-prompt",
                "You are a Murmur capsule.",
            ]
        );
    }

    // ── Scenario 8 — stdin, interrupt, environment ────────────────────────────

    #[test]
    fn the_task_is_one_json_line_that_round_trips_awkward_text() {
        let task = "say \"hello\"\nthen stop\\";
        let mut awkward = request();
        awkward.task = task.to_string();
        let plan = launch(awkward).expect("launch plans every request");

        let stdin = plan.stdin.expect("the task is written to stdin");
        assert_eq!(stdin.last(), Some(&b'\n'));
        assert_eq!(
            stdin.iter().filter(|byte| **byte == b'\n').count(),
            1,
            "the task must be exactly one line"
        );

        let line: Value = serde_json::from_slice(&stdin).expect("stdin must be one JSON line");
        assert_eq!(line["type"], json!("user"));
        assert_eq!(line["message"]["role"], json!("user"));
        assert_eq!(line["message"]["content"][0]["type"], json!("text"));
        assert_eq!(line["message"]["content"][0]["text"], json!(task));
    }

    #[test]
    fn stdin_stays_open_and_carries_an_interrupt_derived_from_the_session_id() {
        let plan = launch(request()).expect("launch plans every request");
        assert!(plan.keep_stdin_open);

        let interrupt = plan.interrupt_stdin.expect("stdin-message needs a message");
        let message: Value =
            serde_json::from_slice(&interrupt).expect("the interrupt must be JSON");
        assert_eq!(message["type"], json!("control_request"));
        assert_eq!(
            message["request_id"],
            json!(format!("murmur-interrupt-{SESSION_ID}"))
        );
        assert_eq!(message["request"]["subtype"], json!("interrupt"));
    }

    #[test]
    fn the_driver_sets_no_environment_variable() {
        let plan = launch(request()).expect("launch plans every request");
        assert!(plan.env_set.is_empty());
    }

    // ── Scenario 9 — reading one line at a time ───────────────────────────────

    /// The context a bridged run reads its output against, built the way `launch` builds it.
    fn bridged_context() -> ParseContext {
        parse_context_for(Some(&Bridge {
            server_name: "claude_bridge".to_string(),
            ..bridge()
        }))
    }

    fn events(line: &str) -> Vec<Event> {
        parse_line(line, &bridged_context())
    }

    #[test]
    fn a_blank_or_whitespace_only_line_carries_nothing() {
        assert_eq!(events(""), Vec::new());
        assert_eq!(events("   \t "), Vec::new());
    }

    #[test]
    fn a_line_the_driver_cannot_read_becomes_exactly_one_note() {
        for unreadable in [
            "not json at all",
            "[]",
            "{\"no\":\"type\"}",
            "{\"type\":\"banana\"}",
        ] {
            assert_eq!(
                events(unreadable),
                vec![Event::Note(format!("unreadable stdout line: {unreadable}"))],
                "{unreadable} must produce one note and nothing else"
            );
        }
    }

    #[test]
    fn a_runaway_line_is_truncated_in_its_note() {
        let line = "x".repeat(4000);
        let Event::Note(note) = events(&line).remove(0) else {
            panic!("an unreadable line becomes a note");
        };
        let repeated = note
            .strip_prefix("unreadable stdout line: ")
            .expect("a note repeats the line it could not read");
        assert_eq!(repeated, format!("{}…", "x".repeat(NOTE_LINE_LIMIT)));
    }

    #[test]
    fn an_init_line_reports_the_session_and_how_the_harness_is_billing() {
        let subscription = events(
            r#"{"type":"system","subtype":"init","session_id":"s1","apiKeySource":"none","model":"claude-opus-5"}"#,
        );
        assert_eq!(
            subscription,
            vec![Event::SessionStarted(SessionInfo {
                id: "s1".to_string(),
                auth: AUTH_SUBSCRIPTION.to_string(),
                model: Some("claude-opus-5".to_string()),
            })]
        );

        // Absent is the ambiguous case, and it fails towards the reading the runtime warns about.
        let absent = events(r#"{"type":"system","subtype":"init","session_id":"s1"}"#);
        assert_eq!(
            absent,
            vec![Event::SessionStarted(SessionInfo {
                id: "s1".to_string(),
                auth: AUTH_API_KEY.to_string(),
                model: None,
            })]
        );
    }

    #[test]
    fn a_system_line_the_driver_does_not_read_carries_nothing() {
        for ignored in [
            r#"{"type":"system","subtype":"status","session_id":"s1"}"#,
            r#"{"type":"system","subtype":"thinking_tokens","session_id":"s1"}"#,
            r#"{"type":"control_response","response":{"subtype":"success"}}"#,
        ] {
            assert_eq!(events(ignored), Vec::new(), "{ignored} carries no event");
        }
    }

    #[test]
    fn a_retry_names_the_error_and_the_status_it_can_read() {
        let reason = |line: &str| match events(line).remove(0) {
            Event::Retry(retry) => retry.reason,
            other => panic!("an api_retry line is a retry, not {other:?}"),
        };
        let retry = |fields: &str| format!(r#"{{"type":"system","subtype":"api_retry"{fields}}}"#);

        assert_eq!(
            events(&retry(
                r#","attempt":2,"error":"rate_limit","error_status":429"#
            )),
            vec![Event::Retry(RetryInfo {
                attempt: 2,
                reason: "rate_limit (429)".to_string(),
            })]
        );
        assert_eq!(reason(&retry(r#","error":"rate_limit""#)), "rate_limit");
        assert_eq!(reason(&retry(r#","error_status":503"#)), "HTTP 503");
        assert_eq!(reason(&retry("")), "unknown");
    }

    #[test]
    fn only_the_two_content_deltas_stream() {
        assert_eq!(
            events(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi "}}}"#
            ),
            vec![Event::TextDelta("hi ".to_string())]
        );
        assert_eq!(
            events(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#
            ),
            vec![Event::ThinkingDelta("hmm".to_string())]
        );
        for framing in ["message_start", "content_block_stop", "message_stop"] {
            assert_eq!(
                events(&format!(
                    r#"{{"type":"stream_event","event":{{"type":"{framing}"}}}}"#
                )),
                Vec::new(),
                "{framing} carries no event"
            );
        }
        assert_eq!(
            events(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}}"#
            ),
            Vec::new()
        );
    }

    #[test]
    fn one_assistant_message_emits_one_text_event_after_its_thinking() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"weighing it"},
            {"type":"text","text":"first "},
            {"type":"text","text":"second"},
            {"type":"tool_use","id":"toolu_9","name":"mcp__claude_bridge__echo_tool","input":{"text":"hi"}}
        ]}}"#;
        assert_eq!(
            events(line),
            vec![
                Event::Thinking("weighing it".to_string()),
                Event::Text("first second".to_string()),
                Event::ToolCall(ToolCallInfo {
                    id: "toolu_9".to_string(),
                    name: "echo_tool".to_string(),
                    input: r#"{"text":"hi"}"#.to_string(),
                }),
            ]
        );
    }

    #[test]
    fn a_thinking_only_message_emits_no_text_event() {
        assert_eq!(
            events(
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#
            ),
            vec![Event::Thinking("hmm".to_string())]
        );
    }

    #[test]
    fn a_tool_use_block_without_input_calls_with_an_empty_object() {
        assert_eq!(
            events(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"bare"}]}}"#
            ),
            vec![Event::ToolCall(ToolCallInfo {
                id: "t1".to_string(),
                name: "bare".to_string(),
                input: "{}".to_string(),
            })]
        );
    }

    #[test]
    fn only_the_prefix_this_driver_added_is_stripped() {
        let name = "mcp__claude_bridge__echo_tool";
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"{name}","input":{{}}}}]}}}}"#
        );
        let called = |context: &ParseContext| match parse_line(&line, context).remove(0) {
            Event::ToolCall(call) => call.name,
            other => panic!("a tool_use block is a tool-call, not {other:?}"),
        };

        assert_eq!(called(&bridged_context()), "echo_tool");
        // A run launched with no bridge, and a run whose bridge the runtime named something
        // else, both leave a name this driver did not write alone.
        assert_eq!(called(&ParseContext::none()), name);
        assert_eq!(
            called(&ParseContext::with_prefix(&tool_name_prefix("other"))),
            name
        );
    }

    #[test]
    fn a_tool_result_is_paired_by_its_own_tool_use_id() {
        let block = |content: &str, rest: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":{content}{rest}}}]}}}}"#
            )
        };

        assert_eq!(
            events(&block(r#""plain output""#, "")),
            vec![Event::ToolResult(ToolResultInfo {
                id: "t1".to_string(),
                output: "plain output".to_string(),
                is_error: false,
            })]
        );
        assert_eq!(
            events(&block(
                r#"[{"type":"text","text":"one"},{"type":"image","source":"s"}]"#,
                r#","is_error":true"#
            )),
            vec![Event::ToolResult(ToolResultInfo {
                id: "t1".to_string(),
                output: "one\n{\"source\":\"s\",\"type\":\"image\"}".to_string(),
                is_error: true,
            })]
        );
    }

    #[test]
    fn a_user_line_that_is_not_a_tool_result_carries_nothing() {
        assert_eq!(
            events(
                r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#
            ),
            Vec::new()
        );
    }

    // ── Scenario 10 — a result line is read by is_error first ─────────────────

    fn failure(line: &str) -> TurnFailure {
        match events(line).remove(0) {
            Event::TurnFailed(failure) => failure,
            other => panic!("a failing result is a turn-failed, not {other:?}"),
        }
    }

    #[test]
    fn a_successful_result_ends_the_turn_with_its_text() {
        assert_eq!(
            events(r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#),
            vec![Event::TurnEnd("done".to_string())]
        );
        assert_eq!(
            events(r#"{"type":"result","subtype":"success","is_error":false,"result":null}"#),
            vec![Event::TurnEnd(String::new())]
        );
    }

    #[test]
    fn subtype_success_does_not_make_a_failing_result_a_success() {
        // The defect this driver exists to close: `claude` 2.1.278 reports a 401 and a 429 as
        // `subtype: "success"`, and only `is_error` and `api_error_status` say otherwise.
        assert_eq!(
            failure(
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":401,"result":"Invalid API key"}"#
            ),
            TurnFailure {
                kind: FailureKind::Auth,
                message: "Invalid API key".to_string(),
            }
        );
        assert_eq!(
            failure(
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"rate limited"}"#
            )
            .kind,
            FailureKind::Quota
        );
        assert_eq!(
            failure(
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":403}"#
            )
            .kind,
            FailureKind::Auth
        );
    }

    #[test]
    fn a_result_with_no_readable_is_error_fails_closed() {
        assert_eq!(
            failure(r#"{"type":"result","subtype":"success","result":"looks fine"}"#),
            TurnFailure {
                kind: FailureKind::HarnessError,
                message: "looks fine".to_string(),
            }
        );
    }

    #[test]
    fn the_turn_limit_is_the_one_failure_subtype_decides() {
        assert_eq!(
            failure(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#),
            TurnFailure {
                kind: FailureKind::MaxTurns,
                message: "error_max_turns".to_string(),
            }
        );
    }

    #[test]
    fn an_aborted_stream_is_canceled_and_describes_itself_from_its_fields() {
        assert_eq!(
            failure(
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming"}"#
            ),
            TurnFailure {
                kind: FailureKind::Canceled,
                message: "error_during_execution, terminal_reason: aborted_streaming".to_string(),
            }
        );
    }

    #[test]
    fn a_failure_the_line_describes_in_no_field_still_says_something() {
        assert_eq!(
            failure(r#"{"type":"result","is_error":true}"#),
            TurnFailure {
                kind: FailureKind::HarnessError,
                message: "the harness reported a failure with no message".to_string(),
            }
        );
    }

    // ── Scenario 11 — a batch is its lines ────────────────────────────────────

    #[test]
    fn a_batch_of_lines_is_the_concatenation_of_its_lines() {
        let lines: Vec<String> = [
            r#"{"type":"system","subtype":"init","session_id":"s1","apiKeySource":"none"}"#,
            "",
            r#"{"type":"result","is_error":false,"result":"done"}"#,
        ]
        .iter()
        .map(|line| (*line).to_string())
        .collect();

        let context = bridged_context();
        let batched = parse_lines(&lines, &context);
        let one_at_a_time: Vec<Event> = lines
            .iter()
            .flat_map(|line| parse_line(line, &context))
            .collect();
        assert_eq!(batched, one_at_a_time);
        assert_eq!(batched.len(), 2);
    }

    // ── Scenario 12 — classify-exit ───────────────────────────────────────────

    fn exit() -> ExitStatus {
        ExitStatus {
            code: Some(0),
            signal: None,
            stderr_tail: String::new(),
            interrupted: false,
            saw_terminal: false,
        }
    }

    fn classified(exit: ExitStatus) -> TurnFailure {
        match classify_exit(exit) {
            Event::TurnFailed(failure) => failure,
            other => panic!("classify-exit never reports a turn that did not end, got {other:?}"),
        }
    }

    #[test]
    fn an_interrupted_run_with_no_terminal_event_is_canceled_whatever_the_exit_says() {
        let canceled = TurnFailure {
            kind: FailureKind::Canceled,
            message: "claude was interrupted before it reported a result".to_string(),
        };
        let interrupted = ExitStatus {
            interrupted: true,
            ..exit()
        };

        assert_eq!(classified(interrupted.clone()), canceled);
        assert_eq!(
            classified(ExitStatus {
                code: Some(1),
                ..interrupted.clone()
            }),
            canceled
        );
        assert_eq!(
            classified(ExitStatus {
                code: None,
                signal: Some(2),
                ..interrupted.clone()
            }),
            canceled
        );
        assert_eq!(
            classified(ExitStatus {
                stderr_tail: "boom".to_string(),
                ..interrupted
            }),
            canceled
        );
    }

    #[test]
    fn a_clean_exit_with_no_result_is_a_harness_error() {
        assert_eq!(
            classified(exit()),
            TurnFailure {
                kind: FailureKind::HarnessError,
                message: "claude exited without a result".to_string(),
            }
        );
    }

    #[test]
    fn any_other_exit_says_what_happened_and_what_the_harness_printed() {
        let message = |exit: ExitStatus| classified(exit).message;

        assert_eq!(
            message(ExitStatus {
                code: Some(1),
                stderr_tail: "boom".to_string(),
                ..exit()
            }),
            "claude exited with code 1: boom"
        );
        assert_eq!(
            message(ExitStatus {
                code: None,
                signal: Some(9),
                ..exit()
            }),
            "claude was killed by signal 9"
        );
        assert_eq!(
            message(ExitStatus {
                code: None,
                stderr_tail: "  weird  ".to_string(),
                ..exit()
            }),
            "claude exited without a status: weird"
        );
    }

    #[test]
    fn classify_exit_never_reports_a_turn_that_ended() {
        for code in [None, Some(0), Some(1), Some(-1)] {
            for signal in [None, Some(9)] {
                for interrupted in [false, true] {
                    for saw_terminal in [false, true] {
                        let event = classify_exit(ExitStatus {
                            code,
                            signal,
                            stderr_tail: String::new(),
                            interrupted,
                            saw_terminal,
                        });
                        assert!(
                            matches!(event, Event::TurnFailed(_)),
                            "classify-exit answered {event:?}"
                        );
                    }
                }
            }
        }
    }
}
