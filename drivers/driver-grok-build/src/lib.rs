//! The process driver for Grok Build.
//!
//! Every fact about xAI's `grok` CLI — its flags, the agent file that points it at the capsule's
//! tool bridge, how it names and calls a bridged tool, the environment switches that keep the
//! operator's other agents' configuration out, and the shape of its `streaming-messages-json`
//! output — lives in this crate and nowhere else. The runtime runs *a* process driver; it never
//! knows it is running the Grok one.
//!
//! The driver is granted nothing: no environment, no filesystem, no network, no Murmur host
//! import. It is pure translation both ways — a `LaunchRequest` into the `LaunchPlan` that
//! spawns the harness, and the lines the harness prints into the events the runtime acts on.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

// ── What this driver drives ───────────────────────────────────────────────────

/// The harness name the runtime traces and reports this driver under.
const HARNESS: &str = "grok";

/// The executable the runtime resolves, unless the manifest overrides it with `command:`.
const BINARY: &str = "grok";

/// The arguments that make `grok` print its version line, `grok 1.0.77 (<commit>) [<channel>]`,
/// spending no turn.
const VERSION_ARGS: &[&str] = &["version"];

/// The `grok` releases this driver release was tested against. An untested version is a warning
/// at run time (`W-RUN-002`), never a refusal, so this list documents what was measured rather
/// than gating anything.
const TESTED_VERSIONS: &[&str] = &["1.0.77"];

/// The variables `grok` cannot run without, each of which the capsule must declare in
/// `capabilities.env.allow` or be refused at load.
///
/// `HOME` locates `~/.grok`, which holds the login store `auth.json` a subscription run reads its
/// credential from. `PATH` is what an npm-shaped install — a launcher that execs `node` — needs
/// to start at all, and what the runtime's own bare-name binary resolution walks.
const REQUIRED_ENV: &[&str] = &["HOME", "PATH"];

/// `parse` emits `text-delta` events: `--include-partial-messages` makes `grok` stream them.
const STREAMS_TEXT: bool = true;

/// `grok` writes a `usage` block on its `result` line, so a turn's counts are reported and
/// `inference.max_session_tokens` can be weighed against them.
const REPORTS_USAGE: bool = true;

/// The arguments that make `grok` print how it is signed in and exit, spending no turn. Its first
/// stdout line says how: see [`LOGGED_IN_LINES`] and [`LOGGED_OUT_LINE`].
const LOGIN_CHECK_ARGS: &[&str] = &["models"];

/// How the first line of `grok models` opens when `grok` can make a turn: on a grok.com login
/// (`You are logged in with grok.com.`) and with an API key (`You are using XAI_API_KEY.`).
const LOGGED_IN_LINES: &[&str] = &["You are logged in", "You are using XAI_API_KEY"];

/// How the first line of `grok models` opens when `grok` holds no credential at all.
const LOGGED_OUT_LINE: &str = "You are not authenticated";

/// What a logged-out reason tells the person to run.
const SIGN_IN_HINT: &str = "run `grok login` as the user mur runs as";

/// The directory holding `grok`'s login store `auth.json`, its sessions, its logs and its managed
/// install under `bin/`. A subscription run reads its credential from here and writes session
/// files and refreshed tokens back.
const GROK_DIR: &str = "~/.grok";

/// The host `grok` calls for inference.
const API_HOST: &str = "api.x.ai";

/// The host `grok` calls to refresh a grok.com login, and to restore a session it cannot find
/// locally.
const AUTH_HOST: &str = "auth.x.ai";

// ── Launch constants ──────────────────────────────────────────────────────────

/// The token the runtime replaces with the path of the private `0700` directory it writes
/// `LaunchPlan::files` into.
const FILES_DIR_TOKEN: &str = "{files_dir}";

/// The file the task goes out in, read by `--prompt-file`. Headless `grok` never reads stdin.
const TASK_FILE_NAME: &str = "task.md";

/// The agent definition file `--agent` points at: the tool list and the bridge's MCP server.
const AGENT_FILE_NAME: &str = "capsule-agent.md";

/// `grok`'s line-per-message output format, in the shape of Claude Code's `stream-json`.
const OUTPUT_FORMAT: &str = "streaming-messages-json";

/// The flag that replaces `grok`'s own system prompt with the runtime's, written in its `=` form
/// so a prompt that starts with `-` is never read as a flag.
///
/// `grok` honours it on every turn of a session on one of its built-in models. On a resumed
/// session on a custom model (a `[model.<id>]` entry in its config) it ignores it and sends its own
/// built-in prompt followed by the agent file's body, which is why the body carries the same
/// prompt. An empty value replaces nothing: `grok` keeps its own prompt.
const SYSTEM_PROMPT_FLAG: &str = "--system-prompt-override=";

/// The permission rule kind that allows every tool of one MCP server without asking, as
/// `MCPTool(<server>__*)`. A headless run cannot ask, so an MCP tool this does not allow is
/// refused and the turn ends `cancelled`.
const MCP_ALLOW_RULE: &str = "MCPTool";

/// The agent's `name` in the agent file.
const AGENT_NAME: &str = "murmur-capsule";

/// The agent's `description` in the agent file.
const AGENT_DESCRIPTION: &str = "Murmur capsule";

/// The agent's `tools` frontmatter: exactly `grok`'s two MCP meta-tools and none of its built-in
/// tools, so the model reaches a tool only through the bridge.
const AGENT_TOOLS: &str = "search_tool, use_tool";

/// The variables the driver sets in `grok`'s environment, in order, and their values.
///
/// The fifteen `GROK_<AGENT>_<KIND>_ENABLED` switches stop `grok` loading the operator's Claude
/// Code, Cursor and Codex MCP servers, skills, rules, agents and hooks. `GROK_TURN_SUMMARY=0` stops
/// the extra summary model call `grok` makes after each turn, `GROK_DISABLE_AUTOUPDATER=1` stops it
/// updating itself mid-run, and `GROK_MEMORY=0` keeps its cross-session memory out of a capsule.
const ENV_SWITCHES: &[(&str, &str)] = &[
    ("GROK_CLAUDE_MCPS_ENABLED", "false"),
    ("GROK_CLAUDE_SKILLS_ENABLED", "false"),
    ("GROK_CLAUDE_RULES_ENABLED", "false"),
    ("GROK_CLAUDE_AGENTS_ENABLED", "false"),
    ("GROK_CLAUDE_HOOKS_ENABLED", "false"),
    ("GROK_CURSOR_MCPS_ENABLED", "false"),
    ("GROK_CURSOR_SKILLS_ENABLED", "false"),
    ("GROK_CURSOR_RULES_ENABLED", "false"),
    ("GROK_CURSOR_AGENTS_ENABLED", "false"),
    ("GROK_CURSOR_HOOKS_ENABLED", "false"),
    ("GROK_CODEX_MCPS_ENABLED", "false"),
    ("GROK_CODEX_SKILLS_ENABLED", "false"),
    ("GROK_CODEX_RULES_ENABLED", "false"),
    ("GROK_CODEX_AGENTS_ENABLED", "false"),
    ("GROK_CODEX_HOOKS_ENABLED", "false"),
    ("GROK_TURN_SUMMARY", "0"),
    ("GROK_DISABLE_AUTOUPDATER", "1"),
    ("GROK_MEMORY", "0"),
];

// ── How grok names an MCP tool ────────────────────────────────────────────────

/// What joins a server name and a tool name into the key `grok` catalogs an MCP tool under:
/// `<server>__<tool>`. A key holding it twice, or holding `___`, is skipped.
const TOOL_DELIMITER: &str = "__";

/// The longest `<server>__<tool>` key `grok` admits into its catalog, in characters.
const MAX_CATALOG_KEY: usize = 256;

/// The meta-tool through which `grok`'s model calls every MCP tool, naming it in `tool_name`.
const USE_TOOL: &str = "use_tool";

/// The `use_tool` argument naming the MCP tool to call, as `<server>__<tool>`.
const TOOL_NAME_KEY: &str = "tool_name";

/// The `use_tool` argument carrying the called tool's own arguments inline.
const TOOL_INPUT_KEY: &str = "tool_input";

/// The `use_tool` arguments that make `grok` read a call from a file instead: `file` holds the
/// whole call, `tool_input_file` the tool's arguments.
const FILE_FORM_KEYS: &[&str] = &["file", "tool_input_file"];

/// The `type` of the JSON string `grok` writes as a bridged call's `tool_result` content.
const MCP_RESULT_TYPE: &str = "MCP";

// ── How grok prints a turn ────────────────────────────────────────────────────

/// The `apiKeySource` `grok` prints for every login that is not an API key.
const OAUTH_KEY_SOURCE: &str = "oauth";

/// The `model` `grok` prints on `init` when it knows of none.
const UNKNOWN_MODEL: &str = "unknown";

/// The `mcp_servers[].status` values that say a server will offer no tool this run. `grok` also
/// prints `connected` and `pending`, the latter for a server still handshaking.
const BRIDGE_DOWN_STATUSES: &[&str] = &["failed", "needs-auth", "disabled"];

/// The `stop_reason` `grok` prints for a turn a signal stopped.
const CANCELLED: &str = "cancelled";

/// The `subtype` `grok` prints when it stopped at its turn limit.
const ERROR_MAX_TURNS: &str = "error_max_turns";

/// What a failure's text holds when `grok`'s provider refused its credential. `grok` prints no
/// status field on `result`, only the provider's error, which spells the status in one of these
/// ways.
const AUTH_MARKERS: &[&str] = &[
    "(401)",
    "(403)",
    "\"http_status\": 401",
    "\"http_status\":401",
    "\"http_status\": 403",
    "\"http_status\":403",
];

/// What a failure's text holds when `grok`'s provider rate-limited it. A bare `429` is never
/// matched: it is as likely a count or a port as a status.
const QUOTA_MARKERS: &[&str] = &[
    "rate_limit_exceeded",
    "(429)",
    "\"http_status\": 429",
    "\"http_status\":429",
];

/// The `type` of the line that ends a turn. It is the only line whose token counts are read:
/// see [`token_usage`].
const RESULT_LINE: &str = "result";

/// The key the `result` line's token counts hang off.
const USAGE_KEY: &str = "usage";

/// What opens a data URL: `grok` writes an MCP `image` item a tool returned into the result's
/// output string as a line `data:<media type>;base64,<data>`.
const DATA_URL_SCHEME: &str = "data:";

/// What ends a data URL's header when its data is base64.
const BASE64_HEADER_END: &str = ";base64";

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
    pub reports_usage: bool,
    pub login_check: LoginCheck,
    pub harness_needs: HarnessNeeds,
}

/// How the runtime asks `grok` whether it is signed in, without spending a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginCheck {
    Command(Vec<String>),
    Unavailable(String),
}

/// What a harness need may do with a host path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    ReadWrite,
}

/// A host directory `grok` needs beyond what the capsule's class gives every command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathNeed {
    pub path: String,
    pub access: PathAccess,
}

/// What `grok` needs beyond the capsule's own grants.
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

/// Whether `grok` is signed in, as [`read_login`] reads the login command's output.
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

/// A tool call the model has begun writing, before its input is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallStart {
    pub id: String,
    pub name: String,
}

/// How much of a started call's input the model has written so far: a running total for the
/// call, never the size of one delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallProgress {
    pub id: String,
    pub input_bytes: u64,
}

/// The result of a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultInfo {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

/// Why a turn failed. The interface's `other` is never produced: every failure this driver reads
/// is one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Auth,
    Quota,
    MaxTurns,
    Canceled,
    HarnessError,
}

/// A failed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    pub kind: FailureKind,
    pub message: String,
}

/// One thing the harness did, read out of its output. `grok` prints no retry line, no
/// subscription window and no suggested prompt, so the interface's `retry`, `quota` and
/// `suggested-prompt` are never produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    SessionStarted(SessionInfo),
    TextDelta(String),
    Text(String),
    ThinkingDelta(String),
    Thinking(String),
    ToolCall(ToolCallInfo),
    ToolResult(ToolResultInfo),
    /// What the harness has spent on this run so far, cumulative. Emitted immediately before
    /// the terminal event of the `result` line it was read from, so the runtime attributes it
    /// to the turn that is still open.
    Usage(TokenUsage),
    TurnEnd(String),
    TurnFailed(TurnFailure),
    Note(String),
    /// A capsule tool call the model has begun writing, under the id and bare name its
    /// `ToolCall` will carry. Sent once the streamed `use_tool` input names a capsule tool.
    ToolCallStarted(ToolCallStart),
    /// How many bytes of a started call's input the model has written so far. It carries the
    /// size and nothing else: no byte of the input itself leaves the parser this way.
    ToolCallProgress(ToolCallProgress),
}

/// Token counts, in the interface's generic names rather than Grok's: only [`token_usage`]
/// knows that `cache_read` is spelled `cache_read_input_tokens` on the wire.
///
/// Every count is optional and absent is not zero. A zero the harness wrote beside a count it
/// did spend is a measurement, and is carried as one. `grok` reports no thinking count, so
/// `thinking` is always absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
    pub thinking: Option<u64>,
}

/// What the harness has spent since the process started, built up from the counts `grok`
/// prints.
///
/// The interface takes every member as **cumulative for the harness run**, and the runtime
/// attributes to each turn only the growth of a member over what it has already attributed. A
/// headless `grok` process runs one turn and prints one `result` line, so the sum is that line's
/// counts; summing keeps the cumulative contract should a process ever print a second one.
///
/// One of these belongs to one process. `launch` starts a new one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunningUsage {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    thinking: Option<u64>,
}

impl RunningUsage {
    /// A process that has reported nothing yet.
    pub const fn new() -> Self {
        Self {
            input: None,
            output: None,
            cache_read: None,
            cache_creation: None,
            thinking: None,
        }
    }

    /// Add one turn's counts and return the run totals to report for it.
    ///
    /// A member the turn did not report leaves its total standing rather than resetting it, and
    /// is itself left absent until some turn reports it: absent means *this harness does not
    /// report this count*, and one turn's silence does not retract what an earlier turn said.
    /// A member that has been reported is sent on every later reading, even unchanged, which
    /// the runtime attributes as no growth.
    pub fn add_turn(&mut self, turn: &TokenUsage) -> TokenUsage {
        fn add(total: &mut Option<u64>, spent: Option<u64>) -> Option<u64> {
            if let Some(spent) = spent {
                *total = Some(total.unwrap_or(0).saturating_add(spent));
            }
            *total
        }

        TokenUsage {
            input: add(&mut self.input, turn.input),
            output: add(&mut self.output, turn.output),
            cache_read: add(&mut self.cache_read, turn.cache_read),
            cache_creation: add(&mut self.cache_creation, turn.cache_creation),
            thinking: add(&mut self.thinking, turn.thinking),
        }
    }
}

/// The most bytes of one `use_tool` call's streamed input the driver holds while it waits for
/// the input to name its tool. A call whose input passes this without naming a capsule tool is
/// never reported as started.
pub const STREAMED_INPUT_CAP: usize = 65_536;

/// The most ids of calls `grok` made outside the capsule's tools the driver remembers, waiting
/// for their results. The oldest is forgotten first.
pub const FOREIGN_CALL_LIMIT: usize = 1_024;

/// The `use_tool` calls the message being streamed has opened, by content-block index.
///
/// An `input_json_delta` names its block by `index` alone, and only the `content_block_start`
/// before it says which call that index is. A new message reuses indices from `0`, which is why
/// `message_start` forgets every one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpenToolCalls {
    calls: BTreeMap<u64, OpenToolCall>,
}

/// One `use_tool` call a block start opened: its id, the bytes of input streamed for it, and
/// what is known of the tool it names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenToolCall {
    id: String,
    input_bytes: u64,
    stage: CallStage,
}

/// How far a streamed `use_tool` call is from being reported.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CallStage {
    /// The input has not yet named its tool; the fragments so far.
    Pending(String),
    /// The input named a capsule tool, and `tool-call-started` went out.
    Started,
    /// The input named something else, or grew past [`STREAMED_INPUT_CAP`] first. Nothing more
    /// is reported for it.
    Abandoned,
}

impl OpenToolCalls {
    /// A message that has opened no tool call yet.
    const fn new() -> Self {
        Self {
            calls: BTreeMap::new(),
        }
    }

    /// Forget every call, because the message that opened them is over.
    fn forget_all(&mut self) {
        self.calls.clear();
    }

    /// Forget whatever call `index` held, because a new block is starting there.
    fn forget(&mut self, index: u64) {
        self.calls.remove(&index);
    }

    /// Remember that the block at `index` is the `use_tool` call `id`, its tool not yet known.
    fn open(&mut self, index: u64, id: &str) {
        self.calls.insert(
            index,
            OpenToolCall {
                id: id.to_string(),
                input_bytes: 0,
                stage: CallStage::Pending(String::new()),
            },
        );
    }

    /// Add one fragment of input to the call at `index`, and report what it makes known.
    ///
    /// While the call's tool is unknown, the fragments are held and tried as a JSON object after
    /// each one. The first time they parse to a capsule call, `tool-call-started` and the running
    /// total go out. After that, each non-empty fragment reports the new total. Only sizes leave
    /// here, never a byte of the input.
    fn grow(&mut self, index: u64, fragment: &str, context: &ParseContext) -> Vec<Event> {
        let Some(call) = self.calls.get_mut(&index) else {
            return Vec::new();
        };
        call.input_bytes = call.input_bytes.saturating_add(fragment.len() as u64);
        if fragment.is_empty() {
            return Vec::new();
        }
        let progress = Event::ToolCallProgress(ToolCallProgress {
            id: call.id.clone(),
            input_bytes: call.input_bytes,
        });
        match &mut call.stage {
            CallStage::Started => vec![progress],
            CallStage::Abandoned => Vec::new(),
            CallStage::Pending(buffer) => {
                if buffer.len() + fragment.len() > STREAMED_INPUT_CAP {
                    call.stage = CallStage::Abandoned;
                    return Vec::new();
                }
                buffer.push_str(fragment);
                let Ok(input) = serde_json::from_str::<Value>(buffer) else {
                    return Vec::new();
                };
                match capsule_tool(&input, context) {
                    Some(name) => {
                        let started = Event::ToolCallStarted(ToolCallStart {
                            id: call.id.clone(),
                            name: name.to_string(),
                        });
                        call.stage = CallStage::Started;
                        vec![started, progress]
                    }
                    None => {
                        call.stage = CallStage::Abandoned;
                        Vec::new()
                    }
                }
            }
        }
    }
}

/// The ids of calls the driver noted rather than reported, oldest first, waiting for the
/// results `grok` gives them. Bounded at [`FOREIGN_CALL_LIMIT`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ForeignCalls {
    ids: VecDeque<String>,
}

impl ForeignCalls {
    /// No call noted yet.
    const fn new() -> Self {
        Self {
            ids: VecDeque::new(),
        }
    }

    /// Remember `id`, forgetting the oldest when the bound is reached.
    fn remember(&mut self, id: &str) {
        if id.is_empty() || self.ids.iter().any(|known| known == id) {
            return;
        }
        if self.ids.len() == FOREIGN_CALL_LIMIT {
            self.ids.pop_front();
        }
        self.ids.push_back(id.to_string());
    }

    /// Whether `id` was noted, forgetting it if so: one call has one result.
    fn take(&mut self, id: &str) -> bool {
        match self.ids.iter().position(|known| known == id) {
            Some(at) => {
                self.ids.remove(at);
                true
            }
            None => false,
        }
    }
}

/// Everything `parse` carries from one call to the next for one harness process.
///
/// The interface keeps one driver instance for a whole run and hands it lines in batches of
/// any size. Carrying this between batches is what makes the events of a run independent of
/// where the batches fall. `launch` starts a new one, because a new process is a new stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseState {
    /// What the run has spent so far: see [`RunningUsage`].
    spent: RunningUsage,
    /// The `use_tool` calls the message being streamed has opened: see [`OpenToolCalls`].
    calls: OpenToolCalls,
    /// The calls noted as not the capsule's, whose results are noted too: see [`ForeignCalls`].
    foreign: ForeignCalls,
}

impl ParseState {
    /// A process that has printed nothing yet.
    pub const fn new() -> Self {
        Self {
            spent: RunningUsage::new(),
            calls: OpenToolCalls::new(),
            foreign: ForeignCalls::new(),
        }
    }
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
        // `grok` closes a turn it is sent SIGINT during with a `result` line of its own.
        interrupt: InterruptMethod::SignalInt,
        required_env: strings(REQUIRED_ENV),
        streams_text: STREAMS_TEXT,
        reports_usage: REPORTS_USAGE,
        login_check: LoginCheck::Command(strings(LOGIN_CHECK_ARGS)),
        harness_needs: HarnessNeeds {
            paths: vec![PathNeed {
                path: GROK_DIR.to_string(),
                access: PathAccess::ReadWrite,
            }],
            hosts: strings(&[API_HOST, AUTH_HOST]),
        },
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// Reads what `grok models` printed into whether `grok` is signed in.
///
/// Only the first non-empty line of stdout is read, trimmed. No reason quotes the output, which
/// goes on to list the account's models.
pub fn read_login(output: &LoginOutput) -> LoginState {
    let Some(first) = output
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
    else {
        return LoginState::Unknown(match output.exit_code {
            Some(code) => format!("`grok models` exited {code} without printing anything"),
            None => "`grok models` was ended by a signal without printing anything".to_string(),
        });
    };
    if LOGGED_IN_LINES
        .iter()
        .any(|opening| first.starts_with(opening))
    {
        return LoginState::LoggedIn;
    }
    if first.starts_with(LOGGED_OUT_LINE) {
        return LoginState::LoggedOut(format!("grok is not signed in: {SIGN_IN_HINT}"));
    }
    LoginState::Unknown(
        "`grok models` printed a first line this driver does not recognise".to_string(),
    )
}

// ── `inference.driver.config` vocabulary ──────────────────────────────────────
//
// Every key here is read. A key that is not is a refused launch, before the harness is spawned —
// a declared setting that is parsed and ignored is a defect, not a convenience.

/// The key that sets `grok`'s reasoning effort.
const REASONING_EFFORT_KEY: &str = "reasoning_effort";

/// Every key `inference.driver.config` accepts on this driver, sorted so the error message
/// listing them is stable.
const ACCEPTED_CONFIG_KEYS: &[&str] = &[REASONING_EFFORT_KEY];

/// `grok`'s canonical reasoning-effort levels, lowest first. A model accepts only the levels its
/// menu advertises; `grok` also takes per-model menu ids, which this driver refuses because they
/// mean nothing across models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    /// Every level, in the order the refusal lists them.
    const ALL: [ReasoningEffort; 7] = [
        ReasoningEffort::None,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Xhigh,
        ReasoningEffort::Max,
    ];

    /// The level as `--reasoning-effort` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}

/// The dials read from `inference.driver.config`. An unconfigured capsule gets
/// [`DriverConfig::default`]: every dial left to `grok`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriverConfig {
    /// `reasoning_effort` — passed as `--reasoning-effort <level>`; absent leaves the model's
    /// default.
    pub reasoning_effort: Option<ReasoningEffort>,
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

    let mut unrecognised = config
        .keys()
        .filter(|key| !ACCEPTED_CONFIG_KEYS.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unrecognised.is_empty() {
        // Sorted, so the offending keys come out in a stable order and one manifest fix clears
        // every one of them.
        unrecognised.sort();
        return Err(format!(
            "driver: unrecognised inference.driver.config key(s): {}. Accepted keys: {}",
            unrecognised.join(", "),
            ACCEPTED_CONFIG_KEYS.join(", ")
        ));
    }

    Ok(DriverConfig {
        reasoning_effort: parse_reasoning_effort(config.get(REASONING_EFFORT_KEY))?,
    })
}

/// Only a JSON string spelling one canonical level exactly is accepted: `"high "`, `"HIGH"`,
/// `3` and `null` are refused rather than coerced.
fn parse_reasoning_effort(value: Option<&Value>) -> Result<Option<ReasoningEffort>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    value
        .as_str()
        .and_then(|level| {
            ReasoningEffort::ALL
                .into_iter()
                .find(|effort| effort.as_str() == level)
        })
        .map(Some)
        .ok_or_else(|| {
            let levels: Vec<&str> = ReasoningEffort::ALL
                .iter()
                .map(|effort| effort.as_str())
                .collect();
            format!(
                "driver: inference.driver.config '{REASONING_EFFORT_KEY}' must be one of {}, got {value}",
                levels.join(", ")
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

/// Plans one turn.
///
/// `inference.driver.config` is read first, then the bridge's names are held to the rule `grok`
/// admits MCP tools by. Either refusal is this function's `Err`, and its message is what the
/// runtime reports. Every other request is plannable.
pub fn launch(request: LaunchRequest) -> Result<Launched, String> {
    let config = parse_driver_config(request.config.as_deref())?;
    if let Some(bridge) = request.bridge.as_ref() {
        check_bridge_names(bridge)?;
    }

    let plan = LaunchPlan {
        args: build_args(&request, &config),
        env_set: ENV_SWITCHES
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        files: vec![
            DriverFile {
                name: TASK_FILE_NAME.to_string(),
                contents: request.task.clone(),
            },
            DriverFile {
                name: AGENT_FILE_NAME.to_string(),
                contents: agent_file(request.bridge.as_ref(), &request.system_prompt),
            },
        ],
        // Headless `grok` reads its prompt from `--prompt-file` and never reads stdin, so there
        // is nothing to write and nothing to keep open: SIGINT is the interrupt.
        stdin: None,
        keep_stdin_open: false,
        interrupt_stdin: None,
    };
    Ok(Launched {
        plan,
        context: parse_context_for(request.bridge.as_ref()),
    })
}

/// Why `grok` would skip one name, or `None` when it admits it.
fn tool_name_fault(server_name: &str, tool_name: &str) -> Option<&'static str> {
    if tool_name.is_empty() {
        return Some("it is empty");
    }
    if !tool_name.chars().all(is_catalog_char) {
        return Some("it uses a character other than ASCII letters, digits, '_' and '-'");
    }
    if tool_name.contains(TOOL_DELIMITER) {
        return Some("it contains '__'");
    }
    if tool_name.starts_with('_') {
        return Some("it starts with '_', which makes '___' in its catalog key");
    }
    if server_name.len() + TOOL_DELIMITER.len() + tool_name.len() > MAX_CATALOG_KEY {
        return Some("its catalog key '<server>__<tool>' is longer than 256 characters");
    }
    None
}

fn is_catalog_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether `grok` admits the bridge's server name and every one of its tool names into its tool
/// catalog. It skips a tool it cannot admit with only a log line, so the model would never see
/// it; refusing the launch says so where the operator reads it.
fn check_bridge_names(bridge: &Bridge) -> Result<(), String> {
    const CONSEQUENCE: &str = "grok skips tools it cannot admit, so the model would never see them";
    let server = &bridge.server_name;
    let server_ok = server
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && server.chars().all(is_catalog_char)
        && !server.contains(TOOL_DELIMITER)
        && !server.ends_with('_');
    if !server_ok {
        return Err(format!(
            "driver: grok cannot admit the tool bridge's server name {}: it must start with an ASCII letter or '_', use only ASCII letters, digits, '_' and '-', hold no '__', and not end with '_'; {CONSEQUENCE}",
            Value::from(server.as_str())
        ));
    }

    let faults: Vec<String> = bridge
        .tool_names
        .iter()
        .filter_map(|name| {
            tool_name_fault(server, name)
                .map(|fault| format!("{} ({fault})", Value::from(name.as_str())))
        })
        .collect();
    if faults.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "driver: grok cannot admit these capsule tool names: {}; {CONSEQUENCE}",
            faults.join(", ")
        ))
    }
}

/// The full argument list after `grok`, one argv entry per flag and per value.
fn build_args(request: &LaunchRequest, config: &DriverConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--prompt-file".to_string(),
        format!("{FILES_DIR_TOKEN}/{TASK_FILE_NAME}"),
        "--output-format".to_string(),
        OUTPUT_FORMAT.to_string(),
        "--include-partial-messages".to_string(),
        "--agent".to_string(),
        format!("{FILES_DIR_TOKEN}/{AGENT_FILE_NAME}"),
        format!("{SYSTEM_PROMPT_FLAG}{}", request.system_prompt),
    ];

    if let Some(bridge) = request.bridge.as_ref() {
        args.push("--allow".to_string());
        args.push(format!(
            "{MCP_ALLOW_RULE}({}*)",
            tool_name_prefix(&bridge.server_name)
        ));
    }

    args.extend(
        ["--no-auto-update", "--disable-web-search", "--no-subagents"]
            .iter()
            .map(|flag| flag.to_string()),
    );

    if let Some(effort) = config.reasoning_effort {
        args.push("--reasoning-effort".to_string());
        args.push(effort.as_str().to_string());
    }

    if let Some(model) = model_arg(request.model.as_deref()) {
        args.push("-m".to_string());
        args.push(model.to_string());
    }

    args.push(
        match request.session.mode {
            SessionMode::New => "-s",
            SessionMode::Resume => "-r",
        }
        .to_string(),
    );
    args.push(request.session.id.clone());
    args
}

/// The model to pass to `-m`, or `none` to leave the choice to the harness. A manifest that
/// writes `model: ""` reaches the driver as a blank string, which is the same as no model.
fn model_arg(model: Option<&str>) -> Option<&str> {
    model.map(str::trim).filter(|model| !model.is_empty())
}

/// The prefix `grok` catalogs a tool from the MCP server `server_name` under: `<server>__`.
///
/// This is the one place the harness's tool naming is built. `parse` strips this same prefix off
/// a `use_tool` call's `tool_name` to recover the bare artifact name the runtime knows — the
/// server name always comes from the request's bridge, never from a constant here.
pub fn tool_name_prefix(server_name: &str) -> String {
    format!("{server_name}{TOOL_DELIMITER}")
}

/// The agent definition `--agent` points `grok` at: the two meta-tools, with a bridge the bridge
/// as its one MCP server, and the runtime's system prompt as its body.
///
/// Wherever `--system-prompt-override` takes effect it replaces the body, so the model sees the
/// prompt once. Where `grok` ignores the override, on a resumed session on a custom model, it sends
/// its own prompt followed by this body, so the capsule's prompt still reaches the model. The body
/// is the prompt byte for byte: `grok` reads only the first `---` fenced block as frontmatter, so a
/// prompt holding `---` lines, or frontmatter-looking keys, stays body text.
///
/// Every value is written as a JSON string, which is a YAML double-quoted scalar, so a URL or
/// token holding a quote, a colon or a backslash cannot break the frontmatter. The server key
/// goes bare: [`check_bridge_names`] has held it to letters, digits, `_` and `-`. The bearer
/// token is in this file, in the runtime's private `0700` directory, and never in argv, where
/// any process on the host can read it.
fn agent_file(bridge: Option<&Bridge>, system_prompt: &str) -> String {
    let quoted = |text: &str| Value::from(text).to_string();
    let mut file = format!(
        "---\nname: {}\ndescription: {}\ntools: {AGENT_TOOLS}\n",
        quoted(AGENT_NAME),
        quoted(AGENT_DESCRIPTION),
    );
    if let Some(bridge) = bridge {
        file.push_str(&format!(
            "mcpServers:\n  - {}:\n      url: {}\n      headers:\n        Authorization: {}\n",
            bridge.server_name,
            quoted(&bridge.url),
            quoted(&format!("Bearer {}", bridge.bearer_token)),
        ));
    }
    file.push_str(&format!("---\n{system_prompt}\n"));
    file
}

// ── parse ────────────────────────────────────────────────────────────────────

/// `session-info.auth` for a harness drawing on a subscription.
pub const AUTH_SUBSCRIPTION: &str = "subscription";

/// `session-info.auth` for a harness billing an API key.
///
/// Every `apiKeySource` but the one below reads as this, absent included: the runtime warns on
/// anything that is not `subscription`, so a reading this driver does not recognise fails
/// towards being warned about rather than towards a quiet subscription claim.
pub const AUTH_API_KEY: &str = "api-key";

/// How many characters of an unreadable line a `note` repeats before truncating it, so one
/// runaway line cannot fill a trace. A tool name a note quotes is cut at the same length.
pub const NOTE_LINE_LIMIT: usize = 512;

/// What a `note` about an unreadable line opens with.
const NOTE_PREFIX: &str = "unreadable stdout line: ";

/// The `input` of a capsule call whose `tool_input` is absent or `null`.
const EMPTY_TOOL_INPUT: &str = "{}";

/// What a `turn-failed` says when the `result` line describes the failure in no field at all.
const UNDESCRIBED_FAILURE: &str = "the harness reported a failure with no message";

/// What `launch` learned that `parse` needs.
///
/// The interface keeps one driver instance for a whole run, which is what lets a batch of lines
/// be read against the bridge that same run was launched with. It is fixed for the run; what the
/// lines themselves build up as they are read is [`ParseState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseContext {
    /// The prefix `launch` gave the capsule's tools, `<server>__`, stripped off a `use_tool`
    /// call's `tool_name` again here. With `None`, no call is a capsule call.
    pub tool_prefix: Option<String>,
}

impl ParseContext {
    /// A run launched with no bridge.
    pub const fn none() -> Self {
        Self { tool_prefix: None }
    }

    /// A run whose capsule tools are named with `prefix`.
    pub fn with_prefix(prefix: &str) -> Self {
        Self {
            tool_prefix: Some(prefix.to_string()),
        }
    }

    /// The bridge's server name, read back out of the prefix `launch` built from it.
    fn server_name(&self) -> Option<&str> {
        self.tool_prefix
            .as_deref()
            .and_then(|prefix| prefix.strip_suffix(TOOL_DELIMITER))
    }
}

/// The context a turn launched with `bridge` reads its output against.
pub fn parse_context_for(bridge: Option<&Bridge>) -> ParseContext {
    match bridge {
        Some(bridge) => ParseContext::with_prefix(&tool_name_prefix(&bridge.server_name)),
        None => ParseContext::none(),
    }
}

/// The bare capsule tool a `use_tool` input calls, or `None` when it is no capsule call.
///
/// A capsule call's input is an object whose `tool_name` is a string starting with the prefix
/// `launch` built, followed by a non-empty rest, and which carries neither file form. Only that
/// prefix comes off: a `tool_name` of an MCP server this driver never registered is not a
/// capsule call, whatever its shape.
fn capsule_tool<'a>(input: &'a Value, context: &ParseContext) -> Option<&'a str> {
    let prefix = context.tool_prefix.as_deref()?;
    let input = input.as_object()?;
    if FILE_FORM_KEYS.iter().any(|key| input.contains_key(*key)) {
        return None;
    }
    input
        .get(TOOL_NAME_KEY)?
        .as_str()?
        .strip_prefix(prefix)
        .filter(|bare| !bare.is_empty())
}

/// Reads a batch of complete stdout lines into events.
///
/// The events of a batch are the events of its lines in order. What one line leaves behind for
/// a later one — the run's spend, the calls the current message has opened, and the calls noted
/// as not the capsule's — lives in `state`, which the caller carries from one batch to the
/// next, so the same lines split into different batches produce the same events.
pub fn parse_lines(lines: &[String], context: &ParseContext, state: &mut ParseState) -> Vec<Event> {
    lines
        .iter()
        .flat_map(|line| parse_line(line, context, state))
        .collect()
}

/// Reads one stdout line into the events it carries.
///
/// Never fails and never panics: a line that is not a JSON object of a shape this driver knows
/// becomes a single `note` for the trace, which is all the interface offers for saying so.
pub fn parse_line(line: &str, context: &ParseContext, state: &mut ParseState) -> Vec<Event> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let Some(value) = line_object(line) else {
        return vec![note_for(line)];
    };

    match value.get("type").and_then(Value::as_str) {
        Some("system") => system_events(&value, context),
        Some("stream_event") => stream_events(&value, context, &mut state.calls),
        Some("assistant") => assistant_events(&value, context, &mut state.foreign),
        Some("user") => user_events(&value, &mut state.foreign),
        // The counts go out before the terminal event, because the runtime attributes them to
        // the turn still open and both terminal events close it.
        Some(RESULT_LINE) => token_usage(&value)
            .map(|turn| Event::Usage(state.spent.add_turn(&turn)))
            .into_iter()
            .chain([result_event(&value)])
            .collect(),
        _ => vec![note_for(line)],
    }
}

/// A line as the JSON object it is, or `None` for anything else.
fn line_object(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line)
        .ok()
        .filter(Value::is_object)
}

/// A `note` repeating a line the driver could not read, truncated at `NOTE_LINE_LIMIT`.
fn note_for(line: &str) -> Event {
    Event::Note(format!("{NOTE_PREFIX}{}", truncated(line)))
}

/// `text` cut at [`NOTE_LINE_LIMIT`] characters, with `…` appended when it was cut.
fn truncated(text: &str) -> String {
    match text.char_indices().nth(NOTE_LINE_LIMIT) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

// ── the tokens a turn spent ──────────────────────────────────────────────────
//
// Only the `result` line reports a count. `grok` also writes a `usage` block on its
// `message_delta` stream events and its `assistant` lines, but each counts one message, and the
// `result` line already sums every model call of the run.

/// The token counts a `result` line reported, or `None` when it reported none at all.
///
/// The one place in this repo where Grok's spelling of a count appears. Each count is read as a
/// non-negative integer, so a value that is absent, `null`, a string, a float or negative reads
/// as absent. `grok` writes all four counts as `0` when it knows none of them — a run stopped by
/// a signal, or refused by its provider — so a block whose present counts are all zero reports
/// nothing rather than a measurement of nothing. `grok` reports no thinking count.
///
/// `modelUsage`, `total_cost_usd`, `server_tool_use` and the `duration_*` fields are not read.
pub fn token_usage(value: &Value) -> Option<TokenUsage> {
    let usage = value.get(USAGE_KEY)?;
    let counts = [
        count_at(usage, "input_tokens"),
        count_at(usage, "output_tokens"),
        count_at(usage, "cache_read_input_tokens"),
        count_at(usage, "cache_creation_input_tokens"),
    ];
    if counts.iter().flatten().all(|count| *count == 0) {
        return None;
    }
    let [input, output, cache_read, cache_creation] = counts;
    Some(TokenUsage {
        input,
        output,
        cache_read,
        cache_creation,
        thinking: None,
    })
}

/// The non-negative integer at `key`, or `None` for every other way a count can fail to be one.
fn count_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

// ── the lines ────────────────────────────────────────────────────────────────

/// Reads a `system` line. Only `init` says anything the runtime acts on.
///
/// An `init` with an empty `session_id` — `grok` prints one when the session could not start —
/// reports nothing: the runtime must never store an empty id, and the `result` that follows
/// says why the turn failed.
fn system_events(value: &Value, context: &ParseContext) -> Vec<Event> {
    if value.get("subtype").and_then(Value::as_str) != Some("init") {
        return Vec::new();
    }
    let id = str_at(value, "session_id");
    if id.is_empty() {
        return Vec::new();
    }
    let mut events = vec![Event::SessionStarted(SessionInfo {
        id: id.to_string(),
        auth: match value.get("apiKeySource").and_then(Value::as_str) {
            Some(OAUTH_KEY_SOURCE) => AUTH_SUBSCRIPTION,
            _ => AUTH_API_KEY,
        }
        .to_string(),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != UNKNOWN_MODEL)
            .map(str::to_string),
    })];
    events.extend(bridge_down_note(value, context));
    events
}

/// One `note` when `init` reports the bridge's server as unable to offer its tools.
fn bridge_down_note(value: &Value, context: &ParseContext) -> Option<Event> {
    let server = context.server_name()?;
    value
        .get("mcp_servers")?
        .as_array()?
        .iter()
        .filter(|entry| entry.get("name").and_then(Value::as_str) == Some(server))
        .find_map(|entry| {
            entry
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| BRIDGE_DOWN_STATUSES.contains(status))
        })
        .map(|status| {
            Event::Note(format!(
                "grok reports the capsule's tool bridge as {status}"
            ))
        })
}

/// Reads a `stream_event` line.
///
/// Text and thinking deltas stream their text: an empty one emits nothing, and any other text,
/// whitespace included, streams byte for byte. A `use_tool` block start opens a call that is
/// reported once its streamed input names a capsule tool. The rest of the message and block
/// framing is `grok`'s own bookkeeping, except that `message_start` ends the previous message's
/// calls.
fn stream_events(value: &Value, context: &ParseContext, calls: &mut OpenToolCalls) -> Vec<Event> {
    let event = &value["event"];
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            calls.forget_all();
            Vec::new()
        }
        Some("content_block_start") => {
            block_started(event, calls);
            Vec::new()
        }
        _ => content_delta(event, context, calls),
    }
}

/// Reads a `content_block_start`. Whatever the block, a call an earlier block opened at the same
/// index is over. A `use_tool` block with an id opens a call whose tool is not yet known; its
/// `input`, always `{}` here, is never read. Any other `tool_use` is never a capsule call, and
/// its complete block is noted when the `assistant` line carries it.
fn block_started(event: &Value, calls: &mut OpenToolCalls) {
    let Some(index) = event.get("index").and_then(Value::as_u64) else {
        return;
    };
    calls.forget(index);
    let block = &event["content_block"];
    if block.get("type").and_then(Value::as_str) != Some("tool_use")
        || str_at(block, "name") != USE_TOOL
    {
        return;
    }
    let id = str_at(block, "id");
    if !id.is_empty() {
        calls.open(index, id);
    }
}

/// Reads the `delta` of a `content_block_delta`, keyed on the delta's own `type`.
fn content_delta(event: &Value, context: &ParseContext, calls: &mut OpenToolCalls) -> Vec<Event> {
    let delta = &event["delta"];
    let (text, event_of): (&str, fn(String) -> Event) =
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => (str_at(delta, "text"), Event::TextDelta),
            Some("thinking_delta") => (str_at(delta, "thinking"), Event::ThinkingDelta),
            Some("input_json_delta") => {
                return event
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|index| calls.grow(index, str_at(delta, "partial_json"), context))
                    .unwrap_or_default();
            }
            _ => return Vec::new(),
        };
    if text.is_empty() {
        Vec::new()
    } else {
        vec![event_of(text.to_string())]
    }
}

/// Reads an `assistant` line into at most one `thinking`, at most one `text`, and then, for each
/// `tool_use` block in order, a `tool-call` for a capsule call or a `note` for anything else.
///
/// One `text` per message is what makes the interface's rule — a `text` replaces the
/// `text-delta`s streamed for that message rather than being appended to them — well defined.
fn assistant_events(
    value: &Value,
    context: &ParseContext,
    foreign: &mut ForeignCalls,
) -> Vec<Event> {
    let mut thinking = String::new();
    let mut text = String::new();
    let mut calls = Vec::new();

    for block in content_blocks(value) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(str_at(block, "text")),
            Some("thinking") => thinking.push_str(str_at(block, "thinking")),
            Some("tool_use") => calls.push(tool_use_event(block, context, foreign)),
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

/// One complete `tool_use` block: a `tool-call` when it is a capsule call, a `note` otherwise.
///
/// A capsule call is reported under its bare name with `tool_input` as its input, which is
/// exactly what the bridge is handed, so the runtime pairs the bridge's report with it. A note
/// names the call and the tool it asked for and carries no other part of its input; its id is
/// remembered so its result is noted too.
fn tool_use_event(block: &Value, context: &ParseContext, foreign: &mut ForeignCalls) -> Event {
    let id = str_at(block, "id");
    let name = str_at(block, "name");
    let input = block.get("input").unwrap_or(&Value::Null);
    if name == USE_TOOL {
        if let Some(bare) = capsule_tool(input, context) {
            return Event::ToolCall(ToolCallInfo {
                id: id.to_string(),
                name: bare.to_string(),
                input: match input.get(TOOL_INPUT_KEY) {
                    None | Some(Value::Null) => EMPTY_TOOL_INPUT.to_string(),
                    Some(tool_input) => tool_input.to_string(),
                },
            });
        }
    }
    foreign.remember(id);
    Event::Note(foreign_call_note(id, name, input))
}

/// The note for a `tool_use` that is not a capsule call. It names the call id and the tool the
/// call asked for, cut at [`NOTE_LINE_LIMIT`], and nothing else of its input — no argument, no
/// file path.
fn foreign_call_note(id: &str, name: &str, input: &Value) -> String {
    if name != USE_TOOL {
        return format!(
            "grok called '{}', which is not one of the capsule's tools (call {id})",
            truncated(name)
        );
    }
    let tool_name = input.get(TOOL_NAME_KEY).and_then(Value::as_str);
    let file_form = FILE_FORM_KEYS.iter().any(|key| input.get(*key).is_some());
    match (tool_name, file_form) {
        (Some(tool), true) => format!(
            "grok called use_tool for '{}' with its arguments in a file, which is never read as a capsule call (call {id})",
            truncated(tool)
        ),
        (None, true) => format!(
            "grok called use_tool with its whole call in a file, which is never read as a capsule call (call {id})"
        ),
        (Some(tool), false) => format!(
            "grok called use_tool for '{}', which is not one of the capsule's tools (call {id})",
            truncated(tool)
        ),
        (None, false) => format!(
            "grok called '{USE_TOOL}', which is not one of the capsule's tools (call {id})"
        ),
    }
}

/// Reads a `user` line, on which `grok` writes tool results back to itself. A result for a call
/// the driver noted is noted too, by size alone; every other is a `tool-result` with its content
/// unwrapped.
fn user_events(value: &Value, foreign: &mut ForeignCalls) -> Vec<Event> {
    content_blocks(value)
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| {
            let id = str_at(block, "tool_use_id");
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or_default();
            if foreign.take(id) {
                let bytes = match block.get("content") {
                    None | Some(Value::Null) => 0,
                    Some(Value::String(content)) => content.len(),
                    Some(content) => content.to_string().len(),
                };
                return Event::Note(format!(
                    "grok answered its own call {id}: {bytes} bytes, is_error {is_error}"
                ));
            }
            Event::ToolResult(ToolResultInfo {
                id: id.to_string(),
                output: unwrap_tool_result(block.get("content")),
                is_error,
            })
        })
        .collect()
}

/// A `tool_result` block's content as the tool's output.
///
/// `grok` writes a bridged call's result as a JSON string: `{"type":"MCP", …, "output":
/// {"<Variant>": <value>}}`, where the one key names how the call went and its value is what the
/// tool returned. That value is [`render`]ed. A refused call is a JSON string `{"error": …,
/// "message": …}` and reads `<error>: <message>`. Any other string is the output verbatim, a
/// list of blocks reads as Claude-shaped content, and an absent content is empty.
fn unwrap_tool_result(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => unwrap_string(text),
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

/// A string `tool_result` content, unwrapped where `grok` wrapped it.
fn unwrap_string(text: &str) -> String {
    let Some(Value::Object(wrapper)) = serde_json::from_str::<Value>(text).ok() else {
        return text.to_string();
    };
    if wrapper.get("type").and_then(Value::as_str) == Some(MCP_RESULT_TYPE) {
        if let Some(Value::Object(output)) = wrapper.get("output") {
            if output.len() == 1 {
                if let Some(value) = output.values().next() {
                    return render(value);
                }
            }
        }
    }
    if let (Some(error), Some(message)) = (
        wrapper.get("error").and_then(Value::as_str),
        wrapper.get("message").and_then(Value::as_str),
    ) {
        return format!("{error}: {message}");
    }
    text.to_string()
}

/// The value inside a bridged result's `output`, as text. A string is itself; a list gives one
/// line per item — a `text` item its text, an image its [`image_reference`] line, anything else
/// its JSON; a single image is its line; anything else is its JSON.
///
/// `grok` writes an MCP `image` item the bridge returned into the output string itself, as a
/// line `data:<media type>;base64,<data>` after the text item's text. Every such data URL in a
/// string or a text item becomes its image line, so no byte of an image's base64 survives.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => without_image_data(text),
        Value::Array(items) => items.iter().map(render_item).collect::<Vec<_>>().join("\n"),
        image if is_image_block(image) => image_reference(image),
        other => other.to_string(),
    }
}

/// One item of a rendered list.
fn render_item(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => without_image_data(str_at(item, "text")),
        Some("image") => image_reference(item),
        _ => item.to_string(),
    }
}

/// `text` with every base64 data URL in it replaced by its image line: `data:image/png;base64,
/// iVBOR…` becomes `[image: image/png, 88 bytes]`. A `data:` that does not open a base64 data URL
/// stays as it is.
fn without_image_data(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(DATA_URL_SCHEME) {
        out.push_str(&rest[..at]);
        let after_scheme = &rest[at + DATA_URL_SCHEME.len()..];
        let header_len = after_scheme
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || b"/.+-_;=".contains(byte))
            .count();
        let header = &after_scheme[..header_len];
        match (
            header.strip_suffix(BASE64_HEADER_END),
            after_scheme[header_len..].strip_prefix(','),
        ) {
            (Some(params), Some(data_and_rest)) => {
                let media_type = params.split(';').next().unwrap_or_default();
                let data_len = data_and_rest
                    .bytes()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(byte))
                    .count();
                out.push_str(&image_line(
                    Some(media_type),
                    Some(&data_and_rest[..data_len]),
                ));
                rest = &data_and_rest[data_len..];
            }
            _ => {
                out.push_str(DATA_URL_SCHEME);
                rest = after_scheme;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The media type an image line names when the block carries none.
pub const UNKNOWN_MEDIA_TYPE: &str = "unknown media type";

/// The size clause an image line carries when the block's data is absent or not valid base64.
pub const UNKNOWN_SIZE: &str = "size unknown";

fn is_image_block(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("image")
}

/// The line the feed carries in place of an image: `[image: <media type>, <n> bytes]`, where
/// `<n>` is the size the base64 decodes to. It is murmur's trace placeholder without the hash,
/// which the trace's `tool_call.attachments` entry for the same call already carries.
///
/// Reads both shapes an image arrives in: the Anthropic one (`source.media_type`,
/// `source.data`) and the MCP one (`mimeType`, `data`), and also `media_type`. A block with no
/// media type names [`UNKNOWN_MEDIA_TYPE`]; one whose data is absent, not a string or not valid
/// base64 says [`UNKNOWN_SIZE`].
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
    .find(|media_type| !media_type.trim().is_empty());
    let data = [
        source.and_then(|source| source.get("data")),
        block.get("data"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str);
    image_line(media_type, data)
}

/// `[image: <media type>, <n> bytes]` for an image of `media_type` whose base64 is `data`, with
/// the fallbacks for either one missing or unreadable.
fn image_line(media_type: Option<&str>, data: Option<&str>) -> String {
    let media_type = media_type
        .filter(|media_type| !media_type.trim().is_empty())
        .unwrap_or(UNKNOWN_MEDIA_TYPE);
    let size = data.and_then(base64_decoded_len).map_or_else(
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
    // `symbols / 4 * 3` and `partial * 3 / 4` are floor(symbols * 3 / 4) without the product,
    // so no length overflows.
    valid.then(|| symbols / 4 * 3 + partial * 3 / 4)
}

/// Reads a `result` line, the one line that ends a turn. Only `is_error: false` ends it; a line
/// with no readable `is_error` fails closed.
fn result_event(value: &Value) -> Event {
    if value.get("is_error").and_then(Value::as_bool) == Some(false) {
        return Event::TurnEnd(text_at(value, "result"));
    }
    Event::TurnFailed(TurnFailure {
        kind: failure_kind(value),
        message: failure_message(value),
    })
}

/// The non-empty strings of a `result` line's `errors`.
fn errors_of(value: &Value) -> Vec<&str> {
    value
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(Value::as_str)
                .filter(|error| !error.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Why the turn failed. `grok` prints no status field, so after the two fields that do decide —
/// a signal's `stop_reason` and the turn limit's `subtype` — the kind is read from the text of
/// the failure, the provider's own words.
fn failure_kind(value: &Value) -> FailureKind {
    if value.get("stop_reason").and_then(Value::as_str) == Some(CANCELLED) {
        return FailureKind::Canceled;
    }
    if value.get("subtype").and_then(Value::as_str) == Some(ERROR_MAX_TURNS) {
        return FailureKind::MaxTurns;
    }
    let mut text: Vec<&str> = errors_of(value);
    text.push(str_at(value, "result"));
    let text = text.join("\n");
    if AUTH_MARKERS.iter().any(|marker| text.contains(marker)) {
        return FailureKind::Auth;
    }
    if QUOTA_MARKERS.iter().any(|marker| text.contains(marker)) {
        return FailureKind::Quota;
    }
    FailureKind::HarnessError
}

/// The failure as `grok` described it: its `errors`, else its `result`, else the fields that
/// say how it ended.
fn failure_message(value: &Value) -> String {
    let errors = errors_of(value);
    if !errors.is_empty() {
        return errors.join("; ");
    }
    let result = str_at(value, "result");
    if !result.is_empty() {
        return result.to_string();
    }
    let described: Vec<String> = [
        scalar_text(value.get("subtype")),
        scalar_text(value.get("stop_reason")).map(|it| format!("stop_reason: {it}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if described.is_empty() {
        UNDESCRIBED_FAILURE.to_string()
    } else {
        described.join(", ")
    }
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
    // to: a code cannot tell a canceled turn from a finished one.
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
    let message = if tail.is_empty() {
        what
    } else {
        format!("{what}: {tail}")
    };
    failed(FailureKind::HarnessError, message)
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

        /// What the lines of this process have built up so far. Carried from one `parse` to the
        /// next, which is what keeps the events of a run independent of how its lines are
        /// batched.
        static STATE: RefCell<super::ParseState> =
            const { RefCell::new(super::ParseState::new()) };
    }

    use exports::murmur::driver::process::{
        Bridge, Description, DriverFile, Event, ExitStatus, FailureKind, Guest, HarnessNeeds,
        InterruptMethod, LaunchPlan, LaunchRequest, LoginCheck, LoginOutput, LoginState,
        PathAccess, PathNeed, Session, SessionInfo, SessionMode, ToolCallInfo, ToolCallProgress,
        ToolCallStart, ToolResultInfo, TurnFailure, Usage,
    };

    pub struct GrokBuildDriver;

    impl Guest for GrokBuildDriver {
        fn describe() -> Description {
            from_description(super::describe())
        }

        fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
            super::launch(to_request(request)).map(|launched| {
                PARSE_CONTEXT.with(|stored| *stored.borrow_mut() = launched.context);
                // A new process has spent nothing and opened no call: its stream starts over.
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
                super::InterruptMethod::StdinMessage => InterruptMethod::StdinMessage,
                super::InterruptMethod::SignalInt => InterruptMethod::SignalInt,
                super::InterruptMethod::Unsupported => InterruptMethod::Unsupported,
            },
            required_env: description.required_env,
            streams_text: description.streams_text,
            reports_usage: description.reports_usage,
            login_check: match description.login_check {
                super::LoginCheck::Command(args) => LoginCheck::Command(args),
                super::LoginCheck::Unavailable(reason) => LoginCheck::Unavailable(reason),
            },
            harness_needs: HarnessNeeds {
                paths: description
                    .harness_needs
                    .paths
                    .into_iter()
                    .map(|need| PathNeed {
                        path: need.path,
                        access: match need.access {
                            super::PathAccess::Read => PathAccess::Read,
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
                    super::FailureKind::MaxTurns => FailureKind::MaxTurns,
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
            super::Event::ToolCallProgress(progress) => Event::ToolCallProgress(ToolCallProgress {
                id: progress.id,
                input_bytes: progress.input_bytes,
            }),
        }
    }

    export!(GrokBuildDriver);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SESSION_ID: &str = "019a7c3e-5b10-7a4c-9e21-3f6d2b8a1c05";
    const SYSTEM_PROMPT: &str = "You are the capsule under test.";
    const SERVER: &str = "capsule_srv";
    const TOKEN: &str = "tok-123";

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
            server_name: SERVER.to_string(),
            url: "http://127.0.0.1:8931/mcp".to_string(),
            bearer_token: TOKEN.to_string(),
            tool_names: vec!["echo_tool".to_string(), "write-file".to_string()],
        }
    }

    fn bridged() -> LaunchRequest {
        LaunchRequest {
            bridge: Some(bridge()),
            ..request()
        }
    }

    fn plan_of(request: LaunchRequest) -> LaunchPlan {
        launch(request).expect("launch plans this request").plan
    }

    fn args_of(request: LaunchRequest) -> Vec<String> {
        plan_of(request).args
    }

    fn file<'a>(plan: &'a LaunchPlan, name: &str) -> &'a str {
        &plan
            .files
            .iter()
            .find(|file| file.name == name)
            .unwrap_or_else(|| panic!("the plan writes {name}"))
            .contents
    }

    fn strs(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    // ── describe and read-login ───────────────────────────────────────────────

    #[test]
    fn describe_names_the_harness_and_what_it_needs() {
        assert_eq!(
            describe(),
            Description {
                harness: "grok".to_string(),
                binary: "grok".to_string(),
                version_args: strs(&["version"]),
                tested_versions: strs(&["1.0.77"]),
                interrupt: InterruptMethod::SignalInt,
                required_env: strs(&["HOME", "PATH"]),
                streams_text: true,
                reports_usage: true,
                login_check: LoginCheck::Command(strs(&["models"])),
                harness_needs: HarnessNeeds {
                    paths: vec![PathNeed {
                        path: "~/.grok".to_string(),
                        access: PathAccess::ReadWrite,
                    }],
                    hosts: strs(&["api.x.ai", "auth.x.ai"]),
                },
            }
        );
    }

    fn login(exit_code: Option<i32>, stdout: &str) -> LoginState {
        read_login(&LoginOutput {
            exit_code,
            stdout: stdout.to_string(),
            stderr: "You are logged in with grok.com.".to_string(),
        })
    }

    #[test]
    fn the_login_check_reads_the_first_line_grok_models_prints() {
        for line in [
            "You are logged in with grok.com.",
            "You are using XAI_API_KEY.",
            "  You are logged in with grok.com.  ",
        ] {
            assert_eq!(
                login(Some(0), &format!("\n\n{line}\nModels:\n  grok-4.6")),
                LoginState::LoggedIn,
                "{line}"
            );
        }
        assert_eq!(
            login(Some(0), "You are not authenticated.\nRun grok login."),
            LoginState::LoggedOut(
                "grok is not signed in: run `grok login` as the user mur runs as".to_string()
            )
        );
    }

    #[test]
    fn only_the_first_non_empty_line_decides() {
        assert_eq!(
            login(Some(0), "Models:\nYou are logged in with grok.com."),
            LoginState::Unknown(
                "`grok models` printed a first line this driver does not recognise".to_string()
            )
        );
        assert_eq!(
            login(Some(0), "   \n\t\nYou are not authenticated."),
            LoginState::LoggedOut(
                "grok is not signed in: run `grok login` as the user mur runs as".to_string()
            )
        );
    }

    #[test]
    fn nothing_printed_says_how_the_command_ended() {
        assert_eq!(
            login(Some(3), " \n"),
            LoginState::Unknown("`grok models` exited 3 without printing anything".to_string())
        );
        assert_eq!(
            login(None, ""),
            LoginState::Unknown(
                "`grok models` was ended by a signal without printing anything".to_string()
            )
        );
    }

    #[test]
    fn no_login_reason_quotes_the_output() {
        const MARKER: &str = "acct-QXZJ@example.invalid";
        for stdout in [
            MARKER.to_string(),
            format!("You are not authenticated as {MARKER}."),
            format!("You are logged in as {MARKER}"),
        ] {
            let reason = match login(Some(0), &stdout) {
                LoginState::LoggedIn => continue,
                LoginState::LoggedOut(reason) | LoginState::Unknown(reason) => reason,
            };
            assert!(!reason.contains(MARKER), "{reason}");
        }
    }

    // ── driver config ─────────────────────────────────────────────────────────

    #[test]
    fn no_config_an_empty_object_or_whitespace_leaves_the_effort_to_grok() {
        for config in [None, Some("{}"), Some("  \n "), Some("")] {
            assert_eq!(
                parse_driver_config(config),
                Ok(DriverConfig::default()),
                "{config:?}"
            );
        }
    }

    #[test]
    fn every_canonical_effort_level_is_read() {
        for (level, effort) in [
            ("none", ReasoningEffort::None),
            ("minimal", ReasoningEffort::Minimal),
            ("low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::Xhigh),
            ("max", ReasoningEffort::Max),
        ] {
            let config = json!({ "reasoning_effort": level }).to_string();
            assert_eq!(
                parse_driver_config(Some(&config)),
                Ok(DriverConfig {
                    reasoning_effort: Some(effort)
                })
            );
            assert_eq!(effort.as_str(), level);
        }
    }

    #[test]
    fn an_effort_that_is_not_exactly_a_canonical_level_is_refused_naming_the_value() {
        for (value, shown) in [
            (json!("high "), "\"high \""),
            (json!("HIGH"), "\"HIGH\""),
            (json!("deep"), "\"deep\""),
            (json!(3), "3"),
            (json!(null), "null"),
            (json!(["low"]), "[\"low\"]"),
        ] {
            let config = json!({ "reasoning_effort": value }).to_string();
            assert_eq!(
                parse_driver_config(Some(&config)),
                Err(format!(
                    "driver: inference.driver.config 'reasoning_effort' must be one of none, minimal, low, medium, high, xhigh, max, got {shown}"
                ))
            );
        }
    }

    #[test]
    fn an_unrecognised_key_is_refused_sorted_with_the_accepted_keys() {
        assert_eq!(
            parse_driver_config(Some(r#"{"zeta":1,"alpha":2,"reasoning_effort":"low"}"#)),
            Err("driver: unrecognised inference.driver.config key(s): alpha, zeta. Accepted keys: reasoning_effort".to_string())
        );
    }

    #[test]
    fn a_config_that_is_not_a_json_object_is_refused() {
        let Err(not_json) = parse_driver_config(Some("{reasoning_effort")) else {
            panic!("not JSON is refused");
        };
        assert!(
            not_json.starts_with("driver: inference.driver.config is not valid JSON: "),
            "{not_json}"
        );
        for config in ["[]", "\"low\"", "3", "null"] {
            assert_eq!(
                parse_driver_config(Some(config)),
                Err("driver: inference.driver.config must be a JSON object".to_string()),
                "{config}"
            );
        }
    }

    #[test]
    fn a_refused_config_refuses_the_turn_before_the_bridge_is_checked() {
        let refused = launch(LaunchRequest {
            config: Some(r#"{"effort":"low"}"#.to_string()),
            bridge: Some(Bridge {
                server_name: "9bad".to_string(),
                ..bridge()
            }),
            ..request()
        });
        assert_eq!(
            refused,
            Err("driver: unrecognised inference.driver.config key(s): effort. Accepted keys: reasoning_effort".to_string())
        );
    }

    // ── the bridge names grok admits ──────────────────────────────────────────

    fn with_names(server: &str, tools: &[&str]) -> Result<Launched, String> {
        launch(LaunchRequest {
            bridge: Some(Bridge {
                server_name: server.to_string(),
                tool_names: strs(tools),
                ..bridge()
            }),
            ..request()
        })
    }

    #[test]
    fn murmurs_own_names_are_admitted() {
        assert!(with_names(
            "murmur",
            &[
                "tool-read",
                "session-status",
                "tool-transit-board-relay",
                "git"
            ]
        )
        .is_ok());
        assert!(with_names("_a-b_9", &["2fa_enable", "x"]).is_ok());
    }

    #[test]
    fn a_server_name_grok_cannot_admit_is_refused_by_name() {
        for server in [
            "", "9srv", "-srv", "srv_", "srv__x", "__srv", "s.rv", "s rv", "sérv",
        ] {
            let refusal = with_names(server, &["echo_tool"]).expect_err(server);
            assert_eq!(
                refusal,
                format!(
                    "driver: grok cannot admit the tool bridge's server name {}: it must start with an ASCII letter or '_', use only ASCII letters, digits, '_' and '-', hold no '__', and not end with '_'; grok skips tools it cannot admit, so the model would never see them",
                    Value::from(server)
                )
            );
        }
    }

    #[test]
    fn each_tool_name_grok_cannot_admit_is_named_with_why() {
        let long = "t".repeat(MAX_CATALOG_KEY - "srv__".len() + 1);
        let refusal = with_names(
            "srv",
            &[
                "ok-tool",
                "",
                "a.b",
                "a__b",
                "_lead",
                long.as_str(),
                "fine_",
            ],
        )
        .expect_err("bad tool names are refused");
        assert_eq!(
            refusal,
            format!(
                "driver: grok cannot admit these capsule tool names: \"\" (it is empty), \"a.b\" (it uses a character other than ASCII letters, digits, '_' and '-'), \"a__b\" (it contains '__'), \"_lead\" (it starts with '_', which makes '___' in its catalog key), \"{long}\" (its catalog key '<server>__<tool>' is longer than 256 characters); grok skips tools it cannot admit, so the model would never see them"
            )
        );
    }

    #[test]
    fn a_catalog_key_of_exactly_256_characters_is_admitted() {
        let longest = "t".repeat(MAX_CATALOG_KEY - "srv__".len());
        assert!(with_names("srv", &[longest.as_str()]).is_ok());
    }

    #[test]
    fn without_a_bridge_no_name_is_checked() {
        assert!(launch(request()).is_ok());
    }

    // ── the argv ──────────────────────────────────────────────────────────────

    #[test]
    fn a_new_bridged_turn_has_exactly_this_argv() {
        assert_eq!(
            args_of(bridged()),
            strs(&[
                "--prompt-file",
                "{files_dir}/task.md",
                "--output-format",
                "streaming-messages-json",
                "--include-partial-messages",
                "--agent",
                "{files_dir}/capsule-agent.md",
                "--system-prompt-override=You are the capsule under test.",
                "--allow",
                "MCPTool(capsule_srv__*)",
                "--no-auto-update",
                "--disable-web-search",
                "--no-subagents",
                "-s",
                SESSION_ID,
            ])
        );
    }

    #[test]
    fn a_resumed_turn_with_a_model_and_an_effort_has_exactly_this_argv() {
        let args = args_of(LaunchRequest {
            model: Some("  grok-4.6 ".to_string()),
            config: Some(r#"{"reasoning_effort":"high"}"#.to_string()),
            session: Session {
                id: SESSION_ID.to_string(),
                mode: SessionMode::Resume,
            },
            ..bridged()
        });
        assert_eq!(
            args,
            strs(&[
                "--prompt-file",
                "{files_dir}/task.md",
                "--output-format",
                "streaming-messages-json",
                "--include-partial-messages",
                "--agent",
                "{files_dir}/capsule-agent.md",
                "--system-prompt-override=You are the capsule under test.",
                "--allow",
                "MCPTool(capsule_srv__*)",
                "--no-auto-update",
                "--disable-web-search",
                "--no-subagents",
                "--reasoning-effort",
                "high",
                "-m",
                "grok-4.6",
                "-r",
                SESSION_ID,
            ])
        );
    }

    #[test]
    fn without_a_bridge_there_is_no_allow_rule() {
        let args = args_of(request());
        assert!(!args.iter().any(|arg| arg == "--allow"), "{args:?}");
        assert!(!args.iter().any(|arg| arg.starts_with(MCP_ALLOW_RULE)));
    }

    #[test]
    fn a_blank_model_is_left_to_the_harness() {
        for model in [None, Some(String::new()), Some("  \t".to_string())] {
            let args = args_of(LaunchRequest {
                model: model.clone(),
                ..request()
            });
            assert!(!args.iter().any(|arg| arg == "-m"), "{model:?}: {args:?}");
        }
    }

    #[test]
    fn a_system_prompt_starting_with_a_dash_stays_one_value() {
        let prompt = "- You are a capsule.\n--yolo\n";
        let args = args_of(LaunchRequest {
            system_prompt: prompt.to_string(),
            ..request()
        });
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("--system-prompt-override"))
                .collect::<Vec<_>>(),
            [&format!("--system-prompt-override={prompt}")]
        );
        assert!(!args.iter().any(|arg| arg == "--yolo"));
    }

    #[test]
    fn no_flag_that_approves_every_tool_is_ever_passed() {
        for request in [request(), bridged()] {
            let args = args_of(request);
            for flag in [
                "--yolo",
                "--always-approve",
                "--permission-mode",
                "bypassPermissions",
                "--trust",
            ] {
                assert!(!args.iter().any(|arg| arg == flag), "{flag} in {args:?}");
            }
        }
    }

    #[test]
    fn the_bearer_token_is_never_in_argv() {
        let args = args_of(bridged());
        assert!(!args.iter().any(|arg| arg.contains(TOKEN)), "{args:?}");
        assert!(file(&plan_of(bridged()), "capsule-agent.md").contains(TOKEN));
    }

    // ── the environment, files and stdin ──────────────────────────────────────

    #[test]
    fn the_env_switches_are_exactly_these_in_this_order() {
        let expected: Vec<(String, String)> = [
            ("GROK_CLAUDE_MCPS_ENABLED", "false"),
            ("GROK_CLAUDE_SKILLS_ENABLED", "false"),
            ("GROK_CLAUDE_RULES_ENABLED", "false"),
            ("GROK_CLAUDE_AGENTS_ENABLED", "false"),
            ("GROK_CLAUDE_HOOKS_ENABLED", "false"),
            ("GROK_CURSOR_MCPS_ENABLED", "false"),
            ("GROK_CURSOR_SKILLS_ENABLED", "false"),
            ("GROK_CURSOR_RULES_ENABLED", "false"),
            ("GROK_CURSOR_AGENTS_ENABLED", "false"),
            ("GROK_CURSOR_HOOKS_ENABLED", "false"),
            ("GROK_CODEX_MCPS_ENABLED", "false"),
            ("GROK_CODEX_SKILLS_ENABLED", "false"),
            ("GROK_CODEX_RULES_ENABLED", "false"),
            ("GROK_CODEX_AGENTS_ENABLED", "false"),
            ("GROK_CODEX_HOOKS_ENABLED", "false"),
            ("GROK_TURN_SUMMARY", "0"),
            ("GROK_DISABLE_AUTOUPDATER", "1"),
            ("GROK_MEMORY", "0"),
        ]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        assert_eq!(plan_of(request()).env_set, expected);
        assert_eq!(plan_of(bridged()).env_set, expected);
    }

    #[test]
    fn no_variable_is_set_from_the_request() {
        let odd = LaunchRequest {
            model: Some("MODEL-QXZJ".to_string()),
            system_prompt: "PROMPT-QXZJ".to_string(),
            task: "TASK-QXZJ".to_string(),
            harness_version: Some("VERSION-QXZJ".to_string()),
            ..bridged()
        };
        for (name, value) in plan_of(odd).env_set {
            assert!(
                !name.contains("QXZJ") && !value.contains("QXZJ"),
                "{name}={value}"
            );
        }
    }

    #[test]
    fn the_task_file_holds_the_task_byte_for_byte() {
        let task = "  /goal not a command\n\"quoted\" \\ and ünïcode\n\n";
        let plan = plan_of(LaunchRequest {
            task: task.to_string(),
            ..request()
        });
        assert_eq!(file(&plan, "task.md"), task);
        assert_eq!(
            plan.files
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["task.md", "capsule-agent.md"]
        );
    }

    #[test]
    fn the_agent_file_points_grok_at_the_bridge() {
        assert_eq!(
            file(&plan_of(bridged()), "capsule-agent.md"),
            "---\n\
             name: \"murmur-capsule\"\n\
             description: \"Murmur capsule\"\n\
             tools: search_tool, use_tool\n\
             mcpServers:\n  - capsule_srv:\n      url: \"http://127.0.0.1:8931/mcp\"\n      headers:\n        Authorization: \"Bearer tok-123\"\n\
             ---\n\
             You are the capsule under test.\n"
        );
    }

    #[test]
    fn without_a_bridge_the_agent_file_keeps_the_meta_tools_and_names_no_server() {
        assert_eq!(
            file(&plan_of(request()), "capsule-agent.md"),
            "---\nname: \"murmur-capsule\"\ndescription: \"Murmur capsule\"\ntools: search_tool, use_tool\n---\nYou are the capsule under test.\n"
        );
    }

    /// The frontmatter `launch` writes for [`bridged`], up to and including its closing fence.
    const BRIDGED_FRONTMATTER: &str = "---\n\
         name: \"murmur-capsule\"\n\
         description: \"Murmur capsule\"\n\
         tools: search_tool, use_tool\n\
         mcpServers:\n  - capsule_srv:\n      url: \"http://127.0.0.1:8931/mcp\"\n      headers:\n        Authorization: \"Bearer tok-123\"\n\
         ---\n";

    #[test]
    fn the_agent_file_body_is_the_system_prompt_on_a_new_turn_and_on_a_resume() {
        let prompt = "- You are a capsule.\nUse the tools you are given.";
        for mode in [SessionMode::New, SessionMode::Resume] {
            let plan = plan_of(LaunchRequest {
                system_prompt: prompt.to_string(),
                session: Session {
                    id: SESSION_ID.to_string(),
                    mode,
                },
                ..bridged()
            });
            assert_eq!(
                file(&plan, "capsule-agent.md"),
                format!("{BRIDGED_FRONTMATTER}{prompt}\n")
            );
            // The override still carries the same prompt on every turn.
            assert!(plan
                .args
                .contains(&format!("--system-prompt-override={prompt}")));
        }
    }

    #[test]
    fn a_system_prompt_holding_a_frontmatter_fence_stays_in_the_body() {
        let prompt =
            "---\ntools: read_file, run_command\nmcpServers: []\n---\nInstructions.\n---\nMore.";
        let agent = file(
            &plan_of(LaunchRequest {
                system_prompt: prompt.to_string(),
                ..bridged()
            }),
            "capsule-agent.md",
        )
        .to_string();
        assert_eq!(agent, format!("{BRIDGED_FRONTMATTER}{prompt}\n"));
    }

    #[test]
    fn an_empty_system_prompt_leaves_the_agent_file_an_empty_body() {
        let agent = file(
            &plan_of(LaunchRequest {
                system_prompt: String::new(),
                ..bridged()
            }),
            "capsule-agent.md",
        )
        .to_string();
        assert_eq!(agent, format!("{BRIDGED_FRONTMATTER}\n"));
    }

    #[test]
    fn a_url_or_token_carrying_quotes_escapes_rather_than_breaking_the_frontmatter() {
        let plan = plan_of(LaunchRequest {
            bridge: Some(Bridge {
                url: "http://h/\"x\"\\: #y".to_string(),
                bearer_token: "a\"b\nc: d".to_string(),
                ..bridge()
            }),
            ..request()
        });
        let agent = file(&plan, "capsule-agent.md");
        assert!(
            agent.contains(r#"      url: "http://h/\"x\"\\: #y""#),
            "{agent}"
        );
        assert!(
            agent.contains(r#"        Authorization: "Bearer a\"b\nc: d""#),
            "{agent}"
        );
        // Every value line is still one line.
        assert_eq!(agent.lines().count(), 11);
    }

    #[test]
    fn grok_never_reads_stdin() {
        for request in [request(), bridged()] {
            let plan = plan_of(request);
            assert_eq!(plan.stdin, None);
            assert!(!plan.keep_stdin_open);
            assert_eq!(plan.interrupt_stdin, None);
        }
    }

    #[test]
    fn launch_returns_the_context_its_output_is_read_against() {
        assert_eq!(
            launch(bridged()).expect("plannable").context,
            ParseContext::with_prefix("capsule_srv__")
        );
        assert_eq!(
            launch(request()).expect("plannable").context,
            ParseContext::none()
        );
        assert_eq!(tool_name_prefix("murmur"), "murmur__");
    }

    #[test]
    fn the_driver_hardcodes_no_server_name() {
        // The runtime names the bridge server; a constant here would silently stop matching it.
        let source = include_str!("lib.rs");
        let non_test = &source[..source.find("\nmod tests {").expect("mod tests must exist")];
        for needle in [concat!("murmur", "__"), concat!("murmur", "_bridge")] {
            assert!(
                !non_test.contains(needle),
                "non-test source must not contain {needle}"
            );
        }
    }

    // ── parse: one line at a time ─────────────────────────────────────────────

    fn context() -> ParseContext {
        parse_context_for(Some(&bridge()))
    }

    fn events(line: &str) -> Vec<Event> {
        parse_line(line, &context(), &mut ParseState::new())
    }

    fn read_in_order(lines: &[String]) -> Vec<Event> {
        parse_lines(lines, &context(), &mut ParseState::new())
    }

    fn note(text: &str) -> Event {
        Event::Note(text.to_string())
    }

    #[test]
    fn a_blank_or_whitespace_only_line_carries_nothing() {
        assert_eq!(events(""), Vec::new());
        assert_eq!(events("  \t "), Vec::new());
    }

    #[test]
    fn a_line_the_driver_cannot_read_becomes_exactly_one_note() {
        for line in [
            "not json",
            "[1,2]",
            "\"text\"",
            "{}",
            r#"{"type":7}"#,
            r#"{"type":"control_response"}"#,
            r#"{"type":"rate_limit_event"}"#,
        ] {
            assert_eq!(
                events(line),
                vec![note(&format!("unreadable stdout line: {line}"))]
            );
        }
    }

    #[test]
    fn a_runaway_line_is_truncated_in_its_note() {
        let line = "é".repeat(NOTE_LINE_LIMIT + 10);
        assert_eq!(
            events(&line),
            vec![note(&format!(
                "unreadable stdout line: {}…",
                "é".repeat(NOTE_LINE_LIMIT)
            ))]
        );
    }

    fn init(fields: Value) -> String {
        let mut line = json!({ "type": "system", "subtype": "init" });
        for (key, value) in fields.as_object().expect("an object") {
            line[key] = value.clone();
        }
        line.to_string()
    }

    fn session(id: &str, auth: &str, model: Option<&str>) -> Event {
        Event::SessionStarted(SessionInfo {
            id: id.to_string(),
            auth: auth.to_string(),
            model: model.map(str::to_string),
        })
    }

    #[test]
    fn an_init_line_reports_the_session_and_how_grok_is_billing() {
        assert_eq!(
            events(&init(
                json!({ "session_id": "s1", "apiKeySource": "oauth", "model": "grok-4.6" })
            )),
            vec![session("s1", "subscription", Some("grok-4.6"))]
        );
        for source in [json!("user"), json!("none"), json!(null), json!(1)] {
            assert_eq!(
                events(&init(json!({ "session_id": "s1", "apiKeySource": source }))),
                vec![session("s1", "api-key", None)]
            );
        }
        assert_eq!(
            events(&init(json!({ "session_id": "s1" }))),
            vec![session("s1", "api-key", None)]
        );
    }

    #[test]
    fn an_unknown_or_empty_model_is_no_model() {
        for model in [json!("unknown"), json!(""), json!(null), json!(4)] {
            assert_eq!(
                events(&init(json!({ "session_id": "s1", "model": model }))),
                vec![session("s1", "api-key", None)]
            );
        }
    }

    #[test]
    fn an_init_without_a_session_id_reports_nothing() {
        for id in [json!(""), json!(null), json!(7)] {
            assert_eq!(
                events(&init(json!({ "session_id": id, "model": "grok-4.6" }))),
                Vec::new()
            );
        }
        assert_eq!(events(&init(json!({}))), Vec::new());
    }

    #[test]
    fn a_bridge_grok_reports_as_down_is_one_note_after_the_session() {
        for status in ["failed", "needs-auth", "disabled"] {
            assert_eq!(
                events(&init(json!({
                    "session_id": "s1",
                    "mcp_servers": [
                        { "name": "other", "status": "connected" },
                        { "name": SERVER, "status": status },
                    ],
                }))),
                vec![
                    session("s1", "api-key", None),
                    note(&format!(
                        "grok reports the capsule's tool bridge as {status}"
                    )),
                ]
            );
        }
    }

    #[test]
    fn a_bridge_that_is_up_or_still_connecting_or_another_server_down_is_not_noted() {
        for servers in [
            json!([{ "name": SERVER, "status": "pending" }]),
            json!([{ "name": SERVER, "status": "connected" }]),
            json!([{ "name": "operator_srv", "status": "failed" }]),
            json!([]),
            json!("failed"),
        ] {
            assert_eq!(
                events(&init(json!({ "session_id": "s1", "mcp_servers": servers }))),
                vec![session("s1", "api-key", None)]
            );
        }
        // With no bridge there is no server to report on.
        let line = init(
            json!({ "session_id": "s1", "mcp_servers": [{ "name": SERVER, "status": "failed" }] }),
        );
        assert_eq!(
            parse_line(&line, &ParseContext::none(), &mut ParseState::new()),
            vec![session("s1", "api-key", None)]
        );
    }

    #[test]
    fn a_system_line_of_another_subtype_carries_nothing() {
        for subtype in [json!("compact_boundary"), json!(null), json!(3)] {
            assert_eq!(
                events(
                    &json!({ "type": "system", "subtype": subtype, "session_id": "s" }).to_string()
                ),
                Vec::new()
            );
        }
    }

    fn stream(event: Value) -> String {
        json!({ "type": "stream_event", "event": event }).to_string()
    }

    fn delta(delta: Value) -> String {
        stream(json!({ "type": "content_block_delta", "index": 0, "delta": delta }))
    }

    #[test]
    fn text_and_thinking_deltas_stream_when_non_empty() {
        assert_eq!(
            events(&delta(json!({ "type": "text_delta", "text": "Hi" }))),
            vec![Event::TextDelta("Hi".to_string())]
        );
        assert_eq!(
            events(&delta(
                json!({ "type": "thinking_delta", "thinking": "Hm" })
            )),
            vec![Event::ThinkingDelta("Hm".to_string())]
        );
        assert_eq!(
            events(&delta(json!({ "type": "text_delta", "text": " \n" }))),
            vec![Event::TextDelta(" \n".to_string())]
        );
        for empty in [
            json!({ "type": "text_delta", "text": "" }),
            json!({ "type": "thinking_delta", "thinking": "" }),
            json!({ "type": "text_delta" }),
            json!({ "type": "text_delta", "text": 5 }),
            json!({ "type": "signature_delta", "signature": "x" }),
        ] {
            assert_eq!(events(&delta(empty.clone())), Vec::new(), "{empty}");
        }
    }

    #[test]
    fn the_rest_of_the_stream_framing_carries_nothing() {
        for event in [
            json!({ "type": "message_start", "message": {} }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta", "usage": { "input_tokens": 9 } }),
            json!({ "type": "message_stop" }),
            json!({ "type": "ping" }),
            json!(null),
        ] {
            assert_eq!(events(&stream(event.clone())), Vec::new(), "{event}");
        }
    }

    fn assistant(content: Value) -> String {
        json!({ "type": "assistant", "message": { "content": content } }).to_string()
    }

    fn use_tool(id: &str, input: Value) -> Value {
        json!({ "type": "tool_use", "id": id, "name": "use_tool", "input": input })
    }

    fn call(id: &str, name: &str, input: &str) -> Event {
        Event::ToolCall(ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        })
    }

    #[test]
    fn one_assistant_message_emits_one_thinking_one_text_then_its_calls() {
        assert_eq!(
            events(&assistant(json!([
                { "type": "thinking", "thinking": "a" },
                { "type": "text", "text": "one " },
                use_tool("c1", json!({ "tool_name": "capsule_srv__echo_tool", "tool_input": { "text": "hi" } })),
                { "type": "thinking", "thinking": "b" },
                { "type": "text", "text": "two" },
            ]))),
            vec![
                Event::Thinking("ab".to_string()),
                Event::Text("one two".to_string()),
                call("c1", "echo_tool", r#"{"text":"hi"}"#),
            ]
        );
        assert_eq!(
            events(&assistant(
                json!([{ "type": "thinking", "thinking": "" }, { "type": "text", "text": "" }])
            )),
            Vec::new()
        );
    }

    #[test]
    fn a_capsule_call_carries_its_bare_name_and_exactly_its_tool_input() {
        for (tool_input, expected) in [
            (
                json!({ "path": "a.txt", "n": [1, 2] }),
                r#"{"n":[1,2],"path":"a.txt"}"#,
            ),
            (json!(null), "{}"),
            (json!("raw"), "\"raw\""),
            (json!([1]), "[1]"),
        ] {
            assert_eq!(
                events(&assistant(json!([use_tool(
                    "c1",
                    json!({ "tool_name": "capsule_srv__write-file", "tool_input": tool_input })
                )]))),
                vec![call("c1", "write-file", expected)]
            );
        }
        assert_eq!(
            events(&assistant(json!([use_tool(
                "c1",
                json!({ "tool_name": "capsule_srv__echo_tool" })
            )]))),
            vec![call("c1", "echo_tool", "{}")]
        );
    }

    #[test]
    fn a_call_that_is_not_the_capsules_is_noted_by_name_alone() {
        let cases = [
            (
                json!({ "type": "tool_use", "id": "c1", "name": "search_tool", "input": { "query": "SECRET" } }),
                "grok called 'search_tool', which is not one of the capsule's tools (call c1)",
            ),
            (
                use_tool("c2", json!({ "tool_name": "operator_srv__echo_tool", "tool_input": { "t": "SECRET" } })),
                "grok called use_tool for 'operator_srv__echo_tool', which is not one of the capsule's tools (call c2)",
            ),
            (
                use_tool("c3", json!({ "tool_name": "echo_tool", "tool_input": { "t": "SECRET" } })),
                "grok called use_tool for 'echo_tool', which is not one of the capsule's tools (call c3)",
            ),
            (
                use_tool("c4", json!({ "tool_name": "capsule_srv__", "tool_input": { "t": "SECRET" } })),
                "grok called use_tool for 'capsule_srv__', which is not one of the capsule's tools (call c4)",
            ),
            (
                use_tool("c5", json!({ "tool_name": "capsule_srv__echo_tool", "tool_input_file": "/tmp/SECRET.json" })),
                "grok called use_tool for 'capsule_srv__echo_tool' with its arguments in a file, which is never read as a capsule call (call c5)",
            ),
            (
                use_tool("c6", json!({ "file": "/tmp/SECRET.json" })),
                "grok called use_tool with its whole call in a file, which is never read as a capsule call (call c6)",
            ),
            (
                use_tool("c7", json!({ "tool_name": 7, "tool_input": "SECRET" })),
                "grok called 'use_tool', which is not one of the capsule's tools (call c7)",
            ),
            (
                use_tool("c8", json!("SECRET")),
                "grok called 'use_tool', which is not one of the capsule's tools (call c8)",
            ),
            (
                json!({ "type": "tool_use", "id": "c9", "name": "run_terminal_cmd", "input": { "command": "SECRET" } }),
                "grok called 'run_terminal_cmd', which is not one of the capsule's tools (call c9)",
            ),
        ];
        for (block, expected) in cases {
            let events = events(&assistant(json!([block])));
            assert_eq!(events, vec![note(expected)]);
            assert!(!format!("{events:?}").contains("SECRET"));
        }
    }

    #[test]
    fn with_no_bridge_nothing_is_a_capsule_call() {
        let line = assistant(json!([use_tool(
            "c1",
            json!({ "tool_name": "capsule_srv__echo_tool", "tool_input": {} })
        )]));
        assert_eq!(
            parse_line(&line, &ParseContext::none(), &mut ParseState::new()),
            vec![note("grok called use_tool for 'capsule_srv__echo_tool', which is not one of the capsule's tools (call c1)")]
        );
    }

    #[test]
    fn only_the_prefix_launch_built_is_stripped() {
        let context = ParseContext::with_prefix("srv__");
        let line = assistant(json!([
            use_tool("c1", json!({ "tool_name": "srv__a", "tool_input": {} })),
            use_tool("c2", json!({ "tool_name": "srv2__a", "tool_input": {} })),
            use_tool("c3", json!({ "tool_name": "xsrv__a", "tool_input": {} })),
            use_tool(
                "c4",
                json!({ "tool_name": "srv__srv__a", "tool_input": {} })
            ),
        ]));
        let events = parse_line(&line, &context, &mut ParseState::new());
        assert_eq!(events[0], call("c1", "a", "{}"));
        assert!(matches!(&events[1], Event::Note(_)));
        assert!(matches!(&events[2], Event::Note(_)));
        assert_eq!(events[3], call("c4", "srv__a", "{}"));
    }

    #[test]
    fn a_quoted_tool_name_in_a_note_is_cut_at_the_note_limit() {
        let name = format!("x{}", "y".repeat(NOTE_LINE_LIMIT + 5));
        let events = events(&assistant(json!([use_tool(
            "c1",
            json!({ "tool_name": name })
        )])));
        assert_eq!(
            events,
            vec![note(&format!(
                "grok called use_tool for '{}…', which is not one of the capsule's tools (call c1)",
                &name[..NOTE_LINE_LIMIT]
            ))]
        );
    }

    fn user(blocks: Value) -> String {
        json!({ "type": "user", "message": { "role": "user", "content": blocks } }).to_string()
    }

    fn result_block(id: &str, content: Value, is_error: Value) -> Value {
        json!({ "type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error })
    }

    fn tool_result(id: &str, output: &str, is_error: bool) -> Event {
        Event::ToolResult(ToolResultInfo {
            id: id.to_string(),
            output: output.to_string(),
            is_error,
        })
    }

    #[test]
    fn the_result_of_a_noted_call_is_noted_by_size_once() {
        let content =
            "[{\"type\":\"content\",\"content\":{\"type\":\"text\",\"text\":\"SECRET\"}}]";
        let lines = [
            assistant(json!([use_tool(
                "c1",
                json!({ "tool_name": "operator_srv__x" })
            )])),
            user(json!([result_block("c1", json!(content), json!(true))])),
            user(json!([result_block("c1", json!(content), json!(true))])),
        ];
        let events = read_in_order(&lines);
        assert_eq!(
            events[1],
            note(&format!(
                "grok answered its own call c1: {} bytes, is_error true",
                content.len()
            ))
        );
        // A second result for the same id is no longer a noted call's.
        assert!(matches!(&events[2], Event::ToolResult(result) if result.id == "c1"));
        assert!(!format!("{:?}", &events[..2]).contains("SECRET"));
    }

    #[test]
    fn a_noted_results_size_counts_whatever_content_it_carries() {
        for (content, bytes, is_error, shown) in [
            (json!(null), 0, json!(null), false),
            (
                json!([{ "type": "text", "text": "ab" }]),
                29,
                json!(false),
                false,
            ),
        ] {
            let lines = [
                assistant(
                    json!([{ "type": "tool_use", "id": "c1", "name": "search_tool", "input": {} }]),
                ),
                user(json!([result_block("c1", content, is_error)])),
            ];
            assert_eq!(
                read_in_order(&lines)[1],
                note(&format!(
                    "grok answered its own call c1: {bytes} bytes, is_error {shown}"
                ))
            );
        }
    }

    #[test]
    fn the_noted_calls_are_bounded_dropping_the_oldest() {
        let mut state = ParseState::new();
        let context = context();
        for n in 0..=FOREIGN_CALL_LIMIT {
            parse_line(
                &assistant(
                    json!([{ "type": "tool_use", "id": format!("c{n}"), "name": "search_tool", "input": {} }]),
                ),
                &context,
                &mut state,
            );
        }
        assert_eq!(state.foreign.ids.len(), FOREIGN_CALL_LIMIT);
        let oldest = parse_line(
            &user(json!([result_block("c0", json!("x"), json!(false))])),
            &context,
            &mut state,
        );
        assert_eq!(oldest, vec![tool_result("c0", "x", false)]);
        let newest = parse_line(
            &user(json!([result_block(
                &format!("c{FOREIGN_CALL_LIMIT}"),
                json!("x"),
                json!(false)
            )])),
            &context,
            &mut state,
        );
        assert!(matches!(&newest[..], [Event::Note(_)]));
    }

    #[test]
    fn a_tool_result_is_paired_by_its_own_id_and_is_error() {
        assert_eq!(
            events(&user(json!([
                result_block("a", json!("one"), json!(true)),
                result_block("b", json!("two"), json!(null)),
                { "type": "text", "text": "ignored" },
                { "type": "tool_result", "tool_use_id": "c", "content": "three" },
            ]))),
            vec![
                tool_result("a", "one", true),
                tool_result("b", "two", false),
                tool_result("c", "three", false),
            ]
        );
        assert_eq!(
            events(&user(json!([{ "type": "text", "text": "x" }]))),
            Vec::new()
        );
        assert_eq!(events(&user(json!("text"))), Vec::new());
    }

    fn output_of(content: Value) -> String {
        match events(&user(json!([result_block("c1", content, json!(false))]))).as_slice() {
            [Event::ToolResult(result)] => result.output.clone(),
            other => panic!("expected one tool-result, got {other:?}"),
        }
    }

    fn mcp(output: Value) -> Value {
        json!(json!({ "type": "MCP", "tool_name": "echo_tool", "server_name": SERVER, "output": output }).to_string())
    }

    #[test]
    fn a_bridged_result_is_unwrapped_to_the_value_of_its_one_output_key() {
        assert_eq!(
            output_of(mcp(json!({ "OkayOutput": "echoed: hi" }))),
            "echoed: hi"
        );
        assert_eq!(output_of(mcp(json!({ "ErrorOutput": "boom" }))), "boom");
        assert_eq!(
            output_of(mcp(json!({ "OkayOutput": { "a": 1 } }))),
            r#"{"a":1}"#
        );
        assert_eq!(output_of(mcp(json!({ "OkayOutput": 3 }))), "3");
        assert_eq!(output_of(mcp(json!({ "OkayOutput": null }))), "null");
    }

    #[test]
    fn a_wrapper_that_is_not_exactly_one_output_key_stays_verbatim() {
        for wrapper in [
            mcp(json!({})),
            mcp(json!({ "A": 1, "B": 2 })),
            mcp(json!("text")),
            json!(json!({ "type": "Other", "output": { "OkayOutput": "x" } }).to_string()),
            json!(json!({ "output": { "OkayOutput": "x" } }).to_string()),
        ] {
            assert_eq!(
                output_of(wrapper.clone()),
                wrapper.as_str().expect("a string")
            );
        }
    }

    #[test]
    fn a_refusal_reads_as_its_error_and_message() {
        assert_eq!(
            output_of(json!(
                r#"{"error":"tool_execution_failed","message":"'x' is not valid"}"#
            )),
            "tool_execution_failed: 'x' is not valid"
        );
        for verbatim in [
            r#"{"error":"e"}"#,
            r#"{"error":1,"message":"m"}"#,
            "plain text",
            "[1, 2]",
            "",
        ] {
            assert_eq!(output_of(json!(verbatim)), verbatim);
        }
    }

    #[test]
    fn a_list_of_blocks_reads_as_its_lines() {
        assert_eq!(
            output_of(json!([
                { "type": "text", "text": "line" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
                { "type": "other", "x": 1 },
            ])),
            "line\n[image: image/png, 3 bytes]\n{\"type\":\"other\",\"x\":1}"
        );
        assert_eq!(output_of(json!(null)), "");
        assert_eq!(
            output_of(json!({ "type": "image", "mimeType": "image/gif", "data": "AAA=" })),
            "[image: image/gif, 2 bytes]"
        );
        assert_eq!(output_of(json!({ "a": 1 })), r#"{"a":1}"#);
        assert_eq!(
            events(&user(
                json!([{ "type": "tool_result", "tool_use_id": "c1" }])
            )),
            vec![tool_result("c1", "", false)]
        );
    }

    #[test]
    fn a_rendered_list_gives_text_image_lines_and_json() {
        assert_eq!(
            output_of(mcp(json!({ "OkayOutput": [
                { "type": "text", "text": "ran" },
                { "type": "image", "data": "AAAAAAAA", "mimeType": "image/png" },
                { "type": "resource", "uri": "u" },
                "bare",
            ] }))),
            "ran\n[image: image/png, 6 bytes]\n{\"type\":\"resource\",\"uri\":\"u\"}\n\"bare\""
        );
        assert_eq!(
            output_of(mcp(
                json!({ "OkayOutput": { "type": "image", "data": "AAAA", "mimeType": "image/jpeg" } })
            )),
            "[image: image/jpeg, 3 bytes]"
        );
    }

    #[test]
    fn a_data_url_grok_wrote_into_the_output_becomes_its_image_line() {
        // The shape recording 12 shows: the text item's text, then the image as a data URL line.
        let data = "iVBORw0KGgoAAAANSUhEUgAAABgAAAAQCAIAAACDRijCAAAAH0lEQVR42mM4ISdHEMnZnCCIGEYNGjVo1KBRgygzCACqlZyQfrF9rQAAAABJRU5ErkJggg==";
        let output = output_of(mcp(
            json!({ "OkayOutput": format!("RAN\n[image attached: p.png, image/png, 88 bytes]\ndata:image/png;base64,{data}") }),
        ));
        assert_eq!(
            output,
            "RAN\n[image attached: p.png, image/png, 88 bytes]\n[image: image/png, 88 bytes]"
        );
        assert_eq!(
            output_of(mcp(
                json!({ "OkayOutput": [{ "type": "text", "text": format!("a data:image/gif;base64,{data} b") }] })
            )),
            "a [image: image/gif, 88 bytes] b"
        );
    }

    #[test]
    fn data_urls_of_every_shape_lose_their_base64() {
        for (text, expected) in [
            ("data:;base64,AAAA", "[image: unknown media type, 3 bytes]"),
            (
                "data:image/png;x=y;base64,AAAA\nnext",
                "[image: image/png, 3 bytes]\nnext",
            ),
            (
                "data:image/png;base64,A",
                "[image: image/png, size unknown]",
            ),
            ("data:image/png;base64,", "[image: image/png, 0 bytes]"),
            (
                "x data:a;base64,AAAA data:b;base64,AAAA",
                "x [image: a, 3 bytes] [image: b, 3 bytes]",
            ),
            ("data:text/plain,hello", "data:text/plain,hello"),
            ("metadata: none", "metadata: none"),
            ("data:", "data:"),
            ("ends data:image/png;base64", "ends data:image/png;base64"),
        ] {
            assert_eq!(
                output_of(mcp(json!({ "OkayOutput": text }))),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn an_image_block_the_driver_cannot_size_or_type_still_gives_one_line() {
        for (block, expected) in [
            (
                json!({ "type": "image", "data": "AAAA" }),
                "[image: unknown media type, 3 bytes]",
            ),
            (
                json!({ "type": "image", "mimeType": "image/png" }),
                "[image: image/png, size unknown]",
            ),
            (
                json!({ "type": "image", "mimeType": " ", "data": "!!" }),
                "[image: unknown media type, size unknown]",
            ),
            (
                json!({ "type": "image", "source": { "type": "url", "url": "u", "media_type": "image/png" } }),
                "[image: image/png, size unknown]",
            ),
            (
                json!({ "type": "image", "media_type": "image/webp", "data": "AA==" }),
                "[image: image/webp, 1 bytes]",
            ),
            (
                json!({ "type": "image", "source": "s", "data": "AAAA" }),
                "[image: unknown media type, 3 bytes]",
            ),
        ] {
            assert_eq!(output_of(json!([block])), expected);
        }
    }

    #[test]
    fn base64_is_sized_without_decoding() {
        assert_eq!(base64_decoded_len(""), Some(0));
        assert_eq!(base64_decoded_len("AAAA"), Some(3));
        assert_eq!(base64_decoded_len("AAA="), Some(2));
        assert_eq!(base64_decoded_len("AA=="), Some(1));
        assert_eq!(base64_decoded_len("AAA"), Some(2));
        assert_eq!(base64_decoded_len("AA\nAA"), Some(3));
        assert_eq!(base64_decoded_len("A"), None);
        assert_eq!(base64_decoded_len("A==="), None);
        assert_eq!(base64_decoded_len("AA=A"), None);
        assert_eq!(base64_decoded_len("AA-_"), None);
    }

    // ── the result line ───────────────────────────────────────────────────────

    fn result(fields: Value) -> String {
        let mut line = json!({ "type": "result" });
        for (key, value) in fields.as_object().expect("an object") {
            line[key] = value.clone();
        }
        line.to_string()
    }

    fn failure(line: &str) -> TurnFailure {
        match events(line).pop() {
            Some(Event::TurnFailed(failure)) => failure,
            other => panic!("expected turn-failed, got {other:?}"),
        }
    }

    fn failed_with(kind: FailureKind, message: &str) -> TurnFailure {
        TurnFailure {
            kind,
            message: message.to_string(),
        }
    }

    #[test]
    fn only_is_error_false_ends_the_turn() {
        assert_eq!(
            events(&result(
                json!({ "subtype": "success", "is_error": false, "result": "done" })
            )),
            vec![Event::TurnEnd("done".to_string())]
        );
        assert_eq!(
            events(&result(json!({ "is_error": false, "result": 3 }))),
            vec![Event::TurnEnd(String::new())]
        );
        for is_error in [json!(true), json!(null), json!("false"), json!(0)] {
            let line =
                result(json!({ "subtype": "success", "is_error": is_error, "result": "done" }));
            assert!(
                matches!(events(&line).as_slice(), [Event::TurnFailed(_)]),
                "{line}"
            );
        }
        assert!(matches!(
            events(&result(json!({ "subtype": "success", "result": "done" }))).as_slice(),
            [Event::TurnFailed(_)]
        ));
    }

    #[test]
    fn a_failure_kind_is_read_in_order() {
        let both = r#"Unauthorized (401) and rate_limit_exceeded"#;
        for (fields, kind) in [
            (
                json!({ "stop_reason": "cancelled", "subtype": "error_max_turns", "errors": [both] }),
                FailureKind::Canceled,
            ),
            (
                json!({ "subtype": "error_max_turns", "errors": [both] }),
                FailureKind::MaxTurns,
            ),
            (json!({ "errors": [both] }), FailureKind::Auth),
            (
                json!({ "errors": ["rate_limit_exceeded: mock 429"] }),
                FailureKind::Quota,
            ),
            (
                json!({ "errors": ["something else"] }),
                FailureKind::HarnessError,
            ),
            (json!({}), FailureKind::HarnessError),
        ] {
            let mut fields = fields;
            fields["is_error"] = json!(true);
            assert_eq!(failure(&result(fields.clone())).kind, kind, "{fields}");
        }
    }

    #[test]
    fn auth_and_quota_are_read_from_the_errors_and_the_result_text() {
        for text in [
            "Forbidden (403) from x",
            "Unauthorized (401) from x",
            "{\n  \"http_status\": 401\n}",
            "{\"http_status\":401}",
            "\"http_status\": 403",
            "\"http_status\":403",
        ] {
            assert_eq!(
                failure(&result(json!({ "is_error": true, "errors": [text] }))).kind,
                FailureKind::Auth,
                "{text}"
            );
            assert_eq!(
                failure(&result(json!({ "is_error": true, "result": text }))).kind,
                FailureKind::Auth,
                "{text}"
            );
        }
        for text in [
            "rate_limit_exceeded: slow down",
            "Too Many Requests (429)",
            "\"http_status\": 429",
            "\"http_status\":429",
        ] {
            assert_eq!(
                failure(&result(json!({ "is_error": true, "errors": ["x", text] }))).kind,
                FailureKind::Quota,
                "{text}"
            );
        }
        for text in [
            "used 429 tokens",
            "port 4290",
            "status 401",
            "http_status: 401",
        ] {
            assert_eq!(
                failure(&result(json!({ "is_error": true, "errors": [text] }))).kind,
                FailureKind::HarnessError,
                "{text}"
            );
        }
    }

    #[test]
    fn a_failure_message_is_its_errors_else_its_result_else_its_fields() {
        assert_eq!(
            failure(&result(
                json!({ "is_error": true, "errors": ["a", "", 7, "b"], "result": "r" })
            )),
            failed_with(FailureKind::HarnessError, "a; b")
        );
        assert_eq!(
            failure(&result(
                json!({ "is_error": true, "errors": [""], "result": "r" })
            )),
            failed_with(FailureKind::HarnessError, "r")
        );
        assert_eq!(
            failure(&result(
                json!({ "is_error": true, "subtype": "error_during_execution", "stop_reason": "refusal" })
            )),
            failed_with(
                FailureKind::HarnessError,
                "error_during_execution, stop_reason: refusal"
            )
        );
        assert_eq!(
            failure(&result(
                json!({ "is_error": true, "stop_reason": "cancelled" })
            )),
            failed_with(FailureKind::Canceled, "stop_reason: cancelled")
        );
        assert_eq!(
            failure(&result(
                json!({ "is_error": true, "subtype": "", "stop_reason": null })
            )),
            failed_with(
                FailureKind::HarnessError,
                "the harness reported a failure with no message"
            )
        );
        assert_eq!(
            failure(&result(
                json!({ "is_error": true, "subtype": "error_max_turns" })
            )),
            failed_with(FailureKind::MaxTurns, "error_max_turns")
        );
    }

    // ── tokens ────────────────────────────────────────────────────────────────

    fn spent(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> TokenUsage {
        TokenUsage {
            input: Some(input),
            output: Some(output),
            cache_read: Some(cache_read),
            cache_creation: Some(cache_creation),
            thinking: None,
        }
    }

    fn with_usage(usage: Value) -> String {
        result(json!({ "is_error": false, "result": "ok", "usage": usage }))
    }

    #[test]
    fn a_result_reports_its_four_counts_then_its_terminal_event() {
        assert_eq!(
            events(&with_usage(json!({
                "input_tokens": 100, "output_tokens": 15, "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 0, "server_tool_use": { "web_search_requests": 9 },
            }))),
            vec![
                Event::Usage(spent(100, 15, 20, 0)),
                Event::TurnEnd("ok".to_string())
            ]
        );
    }

    #[test]
    fn all_zero_or_nothing_readable_reports_no_usage() {
        for usage in [
            json!({ "input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 }),
            json!({ "input_tokens": 0 }),
            json!({}),
            json!(null),
            json!({ "input_tokens": -1, "output_tokens": "5", "cache_read_input_tokens": 1.5 }),
        ] {
            assert_eq!(
                events(&with_usage(usage.clone())),
                vec![Event::TurnEnd("ok".to_string())],
                "{usage}"
            );
        }
        assert_eq!(
            events(&result(json!({ "is_error": false, "result": "ok" }))),
            vec![Event::TurnEnd("ok".to_string())]
        );
    }

    #[test]
    fn a_partial_block_leaves_the_missing_counts_absent() {
        assert_eq!(
            token_usage(
                &json!({ "usage": { "input_tokens": 0, "output_tokens": 7, "cache_read_input_tokens": "x" } })
            ),
            Some(TokenUsage {
                input: Some(0),
                output: Some(7),
                cache_read: None,
                cache_creation: None,
                thinking: None,
            })
        );
    }

    #[test]
    fn no_line_but_result_reports_tokens() {
        let usage = json!({ "input_tokens": 5, "output_tokens": 5 });
        for line in [
            stream(json!({ "type": "message_delta", "usage": usage })),
            assistant(json!([{ "type": "text", "text": "x" }])),
            json!({ "type": "assistant", "message": { "content": [], "usage": usage } })
                .to_string(),
        ] {
            assert!(!events(&line)
                .iter()
                .any(|event| matches!(event, Event::Usage(_))));
        }
        // `modelUsage` and the cost are not read either.
        assert_eq!(
            events(&result(
                json!({ "is_error": false, "result": "ok", "total_cost_usd": 9.0, "modelUsage": { "m": { "inputTokens": 9 } } })
            )),
            vec![Event::TurnEnd("ok".to_string())]
        );
    }

    #[test]
    fn a_failed_turn_reports_its_counts_before_failing() {
        assert_eq!(
            events(&result(
                json!({ "is_error": true, "stop_reason": "cancelled", "errors": ["cancelled"], "usage": { "input_tokens": 100, "output_tokens": 15, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 0 } })
            )),
            vec![
                Event::Usage(spent(100, 15, 20, 0)),
                Event::TurnFailed(failed_with(FailureKind::Canceled, "cancelled"))
            ]
        );
    }

    #[test]
    fn a_second_result_in_one_process_reports_the_run_total() {
        let line = with_usage(json!({ "input_tokens": 2, "output_tokens": 10 }));
        let events = read_in_order(&[line.clone(), line]);
        let totals: Vec<&TokenUsage> = events
            .iter()
            .filter_map(|event| match event {
                Event::Usage(usage) => Some(usage),
                _ => None,
            })
            .collect();
        let total = |input, output| TokenUsage {
            input: Some(input),
            output: Some(output),
            cache_read: None,
            cache_creation: None,
            thinking: None,
        };
        assert_eq!(totals, [&total(2, 10), &total(4, 20)]);
    }

    #[test]
    fn running_usage_saturates_rather_than_wrapping() {
        let mut running = RunningUsage::new();
        let huge = TokenUsage {
            input: Some(u64::MAX),
            output: None,
            cache_read: None,
            cache_creation: None,
            thinking: None,
        };
        running.add_turn(&huge);
        assert_eq!(running.add_turn(&huge).input, Some(u64::MAX));
    }

    // ── a streamed use_tool call ──────────────────────────────────────────────

    fn message_start() -> String {
        stream(json!({ "type": "message_start", "message": {} }))
    }

    fn block_start(index: Value, block: Value) -> String {
        stream(json!({ "type": "content_block_start", "index": index, "content_block": block }))
    }

    fn use_tool_start(index: u64, id: &str) -> String {
        block_start(
            json!(index),
            json!({ "type": "tool_use", "id": id, "name": "use_tool", "input": {} }),
        )
    }

    fn fragment(index: u64, partial_json: &str) -> String {
        stream(
            json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": partial_json } }),
        )
    }

    fn started(id: &str, name: &str) -> Event {
        Event::ToolCallStarted(ToolCallStart {
            id: id.to_string(),
            name: name.to_string(),
        })
    }

    fn progress(id: &str, input_bytes: u64) -> Event {
        Event::ToolCallProgress(ToolCallProgress {
            id: id.to_string(),
            input_bytes,
        })
    }

    const CAPSULE_INPUT: &str =
        r#"{"tool_name":"capsule_srv__echo_tool","tool_input":{"text":"hi"}}"#;

    #[test]
    fn one_complete_fragment_naming_a_capsule_tool_starts_the_call() {
        assert_eq!(
            read_in_order(&[
                message_start(),
                use_tool_start(0, "c1"),
                fragment(0, CAPSULE_INPUT)
            ]),
            vec![
                started("c1", "echo_tool"),
                progress("c1", CAPSULE_INPUT.len() as u64)
            ]
        );
    }

    #[test]
    fn the_call_starts_on_the_fragment_that_completes_the_object_and_then_reports_progress() {
        let (a, rest) = CAPSULE_INPUT.split_at(10);
        let (b, c) = rest.split_at(20);
        let events = read_in_order(&[
            use_tool_start(0, "c1"),
            fragment(0, a),
            fragment(0, ""),
            fragment(0, b),
            fragment(0, c),
            fragment(0, " "),
            fragment(0, ""),
            fragment(0, "\n"),
        ]);
        let total = CAPSULE_INPUT.len() as u64;
        assert_eq!(
            events,
            vec![
                started("c1", "echo_tool"),
                progress("c1", total),
                progress("c1", total + 1),
                progress("c1", total + 2),
            ]
        );
    }

    #[test]
    fn a_streamed_call_that_is_not_the_capsules_reports_nothing() {
        for input in [
            r#"{"tool_name":"operator_srv__echo_tool","tool_input":{}}"#,
            r#"{"tool_name":"capsule_srv__echo_tool","tool_input_file":"/tmp/a.json"}"#,
            r#"{"file":"/tmp/a.json"}"#,
            r#"{"tool_name":"capsule_srv__"}"#,
            r#"["capsule_srv__echo_tool"]"#,
            "\"capsule_srv__echo_tool\"",
        ] {
            let events = read_in_order(&[
                use_tool_start(0, "c1"),
                fragment(0, input),
                fragment(0, "  "),
            ]);
            assert_eq!(events, Vec::new(), "{input}");
        }
        // With no bridge, no streamed call is a capsule call either.
        let lines = [use_tool_start(0, "c1"), fragment(0, CAPSULE_INPUT)];
        assert_eq!(
            parse_lines(&lines, &ParseContext::none(), &mut ParseState::new()),
            Vec::new()
        );
    }

    #[test]
    fn a_tool_use_block_that_is_not_use_tool_or_has_no_id_opens_nothing() {
        for block in [
            json!({ "type": "tool_use", "id": "c1", "name": "search_tool", "input": {} }),
            json!({ "type": "tool_use", "id": "", "name": "use_tool", "input": {} }),
            json!({ "type": "tool_use", "name": "use_tool", "input": {} }),
            json!({ "type": "tool_use", "id": "c1", "name": "capsule_srv__echo_tool" }),
        ] {
            let events = read_in_order(&[
                block_start(json!(0), block.clone()),
                fragment(0, CAPSULE_INPUT),
            ]);
            assert_eq!(events, Vec::new(), "{block}");
        }
    }

    #[test]
    fn input_that_passes_the_cap_before_naming_a_tool_is_abandoned() {
        let padding = " ".repeat(STREAMED_INPUT_CAP - 1);
        let events = read_in_order(&[
            use_tool_start(0, "c1"),
            fragment(0, &padding),
            fragment(0, "{\"tool_name\":\"capsule_srv__echo_tool\"}"),
            fragment(0, " "),
        ]);
        assert_eq!(events, Vec::new());

        // Within the cap it still starts.
        let padding = " ".repeat(STREAMED_INPUT_CAP - CAPSULE_INPUT.len());
        let events = read_in_order(&[
            use_tool_start(0, "c1"),
            fragment(0, &padding),
            fragment(0, CAPSULE_INPUT),
        ]);
        assert_eq!(
            events,
            vec![
                started("c1", "echo_tool"),
                progress("c1", STREAMED_INPUT_CAP as u64)
            ]
        );
    }

    #[test]
    fn message_start_and_a_new_block_at_the_index_forget_the_call() {
        let events = read_in_order(&[
            use_tool_start(0, "c1"),
            message_start(),
            fragment(0, CAPSULE_INPUT),
            use_tool_start(0, "c2"),
            block_start(json!(0), json!({ "type": "text", "text": "" })),
            fragment(0, CAPSULE_INPUT),
            use_tool_start(1, "c3"),
            fragment(1, CAPSULE_INPUT),
        ]);
        assert_eq!(
            events,
            vec![
                started("c3", "echo_tool"),
                progress("c3", CAPSULE_INPUT.len() as u64)
            ]
        );
    }

    #[test]
    fn two_calls_in_one_message_keep_separate_buffers_and_totals() {
        let other = r#"{"tool_name":"capsule_srv__write-file","tool_input":{}}"#;
        let (a1, a2) = CAPSULE_INPUT.split_at(5);
        let events = read_in_order(&[
            use_tool_start(0, "c1"),
            use_tool_start(1, "c2"),
            fragment(0, a1),
            fragment(1, other),
            fragment(0, a2),
            fragment(1, "\n"),
        ]);
        assert_eq!(
            events,
            vec![
                started("c2", "write-file"),
                progress("c2", other.len() as u64),
                started("c1", "echo_tool"),
                progress("c1", CAPSULE_INPUT.len() as u64),
                progress("c2", other.len() as u64 + 1),
            ]
        );
    }

    #[test]
    fn a_started_call_and_its_complete_call_carry_the_same_id_and_name() {
        let events = read_in_order(&[
            use_tool_start(0, "c1"),
            fragment(0, CAPSULE_INPUT),
            assistant(json!([use_tool(
                "c1",
                serde_json::from_str(CAPSULE_INPUT).expect("JSON")
            )])),
        ]);
        assert_eq!(
            events,
            vec![
                started("c1", "echo_tool"),
                progress("c1", CAPSULE_INPUT.len() as u64),
                call("c1", "echo_tool", r#"{"text":"hi"}"#),
            ]
        );
    }

    #[test]
    fn no_event_but_a_tool_call_carries_any_of_the_input() {
        const MARKER: &str = "QXZJ-WKVR";
        let input = format!(
            r#"{{"tool_name":"capsule_srv__echo_tool","tool_input":{{"text":"{MARKER}"}}}}"#
        );
        let (head, tail) = input.split_at(input.find(MARKER).expect("marker") + 4);
        let foreign =
            format!(r#"{{"tool_name":"operator_srv__x","tool_input":{{"text":"{MARKER}"}}}}"#);
        let lines = [
            message_start(),
            use_tool_start(0, "c1"),
            fragment(0, head),
            fragment(0, tail),
            use_tool_start(1, "c2"),
            fragment(1, &foreign),
            assistant(json!([
                use_tool("c1", serde_json::from_str(&input).expect("JSON")),
                use_tool("c2", serde_json::from_str(&foreign).expect("JSON")),
                use_tool(
                    "c3",
                    json!({ "tool_name": "capsule_srv__echo_tool", "tool_input_file": MARKER })
                ),
            ])),
            user(json!([result_block("c2", json!(MARKER), json!(false))])),
            user(json!([result_block("c3", json!(MARKER), json!(false))])),
        ];
        let events = read_in_order(&lines);
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::ToolCallStarted(_))));
        for event in &events {
            if !matches!(event, Event::ToolCall(_)) {
                assert!(
                    !format!("{event:?}").contains(MARKER),
                    "{event:?} carries the input"
                );
            }
        }
    }

    #[test]
    fn the_events_of_a_streamed_call_do_not_depend_on_how_its_lines_are_batched() {
        let (a, b) = CAPSULE_INPUT.split_at(17);
        let lines = vec![
            message_start(),
            use_tool_start(0, "c1"),
            fragment(0, a),
            fragment(0, b),
            fragment(0, " "),
            use_tool_start(1, "c2"),
            fragment(1, r#"{"file":"x"}"#),
            assistant(json!([
                use_tool("c1", serde_json::from_str(CAPSULE_INPUT).expect("JSON")),
                use_tool("c2", json!({ "file": "x" })),
            ])),
            user(json!([
                result_block("c1", mcp(json!({ "OkayOutput": "echoed" })), json!(false)),
                result_block("c2", json!("x"), json!(false)),
            ])),
            result(json!({ "is_error": false, "result": "ok", "usage": { "input_tokens": 1 } })),
        ];
        let whole = read_in_order(&lines);
        assert_eq!(whole.len(), 9, "{whole:?}");
        for split in 0..=lines.len() {
            let (head, tail) = lines.split_at(split);
            let mut state = ParseState::new();
            let mut split_in_two = parse_lines(head, &context(), &mut state);
            split_in_two.extend(parse_lines(tail, &context(), &mut state));
            assert_eq!(split_in_two, whole, "split at line {split}");
        }
    }

    #[test]
    fn a_line_of_any_shape_never_panics() {
        let lines: Vec<String> = [
            block_start(json!(u64::MAX), json!({ "type": "tool_use", "id": "t", "name": "use_tool" })),
            stream(json!({ "type": "content_block_delta", "index": u64::MAX, "delta": { "type": "input_json_delta", "partial_json": CAPSULE_INPUT } })),
            block_start(json!(-1), json!({ "type": "tool_use", "id": "t", "name": "use_tool" })),
            block_start(json!(1e300), json!({ "type": "tool_use", "id": "t", "name": "use_tool" })),
            block_start(json!(0), json!("tool_use")),
            block_start(json!(0), json!(null)),
            block_start(json!(0), json!({ "type": "tool_use", "id": ["t"], "name": 7 })),
            stream(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": null } })),
            stream(json!({ "type": "content_block_delta", "delta": "input_json_delta" })),
            r#"{"type":"stream_event","event":"content_block_start"}"#.to_string(),
            r#"{"type":"stream_event"}"#.to_string(),
            r#"{"type":"assistant"}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use"},7,null,{"type":"tool_use","name":"use_tool","input":{"tool_name":null}}]}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":{"type":"MCP"}},{"type":"tool_result","tool_use_id":5}]}}"#.to_string(),
            r#"{"type":"result","usage":7,"errors":"x"}"#.to_string(),
            r#"{"type":"result","is_error":true,"errors":[null,{}],"usage":{"input_tokens":1e30}}"#.to_string(),
            r#"{"type":"system","subtype":"init","session_id":"s","mcp_servers":[null,{"name":7}]}"#.to_string(),
            "\u{0}\u{feff}{".to_string(),
        ]
        .to_vec();
        let events = read_in_order(&lines);
        assert_eq!(
            events[..2],
            [
                started("t", "echo_tool"),
                progress("t", CAPSULE_INPUT.len() as u64)
            ]
        );
    }

    // ── classify-exit ─────────────────────────────────────────────────────────

    fn exit() -> ExitStatus {
        ExitStatus {
            code: Some(1),
            signal: None,
            stderr_tail: String::new(),
            interrupted: false,
            saw_terminal: false,
        }
    }

    fn classified(exit: ExitStatus) -> TurnFailure {
        match classify_exit(exit) {
            Event::TurnFailed(failure) => failure,
            other => panic!("classify-exit answered {other:?}"),
        }
    }

    #[test]
    fn an_interrupted_run_with_no_terminal_event_is_canceled_whatever_the_exit_says() {
        for (code, signal) in [
            (Some(0), None),
            (Some(130), None),
            (None, Some(9)),
            (None, None),
        ] {
            assert_eq!(
                classified(ExitStatus {
                    code,
                    signal,
                    stderr_tail: "noise".to_string(),
                    interrupted: true,
                    ..exit()
                }),
                failed_with(
                    FailureKind::Canceled,
                    "grok was interrupted before it reported a result"
                )
            );
        }
    }

    #[test]
    fn a_clean_exit_with_no_result_is_a_harness_error() {
        assert_eq!(
            classified(ExitStatus {
                code: Some(0),
                stderr_tail: "tail".to_string(),
                ..exit()
            }),
            failed_with(FailureKind::HarnessError, "grok exited without a result")
        );
    }

    #[test]
    fn any_other_exit_says_what_happened_and_what_grok_printed() {
        assert_eq!(
            classified(exit()),
            failed_with(FailureKind::HarnessError, "grok exited with code 1")
        );
        assert_eq!(
            classified(ExitStatus {
                stderr_tail: "\n  Error: boom\n".to_string(),
                ..exit()
            }),
            failed_with(
                FailureKind::HarnessError,
                "grok exited with code 1: Error: boom"
            )
        );
        assert_eq!(
            classified(ExitStatus {
                code: None,
                signal: Some(9),
                ..exit()
            }),
            failed_with(FailureKind::HarnessError, "grok was killed by signal 9")
        );
        assert_eq!(
            classified(ExitStatus {
                code: None,
                stderr_tail: "x".to_string(),
                ..exit()
            }),
            failed_with(FailureKind::HarnessError, "grok exited without a status: x")
        );
    }

    #[test]
    fn classify_exit_never_reports_a_turn_that_ended() {
        for interrupted in [false, true] {
            for saw_terminal in [false, true] {
                for code in [None, Some(0), Some(1), Some(130)] {
                    let event = classify_exit(ExitStatus {
                        code,
                        interrupted,
                        saw_terminal,
                        ..exit()
                    });
                    assert!(matches!(event, Event::TurnFailed(_)), "{event:?}");
                }
            }
        }
    }
}
