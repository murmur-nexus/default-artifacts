//! The process driver for OpenAI's Codex CLI.
//!
//! Every fact about `codex` — its flags, the features that offer the model a tool of Codex's own,
//! how it is pointed at an MCP server, the JSONL it prints under `exec --json`, and the
//! variables it cannot run without — lives in this crate and nowhere else. The runtime runs *a*
//! process driver; it never knows it is running the Codex one.
//!
//! The driver is granted nothing: no environment, no filesystem, no network, no Murmur host
//! import. It is pure translation both ways — a `LaunchRequest` into the `LaunchPlan` that
//! spawns the harness, and the lines the harness prints into the events the runtime acts on.

use std::collections::VecDeque;

use serde_json::Value;

// ── What this driver drives ───────────────────────────────────────────────────

/// The harness name the runtime traces and reports this driver under.
const HARNESS: &str = "codex";

/// The executable the runtime resolves, unless the manifest overrides it with `command:`.
const BINARY: &str = "codex";

/// The arguments that make the harness print its version, for the runtime's version probe.
/// `codex --version` prints `codex-cli <version>`, and the runtime matches any whitespace token
/// of that line against [`TESTED_VERSIONS`].
const VERSION_ARGS: &[&str] = &["--version"];

/// The `codex` releases this driver release was recorded against with exactly the plan
/// [`launch`] builds: each has its own directory under `tests/fixtures/`. An untested version is
/// a warning at run time (`W-RUN-002`), never a refusal.
const TESTED_VERSIONS: &[&str] = &["0.154.0", "0.167.2"];

/// The variables `codex` cannot run without, each of which the capsule must declare in
/// `capabilities.env.allow` or be refused at load.
///
/// `HOME` locates the login store `~/.codex/auth.json` a ChatGPT login reads its credential from.
/// `PATH` is what the runtime's own bare-name binary resolution walks.
const REQUIRED_ENV: &[&str] = &["HOME", "PATH"];

/// `codex exec --json` prints an agent message and a reasoning summary only once each is
/// complete, so `parse` emits no `text-delta`.
const STREAMS_TEXT: bool = false;

/// `codex exec --json` ends every successful turn with a `turn.completed` line carrying token
/// counts. They are reported for a turn that starts a session: see [`token_usage`].
const REPORTS_USAGE: bool = true;

/// The arguments that make `codex` print its sign-in state and exit, spending no turn. It writes
/// that state to stderr.
const LOGIN_CHECK_ARGS: &[&str] = &["login", "status"];

/// The line `codex login status` prints when no credential is stored.
const LOGGED_OUT_LINE: &str = "Not logged in";

/// What a logged-out reason tells the person to run.
const SIGN_IN_HINT: &str = "run `codex login` as the user mur runs as";

/// The directory holding `codex`'s login store and state: `auth.json` and its token refresh, the
/// `sessions/` rollouts `resume` reads, the log and state databases, and the managed install
/// under `packages/`.
const CODEX_DIR: &str = "~/.codex";

/// The hosts a ChatGPT login reaches: the ChatGPT backend that serves its turns, and the token
/// refresh endpoint.
const HARNESS_HOSTS: &[&str] = &["chatgpt.com", "auth.openai.com"];

// ── Launch constants ──────────────────────────────────────────────────────────

/// The token the runtime replaces with the path of the private `0700` directory it writes
/// `LaunchPlan::files` into, in every `args` entry and every `env-set` value.
const FILES_DIR_TOKEN: &str = "{files_dir}";

/// The file holding the system prompt, which `model_instructions_file` makes Codex's base
/// instructions in place of its own.
const INSTRUCTIONS_FILE_NAME: &str = "instructions.md";

/// The variable the bridge's bearer token travels in. `codex` reads it through
/// `mcp_servers.<server>.bearer_token_env_var`, so the token is in neither argv nor a file.
pub const BRIDGE_TOKEN_VAR: &str = "MURMUR_BRIDGE_TOKEN";

/// The subcommand that runs one non-interactive turn.
const EXEC_SUBCOMMAND: &str = "exec";

/// The `exec` subcommand that continues a stored thread. Every flag goes before it.
const RESUME_SUBCOMMAND: &str = "resume";

/// The prompt argument that makes `codex exec` read the task from stdin.
const STDIN_PROMPT: &str = "-";

/// The arguments every turn starts with, in order, after `exec`.
///
/// - `--json` prints one JSON event per line.
/// - `--skip-git-repo-check` runs in any working directory.
/// - `--ignore-user-config` keeps `$CODEX_HOME/config.toml` out, its MCP servers included,
///   while keeping the stored login.
/// - `--strict-config` refuses an unknown `-c` key, so a renamed key fails the turn rather than
///   doing nothing.
/// - `-s read-only` refuses any write Codex's own tools attempt, `apply_patch` included.
/// - `approval_policy="never"` never stops a headless turn to ask.
const BASE_ARGS: &[&str] = &[
    "--json",
    "--skip-git-repo-check",
    "--ignore-user-config",
    "--strict-config",
    "-s",
    "read-only",
    "-c",
    "approval_policy=\"never\"",
];

/// The Codex features that offer the model a tool of Codex's own, each passed as
/// `--disable <name>`: the shell, `exec`/`write_stdin`, `view_image` (which reads any image on
/// the host), sub-agents, goals, browser and computer use, image generation, connectors,
/// plugins, the in-app browser, tool and skill suggestion, and sleep.
///
/// Every name must be one each tested version knows: `codex` refuses an unknown one with
/// `Error: Unknown feature flag: <name>` before any request, which fails every turn.
const DISABLED_FEATURES: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "view_image",
    "multi_agent",
    "goals",
    "browser_use",
    "computer_use",
    "image_generation",
    "apps",
    "plugins",
    "in_app_browser",
    "tool_suggest",
    "skill_search",
    "sleep_tool",
];

/// The Codex features a headless turn cannot live with, each passed as `--disable <name>` after
/// [`DISABLED_FEATURES`]. `unbounded_connection_retries` retries a request whose connection fails
/// forever, printing `Reconnecting... waiting for network` at least once a minute, so a turn
/// whose harness cannot reach its provider never ends and the runtime's inactivity limit never
/// fires. Disabled, a connection failure is retried five times and then fails the turn.
///
/// The same rule holds as for [`DISABLED_FEATURES`]: every tested version must know the name.
const HEADLESS_DISABLED_FEATURES: &[&str] = &["unbounded_connection_retries"];

/// The `-c` overrides every turn carries after the disable list, in order.
///
/// - `web_search="disabled"` removes the hosted web search tool.
/// - `project_doc_max_bytes=0` keeps an `AGENTS.md` in the working directory out.
/// - `check_for_update_on_startup=false` keeps the version check off the network.
const FIXED_OVERRIDES: &[&str] = &[
    "web_search=\"disabled\"",
    "project_doc_max_bytes=0",
    "check_for_update_on_startup=false",
];

/// The `-c` key that makes the instructions file Codex's base prompt, replacing its own.
const INSTRUCTIONS_KEY: &str = "model_instructions_file";

/// The `-c` key that sets the reasoning effort.
const EFFORT_KEY: &str = "model_reasoning_effort";

/// The `-c` key that sets the reasoning summary mode.
const SUMMARY_KEY: &str = "model_reasoning_summary";

/// The `mcp_servers.<server>` member naming the bridge's URL.
const MCP_URL_MEMBER: &str = "url";

/// The `mcp_servers.<server>` member naming the variable the bearer token is read from.
const MCP_TOKEN_VAR_MEMBER: &str = "bearer_token_env_var";

/// The `mcp_servers.<server>` member that approves every tool of the server up front. Under
/// `approval_policy="never"` without it, every bridged call fails with `MCP tool call requires
/// approval, but approval policy is never`. The bridge enforces the capsule's capabilities on
/// every call itself.
const MCP_APPROVAL_MEMBER: &str = "default_tools_approval_mode";

/// The value of [`MCP_APPROVAL_MEMBER`] that approves every call.
const MCP_APPROVE_ALL: &str = "approve";

// ── Mirrors of the WIT records ────────────────────────────────────────────────
//
// The `#[cfg(target_arch = "wasm32")]` adapter converts the generated WIT types to these and
// back. Every decision lives on this side of that gate, where `cargo test` reaches it. Only the
// variants this driver produces are mirrored.

/// How a turn in progress can be stopped gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMethod {
    SignalInt,
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
    pub reports_usage: bool,
    pub login_check: LoginCheck,
    pub harness_needs: HarnessNeeds,
}

/// How the runtime asks `codex` whether it is signed in, without spending a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginCheck {
    Command(Vec<String>),
}

/// What a harness need may do with a host path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    ReadWrite,
}

/// A host directory `codex` needs beyond what the capsule's class gives every command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathNeed {
    pub path: String,
    pub access: PathAccess,
}

/// What `codex` needs beyond the capsule's own grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessNeeds {
    pub paths: Vec<PathNeed>,
    pub hosts: Vec<String>,
}

/// What the login command printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Whether `codex` is signed in, as [`read_login`] reads the login command's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    LoggedIn,
    LoggedOut(String),
    Unknown(String),
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

/// A tool call the harness has begun, before its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallStart {
    pub id: String,
    pub name: String,
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
    Canceled,
    HarnessError,
}

/// A failed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    pub kind: FailureKind,
    pub message: String,
}

/// Token counts, in the interface's generic names rather than Codex's: only [`token_usage`]
/// knows that `cache_read` is spelled `cached_input_tokens` on the wire.
///
/// Every count is optional and absent is not zero. A zero the harness did write is a
/// measurement, and is carried as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
    pub thinking: Option<u64>,
}

/// One thing the harness did, read out of its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    SessionStarted(SessionInfo),
    Text(String),
    Thinking(String),
    ToolCall(ToolCallInfo),
    ToolResult(ToolResultInfo),
    Retry(RetryInfo),
    /// What the harness has spent on this run, cumulative. Emitted immediately before the
    /// `turn-end` of the `turn.completed` line it was read from, and only for a new session.
    Usage(TokenUsage),
    TurnEnd(String),
    TurnFailed(TurnFailure),
    Note(String),
    /// A bridged call Codex has begun, under the id and bare name its `ToolCall` carries.
    ToolCallStarted(ToolCallStart),
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
        version_args: strings(VERSION_ARGS),
        tested_versions: strings(TESTED_VERSIONS),
        interrupt: InterruptMethod::SignalInt,
        required_env: strings(REQUIRED_ENV),
        streams_text: STREAMS_TEXT,
        reports_usage: REPORTS_USAGE,
        login_check: LoginCheck::Command(strings(LOGIN_CHECK_ARGS)),
        harness_needs: HarnessNeeds {
            paths: vec![PathNeed {
                path: CODEX_DIR.to_string(),
                access: PathAccess::ReadWrite,
            }],
            hosts: strings(HARNESS_HOSTS),
        },
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// Reads what `codex login status` printed into whether `codex` is signed in.
///
/// Only the exit code and the first non-empty line are read: stderr's, where `codex` writes its
/// state, or stdout's when stderr has none. [`LOGGED_OUT_LINE`] is logged out whatever the exit
/// code; any other line with exit `0` is logged in, an API-key login included. No reason quotes
/// the output, because an API-key login prints a masked key.
pub fn read_login(output: &LoginOutput) -> LoginState {
    let line = first_line(&output.stderr).or_else(|| first_line(&output.stdout));
    if line == Some(LOGGED_OUT_LINE) {
        return LoginState::LoggedOut(format!("codex is not signed in: {SIGN_IN_HINT}"));
    }
    match (output.exit_code, line) {
        (Some(0), _) => LoginState::LoggedIn,
        (Some(code), Some(_)) => LoginState::Unknown(format!(
            "`codex login status` exited {code} with output this driver does not recognise"
        )),
        (Some(code), None) => LoginState::Unknown(format!(
            "`codex login status` exited {code} without printing anything"
        )),
        (None, _) => LoginState::Unknown("`codex login status` was ended by a signal".to_string()),
    }
}

/// The first line of `text` that is not blank, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a refused launch, before the harness is spawned —
// a declared setting that is parsed and ignored is a defect, not a convenience.

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &[REASONING_EFFORT_KEY, REASONING_SUMMARY_KEY];

/// The key that sets `model_reasoning_effort`.
const REASONING_EFFORT_KEY: &str = "reasoning_effort";

/// The key that sets `model_reasoning_summary`.
const REASONING_SUMMARY_KEY: &str = "reasoning_summary";

/// The reasoning efforts `codex` names, in its order. Which of them a model honours is up to the
/// model.
const REASONING_EFFORTS: &[&str] = &[
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
    "persistent",
];

/// The reasoning summary modes `codex` accepts, in its order.
const REASONING_SUMMARIES: &[&str] = &["auto", "concise", "detailed", "none"];

/// A reasoning effort `codex` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    Persistent,
}

impl ReasoningEffort {
    const ALL: [Self; 9] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
        Self::Persistent,
    ];

    /// The value `codex` reads for `model_reasoning_effort`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => REASONING_EFFORTS[0],
            Self::Minimal => REASONING_EFFORTS[1],
            Self::Low => REASONING_EFFORTS[2],
            Self::Medium => REASONING_EFFORTS[3],
            Self::High => REASONING_EFFORTS[4],
            Self::XHigh => REASONING_EFFORTS[5],
            Self::Max => REASONING_EFFORTS[6],
            Self::Ultra => REASONING_EFFORTS[7],
            Self::Persistent => REASONING_EFFORTS[8],
        }
    }
}

/// A reasoning summary mode `codex` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
    None,
}

impl ReasoningSummary {
    const ALL: [Self; 4] = [Self::Auto, Self::Concise, Self::Detailed, Self::None];

    /// The value `codex` reads for `model_reasoning_summary`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => REASONING_SUMMARIES[0],
            Self::Concise => REASONING_SUMMARIES[1],
            Self::Detailed => REASONING_SUMMARIES[2],
            Self::None => REASONING_SUMMARIES[3],
        }
    }
}

/// The dials read from `inference.driver.config`. An unconfigured capsule gets
/// [`DriverConfig::default`]: every dial left to `codex` and the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriverConfig {
    /// `reasoning_effort` — passed as `-c model_reasoning_effort="<level>"` when set.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// `reasoning_summary` — passed as `-c model_reasoning_summary="<mode>"` when set.
    pub reasoning_summary: Option<ReasoningSummary>,
}

/// Reads `inference.driver.config`, which murmur hands a process driver as compact JSON in
/// `launch-request.config`, into the dial set.
///
/// An absent config, `{}` and a whitespace-only value all yield [`DriverConfig::default`].
/// Anything else that is not an object of accepted keys with accepted values is refused with a
/// message naming the problem.
pub fn parse_driver_config(config: Option<&str>) -> Result<DriverConfig, String> {
    let Some(raw) = config else {
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
        reasoning_effort: one_of(
            config.get(REASONING_EFFORT_KEY),
            REASONING_EFFORT_KEY,
            &ReasoningEffort::ALL,
            |effort| effort.as_str(),
        )?,
        reasoning_summary: one_of(
            config.get(REASONING_SUMMARY_KEY),
            REASONING_SUMMARY_KEY,
            &ReasoningSummary::ALL,
            |summary| summary.as_str(),
        )?,
    })
}

/// The choice whose name is exactly the JSON string `value`, `None` when the key is absent, and a
/// refusal naming every choice for anything else: `"HIGH"`, `"high "`, `3` and `null` alike.
fn one_of<T: Copy>(
    value: Option<&Value>,
    key: &str,
    choices: &[T],
    name: fn(T) -> &'static str,
) -> Result<Option<T>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    value
        .as_str()
        .and_then(|given| {
            choices
                .iter()
                .copied()
                .find(|choice| name(*choice) == given)
        })
        .map(Some)
        .ok_or_else(|| {
            let names: Vec<&str> = choices.iter().map(|choice| name(*choice)).collect();
            format!(
                "driver: inference.driver.config '{key}' must be one of {}, got {value}",
                names.join(", ")
            )
        })
}

// ── launch ───────────────────────────────────────────────────────────────────

/// What one accepted `launch` produced: the plan that spawns the harness, and the context its
/// output is read against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    pub plan: LaunchPlan,
    pub context: ParseContext,
}

/// Plans one turn as one `codex exec --json` run.
///
/// `inference.driver.config` is read first, and a config [`parse_driver_config`] refuses is
/// this function's `Err`. A bridge whose server name cannot be a TOML key segment, and a resume
/// whose id is blank or would be read as a flag, are refused next, before anything is planned.
pub fn launch(request: LaunchRequest) -> Result<Launched, String> {
    let config = parse_driver_config(request.config.as_deref())?;
    if let Some(bridge) = request.bridge.as_ref() {
        check_server_name(&bridge.server_name)?;
    }
    if request.session.mode == SessionMode::Resume {
        check_resume_id(&request.session.id)?;
    }

    let plan = LaunchPlan {
        args: build_args(&request, &config),
        // The token reaches `codex` through `bearer_token_env_var`, so it is in neither argv,
        // which any process on the host can read, nor a file. Nothing else is set: the harness
        // gets exactly what `capabilities.env.allow` passes through.
        env_set: request
            .bridge
            .as_ref()
            .map(|bridge| (BRIDGE_TOKEN_VAR.to_string(), bridge.bearer_token.clone()))
            .into_iter()
            .collect(),
        files: vec![DriverFile {
            name: INSTRUCTIONS_FILE_NAME.to_string(),
            contents: request.system_prompt.clone(),
        }],
        // `-` makes `codex exec` read the prompt from stdin until it closes.
        stdin: Some(request.task.clone().into_bytes()),
        keep_stdin_open: false,
        // The turn is interrupted with SIGINT; nothing is written to stdin for it.
        interrupt_stdin: None,
    };
    Ok(Launched {
        plan,
        context: parse_context_for(request.bridge.as_ref(), request.session.mode),
    })
}

/// A server name that can be written as one segment of a `-c mcp_servers.<name>.*` key path.
fn check_server_name(name: &str) -> Result<(), String> {
    let usable = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if usable {
        Ok(())
    } else {
        Err(format!(
            "driver: the tool bridge's server name '{name}' cannot name a codex MCP server: it becomes part of a -c mcp_servers.<name> path, so it may use only ASCII letters, digits, '_' and '-'"
        ))
    }
}

/// A session id `codex exec resume` reads as the id: not blank, and not starting with `-`, which
/// it would read as a flag.
fn check_resume_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() || id.starts_with('-') {
        Err(format!(
            "driver: cannot resume codex session '{id}': the id is blank or would be read as a flag"
        ))
    } else {
        Ok(())
    }
}

/// The full argument list after `codex`.
///
/// A new session puts no id in argv: `codex` mints its own and reports it on `thread.started`,
/// which the runtime stores and hands back for the next turn to resume. Every flag is passed
/// again on a resume, because none of them is stored with the thread, and every flag goes before
/// `resume`.
fn build_args(request: &LaunchRequest, config: &DriverConfig) -> Vec<String> {
    let mut args = vec![EXEC_SUBCOMMAND.to_string()];
    args.extend(strings(BASE_ARGS));
    for feature in DISABLED_FEATURES.iter().chain(HEADLESS_DISABLED_FEATURES) {
        args.push("--disable".to_string());
        args.push(feature.to_string());
    }
    for override_ in FIXED_OVERRIDES {
        push_override(&mut args, override_.to_string());
    }
    push_override(
        &mut args,
        format!(
            "{INSTRUCTIONS_KEY}={}",
            toml_string(&format!("{FILES_DIR_TOKEN}/{INSTRUCTIONS_FILE_NAME}"))
        ),
    );

    if let Some(bridge) = request.bridge.as_ref() {
        let server = &bridge.server_name;
        push_override(
            &mut args,
            format!(
                "mcp_servers.{server}.{MCP_URL_MEMBER}={}",
                toml_string(&bridge.url)
            ),
        );
        push_override(
            &mut args,
            format!(
                "mcp_servers.{server}.{MCP_TOKEN_VAR_MEMBER}={}",
                toml_string(BRIDGE_TOKEN_VAR)
            ),
        );
        push_override(
            &mut args,
            format!(
                "mcp_servers.{server}.{MCP_APPROVAL_MEMBER}={}",
                toml_string(MCP_APPROVE_ALL)
            ),
        );
    }

    if let Some(effort) = config.reasoning_effort {
        push_override(
            &mut args,
            format!("{EFFORT_KEY}={}", toml_string(effort.as_str())),
        );
    }
    if let Some(summary) = config.reasoning_summary {
        push_override(
            &mut args,
            format!("{SUMMARY_KEY}={}", toml_string(summary.as_str())),
        );
    }

    if let Some(model) = model_arg(request.model.as_deref()) {
        args.push("-m".to_string());
        args.push(model.to_string());
    }

    if request.session.mode == SessionMode::Resume {
        args.push(RESUME_SUBCOMMAND.to_string());
        args.push(request.session.id.clone());
    }
    args.push(STDIN_PROMPT.to_string());
    args
}

fn push_override(args: &mut Vec<String>, assignment: String) {
    args.push("-c".to_string());
    args.push(assignment);
}

/// `value` as a TOML basic string. A JSON string literal as `serde_json` writes it is one: the
/// same quotes, and escapes TOML reads identically.
fn toml_string(value: &str) -> String {
    Value::String(value.to_string()).to_string()
}

/// The model to pass to `-m`, or `None` to leave the choice to the harness. A blank model is the
/// same as no model at all.
fn model_arg(model: Option<&str>) -> Option<&str> {
    model.map(str::trim).filter(|model| !model.is_empty())
}

// ── parse ────────────────────────────────────────────────────────────────────

/// `session-info.auth` for every session: `codex exec --json` does not say whether a turn bills
/// a ChatGPT plan or an API key.
pub const AUTH_UNKNOWN: &str = "unknown";

/// How many characters of Codex's text a `note` repeats before truncating it, so one runaway
/// line cannot fill a trace.
pub const NOTE_LINE_LIMIT: usize = 512;

/// What a `note` about an unreadable line opens with.
const NOTE_PREFIX: &str = "unreadable stdout line: ";

/// How many capsule call ids [`ParseState`] remembers as announced before forgetting the oldest.
pub const ANNOUNCED_LIMIT: usize = 1_024;

/// The `arguments` of a call that carries none.
const EMPTY_TOOL_INPUT: &str = "{}";

/// The `status` of an MCP call that returned a result.
const CALL_COMPLETED: &str = "completed";

/// The `status` of an MCP call that returned no result, with its reason in `error.message`.
const CALL_FAILED: &str = "failed";

/// The output of a failed call whose `error` carries no message.
const FAILED_CALL_WITHOUT_MESSAGE: &str = "codex reported the call failed with no message";

/// What a `codex` retry message opens with, with the ASCII and the Unicode ellipsis, before
/// `<attempt>/<limit>`.
const RECONNECTING_PREFIXES: &[&str] = &["Reconnecting... ", "Reconnecting… "];

/// What a retry says when the harness names no reason.
const UNKNOWN_RETRY_REASON: &str = "unknown";

/// What a `turn-failed` says when the `turn.failed` line carries no message.
const UNDESCRIBED_FAILURE: &str = "the harness reported a failure with no message";

/// The texts in a failure message that mean the provider refused the credential. Codex reports
/// a failure as text only, so the status it quotes is what is read.
const AUTH_MARKERS: &[&str] = &[
    "401 Unauthorized",
    "403 Forbidden",
    "status 401",
    "status 403",
];

/// The texts in a failure message that mean a rate limit or a spent plan. `codex` 0.154.0 writes
/// the usage-limit message with an ASCII apostrophe and 0.167.2 with a typographic one.
const QUOTA_MARKERS: &[&str] = &[
    "429 Too Many Requests",
    "status 429",
    "You've hit your usage limit",
    "You\u{2019}ve hit your usage limit",
    "Quota exceeded",
];

/// The stderr line every run prints after the turn: murmur's bridge answers the MCP session
/// `DELETE` with 400. It says nothing about the turn.
const DELETE_SESSION_NOISE: &str = "fail to delete session";

/// The stderr line a run with `CODEX_HOME` under a temporary directory prints: `codex` refuses to
/// create its helper binaries there. It says nothing about the turn.
const PATH_ALIASES_NOISE: &str = "could not create PATH aliases";

/// What `launch` learned that `parse` needs. Fixed for the run; what the lines build up as they
/// are read is [`ParseState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseContext {
    /// The MCP server the capsule's tools were registered under, or `None` for a run with no
    /// bridge, in which no call is a capsule call.
    pub server_name: Option<String>,
    /// Whether the run resumes a thread, whose `turn.completed` counts are the thread's running
    /// total rather than this run's spend.
    pub resumed: bool,
}

impl ParseContext {
    /// A new session with no bridge.
    pub const fn none() -> Self {
        Self {
            server_name: None,
            resumed: false,
        }
    }
}

/// The context a turn launched with `bridge` in session mode `mode` reads its output against.
pub fn parse_context_for(bridge: Option<&Bridge>, mode: SessionMode) -> ParseContext {
    ParseContext {
        server_name: bridge.map(|bridge| bridge.server_name.clone()),
        resumed: mode == SessionMode::Resume,
    }
}

/// Everything `parse` carries from one batch to the next for one harness process.
///
/// The interface hands a run's lines over in batches of any size; carrying this between them is
/// what makes the events of a run independent of where the batches fall. `launch` starts a new
/// one, because a new process is a new stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseState {
    /// The capsule calls already given `tool-call-started` and `tool-call`, oldest first, at
    /// most [`ANNOUNCED_LIMIT`] of them.
    announced: VecDeque<String>,
    /// The text of the last non-empty agent message, which `turn-end` carries.
    last_text: Option<String>,
}

impl ParseState {
    /// A process that has printed nothing yet.
    pub const fn new() -> Self {
        Self {
            announced: VecDeque::new(),
            last_text: None,
        }
    }

    fn is_announced(&self, id: &str) -> bool {
        self.announced.iter().any(|known| known == id)
    }

    fn announce(&mut self, id: &str) {
        if self.announced.len() >= ANNOUNCED_LIMIT {
            self.announced.pop_front();
        }
        self.announced.push_back(id.to_string());
    }
}

/// Reads a batch of complete stdout lines into events: the events of its lines, in order.
pub fn parse_lines(lines: &[String], context: &ParseContext, state: &mut ParseState) -> Vec<Event> {
    lines
        .iter()
        .flat_map(|line| parse_line(line, context, state))
        .collect()
}

/// Reads one stdout line into the events it carries.
///
/// Never fails and never panics: a line that is not a JSON object of a type `codex exec --json`
/// prints becomes a single `note` for the trace.
pub fn parse_line(line: &str, context: &ParseContext, state: &mut ParseState) -> Vec<Event> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let Some(value) = line_object(line) else {
        return vec![note_for(line)];
    };

    match value.get("type").and_then(Value::as_str) {
        Some("thread.started") => {
            let id = str_at(&value, "thread_id");
            if id.is_empty() {
                Vec::new()
            } else {
                vec![Event::SessionStarted(SessionInfo {
                    id: id.to_string(),
                    auth: AUTH_UNKNOWN.to_string(),
                    model: None,
                })]
            }
        }
        Some("turn.started") | Some("item.updated") => Vec::new(),
        Some("item.started") => item_started(&value["item"], context, state),
        Some("item.completed") => item_completed(line, &value["item"], context, state),
        Some("error") => vec![error_event(str_at(&value, "message"))],
        Some("turn.completed") => turn_completed(&value, context, state),
        Some("turn.failed") => vec![turn_failed(&value)],
        _ => vec![note_for(line)],
    }
}

/// A line as the JSON object it is, or `None` for anything else.
fn line_object(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line)
        .ok()
        .filter(Value::is_object)
}

/// A `note` repeating a line the driver could not read, truncated at [`NOTE_LINE_LIMIT`].
fn note_for(line: &str) -> Event {
    Event::Note(format!("{NOTE_PREFIX}{}", clip(line)))
}

/// Codex's `text` cut at [`NOTE_LINE_LIMIT`] characters, with `…` marking the cut.
fn clip(text: &str) -> String {
    match text.char_indices().nth(NOTE_LINE_LIMIT) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// An `mcp_tool_call` item addressed to the capsule's own bridge, with the id and bare name its
/// events carry.
struct CapsuleCall<'a> {
    id: &'a str,
    name: &'a str,
}

/// The item as a capsule call: an `mcp_tool_call` with a non-empty `id`, a `server` equal to the
/// bridge's server name, and a non-empty `tool`. `codex` reports `tool` under the name the
/// server listed, which is already the bare artifact name.
fn capsule_call<'a>(item: &'a Value, context: &ParseContext) -> Option<CapsuleCall<'a>> {
    let server = context.server_name.as_deref()?;
    let id = str_at(item, "id");
    let name = str_at(item, "tool");
    (str_at(item, "server") == server && !id.is_empty() && !name.is_empty())
        .then_some(CapsuleCall { id, name })
}

fn is_mcp_call(item: &Value) -> bool {
    str_at(item, "type") == "mcp_tool_call"
}

/// Reads an `item.started` line. Only a capsule call's start is reported; an MCP call to any
/// other server is noted, and every other item is reported when it completes.
fn item_started(item: &Value, context: &ParseContext, state: &mut ParseState) -> Vec<Event> {
    if !is_mcp_call(item) {
        return Vec::new();
    }
    match capsule_call(item, context) {
        Some(call) => announce(&call, item, state),
        None => vec![Event::Note(format!(
            "codex started a call to '{}' on MCP server '{}', which is not one of the capsule's tools (call {})",
            clip(str_at(item, "tool")),
            clip(str_at(item, "server")),
            clip(str_at(item, "id")),
        ))],
    }
}

/// `tool-call-started` then `tool-call` for a capsule call not yet announced, nothing for one
/// that was. The arguments are read only into the `tool-call`.
fn announce(call: &CapsuleCall, item: &Value, state: &mut ParseState) -> Vec<Event> {
    if state.is_announced(call.id) {
        return Vec::new();
    }
    state.announce(call.id);
    vec![
        Event::ToolCallStarted(ToolCallStart {
            id: call.id.to_string(),
            name: call.name.to_string(),
        }),
        Event::ToolCall(ToolCallInfo {
            id: call.id.to_string(),
            name: call.name.to_string(),
            input: match item.get("arguments") {
                Some(arguments) if !arguments.is_null() => arguments.to_string(),
                _ => EMPTY_TOOL_INPUT.to_string(),
            },
        }),
    ]
}

/// Reads an `item.completed` line.
fn item_completed(
    line: &str,
    item: &Value,
    context: &ParseContext,
    state: &mut ParseState,
) -> Vec<Event> {
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return vec![note_for(line)];
    };
    match kind {
        "agent_message" => {
            let text = str_at(item, "text");
            if text.is_empty() {
                return Vec::new();
            }
            state.last_text = Some(text.to_string());
            vec![Event::Text(text.to_string())]
        }
        "reasoning" => {
            let text = str_at(item, "text");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Event::Thinking(text.to_string())]
            }
        }
        "error" => vec![Event::Note(format!(
            "codex error item: {}",
            clip(str_at(item, "message"))
        ))],
        "mcp_tool_call" => match capsule_call(item, context) {
            Some(call) => {
                let mut events = announce(&call, item, state);
                events.push(Event::ToolResult(tool_result(call.id, item)));
                events
            }
            None => vec![Event::Note(format!(
                "codex finished a call to '{}' on MCP server '{}', which is not one of the capsule's tools (call {}, status {})",
                clip(str_at(item, "tool")),
                clip(str_at(item, "server")),
                clip(str_at(item, "id")),
                clip(str_at(item, "status")),
            ))],
        },
        other => {
            let mut note = format!(
                "codex reported its own '{}' item ({})",
                clip(other),
                clip(str_at(item, "id"))
            );
            if let Some(status) = item.get("status").and_then(Value::as_str) {
                note.push_str(&format!(", status {}", clip(status)));
            }
            vec![Event::Note(note)]
        }
    }
}

/// The `tool-result` of a completed capsule call.
///
/// A call is an error when its `status` is not `completed`, or its result says `isError` (or
/// `is_error`) as a JSON `true`. `codex` reports a call it could not make as `failed` with the
/// reason in `error.message` and no result, and a call whose tool answered `isError: true` as
/// `failed` with no `error` and the tool's own answer in `result`. So a failed call's output is
/// its `error.message`, else its result, else [`FAILED_CALL_WITHOUT_MESSAGE`]. A result's output
/// is its `content` items, one per line, with an image as its [`image_reference`] line; or, when
/// that content is empty, its `structured_content` as JSON.
fn tool_result(id: &str, item: &Value) -> ToolResultInfo {
    let status = str_at(item, "status");
    let result = &item["result"];
    let flagged = ["isError", "is_error"]
        .iter()
        .any(|key| result.get(key).and_then(Value::as_bool) == Some(true));

    let answer = || {
        let content = tool_result_output(result.get("content"));
        match result.get("structured_content") {
            Some(structured) if content.is_empty() && !structured.is_null() => {
                structured.to_string()
            }
            _ => content,
        }
    };
    let output = if status == CALL_FAILED {
        let message = str_at(&item["error"], "message");
        if !message.is_empty() {
            message.to_string()
        } else {
            Some(answer())
                .filter(|answer| !answer.is_empty())
                .unwrap_or_else(|| FAILED_CALL_WITHOUT_MESSAGE.to_string())
        }
    } else {
        answer()
    };

    ToolResultInfo {
        id: id.to_string(),
        output,
        is_error: status != CALL_COMPLETED || flagged,
    }
}

/// A result's content as one string. An image item becomes its [`image_reference`] line, so the
/// feed names the image and never carries its base64; any other item that is not text keeps its
/// JSON rather than being dropped.
fn tool_result_output(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(output)) => output.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => text_at(block, "text"),
                Some("image") => image_reference(block),
                _ => block.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(image) if is_image_block(image) => image_reference(image),
        Some(other) => other.to_string(),
    }
}

/// The media type an image line names when the block carries none.
pub const UNKNOWN_MEDIA_TYPE: &str = "unknown media type";

/// The size clause an image line carries when the block's data is absent or not valid base64.
pub const UNKNOWN_SIZE: &str = "size unknown";

fn is_image_block(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("image")
}

/// The line the feed carries in place of an image block: `[image: <media type>, <n> bytes]`,
/// where `<n>` is the size of the image the base64 decodes to. It is murmur's trace placeholder
/// without the hash, which the trace's `tool_call.attachments` entry for the same call carries.
///
/// Reads both shapes an image block arrives in: the MCP one (`mimeType`, `data`), which is what
/// murmur's bridge sends and `codex` reports, and the Anthropic one (`source.media_type`,
/// `source.data`). A block with no media type names [`UNKNOWN_MEDIA_TYPE`]; one whose data is
/// absent, not a string or not valid base64 says [`UNKNOWN_SIZE`].
fn image_reference(block: &Value) -> String {
    let source = block.get("source").filter(|source| source.is_object());
    let media_type = [
        source.and_then(|source| source.get("media_type")),
        block.get("mimeType"),
        block.get("media_type"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|media_type| !media_type.trim().is_empty())
    .unwrap_or(UNKNOWN_MEDIA_TYPE);
    let size = [
        source.and_then(|source| source.get("data")),
        block.get("data"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .and_then(base64_decoded_len)
    .map_or_else(
        || UNKNOWN_SIZE.to_string(),
        |bytes| format!("{bytes} bytes"),
    );
    format!("[image: {media_type}, {size}]")
}

/// The number of bytes standard base64 `data` decodes to, worked out from its length alone:
/// nothing is decoded and nothing allocated.
///
/// ASCII whitespace anywhere is ignored. Every other character must be in `A–Z a–z 0–9 + /`, or
/// `=` in the last one or two places. Padded data must be a whole number of 4-character groups;
/// unpadded data may end on a partial group of 2 or 3 characters, never 1. `None` for anything
/// else.
fn base64_decoded_len(data: &str) -> Option<u64> {
    let mut symbols: u64 = 0;
    let mut padding: u64 = 0;
    for byte in data.bytes() {
        match byte {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'=' => padding += 1,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' if padding == 0 => symbols += 1,
            _ => return None,
        }
    }
    let partial = symbols % 4;
    let valid = match padding {
        0 => partial != 1,
        1 => partial == 3,
        2 => partial == 2,
        _ => false,
    };
    valid.then(|| symbols / 4 * 3 + partial * 3 / 4)
}

/// Reads an `error` line: a numbered `Reconnecting…` message is a retry, anything else a note.
fn error_event(message: &str) -> Event {
    match retry_of(message) {
        Some(retry) => Event::Retry(retry),
        None => Event::Note(format!("codex error: {}", clip(message))),
    }
}

/// The retry a `Reconnecting... <n>/<m> (<reason>)` message reports, or `None` for any other
/// message. The reason is the text inside the outermost parentheses after `<n>/<m>`, or else the
/// rest of the message.
fn retry_of(message: &str) -> Option<RetryInfo> {
    let rest = RECONNECTING_PREFIXES
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))?;
    let (attempt, rest) = rest.split_once('/')?;
    let attempt = digits(attempt)?.parse::<u32>().ok()?;
    let limit_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if limit_len == 0 {
        return None;
    }
    let rest = &rest[limit_len..];

    let in_parentheses = match (rest.find('('), rest.rfind(')')) {
        (Some(open), Some(close)) if open < close => Some(rest[open + 1..close].trim()),
        _ => None,
    };
    let reason = in_parentheses
        .filter(|reason| !reason.is_empty())
        .or_else(|| Some(rest.trim()).filter(|rest| !rest.is_empty()))
        .unwrap_or(UNKNOWN_RETRY_REASON);
    Some(RetryInfo {
        attempt,
        reason: reason.to_string(),
    })
}

/// `text` when it is one or more ASCII digits.
fn digits(text: &str) -> Option<&str> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then_some(text)
}

/// Reads a `turn.completed` line: the counts for a new session, then `turn-end`.
///
/// A resumed thread's counts are the thread's running total since its first turn, not what this
/// run spent, and the runtime would charge the whole history to this turn. So a resumed turn
/// reports no `usage`, and notes the total instead.
fn turn_completed(value: &Value, context: &ParseContext, state: &ParseState) -> Vec<Event> {
    let mut events = Vec::new();
    if let Some(usage) = value.get("usage") {
        if let Some(counts) = token_usage(usage) {
            if context.resumed {
                events.push(Event::Note(format!(
                    "codex reports a resumed thread's running total, not this turn's spend, so no usage is reported: {}",
                    clip(&usage.to_string())
                )));
            } else {
                events.push(Event::Usage(counts));
            }
        }
    }
    events.push(Event::TurnEnd(state.last_text.clone().unwrap_or_default()));
    events
}

/// The token counts a `turn.completed` line's `usage` object reports, or `None` when it reports
/// none at all.
///
/// The one place in this repo where Codex's spelling of a count appears. Each count is read as a
/// non-negative integer, so one that is absent, `null`, a string, a float or negative reads as
/// absent rather than as zero. `input_tokens` counts every prompt token, cache reads and cache
/// writes included, so `input` is what is left once both are taken out — saturating at zero, an
/// absent cache count taken as none.
pub fn token_usage(usage: &Value) -> Option<TokenUsage> {
    let cache_read = count_at(usage, "cached_input_tokens");
    let cache_creation = count_at(usage, "cache_write_input_tokens");
    let counts = TokenUsage {
        input: count_at(usage, "input_tokens").map(|input| {
            input
                .saturating_sub(cache_read.unwrap_or(0))
                .saturating_sub(cache_creation.unwrap_or(0))
        }),
        output: count_at(usage, "output_tokens"),
        cache_read,
        cache_creation,
        thinking: count_at(usage, "reasoning_output_tokens"),
    };
    [
        counts.input,
        counts.output,
        counts.cache_read,
        counts.cache_creation,
        counts.thinking,
    ]
    .iter()
    .any(Option::is_some)
    .then_some(counts)
}

/// The non-negative integer at `key`, or `None` for every other way a count can fail to be one.
fn count_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// Reads a `turn.failed` line. Codex carries a failure as text alone, so its kind is read from
/// the status or limit message the text quotes.
fn turn_failed(value: &Value) -> Event {
    let message = str_at(&value["error"], "message");
    let message = if message.is_empty() {
        UNDESCRIBED_FAILURE
    } else {
        message
    };
    failed(failure_kind(message), message.to_string())
}

/// The kind a failure message names: auth before quota, else a harness error. Only a status
/// spelled out with its reason phrase or after `status ` counts, never a bare number.
fn failure_kind(message: &str) -> FailureKind {
    if AUTH_MARKERS.iter().any(|marker| message.contains(marker)) {
        FailureKind::Auth
    } else if QUOTA_MARKERS.iter().any(|marker| message.contains(marker)) {
        FailureKind::Quota
    } else {
        FailureKind::HarnessError
    }
}

/// The string at `key`, or `""` when the harness wrote none there.
fn str_at<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The owned string at `key`, or `""` when the harness wrote none there.
fn text_at(value: &Value, key: &str) -> String {
    str_at(value, key).to_string()
}

// ── classify-exit ────────────────────────────────────────────────────────────

/// Classifies a run whose output ended without a terminal event.
///
/// It only ever answers `turn-failed`: a turn whose result nobody read is not one that
/// succeeded.
pub fn classify_exit(exit: ExitStatus) -> Event {
    // Before the exit code is read at all: an interrupted turn's code says nothing about it.
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
    let tail = stderr_tail(&exit.stderr_tail);
    let message = if tail.is_empty() {
        what
    } else {
        format!("{what}: {tail}")
    };
    failed(FailureKind::HarnessError, message)
}

/// The stderr tail without the lines every run prints whatever happened, or the whole trimmed
/// tail when nothing else is left.
fn stderr_tail(tail: &str) -> String {
    let telling: Vec<&str> = tail
        .lines()
        .filter(|line| !line.contains(DELETE_SESSION_NOISE) && !line.contains(PATH_ALIASES_NOISE))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if telling.is_empty() {
        tail.trim().to_string()
    } else {
        telling.join("\n")
    }
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

        /// What the lines of this process have built up so far: the capsule calls already
        /// announced and the last agent message. Carried from one `parse` to the next, which
        /// is what keeps the events of a run independent of how its lines are batched.
        static STATE: RefCell<super::ParseState> =
            const { RefCell::new(super::ParseState::new()) };
    }

    use exports::murmur::driver::process::{
        Bridge, Description, DriverFile, Event, ExitStatus, FailureKind, Guest, HarnessNeeds,
        InterruptMethod, LaunchPlan, LaunchRequest, LoginCheck, LoginOutput, LoginState,
        PathAccess, PathNeed, RetryInfo, Session, SessionInfo, SessionMode, ToolCallInfo,
        ToolCallStart, ToolResultInfo, TurnFailure, Usage,
    };

    pub struct CodexCliDriver;

    impl Guest for CodexCliDriver {
        fn describe() -> Description {
            from_description(super::describe())
        }

        fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
            super::launch(to_request(request)).map(|launched| {
                PARSE_CONTEXT.with(|stored| *stored.borrow_mut() = launched.context);
                // A new process has announced no call and printed no message.
                STATE.with(|state| *state.borrow_mut() = super::ParseState::new());
                from_plan(launched.plan)
            })
        }

        fn parse(lines: Vec<String>) -> Vec<Event> {
            PARSE_CONTEXT
                .with(|context| {
                    STATE.with(|state| {
                        super::parse_lines(&lines, &context.borrow(), &mut state.borrow_mut())
                    })
                })
                .into_iter()
                .map(from_event)
                .collect()
        }

        fn classify_exit(exit: ExitStatus) -> Event {
            from_event(super::classify_exit(to_exit(exit)))
        }

        fn read_login(output: LoginOutput) -> LoginState {
            match super::read_login(&super::LoginOutput {
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            }) {
                super::LoginState::LoggedIn => LoginState::LoggedIn,
                super::LoginState::LoggedOut(reason) => LoginState::LoggedOut(reason),
                super::LoginState::Unknown(reason) => LoginState::Unknown(reason),
            }
        }
    }

    fn from_description(description: super::Description) -> Description {
        Description {
            harness: description.harness,
            binary: description.binary,
            version_args: description.version_args,
            tested_versions: description.tested_versions,
            interrupt: match description.interrupt {
                super::InterruptMethod::SignalInt => InterruptMethod::SignalInt,
            },
            required_env: description.required_env,
            streams_text: description.streams_text,
            reports_usage: description.reports_usage,
            login_check: match description.login_check {
                super::LoginCheck::Command(args) => LoginCheck::Command(args),
            },
            harness_needs: HarnessNeeds {
                paths: description
                    .harness_needs
                    .paths
                    .into_iter()
                    .map(|need| PathNeed {
                        path: need.path,
                        access: match need.access {
                            super::PathAccess::ReadWrite => PathAccess::ReadWrite,
                        },
                    })
                    .collect(),
                hosts: description.harness_needs.hosts,
            },
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
            super::Event::Text(text) => Event::Text(text),
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
            super::Event::Usage(usage) => Event::Usage(Usage {
                input: usage.input,
                output: usage.output,
                cache_read: usage.cache_read,
                cache_creation: usage.cache_creation,
                thinking: usage.thinking,
            }),
            super::Event::TurnEnd(result) => Event::TurnEnd(result),
            super::Event::TurnFailed(failure) => Event::TurnFailed(TurnFailure {
                kind: match failure.kind {
                    super::FailureKind::Auth => FailureKind::Auth,
                    super::FailureKind::Quota => FailureKind::Quota,
                    super::FailureKind::Canceled => FailureKind::Canceled,
                    super::FailureKind::HarnessError => FailureKind::HarnessError,
                },
                message: failure.message,
            }),
            super::Event::Note(note) => Event::Note(note),
            super::Event::ToolCallStarted(start) => Event::ToolCallStarted(ToolCallStart {
                id: start.id,
                name: start.name,
            }),
        }
    }

    export!(CodexCliDriver);
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    const SERVER: &str = "murmur";
    const TOKEN: &str = "tok-123";
    const THREAD: &str = "01a1cd9a-a4b0-79d0-ae59-160f5df00b3d";
    const SYSTEM_PROMPT: &str = "- You are a Murmur capsule.\n";

    fn request() -> LaunchRequest {
        LaunchRequest {
            model: None,
            system_prompt: SYSTEM_PROMPT.to_string(),
            config: None,
            bridge: None,
            session: Session {
                id: "ignored-for-a-new-session".to_string(),
                mode: SessionMode::New,
            },
            harness_version: None,
            task: "Do the thing.".to_string(),
        }
    }

    fn bridge() -> Bridge {
        Bridge {
            server_name: SERVER.to_string(),
            url: "http://127.0.0.1:8931/mcp".to_string(),
            bearer_token: TOKEN.to_string(),
            tool_names: vec!["echo_tool".to_string(), "session-status".to_string()],
        }
    }

    fn bridged() -> LaunchRequest {
        LaunchRequest {
            bridge: Some(bridge()),
            ..request()
        }
    }

    fn resumed() -> LaunchRequest {
        let mut request = bridged();
        request.session = Session {
            id: THREAD.to_string(),
            mode: SessionMode::Resume,
        };
        request
    }

    fn plan_of(request: LaunchRequest) -> LaunchPlan {
        launch(request).expect("launch plans this request").plan
    }

    fn refusal_of(request: LaunchRequest) -> String {
        launch(request).expect_err("launch refuses this request")
    }

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    /// The argv every turn opens with, through `model_instructions_file`.
    const COMMON_ARGS: &[&str] = &[
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--strict-config",
        "-s",
        "read-only",
        "-c",
        "approval_policy=\"never\"",
        "--disable",
        "shell_tool",
        "--disable",
        "unified_exec",
        "--disable",
        "view_image",
        "--disable",
        "multi_agent",
        "--disable",
        "goals",
        "--disable",
        "browser_use",
        "--disable",
        "computer_use",
        "--disable",
        "image_generation",
        "--disable",
        "apps",
        "--disable",
        "plugins",
        "--disable",
        "in_app_browser",
        "--disable",
        "tool_suggest",
        "--disable",
        "skill_search",
        "--disable",
        "sleep_tool",
        "--disable",
        "unbounded_connection_retries",
        "-c",
        "web_search=\"disabled\"",
        "-c",
        "project_doc_max_bytes=0",
        "-c",
        "check_for_update_on_startup=false",
        "-c",
        "model_instructions_file=\"{files_dir}/instructions.md\"",
    ];

    /// The three overrides that register the test bridge.
    const BRIDGE_ARGS: &[&str] = &[
        "-c",
        "mcp_servers.murmur.url=\"http://127.0.0.1:8931/mcp\"",
        "-c",
        "mcp_servers.murmur.bearer_token_env_var=\"MURMUR_BRIDGE_TOKEN\"",
        "-c",
        "mcp_servers.murmur.default_tools_approval_mode=\"approve\"",
    ];

    fn context() -> ParseContext {
        parse_context_for(Some(&bridge()), SessionMode::New)
    }

    fn resumed_context() -> ParseContext {
        parse_context_for(Some(&bridge()), SessionMode::Resume)
    }

    fn events(line: Value) -> Vec<Event> {
        parse_line(&line.to_string(), &context(), &mut ParseState::new())
    }

    fn events_in(lines: &[Value], context: &ParseContext) -> Vec<Event> {
        let lines: Vec<String> = lines.iter().map(Value::to_string).collect();
        parse_lines(&lines, context, &mut ParseState::new())
    }

    fn note(text: &str) -> Event {
        Event::Note(text.to_string())
    }

    fn mcp_item(server: &str, status: &str, result: Value, error: Value) -> Value {
        json!({
            "id": "item_1",
            "type": "mcp_tool_call",
            "server": server,
            "tool": "echo_tool",
            "arguments": {"text": "hi"},
            "result": result,
            "error": error,
            "status": status,
        })
    }

    fn started(item: Value) -> Value {
        json!({"type": "item.started", "item": item})
    }

    fn completed(item: Value) -> Value {
        json!({"type": "item.completed", "item": item})
    }

    fn call_started() -> Value {
        started(mcp_item(SERVER, "in_progress", Value::Null, Value::Null))
    }

    fn call_completed(result: Value) -> Value {
        completed(mcp_item(SERVER, "completed", result, Value::Null))
    }

    fn text_result(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}], "structured_content": null})
    }

    fn started_event() -> Event {
        Event::ToolCallStarted(ToolCallStart {
            id: "item_1".to_string(),
            name: "echo_tool".to_string(),
        })
    }

    fn call_event() -> Event {
        Event::ToolCall(ToolCallInfo {
            id: "item_1".to_string(),
            name: "echo_tool".to_string(),
            input: r#"{"text":"hi"}"#.to_string(),
        })
    }

    fn result_event(output: &str, is_error: bool) -> Event {
        Event::ToolResult(ToolResultInfo {
            id: "item_1".to_string(),
            output: output.to_string(),
            is_error,
        })
    }

    /// The `tool-result` a completed capsule call with `item` reports.
    fn result_of(item: Value) -> ToolResultInfo {
        match events(completed(item)).pop() {
            Some(Event::ToolResult(result)) => result,
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    fn turn_failed_line(message: &str) -> Value {
        json!({"type": "turn.failed", "error": {"message": message}})
    }

    fn failure(message: &str) -> TurnFailure {
        match events(turn_failed_line(message)).as_slice() {
            [Event::TurnFailed(failure)] => failure.clone(),
            other => panic!("expected one turn-failed, got {other:?}"),
        }
    }

    fn usage_line() -> Value {
        json!({"type": "turn.completed", "usage": {
            "input_tokens": 120,
            "cached_input_tokens": 20,
            "cache_write_input_tokens": 0,
            "output_tokens": 15,
            "reasoning_output_tokens": 5
        }})
    }

    fn exit(code: Option<i32>, signal: Option<i32>, tail: &str, interrupted: bool) -> ExitStatus {
        ExitStatus {
            code,
            signal,
            stderr_tail: tail.to_string(),
            interrupted,
            saw_terminal: false,
        }
    }

    fn turn_failed(kind: FailureKind, message: &str) -> Event {
        Event::TurnFailed(TurnFailure {
            kind,
            message: message.to_string(),
        })
    }

    fn login(exit_code: Option<i32>, stdout: &str, stderr: &str) -> LoginState {
        read_login(&LoginOutput {
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    // ── describe ──────────────────────────────────────────────────────────────

    #[test]
    fn describe_names_codex_and_what_it_needs() {
        let description = describe();
        assert_eq!(description.harness, "codex");
        assert_eq!(description.binary, "codex");
        assert_eq!(description.version_args, owned(&["--version"]));
        assert_eq!(description.tested_versions, owned(&["0.154.0", "0.167.2"]));
        assert_eq!(description.interrupt, InterruptMethod::SignalInt);
        assert_eq!(description.required_env, owned(&["HOME", "PATH"]));
        assert!(!description.streams_text);
        assert!(description.reports_usage);
        assert_eq!(
            description.login_check,
            LoginCheck::Command(owned(&["login", "status"]))
        );
        assert_eq!(
            description.harness_needs,
            HarnessNeeds {
                paths: vec![PathNeed {
                    path: "~/.codex".to_string(),
                    access: PathAccess::ReadWrite,
                }],
                hosts: owned(&["chatgpt.com", "auth.openai.com"]),
            }
        );
    }

    // ── read_login ────────────────────────────────────────────────────────────

    const LOGGED_OUT: &str = "codex is not signed in: run `codex login` as the user mur runs as";

    #[test]
    fn the_logged_out_line_reads_as_logged_out_whatever_the_exit_code() {
        for code in [Some(1), Some(0), Some(2), None] {
            assert_eq!(
                login(code, "", "Not logged in\n"),
                LoginState::LoggedOut(LOGGED_OUT.to_string()),
                "exit {code:?}"
            );
        }
        assert_eq!(
            login(Some(1), "", "\n   Not logged in   \nmore\n"),
            LoginState::LoggedOut(LOGGED_OUT.to_string())
        );
    }

    #[test]
    fn stdout_is_read_only_when_stderr_has_nothing() {
        assert_eq!(
            login(Some(1), "Not logged in\n", "  \n"),
            LoginState::LoggedOut(LOGGED_OUT.to_string())
        );
        assert_eq!(
            login(
                Some(1),
                "Not logged in\n",
                "Error checking login status: boom"
            ),
            LoginState::Unknown(
                "`codex login status` exited 1 with output this driver does not recognise"
                    .to_string()
            )
        );
    }

    #[test]
    fn any_other_line_with_exit_zero_reads_as_logged_in_an_api_key_included() {
        for line in [
            "Logged in using ChatGPT",
            "Logged in using an API key - sk-proj-***ABCD",
            "Logged in using workload identity",
            "something new",
        ] {
            assert_eq!(login(Some(0), "", line), LoginState::LoggedIn, "{line}");
        }
        assert_eq!(login(Some(0), "", ""), LoginState::LoggedIn);
    }

    #[test]
    fn a_failure_reads_as_unknown_and_never_quotes_the_output() {
        let unknown = login(
            Some(1),
            "",
            "Error checking login status: sk-proj-***SECRET could not be read",
        );
        assert_eq!(
            unknown,
            LoginState::Unknown(
                "`codex login status` exited 1 with output this driver does not recognise"
                    .to_string()
            )
        );
        assert_eq!(
            login(Some(3), " \n", "\n"),
            LoginState::Unknown(
                "`codex login status` exited 3 without printing anything".to_string()
            )
        );
        assert_eq!(
            login(None, "", "Logged in using ChatGPT"),
            LoginState::Unknown("`codex login status` was ended by a signal".to_string())
        );
    }

    // ── driver config ─────────────────────────────────────────────────────────

    #[test]
    fn no_config_an_empty_object_and_whitespace_all_mean_defaults() {
        for config in [None, Some("{}"), Some(""), Some("  \n ")] {
            assert_eq!(
                parse_driver_config(config),
                Ok(DriverConfig::default()),
                "{config:?}"
            );
        }
    }

    #[test]
    fn every_effort_and_summary_codex_names_is_accepted() {
        for effort in ReasoningEffort::ALL {
            let config = json!({"reasoning_effort": effort.as_str()}).to_string();
            assert_eq!(
                parse_driver_config(Some(&config)).map(|c| c.reasoning_effort),
                Ok(Some(effort))
            );
        }
        for summary in ReasoningSummary::ALL {
            let config = json!({"reasoning_summary": summary.as_str()}).to_string();
            assert_eq!(
                parse_driver_config(Some(&config)).map(|c| c.reasoning_summary),
                Ok(Some(summary))
            );
        }
        assert_eq!(
            REASONING_EFFORTS,
            &[
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
                "persistent"
            ]
        );
        assert_eq!(
            REASONING_SUMMARIES,
            &["auto", "concise", "detailed", "none"]
        );
    }

    #[test]
    fn a_value_that_is_not_exactly_one_of_the_names_is_refused_naming_them() {
        for bad in [
            json!("high "),
            json!("HIGH"),
            json!(3),
            json!(null),
            json!(["high"]),
        ] {
            let config = json!({"reasoning_effort": bad}).to_string();
            assert_eq!(
                parse_driver_config(Some(&config)),
                Err(format!(
                    "driver: inference.driver.config 'reasoning_effort' must be one of none, minimal, low, medium, high, xhigh, max, ultra, persistent, got {bad}"
                ))
            );
        }
        assert_eq!(
            parse_driver_config(Some(r#"{"reasoning_summary":"brief"}"#)),
            Err(r#"driver: inference.driver.config 'reasoning_summary' must be one of auto, concise, detailed, none, got "brief""#.to_string())
        );
    }

    #[test]
    fn config_that_is_not_a_json_object_is_refused() {
        let not_json = parse_driver_config(Some("{reasoning_effort: high}")).unwrap_err();
        assert!(
            not_json.starts_with("driver: inference.driver.config is not valid JSON: "),
            "{not_json}"
        );
        for not_object in ["[]", "\"high\"", "3", "null"] {
            assert_eq!(
                parse_driver_config(Some(not_object)),
                Err("driver: inference.driver.config must be a JSON object".to_string())
            );
        }
    }

    #[test]
    fn unrecognised_keys_are_refused_in_sorted_order_with_the_accepted_keys() {
        assert_eq!(
            parse_driver_config(Some(r#"{"zeta":1,"reasoning_effort":"high","alpha":true}"#)),
            Err("driver: unrecognised inference.driver.config key(s): alpha, zeta. Accepted keys: reasoning_effort, reasoning_summary".to_string())
        );
    }

    // ── launch ────────────────────────────────────────────────────────────────

    #[test]
    fn a_new_bridged_turn_has_exactly_the_planned_argv() {
        let mut expected = owned(COMMON_ARGS);
        expected.extend(owned(BRIDGE_ARGS));
        expected.push("-".to_string());
        assert_eq!(plan_of(bridged()).args, expected);
    }

    #[test]
    fn a_resumed_turn_with_a_model_and_both_dials_has_exactly_the_planned_argv() {
        let mut request = resumed();
        request.model = Some("  gpt-5.5 ".to_string());
        request.config =
            Some(r#"{"reasoning_effort":"xhigh","reasoning_summary":"auto"}"#.to_string());
        let mut expected = owned(COMMON_ARGS);
        expected.extend(owned(BRIDGE_ARGS));
        expected.extend(owned(&[
            "-c",
            "model_reasoning_effort=\"xhigh\"",
            "-c",
            "model_reasoning_summary=\"auto\"",
            "-m",
            "gpt-5.5",
            "resume",
            THREAD,
            "-",
        ]));
        assert_eq!(plan_of(request).args, expected);
    }

    #[test]
    fn a_turn_without_a_bridge_registers_no_server_and_sets_no_variable() {
        let plan = plan_of(request());
        let mut expected = owned(COMMON_ARGS);
        expected.push("-".to_string());
        assert_eq!(plan.args, expected);
        assert!(plan.env_set.is_empty());
    }

    #[test]
    fn the_bearer_token_travels_only_in_its_variable() {
        let plan = plan_of(resumed());
        assert_eq!(
            plan.env_set,
            vec![("MURMUR_BRIDGE_TOKEN".to_string(), TOKEN.to_string())]
        );
        assert!(plan.args.iter().all(|arg| !arg.contains(TOKEN)));
        assert!(plan.files.iter().all(|file| !file.contents.contains(TOKEN)));
    }

    #[test]
    fn the_system_prompt_is_the_instructions_file_byte_for_byte() {
        let mut request = bridged();
        request.system_prompt = "- line one\n\"quoted\" \\ back\n\u{1F600}\n".to_string();
        let plan = plan_of(request.clone());
        assert_eq!(
            plan.files,
            vec![DriverFile {
                name: "instructions.md".to_string(),
                contents: request.system_prompt,
            }]
        );
    }

    #[test]
    fn the_task_goes_on_stdin_byte_for_byte_and_nothing_else_is_written_there() {
        let mut request = bridged();
        request.task = "/status\n  CALL_TOOL=echo_tool \u{e9}\n".to_string();
        let plan = plan_of(request.clone());
        assert_eq!(plan.stdin, Some(request.task.into_bytes()));
        assert!(!plan.keep_stdin_open);
        assert_eq!(plan.interrupt_stdin, None);
    }

    #[test]
    fn a_blank_model_is_left_to_codex_and_a_padded_one_is_trimmed() {
        for blank in [None, Some(String::new()), Some("   ".to_string())] {
            let mut request = bridged();
            request.model = blank.clone();
            assert!(
                !plan_of(request).args.contains(&"-m".to_string()),
                "{blank:?}"
            );
        }
        let mut request = bridged();
        request.model = Some(" gpt-5.6-terra\t".to_string());
        let args = plan_of(request).args;
        let at = args.iter().position(|arg| arg == "-m").expect("-m");
        assert_eq!(args[at + 1], "gpt-5.6-terra");
        assert_eq!(args[at + 2], "-");
    }

    #[test]
    fn toml_values_are_written_as_json_string_literals() {
        let mut request = bridged();
        request.bridge.as_mut().unwrap().url = "http://h/\"q\"\\p\n".to_string();
        let args = plan_of(request).args;
        assert!(args.contains(&r#"mcp_servers.murmur.url="http://h/\"q\"\\p\n""#.to_string()));
    }

    #[test]
    fn a_server_name_that_cannot_be_a_key_segment_is_refused() {
        for name in ["", "mur.mur", "mur mur", "mur\"mur", "múrmur", "a=b", "x]"] {
            let mut request = bridged();
            request.bridge.as_mut().unwrap().server_name = name.to_string();
            assert_eq!(
                refusal_of(request),
                format!("driver: the tool bridge's server name '{name}' cannot name a codex MCP server: it becomes part of a -c mcp_servers.<name> path, so it may use only ASCII letters, digits, '_' and '-'")
            );
        }
        for name in ["murmur", "murmur_bridge", "a-b_C9", "-"] {
            let mut request = bridged();
            request.bridge.as_mut().unwrap().server_name = name.to_string();
            assert!(launch(request).is_ok(), "{name}");
        }
    }

    #[test]
    fn a_resume_whose_id_is_blank_or_flag_like_is_refused() {
        for id in [
            "",
            "   ",
            "-x",
            "--dangerously-bypass-approvals-and-sandbox",
        ] {
            let mut request = resumed();
            request.session.id = id.to_string();
            assert_eq!(
                refusal_of(request),
                format!("driver: cannot resume codex session '{id}': the id is blank or would be read as a flag")
            );
        }
        // A new session's id is never read, so it is never refused.
        let mut request = bridged();
        request.session.id = "-x".to_string();
        let args = plan_of(request).args;
        assert!(!args.contains(&"-x".to_string()));
        assert!(!args.contains(&"resume".to_string()));
    }

    #[test]
    fn the_config_is_read_before_anything_else_is_checked() {
        let mut request = resumed();
        request.config = Some("[]".to_string());
        request.session.id = String::new();
        request.bridge.as_mut().unwrap().server_name = String::new();
        assert_eq!(
            refusal_of(request),
            "driver: inference.driver.config must be a JSON object"
        );
    }

    #[test]
    fn no_plan_carries_a_flag_this_driver_must_never_pass() {
        let mut full = resumed();
        full.model = Some("gpt-5.5".to_string());
        full.config = Some(r#"{"reasoning_effort":"max","reasoning_summary":"none"}"#.to_string());
        for request in [request(), bridged(), resumed(), full] {
            let args = plan_of(request).args;
            for forbidden in [
                "--dangerously-bypass-approvals-and-sandbox",
                "--approve-for-me",
                "--dangerously-bypass-hook-trust",
                "--ephemeral",
                "--oss",
                "-p",
                "--profile",
            ] {
                assert!(!args.contains(&forbidden.to_string()), "{forbidden}");
            }
            assert!(args.iter().all(|arg| !arg.contains("forced_login_method")));
            assert_eq!(args.last().map(String::as_str), Some("-"));
        }
    }

    #[test]
    fn the_parse_context_carries_the_bridge_server_and_whether_the_turn_resumes() {
        assert_eq!(
            launch(resumed()).unwrap().context,
            ParseContext {
                server_name: Some(SERVER.to_string()),
                resumed: true,
            }
        );
        assert_eq!(launch(request()).unwrap().context, ParseContext::none());
    }

    // ── parse: framing ────────────────────────────────────────────────────────

    #[test]
    fn blank_lines_produce_nothing() {
        for line in ["", "   ", "\t\r"] {
            assert!(parse_line(line, &context(), &mut ParseState::new()).is_empty());
        }
    }

    #[test]
    fn a_line_that_is_not_a_json_object_is_one_note() {
        for line in ["not json", "[1,2]", "\"text\"", "42", "{\"type\":"] {
            assert_eq!(
                parse_line(line, &context(), &mut ParseState::new()),
                vec![note(&format!("unreadable stdout line: {line}"))]
            );
        }
    }

    #[test]
    fn a_line_whose_type_is_absent_or_unknown_is_one_note() {
        for line in [
            json!({}),
            json!({"type": "turn.paused"}),
            json!({"type": 3}),
        ] {
            assert_eq!(
                events(line.clone()),
                vec![note(&format!("unreadable stdout line: {line}"))]
            );
        }
    }

    #[test]
    fn an_unreadable_line_is_cut_at_the_note_limit() {
        let line = "é".repeat(NOTE_LINE_LIMIT + 10);
        let expected = format!("unreadable stdout line: {}…", "é".repeat(NOTE_LINE_LIMIT));
        assert_eq!(
            parse_line(&line, &context(), &mut ParseState::new()),
            vec![note(&expected)]
        );
        let exact = "x".repeat(NOTE_LINE_LIMIT);
        assert_eq!(
            parse_line(&exact, &context(), &mut ParseState::new()),
            vec![note(&format!("unreadable stdout line: {exact}"))]
        );
    }

    #[test]
    fn thread_started_reports_codexs_own_id_with_unknown_auth() {
        assert_eq!(
            events(json!({"type": "thread.started", "thread_id": THREAD})),
            vec![Event::SessionStarted(SessionInfo {
                id: THREAD.to_string(),
                auth: "unknown".to_string(),
                model: None,
            })]
        );
    }

    #[test]
    fn thread_started_without_an_id_reports_nothing() {
        for line in [
            json!({"type": "thread.started"}),
            json!({"type": "thread.started", "thread_id": ""}),
            json!({"type": "thread.started", "thread_id": 7}),
        ] {
            assert!(events(line).is_empty());
        }
    }

    #[test]
    fn turn_started_and_item_updated_report_nothing() {
        assert!(events(json!({"type": "turn.started"})).is_empty());
        assert!(events(json!({"type": "item.updated", "item": {"type": "todo_list"}})).is_empty());
    }

    // ── parse: tool calls ─────────────────────────────────────────────────────

    #[test]
    fn a_capsule_calls_start_reports_the_started_call_and_its_exact_arguments() {
        assert_eq!(events(call_started()), vec![started_event(), call_event()]);
    }

    #[test]
    fn a_capsule_calls_completion_after_its_start_reports_only_the_result() {
        assert_eq!(
            events_in(
                &[call_started(), call_completed(text_result("echoed: hi"))],
                &context()
            ),
            vec![
                started_event(),
                call_event(),
                result_event("echoed: hi", false)
            ]
        );
    }

    #[test]
    fn a_completion_whose_start_was_never_seen_reports_all_three() {
        assert_eq!(
            events(call_completed(text_result("echoed: hi"))),
            vec![
                started_event(),
                call_event(),
                result_event("echoed: hi", false)
            ]
        );
    }

    #[test]
    fn a_call_is_announced_once_however_often_it_starts() {
        assert_eq!(
            events_in(&[call_started(), call_started()], &context()),
            vec![started_event(), call_event()]
        );
    }

    #[test]
    fn absent_or_null_arguments_are_reported_as_an_empty_object() {
        for arguments in [None, Some(Value::Null)] {
            let mut item = mcp_item(SERVER, "in_progress", Value::Null, Value::Null);
            match arguments {
                None => {
                    item.as_object_mut().unwrap().remove("arguments");
                }
                Some(value) => item["arguments"] = value,
            }
            assert_eq!(
                events(started(item))[1],
                Event::ToolCall(ToolCallInfo {
                    id: "item_1".to_string(),
                    name: "echo_tool".to_string(),
                    input: "{}".to_string(),
                })
            );
        }
    }

    #[test]
    fn a_call_on_another_server_is_noted_without_its_arguments() {
        let secret = "ARG-SECRET-1234";
        let mut start = mcp_item("operator_srv", "in_progress", Value::Null, Value::Null);
        start["arguments"] = json!({"text": secret});
        let mut finish = start.clone();
        finish["status"] = json!("completed");
        finish["result"] = text_result(secret);
        let all = events_in(&[started(start), completed(finish)], &context());
        assert_eq!(
            all,
            vec![
                note("codex started a call to 'echo_tool' on MCP server 'operator_srv', which is not one of the capsule's tools (call item_1)"),
                note("codex finished a call to 'echo_tool' on MCP server 'operator_srv', which is not one of the capsule's tools (call item_1, status completed)"),
            ]
        );
        assert!(format!("{all:?}").find(secret).is_none());
    }

    #[test]
    fn without_a_bridge_no_call_is_a_capsule_call() {
        let all = events_in(
            &[call_started(), call_completed(text_result("x"))],
            &ParseContext::none(),
        );
        assert!(
            all.iter().all(|event| matches!(event, Event::Note(_))),
            "{all:?}"
        );
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn a_call_without_an_id_or_a_tool_name_is_not_a_capsule_call() {
        for (key, value) in [("id", json!("")), ("tool", json!("")), ("tool", json!(5))] {
            let mut item = mcp_item(SERVER, "in_progress", Value::Null, Value::Null);
            item[key] = value;
            assert!(matches!(events(started(item)).as_slice(), [Event::Note(_)]));
        }
    }

    #[test]
    fn the_server_name_comes_from_the_bridge_and_never_from_the_driver() {
        let mut other = bridge();
        other.server_name = "capsule_tools".to_string();
        let context = parse_context_for(Some(&other), SessionMode::New);
        let item = mcp_item("capsule_tools", "in_progress", Value::Null, Value::Null);
        assert_eq!(
            events_in(&[started(item)], &context),
            vec![started_event(), call_event()]
        );
        // `murmur` is now just another server.
        assert!(matches!(
            events_in(&[call_started()], &context).as_slice(),
            [Event::Note(_)]
        ));
    }

    #[test]
    fn the_start_of_any_other_item_reports_nothing() {
        for kind in [
            "agent_message",
            "reasoning",
            "command_execution",
            "file_change",
            "todo_list",
        ] {
            assert!(events(started(json!({"id": "item_3", "type": kind}))).is_empty());
        }
    }

    #[test]
    fn the_announced_calls_are_bounded_and_the_oldest_is_forgotten() {
        let mut state = ParseState::new();
        let context = context();
        for n in 0..=ANNOUNCED_LIMIT {
            let mut item = mcp_item(SERVER, "in_progress", Value::Null, Value::Null);
            item["id"] = json!(format!("item_{n}"));
            parse_line(&started(item).to_string(), &context, &mut state);
        }
        assert_eq!(state.announced.len(), ANNOUNCED_LIMIT);
        assert!(!state.is_announced("item_0"));
        assert!(state.is_announced("item_1"));
        assert!(state.is_announced(&format!("item_{ANNOUNCED_LIMIT}")));
    }

    // ── parse: tool results ───────────────────────────────────────────────────

    #[test]
    fn a_failed_call_reports_its_error_message_as_an_error() {
        let item = mcp_item(
            SERVER,
            "failed",
            Value::Null,
            json!({"message": "MCP tool call requires approval, but approval policy is never"}),
        );
        assert_eq!(
            result_of(item),
            ToolResultInfo {
                id: "item_1".to_string(),
                output: "MCP tool call requires approval, but approval policy is never".to_string(),
                is_error: true,
            }
        );
    }

    #[test]
    fn a_failed_call_whose_tool_answered_reports_the_answer_as_an_error() {
        // Recording 07: the tool answered `isError: true`, and `codex` dropped the flag, kept the
        // answer and wrote no `error`.
        let result = result_of(mcp_item(
            SERVER,
            "failed",
            text_result("fail_tool refused: hi"),
            Value::Null,
        ));
        assert_eq!(result.output, "fail_tool refused: hi");
        assert!(result.is_error);
        // A message, when there is one, is the reason the call was never made.
        let both = result_of(mcp_item(
            SERVER,
            "failed",
            text_result("ignored"),
            json!({"message": "the reason"}),
        ));
        assert_eq!(both.output, "the reason");
    }

    #[test]
    fn a_failed_call_without_a_message_or_an_answer_says_so() {
        for error in [Value::Null, json!({}), json!({"message": ""})] {
            for result in [
                Value::Null,
                json!({"content": [], "structured_content": null}),
            ] {
                let result = result_of(mcp_item(SERVER, "failed", result, error.clone()));
                assert_eq!(
                    result.output,
                    "codex reported the call failed with no message"
                );
                assert!(result.is_error);
            }
        }
    }

    #[test]
    fn a_result_flagged_as_an_error_is_one() {
        for key in ["isError", "is_error"] {
            let mut result = text_result("refused");
            result[key] = json!(true);
            assert!(result_of(mcp_item(SERVER, "completed", result, Value::Null)).is_error);
        }
        for flag in [json!(false), json!("true"), json!(1)] {
            let mut result = text_result("fine");
            result["isError"] = flag;
            assert!(!result_of(mcp_item(SERVER, "completed", result, Value::Null)).is_error);
        }
    }

    #[test]
    fn a_status_other_than_completed_is_an_error() {
        let result = result_of(mcp_item(
            SERVER,
            "in_progress",
            text_result("partial"),
            Value::Null,
        ));
        assert!(result.is_error);
        assert_eq!(result.output, "partial");
    }

    #[test]
    fn an_image_item_becomes_its_line_and_never_its_bytes() {
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==";
        let result = json!({"content": [
            {"type": "text", "text": "PICTURE-TOOL-RAN"},
            {"type": "image", "data": data, "mimeType": "image/png"}
        ], "structured_content": null});
        let output = result_of(mcp_item(SERVER, "completed", result, Value::Null)).output;
        assert_eq!(output, "PICTURE-TOOL-RAN\n[image: image/png, 70 bytes]");
        assert!(!output.contains(&data[..16]));
    }

    #[test]
    fn an_image_without_a_type_or_readable_data_says_so() {
        assert_eq!(
            image_reference(&json!({"type": "image", "data": "not base64!"})),
            "[image: unknown media type, size unknown]"
        );
        assert_eq!(
            image_reference(
                &json!({"type": "image", "source": {"media_type": "image/jpeg", "data": "QUJD"}})
            ),
            "[image: image/jpeg, 3 bytes]"
        );
    }

    #[test]
    fn the_base64_size_is_read_from_the_length_alone() {
        assert_eq!(base64_decoded_len(""), Some(0));
        assert_eq!(base64_decoded_len("QQ=="), Some(1));
        assert_eq!(base64_decoded_len("QUI="), Some(2));
        assert_eq!(base64_decoded_len("QUJD"), Some(3));
        assert_eq!(base64_decoded_len("QUJD\nRA"), Some(4));
        assert_eq!(base64_decoded_len("Q"), None);
        assert_eq!(base64_decoded_len("Q==="), None);
        assert_eq!(base64_decoded_len("QQ=a"), None);
        assert_eq!(base64_decoded_len("QU-D"), None);
    }

    #[test]
    fn structured_content_is_the_output_only_when_content_is_empty() {
        for content in [json!([]), Value::Null] {
            let result = json!({"content": content, "structured_content": {"answer": 42}});
            assert_eq!(
                result_of(mcp_item(SERVER, "completed", result, Value::Null)).output,
                r#"{"answer":42}"#
            );
        }
        let both =
            json!({"content": [{"type": "text", "text": "t"}], "structured_content": {"a": 1}});
        assert_eq!(
            result_of(mcp_item(SERVER, "completed", both, Value::Null)).output,
            "t"
        );
        let neither = json!({"content": [], "structured_content": null});
        assert_eq!(
            result_of(mcp_item(SERVER, "completed", neither, Value::Null)).output,
            ""
        );
    }

    #[test]
    fn an_item_that_is_neither_text_nor_image_keeps_its_json() {
        let result = json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "resource_link", "uri": "file:///x"}
        ]});
        assert_eq!(
            result_of(mcp_item(SERVER, "completed", result, Value::Null)).output,
            "a\n{\"type\":\"resource_link\",\"uri\":\"file:///x\"}"
        );
    }

    // ── parse: messages, reasoning, Codex's own items ─────────────────────────

    #[test]
    fn an_agent_message_is_text_and_becomes_the_turns_result() {
        let lines = [
            completed(json!({"id": "item_0", "type": "agent_message", "text": "first"})),
            completed(json!({"id": "item_1", "type": "agent_message", "text": ""})),
            completed(json!({"id": "item_2", "type": "agent_message", "text": "last"})),
            json!({"type": "turn.completed"}),
        ];
        assert_eq!(
            events_in(&lines, &context()),
            vec![
                Event::Text("first".to_string()),
                Event::Text("last".to_string()),
                Event::TurnEnd("last".to_string()),
            ]
        );
    }

    #[test]
    fn a_turn_with_no_agent_message_ends_with_an_empty_result() {
        assert_eq!(
            events(json!({"type": "turn.completed"})),
            vec![Event::TurnEnd(String::new())]
        );
    }

    #[test]
    fn reasoning_is_thinking_when_it_has_text() {
        assert_eq!(
            events(completed(
                json!({"id": "item_0", "type": "reasoning", "text": "Thinking about it."})
            )),
            vec![Event::Thinking("Thinking about it.".to_string())]
        );
        assert!(events(completed(
            json!({"id": "item_0", "type": "reasoning", "text": ""})
        ))
        .is_empty());
    }

    #[test]
    fn an_error_item_is_noted() {
        assert_eq!(
            events(completed(
                json!({"id": "item_0", "type": "error", "message": "Model metadata for `x` not found."})
            )),
            vec![note("codex error item: Model metadata for `x` not found.")]
        );
    }

    #[test]
    fn codexs_own_items_are_noted_by_type_id_and_status_alone() {
        assert_eq!(
            events(completed(json!({
                "id": "item_4",
                "type": "command_execution",
                "command": "cat /etc/SECRET",
                "aggregated_output": "SECRET",
                "status": "declined"
            }))),
            vec![note(
                "codex reported its own 'command_execution' item (item_4), status declined"
            )]
        );
        assert_eq!(
            events(completed(
                json!({"id": "item_5", "type": "web_search", "query": "SECRET"})
            )),
            vec![note("codex reported its own 'web_search' item (item_5)")]
        );
    }

    #[test]
    fn a_completed_item_with_no_readable_type_is_noted_as_unreadable() {
        let line = json!({"type": "item.completed", "item": {"id": "x"}});
        assert_eq!(
            events(line.clone()),
            vec![note(&format!("unreadable stdout line: {line}"))]
        );
    }

    #[test]
    fn notes_cut_codexs_text_at_the_limit() {
        let long = "m".repeat(NOTE_LINE_LIMIT + 1);
        assert_eq!(
            events(completed(
                json!({"id": "item_0", "type": "error", "message": long})
            )),
            vec![note(&format!(
                "codex error item: {}…",
                "m".repeat(NOTE_LINE_LIMIT)
            ))]
        );
        assert_eq!(
            events(json!({"type": "error", "message": long})),
            vec![note(&format!(
                "codex error: {}…",
                "m".repeat(NOTE_LINE_LIMIT)
            ))]
        );
    }

    // ── parse: retries and errors ─────────────────────────────────────────────

    fn retry(attempt: u32, reason: &str) -> Event {
        Event::Retry(RetryInfo {
            attempt,
            reason: reason.to_string(),
        })
    }

    #[test]
    fn a_numbered_reconnecting_error_is_a_retry_with_the_reason_in_its_parentheses() {
        let message = "Reconnecting... 2/5 (unexpected status 401 Unauthorized: Missing bearer (or basic) authentication in header, url: wss://api.openai.com/v1/responses, cf-ray: …)";
        assert_eq!(
            events(json!({"type": "error", "message": message})),
            vec![retry(2, "unexpected status 401 Unauthorized: Missing bearer (or basic) authentication in header, url: wss://api.openai.com/v1/responses, cf-ray: …")]
        );
        assert_eq!(
            events(json!({"type": "error", "message": "Reconnecting… 3/5 (stream closed)"})),
            vec![retry(3, "stream closed")]
        );
    }

    #[test]
    fn a_retry_without_parentheses_keeps_the_rest_of_the_line_or_says_unknown() {
        assert_eq!(
            events(json!({"type": "error", "message": "Reconnecting... 1/5 stream closed early "})),
            vec![retry(1, "stream closed early")]
        );
        assert_eq!(
            events(json!({"type": "error", "message": "Reconnecting... 4/5"})),
            vec![retry(4, "unknown")]
        );
        assert_eq!(
            events(json!({"type": "error", "message": "Reconnecting... 4/5 ()"})),
            vec![retry(4, "()")]
        );
    }

    #[test]
    fn any_other_error_line_is_noted() {
        for message in [
            "Reconnecting... soon",
            "Reconnecting... x/5 (a)",
            "Reconnecting... 2/ (a)",
            "Reconnecting...2/5 (a)",
            "unexpected status 401 Unauthorized",
            "",
        ] {
            assert_eq!(
                events(json!({"type": "error", "message": message})),
                vec![note(&format!("codex error: {message}"))],
                "{message}"
            );
        }
    }

    // ── parse: the end of a turn ──────────────────────────────────────────────

    #[test]
    fn a_new_sessions_turn_reports_its_counts_then_ends() {
        let lines = [
            completed(json!({"id": "item_1", "type": "agent_message", "text": "MOCKREPLY ok"})),
            usage_line(),
        ];
        assert_eq!(
            events_in(&lines, &context()),
            vec![
                Event::Text("MOCKREPLY ok".to_string()),
                Event::Usage(TokenUsage {
                    input: Some(100),
                    output: Some(15),
                    cache_read: Some(20),
                    cache_creation: Some(0),
                    thinking: Some(5),
                }),
                Event::TurnEnd("MOCKREPLY ok".to_string()),
            ]
        );
    }

    #[test]
    fn a_turn_whose_usage_names_no_count_reports_no_usage() {
        for line in [
            json!({"type": "turn.completed", "usage": {}}),
            json!({"type": "turn.completed", "usage": {"input_tokens": "120"}}),
            json!({"type": "turn.completed", "usage": null}),
        ] {
            assert_eq!(events(line), vec![Event::TurnEnd(String::new())]);
        }
    }

    #[test]
    fn a_resumed_turn_reports_no_usage_and_notes_the_running_total() {
        let events = events_in(&[usage_line()], &resumed_context());
        assert_eq!(
            events,
            vec![
                note(
                    r#"codex reports a resumed thread's running total, not this turn's spend, so no usage is reported: {"cache_write_input_tokens":0,"cached_input_tokens":20,"input_tokens":120,"output_tokens":15,"reasoning_output_tokens":5}"#
                ),
                Event::TurnEnd(String::new()),
            ]
        );
    }

    #[test]
    fn a_resumed_turn_without_counts_notes_nothing() {
        assert_eq!(
            events_in(&[json!({"type": "turn.completed"})], &resumed_context()),
            vec![Event::TurnEnd(String::new())]
        );
        assert_eq!(
            events_in(
                &[json!({"type": "turn.completed", "usage": {"x": 1}})],
                &resumed_context()
            ),
            vec![Event::TurnEnd(String::new())]
        );
    }

    #[test]
    fn the_counts_are_read_as_non_negative_integers_and_input_leaves_both_caches_out() {
        assert_eq!(token_usage(&json!({})), None);
        assert_eq!(
            token_usage(
                &json!({"input_tokens": 10, "cached_input_tokens": 8, "cache_write_input_tokens": 5})
            ),
            Some(TokenUsage {
                input: Some(0),
                output: None,
                cache_read: Some(8),
                cache_creation: Some(5),
                thinking: None,
            })
        );
        assert_eq!(
            token_usage(
                &json!({"input_tokens": 0, "output_tokens": -1, "reasoning_output_tokens": 1.5})
            ),
            Some(TokenUsage {
                input: Some(0),
                output: None,
                cache_read: None,
                cache_creation: None,
                thinking: None,
            })
        );
        assert_eq!(
            token_usage(&json!({"cached_input_tokens": 7})),
            Some(TokenUsage {
                input: None,
                output: None,
                cache_read: Some(7),
                cache_creation: None,
                thinking: None,
            })
        );
    }

    #[test]
    fn a_failure_is_auth_when_its_text_names_a_refused_credential() {
        for message in [
            "unexpected status 401 Unauthorized: Missing bearer",
            "403 Forbidden",
            "http status 401",
            "got status 403 back",
            "401 Unauthorized and then 429 Too Many Requests",
        ] {
            assert_eq!(failure(message).kind, FailureKind::Auth, "{message}");
        }
    }

    #[test]
    fn a_failure_is_quota_when_its_text_names_a_limit() {
        for message in [
            "exceeded retry limit, last status: 429 Too Many Requests",
            "status 429",
            "You've hit your usage limit. Try again later.",
            "You\u{2019}ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro).",
            "Quota exceeded. Check your plan.",
        ] {
            assert_eq!(failure(message).kind, FailureKind::Quota, "{message}");
        }
    }

    #[test]
    fn a_bare_number_names_no_kind() {
        for message in [
            "code 401",
            "429",
            "error 403 in request 4291",
            "stream disconnected before completion: eof",
        ] {
            assert_eq!(
                failure(message).kind,
                FailureKind::HarnessError,
                "{message}"
            );
        }
    }

    #[test]
    fn a_failure_carries_codexs_message_or_says_it_had_none() {
        assert_eq!(failure("boom").message, "boom");
        for line in [
            json!({"type": "turn.failed"}),
            json!({"type": "turn.failed", "error": {"message": ""}}),
            json!({"type": "turn.failed", "error": "flat"}),
        ] {
            assert_eq!(
                events(line),
                vec![turn_failed(
                    FailureKind::HarnessError,
                    "the harness reported a failure with no message"
                )]
            );
        }
    }

    #[test]
    fn a_failed_turn_never_ends_the_turn() {
        let lines = [
            completed(json!({"id": "item_0", "type": "agent_message", "text": "partial"})),
            turn_failed_line("unexpected status 401 Unauthorized"),
        ];
        let all = events_in(&lines, &context());
        assert!(!all.iter().any(|event| matches!(event, Event::TurnEnd(_))));
        assert!(matches!(all.last(), Some(Event::TurnFailed(_))));
    }

    // ── classify_exit ─────────────────────────────────────────────────────────

    #[test]
    fn an_interrupted_run_without_a_result_is_canceled_whatever_its_code() {
        for code in [Some(0), Some(1), Some(130), None] {
            assert_eq!(
                classify_exit(exit(code, Some(2), "Error: boom", true)),
                turn_failed(
                    FailureKind::Canceled,
                    "codex was interrupted before it reported a result"
                )
            );
        }
    }

    #[test]
    fn a_clean_exit_without_a_result_is_a_harness_error() {
        assert_eq!(
            classify_exit(exit(Some(0), None, "anything", false)),
            turn_failed(FailureKind::HarnessError, "codex exited without a result")
        );
    }

    #[test]
    fn a_failing_exit_names_its_code_or_signal_and_the_telling_stderr() {
        assert_eq!(
            classify_exit(exit(
                Some(1),
                None,
                "  Error: thread/resume failed\n",
                false
            )),
            turn_failed(
                FailureKind::HarnessError,
                "codex exited with code 1: Error: thread/resume failed"
            )
        );
        assert_eq!(
            classify_exit(exit(None, Some(9), "", false)),
            turn_failed(FailureKind::HarnessError, "codex was killed by signal 9")
        );
        assert_eq!(
            classify_exit(exit(None, None, " \n", false)),
            turn_failed(FailureKind::HarnessError, "codex exited without a status")
        );
    }

    #[test]
    fn the_lines_every_run_prints_are_left_out_of_the_tail() {
        let tail = "WARNING: proceeding, even though we could not create PATH aliases: Refusing\n\
                    Error: one\n\
                    2026-11-12T02:33:46Z ERROR rmcp: fail to delete session: DELETE returned HTTP 400\n\
                    \n  Error: two  \n";
        assert_eq!(
            classify_exit(exit(Some(1), None, tail, false)),
            turn_failed(
                FailureKind::HarnessError,
                "codex exited with code 1: Error: one\nError: two"
            )
        );
    }

    #[test]
    fn a_tail_of_nothing_but_those_lines_is_kept_whole() {
        let tail = "\n ERROR rmcp: fail to delete session: DELETE returned HTTP 400 \n";
        assert_eq!(
            classify_exit(exit(Some(2), None, tail, false)),
            turn_failed(
                FailureKind::HarnessError,
                "codex exited with code 2: ERROR rmcp: fail to delete session: DELETE returned HTTP 400"
            )
        );
    }

    #[test]
    fn classify_exit_never_answers_turn_end() {
        for interrupted in [false, true] {
            for code in [Some(0), Some(1), None] {
                assert!(matches!(
                    classify_exit(exit(code, None, "x", interrupted)),
                    Event::TurnFailed(_)
                ));
            }
        }
    }

    // ── invariants ────────────────────────────────────────────────────────────

    /// Lines a broken or hostile `codex` could print.
    fn hostile_lines() -> Vec<String> {
        let mut lines: Vec<String> = [
            json!({"type": "item.started", "item": null}),
            json!({"type": "item.started", "item": "x"}),
            json!({"type": "item.completed"}),
            json!({"type": "item.completed", "item": {"type": null}}),
            json!({"type": "item.completed", "item": {"type": "mcp_tool_call", "server": SERVER, "id": "i", "tool": "t", "result": "flat", "error": 3, "status": 9}}),
            json!({"type": "item.completed", "item": {"type": "mcp_tool_call", "server": SERVER, "id": "i", "tool": "t", "result": {"content": {"type": "image", "data": 5}}}}),
            json!({"type": "turn.completed", "usage": {"input_tokens": u64::MAX, "cached_input_tokens": u64::MAX}}),
            json!({"type": "turn.completed", "usage": [1, 2]}),
            json!({"type": "turn.failed", "error": null}),
            json!({"type": "error", "message": "Reconnecting... 99999999999999999999/5 (x)"}),
            json!({"type": "error", "message": "Reconnecting... 1/5 )("}),
            json!({"type": "error", "message": null}),
            json!({"type": "thread.started", "thread_id": null}),
        ]
        .iter()
        .map(Value::to_string)
        .collect();
        lines.extend(
            [
                "\u{0}",
                "{",
                "}{",
                "\u{feff}{}",
                "null",
                "{\"type\":\"error\",\"message\":\"\\ud800\"}",
            ]
            .iter()
            .map(|line| line.to_string()),
        );
        lines.push("é".repeat(10_000));
        lines
    }

    #[test]
    fn parse_never_panics_and_never_starts_a_session_without_an_id() {
        for context in [context(), resumed_context(), ParseContext::none()] {
            let events = parse_lines(&hostile_lines(), &context, &mut ParseState::new());
            for event in &events {
                if let Event::SessionStarted(info) = event {
                    assert!(!info.id.is_empty());
                }
                if let Event::Note(note) = event {
                    assert!(note.chars().count() <= NOTE_LINE_LIMIT + 200, "{note}");
                }
            }
        }
    }

    #[test]
    fn no_event_but_tool_call_carries_a_byte_of_a_calls_arguments() {
        let secret = "ARGUMENT-BYTES-9F3A";
        let mut capsule = mcp_item(SERVER, "in_progress", Value::Null, Value::Null);
        capsule["arguments"] = json!({"text": secret});
        let mut capsule_done = capsule.clone();
        capsule_done["status"] = json!("failed");
        capsule_done["error"] = json!({"message": "refused"});
        let mut foreign = capsule.clone();
        foreign["server"] = json!("operator_srv");
        let lines = [
            started(capsule),
            completed(capsule_done),
            started(foreign.clone()),
            completed(foreign),
            completed(
                json!({"id": "c", "type": "collab_tool_call", "prompt": secret, "status": "completed"}),
            ),
        ];
        for event in events_in(&lines, &context()) {
            if !matches!(event, Event::ToolCall(_)) {
                assert!(!format!("{event:?}").contains(secret), "{event:?}");
            }
        }
    }

    #[test]
    fn any_partition_of_a_run_into_batches_gives_the_same_events() {
        let lines: Vec<String> = [
            json!({"type": "thread.started", "thread_id": THREAD}),
            json!({"type": "turn.started"}),
            call_started(),
            call_completed(text_result("echoed: hi")),
            completed(json!({"id": "item_2", "type": "agent_message", "text": "done"})),
            usage_line(),
        ]
        .iter()
        .map(Value::to_string)
        .collect();
        let whole = parse_lines(&lines, &context(), &mut ParseState::new());
        for split in 0..=lines.len() {
            let mut state = ParseState::new();
            let mut parts = parse_lines(&lines[..split], &context(), &mut state);
            parts.extend(parse_lines(&lines[split..], &context(), &mut state));
            assert_eq!(parts, whole, "split at {split}");
        }
    }
}
