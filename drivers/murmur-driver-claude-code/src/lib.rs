// Functions and types are only referenced from the wasm_driver module (cfg-gated to wasm32)
// or from cfg(test). Suppress dead_code noise in plain host library builds.
#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

//! The process driver for Claude Code.
//!
//! Every fact about the `claude` CLI — its flags, the shape of its MCP config, how it spells a
//! bridged tool, the JSON line it reads off stdin, the control request that interrupts a turn,
//! and the variables it cannot run without — lives in this crate and nowhere else. The runtime
//! runs *a* process driver; it never knows it is running the Claude one.
//!
//! The driver is granted nothing: no environment, no filesystem, no network, no Murmur host
//! import. It is pure translation from a `LaunchRequest` to a `LaunchPlan`.

use serde_json::json;

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

// ── parse and classify-exit: not implemented at 0.1.0 ────────────────────────

/// What the next release says about a line the reader cannot yet read.
const NO_PARSER_YET: &str =
    "murmur-driver-claude-code 0.1.0 plans a turn but does not read the harness's output yet: \
     parse and classify-exit are not implemented";

/// Reads a batch of complete stdout lines into events.
///
/// Not implemented at `0.1.0`. The interface allows a `parse` call to return no events, so an
/// empty list is a legal answer rather than a claim about what the harness said.
pub fn parse(_lines: Vec<String>) -> Vec<Event> {
    Vec::new()
}

/// Classifies a run whose output ended without a terminal event.
///
/// Not implemented at `0.1.0` beyond the one rule the contract holds every driver to: a run the
/// runtime interrupted, with no terminal event seen, is `canceled` whatever the exit code says.
/// Since `parse` emits nothing yet, every run reaches here.
pub fn classify_exit(exit: ExitStatus) -> Event {
    if exit.interrupted && !exit.saw_terminal {
        return Event::TurnFailed(TurnFailure {
            kind: FailureKind::Canceled,
            message: "the turn was interrupted".to_string(),
        });
    }
    Event::TurnFailed(TurnFailure {
        kind: FailureKind::Other,
        message: NO_PARSER_YET.to_string(),
    })
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
            super::launch(to_request(request)).map(from_plan)
        }

        fn parse(lines: Vec<String>) -> Vec<Event> {
            super::parse(lines).into_iter().map(from_event).collect()
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
        assert_eq!(value_after(&new, "--session-id").as_deref(), Some(SESSION_ID));
        assert_eq!(index_of(&new, "--resume"), None);

        let mut resumed = request();
        resumed.session.mode = SessionMode::Resume;
        let resumed = args_of(resumed);
        assert_eq!(value_after(&resumed, "--resume").as_deref(), Some(SESSION_ID));
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
        assert_eq!(value_after(&args, "--model").as_deref(), Some("claude-opus-5"));

        let mut padded = request();
        padded.model = Some("  claude-opus-5  ".to_string());
        let args = args_of(padded);
        assert_eq!(value_after(&args, "--model").as_deref(), Some("claude-opus-5"));

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
                assert!(!value.is_empty(), "--model followed by an empty argument for {model:?}");
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
        let config: Value = serde_json::from_str(&plan.files[0].contents)
            .expect("the MCP config must be JSON");
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
        assert_eq!(server["headers"]["Authorization"], json!("Bearer tok\"\n123"));
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

    // ── Scenario 9 — the stubs ────────────────────────────────────────────────

    #[test]
    fn an_interrupted_run_with_no_terminal_event_is_canceled_whatever_the_exit_code() {
        let event = classify_exit(ExitStatus {
            code: Some(0),
            signal: None,
            stderr_tail: String::new(),
            interrupted: true,
            saw_terminal: false,
        });
        let Event::TurnFailed(failure) = event else {
            panic!("classify-exit returns only turn-end or turn-failed");
        };
        assert_eq!(failure.kind, FailureKind::Canceled);
    }

    #[test]
    fn any_other_exit_says_the_parser_is_not_written_yet() {
        let event = classify_exit(ExitStatus {
            code: Some(1),
            signal: None,
            stderr_tail: "boom".to_string(),
            interrupted: false,
            saw_terminal: false,
        });
        let Event::TurnFailed(failure) = event else {
            panic!("classify-exit returns only turn-end or turn-failed");
        };
        assert_eq!(failure.kind, FailureKind::Other);
        assert!(failure.message.contains("parse"));
    }

    #[test]
    fn parse_reads_nothing_yet() {
        assert_eq!(parse(vec!["{\"type\":\"system\"}".to_string()]), Vec::new());
    }
}
