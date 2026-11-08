//! One call, from its input to its reply: find the session's browser by its process name, or
//! start it; take the browser's call lock; open this call's DevTools connection; run the
//! action; save what the next call needs.
//!
//! Calls sent together each run this in their own instance, with nothing shared but the work
//! folder and murmur's session processes. Each call looks at the latest `browser` process and
//! the `state.json` recording it until it can attach to a running browser or start one (see
//! [`next_step`]); the call that starts it holds its `call.lock` from before Chrome starts, so
//! its action runs first, and the waiting calls take the lock in turn.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::json;

use crate::{
    cdp::{Cdp, OUT_OF_TIME},
    discovery,
    envelope::{self, Reply},
    host::{Clock, ProcError, ProcState, Processes},
    launch::{self, PROCESS_NAME},
    page::{Outcome, Page},
    request::{self, Action, Request},
    state::{self, BrowserInfo, CallLock, DocumentState, PageLog, State, StateDir},
    ws::{Random, WebSocket},
};

/// The longest a call runs, by its own clock.
pub const CALL_LIMIT_MS: u64 = 90_000;
/// `browser.log` stops growing here, with one closing line.
pub const LOG_MAX_BYTES: u64 = 1024 * 1024;
const LOG_CUT_LINE: &str = "\n… browser.log cut at 1 MiB; later output is not kept\n";
/// How long each read of Chrome's output and of the DevTools socket may wait.
const POLL_MS: u64 = 100;
/// How long the WebSocket upgrade may take.
const HANDSHAKE_MS: u64 = 10_000;
/// How long a failed call waits for the runtime to report a browser exited once its DevTools
/// connection closed.
const EXIT_WAIT_MS: u64 = 2_000;
/// How long the last lines of an exited browser may take through the runtime's reader.
const EXIT_SETTLE_MS: u64 = 300;
/// How much of `browser.log` a failed call reads when the output it drains holds no `FATAL` line.
const LOG_TAIL_BYTES: u64 = 64 * 1024;
/// How long a start waits, once Chrome has exited, for its last output to come through.
const EXIT_DRAIN_MS: u64 = 300;
/// How long a running `browser` that no `state.json` records is left to the call starting it
/// before a waiting call stops it. A start saves its `state.json` within
/// [`launch::START_LIMIT_MS`] or stops its process (see [`start`]).
const ORPHAN_MS: u64 = launch::START_LIMIT_MS + 5_000;
/// How long an exited `browser` that no `state.json` records is left alone before a waiting call
/// starts its own: it was a start attempt, and the call that made it may be about to retry
/// without the sandbox. That call's gap between the exit and the retry is up to [`POLL_MS`] to
/// see the exit, then [`EXIT_DRAIN_MS`] and the reads of the last output.
const RESTART_GAP_MS: u64 = 1_000;
// The gap holds a starter's exit-to-retry time twice over, and a start runs out before a waiting
// call takes its browser for an orphan.
const _: () = assert!(RESTART_GAP_MS >= 2 * (POLL_MS + EXIT_DRAIN_MS));
const _: () = assert!(ORPHAN_MS > launch::START_LIMIT_MS);

/// What a call runs with.
pub struct Ctx<'a> {
    pub procs: &'a mut dyn Processes,
    pub clock: &'a dyn Clock,
    pub random: Random<'a>,
    /// The work folder, as the tool opens it: `.` in the component.
    pub work: PathBuf,
    /// The session's runtime directory, `murmur-runtime` in the component; `None` when it is not
    /// preopened.
    pub runtime: Option<PathBuf>,
}

/// Run one call. `data` is the call's input and `config` the entry's `config:` JSON.
pub fn call(data: &str, config: Option<&str>, ctx: &mut Ctx) -> Reply {
    match run(data, config, ctx) {
        Ok(reply) => reply,
        Err(line) => envelope::error(&line),
    }
}

fn run(data: &str, config: Option<&str>, ctx: &mut Ctx) -> Result<Reply, String> {
    let params = request::params_from_data(data)?;
    let req = request::parse_request(&params)?;
    discovery::check_config(config)?;
    if let (Action::Open, Some(url)) = (req.action, req.url.as_deref()) {
        // An absolute path is checked once Chrome has shown where the work folder is.
        if !Path::new(url.trim()).is_absolute() {
            request::open_target(url, &ctx.work, None)?;
        }
    }
    let deadline = ctx.clock.now_ms() + CALL_LIMIT_MS;

    // Close needs no grant: with no browser running it starts nothing.
    let closing = req.action == Action::Close;
    let seen = look(ctx);
    if closing && !matches!(seen, Seen::Running { .. }) {
        return Ok(closed_reply());
    }

    let Some(runtime) = ctx.runtime.clone() else {
        return Err(discovery::SCOPE_LINE.into());
    };
    let granted = discovery::granted_names(&runtime);
    if granted.is_empty() {
        return Err(discovery::NO_GRANT_LINE.into());
    }

    match find_or_start(ctx, &granted, closing, seen, deadline)? {
        Some(attached) => Ok(finish(ctx, attached, &req)),
        None => Ok(closed_reply()),
    }
}

fn closed_reply() -> Reply {
    envelope::passed("close", "browser closed", None)
}

/// What a `state.json` under `.tool-browser/` says of a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// None records it.
    Missing,
    /// One records it, not closed.
    Open,
    /// One records it, closed by `close`.
    Closed,
}

/// One look at the session's latest `browser` process and the `state.json` recording it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// No `browser` process was ever started in the session.
    Nothing,
    Running {
        handle: String,
        record: Record,
    },
    Exited {
        handle: String,
        code: i32,
        record: Record,
    },
}

/// What a call does after a look.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// Start a browser; `note` goes under the banner.
    Start { note: Option<String> },
    /// Take the lock of this running browser and connect to it.
    Attach(String),
    /// Sleep [`POLL_MS`] and look again: another call is starting the browser.
    Wait,
    /// Stop this running browser, which no call is starting any more, and answer so.
    StopOrphan(String),
}

/// The next step after seeing `seen`, unchanged for `held_for_ms` by this call's clock.
fn next_step(seen: &Seen, held_for_ms: u64) -> Next {
    match seen {
        Seen::Nothing => Next::Start { note: None },
        Seen::Running {
            handle,
            record: Record::Missing,
        } if held_for_ms >= ORPHAN_MS => Next::StopOrphan(handle.clone()),
        Seen::Running {
            record: Record::Missing,
            ..
        } => Next::Wait,
        Seen::Running { handle, .. } => Next::Attach(handle.clone()),
        Seen::Exited {
            record: Record::Missing,
            ..
        } if held_for_ms < RESTART_GAP_MS => Next::Wait,
        // A start that failed left no state.json, and `close` marks its state closed: only a
        // browser that was running gets a note.
        Seen::Exited {
            record: Record::Missing | Record::Closed,
            ..
        } => Next::Start { note: None },
        Seen::Exited {
            code,
            record: Record::Open,
            ..
        } => Next::Start {
            note: Some(launch::replaced_note(*code)),
        },
    }
}

/// How long this call has seen one handle in one condition, by its clock.
#[derive(Default)]
struct Held(Option<(Seen, u64)>);

impl Held {
    /// How long `seen` has held at `now`: 0 the first time, and again on any change of handle or
    /// condition.
    fn update(&mut self, seen: &Seen, now: u64) -> u64 {
        match &self.0 {
            Some((s, since)) if s == seen => now.saturating_sub(*since),
            _ => {
                self.0 = Some((seen.clone(), now));
                0
            }
        }
    }
}

/// Look at the latest `browser` process, then for the `state.json` recording it.
fn look(ctx: &mut Ctx) -> Seen {
    let Some(handle) = ctx.procs.find(PROCESS_NAME) else {
        return Seen::Nothing;
    };
    let status = ctx.procs.status(&handle);
    let record = match state::find_by_handle(&ctx.work, &handle) {
        None => Record::Missing,
        Some((_, s)) if s.closed => Record::Closed,
        Some(_) => Record::Open,
    };
    match status {
        Ok(ProcState::Running) => Seen::Running { handle, record },
        Ok(ProcState::Exited(code)) => Seen::Exited {
            handle,
            code,
            record,
        },
        Err(_) => Seen::Nothing,
    }
}

/// Look and act on [`next_step`] until this call holds a browser, starting from `seen`. `None`
/// when `closing` and no browser runs.
fn find_or_start<'a>(
    ctx: &mut Ctx<'a>,
    granted: &[&'static str],
    closing: bool,
    mut seen: Seen,
    deadline: u64,
) -> Result<Option<Attached<'a>>, String> {
    let mut held = Held::default();
    loop {
        if closing && !matches!(seen, Seen::Running { .. }) {
            return Ok(None);
        }
        let now = ctx.clock.now_ms();
        if now >= deadline {
            return Err(OUT_OF_TIME.into());
        }
        match next_step(&seen, held.update(&seen, now)) {
            Next::Wait => ctx.clock.sleep_ms(POLL_MS.min(deadline - now)),
            Next::StopOrphan(handle) => {
                let _ = ctx.procs.stop(&handle);
                return Err(format!(
                    "tool-browser: the running browser ({handle}) has no state.json under {}/, so it was stopped; the next call starts a new one",
                    state::STATE_ROOT
                ));
            }
            Next::Attach(handle) => {
                if let Some(attached) = join(ctx, &handle, deadline)? {
                    return Ok(Some(attached));
                }
            }
            Next::Start { note } => match start(ctx, granted, note, deadline)? {
                Started::Fresh(attached) => return Ok(Some(*attached)),
                // Another call's start is under way: wait for its state.json.
                Started::Joined => {}
            },
        }
        seen = look(ctx);
    }
}

/// A browser this call holds the lock of, with its open connection.
struct Attached<'a> {
    dir: StateDir,
    state: State,
    /// Taken by the action that uses it.
    cdp: Option<Cdp<'a>>,
    _lock: CallLock,
    /// This call started the browser.
    fresh: bool,
}

enum Started<'a> {
    Fresh(Box<Attached<'a>>),
    /// A `browser` process was already running under the name; nothing new ran.
    Joined,
}

/// Take the call lock of the running browser `handle` and connect to it. `None` when no
/// `state.json` records it, or when it stopped running while this call waited for the lock.
fn join<'a>(
    ctx: &mut Ctx<'a>,
    handle: &str,
    deadline: u64,
) -> Result<Option<Attached<'a>>, String> {
    let Some((dir, _)) = state::find_by_handle(&ctx.work, handle) else {
        return Ok(None);
    };
    let wait = state::LOCK_WAIT_MS.min(deadline.saturating_sub(ctx.clock.now_ms()));
    let lock = CallLock::acquire(&dir, ctx.clock, wait)?;
    // The call that held the lock may have closed the browser.
    if ctx.procs.status(handle) != Ok(ProcState::Running) {
        return Ok(None);
    }
    let state = dir
        .load()
        .ok_or_else(|| format!("tool-browser: cannot read {}/state.json", dir.rel))?;
    let ws = open_ws(
        ctx,
        &state.handle,
        state.port,
        &state.browser_ws_path,
        deadline,
    )?;
    let log = state.log.clone();
    Ok(Some(Attached {
        cdp: Some(Cdp::new(ws, ctx.clock, deadline, log)),
        dir,
        state,
        _lock: lock,
        fresh: false,
    }))
}

/// Connect to the browser's DevTools WebSocket, the upgrade cut to `until`.
fn open_ws<'a>(
    ctx: &mut Ctx<'a>,
    handle: &str,
    port: u16,
    path: &str,
    until: u64,
) -> Result<WebSocket<'a>, String> {
    let left = until.saturating_sub(ctx.clock.now_ms());
    if left == 0 {
        return Err(OUT_OF_TIME.into());
    }
    let conn = ctx.procs.connect(handle, port).map_err(|e| match e {
        ProcError::Refused { rule, reason } => {
            format!("tool-browser: murmur refused the DevTools connection: {rule}: {reason}")
        }
        other => format!("tool-browser: cannot reach the browser's DevTools port {port}: {other}"),
    })?;
    WebSocket::handshake(conn, port, path, ctx.random, HANDSHAKE_MS.min(left) as u32)
        .map_err(|e| format!("tool-browser: the browser's DevTools connection failed: {e}"))
}

/// Append what Chrome printed to `browser.log`, which stops at [`LOG_MAX_BYTES`].
fn append_log(dir: &StateDir, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let path = dir.file("browser.log");
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size >= LOG_MAX_BYTES {
        return;
    }
    let room = (LOG_MAX_BYTES - size) as usize;
    let mut out = bytes[..bytes.len().min(room)].to_vec();
    if bytes.len() > room {
        out.extend_from_slice(LOG_CUT_LINE.as_bytes());
    }
    use std::io::Write as _;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(&out);
    }
}

/// Read everything Chrome printed since the last read into `browser.log`; the stderr text.
fn drain_output(procs: &mut dyn Processes, handle: &str, dir: &StateDir) -> String {
    let mut stderr = Vec::new();
    for _ in 0..64 {
        match procs.read(handle, 65_536) {
            Ok(out) if out.stdout.is_empty() && out.stderr.is_empty() => break,
            Ok(out) => {
                append_log(dir, &out.stdout);
                append_log(dir, &out.stderr);
                stderr.extend_from_slice(&out.stderr);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&stderr).into_owned()
}

/// The last [`LOG_TAIL_BYTES`] of `browser.log`, from the browser's DevTools line on when the tail
/// holds it: the sandbox retry starts Chrome again in the same folder, so what precedes that line
/// can be an earlier attempt's output, its FATAL line included.
fn log_tail(dir: &StateDir) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut f) = fs::File::open(dir.file("browser.log")) else {
        return String::new();
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    if f.seek(SeekFrom::Start(size.saturating_sub(LOG_TAIL_BYTES)))
        .and_then(|_| f.read_to_end(&mut bytes))
        .is_err()
    {
        return String::new();
    }
    let tail = String::from_utf8_lossy(&bytes);
    let from = tail.rfind(launch::DEVTOOLS_LINE).unwrap_or(0);
    tail[from..].to_string()
}

/// A call that failed on an attached browser.
struct Failure<'s> {
    /// The error as the failing step gave it.
    line: String,
    /// The DevTools method in flight when the connection closed, else the action's name; `None`
    /// while the browser is still being started.
    during: Option<&'s str>,
    /// The DevTools connection was seen closed.
    closed: bool,
}

/// The line a failed call answers with. When the browser `handle` has exited, it names Chrome's
/// exit code, what was in flight and Chrome's last `FATAL` line ([`launch::died_line`], or
/// [`launch::exit_line`] while starting); while the browser runs, or when its status cannot be
/// read, the failure's own line stands. A closed connection usually means Chrome has exited, but
/// the runtime can report it a moment later, so status is then read every [`POLL_MS`] for up to
/// [`EXIT_WAIT_MS`], within `deadline`; otherwise it is read once.
fn explain_exit(
    ctx: &mut Ctx,
    handle: &str,
    dir: &StateDir,
    name: &str,
    deadline: u64,
    failure: Failure,
) -> String {
    let until = (ctx.clock.now_ms() + EXIT_WAIT_MS).min(deadline);
    let code = loop {
        match ctx.procs.status(handle) {
            Ok(ProcState::Exited(code)) => break code,
            Ok(ProcState::Running) if failure.closed && ctx.clock.now_ms() < until => {
                ctx.clock.sleep_ms(POLL_MS);
            }
            _ => return failure.line,
        }
    };
    ctx.clock.sleep_ms(EXIT_SETTLE_MS);
    let drained = drain_output(ctx.procs, handle, dir);
    // Output an earlier read already moved to browser.log, or a FATAL line the drain split.
    let stderr = if drained.contains(":FATAL:") {
        drained
    } else {
        let tail = log_tail(dir);
        if tail.contains(":FATAL:") || drained.trim().is_empty() {
            tail
        } else {
            drained
        }
    };
    match failure.during {
        Some(during) => launch::died_line(name, code, during, &stderr),
        None => launch::exit_line(name, code, &stderr),
    }
}

enum DevTools {
    Ready(u16, String),
    Exited(i32, String),
    Silent,
}

/// Wait for Chrome's `DevTools listening on …` line.
fn wait_devtools(ctx: &mut Ctx, handle: &str, dir: &StateDir, until: u64) -> DevTools {
    let mut stderr = String::new();
    loop {
        stderr.push_str(&drain_output(ctx.procs, handle, dir));
        if let Some((port, path)) = launch::parse_devtools_line(&stderr) {
            return DevTools::Ready(port, path);
        }
        match ctx.procs.status(handle) {
            Ok(ProcState::Exited(code)) => {
                // The last lines can still be on their way through the runtime's reader.
                ctx.clock.sleep_ms(EXIT_DRAIN_MS);
                stderr.push_str(&drain_output(ctx.procs, handle, dir));
                return DevTools::Exited(code, stderr);
            }
            Err(_) => return DevTools::Exited(-1, stderr),
            Ok(ProcState::Running) => {}
        }
        if ctx.clock.now_ms() >= until {
            return DevTools::Silent;
        }
        ctx.clock.sleep_ms(POLL_MS);
    }
}

/// Remove a folder this call made and does not keep.
fn discard(dir: &StateDir) {
    let _ = fs::remove_dir_all(&dir.root);
    if let Some(parent) = dir.root.parent() {
        // Only when empty: another browser's folder stays.
        let _ = fs::remove_dir(parent);
    }
}

/// Start Chrome under the process name `browser`, holding the new folder's call lock from before
/// Chrome starts. Every step until `state.json` is saved is cut to [`launch::START_LIMIT_MS`]; a
/// start that runs out stops its process. `Joined` when the name already runs a process, which
/// leaves no folder.
fn start<'a>(
    ctx: &mut Ctx<'a>,
    granted: &[&'static str],
    note: Option<String>,
    deadline: u64,
) -> Result<Started<'a>, String> {
    let start_deadline = (ctx.clock.now_ms() + launch::START_LIMIT_MS).min(deadline);
    let id = crate::util::hex(&(ctx.random)(8));
    let dir = StateDir::new(&ctx.work, &id);
    for sub in ["profile", "downloads"] {
        fs::create_dir_all(dir.file(sub))
            .map_err(|e| format!("tool-browser: cannot create {}/{sub}: {e}", dir.rel))?;
    }
    fs::write(dir.file(launch::BLANK_PAGE), launch::BLANK_HTML).map_err(|e| {
        format!(
            "tool-browser: cannot write {}/{}: {e}",
            dir.rel,
            launch::BLANK_PAGE
        )
    })?;
    let lock = CallLock::acquire(&dir, ctx.clock, 0)?;

    let mut sandbox_off = false;
    let mut chosen: Option<&'static str> = None;
    let (name, handle, port, path) = loop {
        let names: Vec<&'static str> = chosen.map_or_else(|| granted.to_vec(), |n| vec![n]);
        let args = launch::browser_args(&dir.rel, sandbox_off);
        let mut started = None;
        for name in names {
            match ctx.procs.start(PROCESS_NAME, name, &args, &[]) {
                Err(ProcError::Failed(text)) if discovery::is_not_found(&text) => continue,
                Err(ProcError::Refused { rule, reason }) => {
                    drop(lock);
                    discard(&dir);
                    return Err(launch::refused_line(name, &rule, &reason));
                }
                Err(ProcError::Failed(text)) => {
                    drop(lock);
                    discard(&dir);
                    return Err(launch::spawn_failed_line(name, &text));
                }
                Err(other) => {
                    drop(lock);
                    discard(&dir);
                    return Err(launch::spawn_failed_line(name, &other.to_string()));
                }
                Ok(s) if s.already_running => {
                    drop(lock);
                    discard(&dir);
                    return Ok(Started::Joined);
                }
                Ok(s) => {
                    started = Some((name, s.handle));
                    break;
                }
            }
        }
        let Some((name, handle)) = started else {
            drop(lock);
            discard(&dir);
            return Err(discovery::not_installed_line(granted));
        };
        let until = (ctx.clock.now_ms() + launch::DEVTOOLS_WAIT_MS).min(start_deadline);
        match wait_devtools(ctx, &handle, &dir, until) {
            DevTools::Ready(port, path) => break (name, handle, port, path),
            DevTools::Exited(_, stderr) if !sandbox_off && launch::sandbox_failed(&stderr) => {
                sandbox_off = true;
                chosen = Some(name);
                let _ = fs::remove_dir_all(dir.file("profile"));
                let _ = fs::create_dir_all(dir.file("profile"));
            }
            DevTools::Exited(code, stderr) => return Err(launch::exit_line(name, code, &stderr)),
            DevTools::Silent => {
                let _ = ctx.procs.stop(&handle);
                drain_output(ctx.procs, &handle, &dir);
                return Err(launch::no_port_line(name, &dir.rel));
            }
        }
    };

    let fail = |ctx: &mut Ctx, line: String, closed: bool| -> String {
        let failure = Failure {
            line,
            during: None,
            closed,
        };
        let line = explain_exit(ctx, &handle, &dir, name, deadline, failure);
        let _ = ctx.procs.stop(&handle);
        if ctx.clock.now_ms() < start_deadline {
            line
        } else if start_deadline >= deadline {
            OUT_OF_TIME.into()
        } else {
            launch::start_timeout_line(name, &dir.rel)
        }
    };
    let ws = match open_ws(ctx, &handle, port, &path, start_deadline) {
        Ok(ws) => ws,
        Err(e) => return Err(fail(ctx, e, false)),
    };
    let mut cdp = Cdp::new(ws, ctx.clock, start_deadline, PageLog::default());
    let version = cdp
        .browser("Browser.getVersion", json!({}))
        .ok()
        .and_then(|v| v["product"].as_str().map(launch::version_from_product))
        .unwrap_or_else(|| "(unknown version)".into());
    let targets = match cdp.browser("Target.getTargets", json!({})) {
        Ok(t) => t,
        Err(e) => {
            let closed = cdp.ev.closed.is_some();
            return Err(fail(ctx, format!("tool-browser: {e}"), closed));
        }
    };
    let Some((target_id, workdir_url)) = launch::initial_page(&targets, &dir.rel) else {
        let closed = cdp.ev.closed.is_some();
        return Err(fail(
            ctx,
            format!(
                "tool-browser: {name} did not open {}/{} as a file in the work folder, so the tool cannot tell where the work folder is",
                dir.rel,
                launch::BLANK_PAGE
            ),
            closed,
        ));
    };
    if ctx.clock.now_ms() >= start_deadline {
        return Err(fail(ctx, OUT_OF_TIME.into(), false));
    }
    let state = State {
        handle,
        browser: BrowserInfo {
            name: name.to_string(),
            version,
            sandbox: !sandbox_off,
        },
        port,
        browser_ws_path: path,
        workdir_url,
        target_id,
        started_at: ctx.clock.unix_secs(),
        banner_shown: false,
        screenshots: 0,
        closed: false,
        viewport_override: false,
        replaced_note: note,
        dialogs: Vec::new(),
        document: DocumentState::default(),
        log: PageLog::default(),
    };
    if let Err(e) = dir.save(&state) {
        let _ = ctx.procs.stop(&state.handle);
        return Err(e);
    }
    cdp.set_deadline(deadline);
    Ok(Started::Fresh(Box::new(Attached {
        dir,
        state,
        cdp: Some(cdp),
        _lock: lock,
        fresh: true,
    })))
}

/// Attach to the page target, creating one when it is gone; true when it was created.
fn attach(cdp: &mut Cdp, state: &mut State) -> Result<bool, String> {
    let mut created = false;
    let attached = match cdp.browser(
        "Target.attachToTarget",
        json!({"targetId": state.target_id, "flatten": true}),
    ) {
        Ok(a) => a,
        Err(_) => {
            let target = cdp.browser("Target.createTarget", json!({"url": "about:blank"}))?;
            state.target_id = target["targetId"]
                .as_str()
                .ok_or("Target.createTarget returned no targetId")?
                .to_string();
            created = true;
            cdp.browser(
                "Target.attachToTarget",
                json!({"targetId": state.target_id, "flatten": true}),
            )?
        }
    };
    let session = attached["sessionId"]
        .as_str()
        .ok_or("Target.attachToTarget returned no sessionId")?
        .to_string();
    cdp.ev.page_session = Some(session);
    cdp.ev.main_frame = Some(state.target_id.clone());
    Ok(created)
}

/// How long `Page.enable` may take: a page blocked by a dialog never answers it.
const ENABLE_TIMEOUT_MS: u64 = 10_000;

/// Replaces `alert`, `confirm` and `prompt` in a document with functions that record the dialog
/// and return what dismissing it returns. Full Chrome keeps a dialog that opens while no DevTools
/// session has the page enabled, blocking the page, and refuses to let a later session dismiss
/// it (`Page.handleJavaScriptDialog` answers "No dialog is showing"), so the tool keeps such
/// dialogs from opening between its calls. Defined on every document a call sees.
pub const DIALOG_SHIM: &str = r#"(() => {
  if (window.__toolBrowserDialogs) return;
  const seen = [];
  Object.defineProperty(window, '__toolBrowserDialogs', {value: seen});
  const stand_in = (type, answer) => function (message) {
    seen.push([type, message === undefined ? '' : String(message)]);
    return answer;
  };
  window.alert = stand_in('alert', undefined);
  window.confirm = stand_in('confirm', false);
  window.prompt = stand_in('prompt', null);
})()"#;

/// The dialogs the stand-ins recorded since the last call, taken from the page.
const TAKE_DIALOGS: &str = "(window.__toolBrowserDialogs || []).splice(0)";

/// Turn on what the actions read, dismiss a dialog left open between calls, and point downloads
/// at the browser's folder.
fn prepare(cdp: &mut Cdp, dir: &StateDir, workdir_abs: &str) -> Result<(), String> {
    let session = cdp.ev.page_session.clone();
    cdp.command(session.as_deref(), "Page.enable", json!({}), ENABLE_TIMEOUT_MS)
        .map_err(|e| {
            if e.contains("got no answer") {
                "the page does not answer within 10 s (a JavaScript dialog may be open on it, or its renderer could not start); call close, and the next call starts a new browser".to_string()
            } else {
                e
            }
        })?;
    let dialogs_before = cdp.ev.dialogs.len();
    let dismissed = cdp
        .page("Page.handleJavaScriptDialog", json!({"accept": false}))
        .is_ok();
    if dismissed && cdp.ev.dialogs.len() == dialogs_before {
        cdp.ev.dialogs.push("dialog: (dismissed)".into());
    }
    for domain in ["Runtime", "Log", "Network", "DOM", "Accessibility"] {
        cdp.page(&format!("{domain}.enable"), json!({}))?;
    }
    let taken = cdp
        .page(
            "Runtime.evaluate",
            json!({"expression": TAKE_DIALOGS, "returnByValue": true}),
        )
        .ok()
        .and_then(|r| r.pointer("/result/value").cloned());
    for d in taken
        .as_ref()
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let kind = d.get(0).and_then(|v| v.as_str()).unwrap_or("alert");
        let message = d.get(1).and_then(|v| v.as_str()).unwrap_or("");
        cdp.ev.dialogs.push(format!(
            "dialog: {kind} \"{}\" (dismissed)",
            crate::util::one_line(message, 300)
        ));
    }
    cdp.page(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": DIALOG_SHIM}),
    )?;
    let _ = cdp.page("Runtime.evaluate", json!({"expression": DIALOG_SHIM}));
    cdp.browser(
        "Browser.setDownloadBehavior",
        json!({"behavior": "allow", "downloadPath": format!("{workdir_abs}/{}/downloads", dir.rel), "eventsEnabled": false}),
    )?;
    Ok(())
}

/// Make the page's viewport 1280×800. Full Chrome's `--window-size` counts its window frame, so
/// the window is grown by the difference; where that does not land on 1280×800, the call keeps
/// the size with `Emulation.setDeviceMetricsOverride`, which ends with the DevTools session.
fn fit_viewport(cdp: &mut Cdp, state: &mut State) {
    let measure = |cdp: &mut Cdp| -> Option<(i64, i64)> {
        let v = cdp
            .page(
                "Runtime.evaluate",
                json!({"expression": "[innerWidth, innerHeight]", "returnByValue": true}),
            )
            .ok()?;
        let a = v.pointer("/result/value")?.as_array()?.clone();
        Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?))
    };
    let (want_w, want_h) = (
        crate::page::VIEWPORT.0 as i64,
        crate::page::VIEWPORT.1 as i64,
    );
    let Some((w, h)) = measure(cdp) else { return };
    if (w, h) == (want_w, want_h) {
        return;
    }
    let target = state.target_id.clone();
    let grown = cdp
        .browser("Browser.getWindowForTarget", json!({"targetId": target}))
        .ok()
        .and_then(|win| {
            let id = win.get("windowId")?.as_i64()?;
            let bw = win.pointer("/bounds/width")?.as_i64()?;
            let bh = win.pointer("/bounds/height")?.as_i64()?;
            cdp.browser(
                "Browser.setWindowBounds",
                json!({"windowId": id, "bounds": {"width": bw + want_w - w, "height": bh + want_h - h}}),
            )
            .ok()
        })
        .is_some();
    if grown {
        cdp.pump(200);
    }
    if !grown || measure(cdp) != Some((want_w, want_h)) {
        state.viewport_override = true;
    }
}

/// Run the action on an attached browser, then save and release it.
fn finish(ctx: &mut Ctx, mut a: Attached, req: &Request) -> Reply {
    let workdir_abs = launch::workdir_path(&a.state.workdir_url).unwrap_or_default();
    let mut reply = if req.action == Action::Close {
        close(ctx, &mut a)
    } else {
        act(ctx, &mut a, req, &workdir_abs)
    };
    if !a.state.banner_shown && req.action != Action::Close {
        let mut head = launch::banner(
            &a.state.browser.name,
            &a.state.browser.version,
            a.state.browser.sandbox,
        );
        if let Some(note) = a.state.replaced_note.take() {
            head = format!("{head}\n{note}");
        }
        envelope::prepend_banner(&mut reply, &head);
        a.state.banner_shown = true;
    }
    drain_output(ctx.procs, &a.state.handle, &a.dir);
    if let Err(e) = a.dir.save(&a.state) {
        if reply.passed {
            return envelope::error(&e);
        }
    }
    reply
}

/// Run the action, then close this call's connection. A failure on a browser that has exited
/// names the exit (see [`explain_exit`]).
fn act(ctx: &mut Ctx, a: &mut Attached, req: &Request, workdir_abs: &str) -> Reply {
    let Some(cdp) = a.cdp.take() else {
        return envelope::error("tool-browser: no DevTools connection");
    };
    let (outcome, mut cdp) = act_on(ctx, a, req, workdir_abs, cdp);
    a.state.log = std::mem::take(&mut cdp.ev.log);
    cdp.close();
    let action = req.action.name();
    match outcome {
        Ok(out) => envelope::passed(action, &out.data, out.attachment.as_deref()),
        Err(line) => {
            let failure = Failure {
                line,
                during: Some(cdp.ev.closed_during.as_deref().unwrap_or(action)),
                closed: cdp.ev.closed.is_some(),
            };
            let line = explain_exit(
                ctx,
                &a.state.handle,
                &a.dir,
                &a.state.browser.name,
                cdp.deadline_ms(),
                failure,
            );
            envelope::error(&line)
        }
    }
}

/// Attach, prepare the page and run the action; hands the connection back with the outcome.
fn act_on<'c>(
    ctx: &Ctx,
    a: &mut Attached<'c>,
    req: &Request,
    workdir_abs: &str,
    mut cdp: Cdp<'c>,
) -> (Result<Outcome, String>, Cdp<'c>) {
    let created = match attach(&mut cdp, &mut a.state) {
        Ok(c) => c,
        Err(e) => return (Err(format!("tool-browser: {e}")), cdp),
    };
    if created {
        a.state.document = DocumentState::default();
        cdp.ev.log = PageLog::default();
    }
    if let Err(e) = prepare(&mut cdp, &a.dir, workdir_abs) {
        return (Err(format!("tool-browser: {e}")), cdp);
    }
    if a.fresh {
        let before = cdp.ev.load_events;
        if cdp
            .page("Page.navigate", json!({"url": "about:blank"}))
            .is_ok()
        {
            let until = ctx.clock.now_ms() + 5_000;
            cdp.wait_until(until, |s| s.load_events > before);
        }
        // The initial page is not history `back` can return to.
        let _ = cdp.page("Page.resetNavigationHistory", json!({}));
        fit_viewport(&mut cdp, &mut a.state);
    }
    if a.state.viewport_override {
        let (w, h) = crate::page::VIEWPORT;
        if let Err(e) = cdp.page(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false}),
        ) {
            return (Err(format!("tool-browser: {e}")), cdp);
        }
    }
    let mut page = Page::new(
        cdp,
        a.state.target_id.clone(),
        &a.state.document,
        &ctx.work,
        PathBuf::from(workdir_abs),
        a.dir.clone(),
        a.state.screenshots,
        std::mem::take(&mut a.state.dialogs),
    );
    let outcome = page.run(req);
    let (document, dialogs) = page.saved();
    a.state.document = document;
    a.state.dialogs = dialogs;
    a.state.screenshots = page.screenshots;
    (outcome, page.cdp)
}

fn close(ctx: &mut Ctx, a: &mut Attached) -> Reply {
    if let Some(cdp) = a.cdp.as_mut() {
        let _ = cdp.command(None, "Browser.close", json!({}), 2_000);
        cdp.close();
    }
    let _ = ctx.procs.stop(&a.state.handle);
    let _ = fs::remove_dir_all(a.dir.file("profile"));
    a.state.closed = true;
    a.state.banner_shown = true;
    envelope::passed("close", "browser closed", None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Conn, Output, Started as ProcStarted};
    use crate::state::tests::FakeClock;
    use std::{cell::Cell, collections::VecDeque, rc::Rc};

    /// A scripted `murmur:process`: each start takes the next answer; a started process prints
    /// `stderr` once and is then exited with `exit`, or, with `running`, runs until stopped.
    /// With `hang`, `connect` answers the WebSocket upgrade and then nothing, moving that clock.
    #[derive(Default)]
    struct FakeProcs {
        starts: VecDeque<Result<(), ProcError>>,
        started: Vec<(String, Vec<String>)>,
        stderr: Vec<u8>,
        exit: i32,
        unread: bool,
        /// Status reads answering `Running` before the process reads as exited.
        running_reads: u32,
        status_reads: u32,
        running: bool,
        stopped: Vec<String>,
        hang: Option<Rc<Cell<u64>>>,
    }

    /// A clock the test and a [`HangingConn`] share; it moves only when slept on or read from.
    struct SharedClock(Rc<Cell<u64>>);

    impl Clock for SharedClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
        fn sleep_ms(&self, ms: u64) {
            self.0.set(self.0.get() + ms);
        }
        fn unix_secs(&self) -> u64 {
            crate::util::unix_now()
        }
    }

    /// A DevTools port that completes the WebSocket upgrade for the key sixteen zero bytes
    /// make, then never answers; every read waits out its timeout on the shared clock.
    struct HangingConn {
        clock: Rc<Cell<u64>>,
        upgraded: bool,
    }

    impl Conn for HangingConn {
        fn read(&mut self, _max: u32, timeout_ms: u32) -> Result<Vec<u8>, ProcError> {
            if !std::mem::replace(&mut self.upgraded, true) {
                use base64::Engine as _;
                use sha1::Digest as _;
                let mut sha = sha1::Sha1::new();
                sha.update(b"AAAAAAAAAAAAAAAAAAAAAA==258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
                let accept = base64::engine::general_purpose::STANDARD.encode(sha.finalize());
                return Ok(format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .into_bytes());
            }
            self.clock.set(self.clock.get() + u64::from(timeout_ms));
            Err(ProcError::TimedOut)
        }
        fn write(&mut self, _bytes: &[u8]) -> Result<(), ProcError> {
            Ok(())
        }
    }

    impl Processes for FakeProcs {
        fn start(
            &mut self,
            _name: &str,
            program: &str,
            args: &[String],
            _env: &[(String, String)],
        ) -> Result<ProcStarted, ProcError> {
            self.starts
                .pop_front()
                .unwrap_or(Err(ProcError::Failed("unscripted".into())))?;
            self.started.push((program.to_string(), args.to_vec()));
            self.unread = true;
            Ok(ProcStarted {
                handle: format!("prc_{:02x}", self.started.len()),
                already_running: false,
            })
        }
        fn find(&mut self, _name: &str) -> Option<String> {
            (!self.started.is_empty()).then(|| format!("prc_{:02x}", self.started.len()))
        }
        fn status(&mut self, handle: &str) -> Result<ProcState, ProcError> {
            self.status_reads += 1;
            if self.status_reads <= self.running_reads {
                return Ok(ProcState::Running);
            }
            if self.running && !self.stopped.iter().any(|h| h == handle) {
                Ok(ProcState::Running)
            } else {
                Ok(ProcState::Exited(self.exit))
            }
        }
        fn read(&mut self, _handle: &str, _max: u32) -> Result<Output, ProcError> {
            let stderr = if std::mem::take(&mut self.unread) {
                self.stderr.clone()
            } else {
                Vec::new()
            };
            Ok(Output {
                stdout: Vec::new(),
                stderr,
            })
        }
        fn stop(&mut self, handle: &str) -> Result<ProcState, ProcError> {
            self.stopped.push(handle.to_string());
            Ok(ProcState::Exited(self.exit))
        }
        fn connect(&mut self, _handle: &str, _port: u16) -> Result<Box<dyn Conn>, ProcError> {
            match &self.hang {
                Some(clock) => Ok(Box::new(HangingConn {
                    clock: clock.clone(),
                    upgraded: false,
                })),
                None => Err(ProcError::NotRunning),
            }
        }
    }

    fn zeros(n: usize) -> Vec<u8> {
        vec![0; n]
    }

    /// A work folder and a runtime directory granting `granted`.
    fn folders(granted: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let runtime = dir.path().join("runtime");
        fs::create_dir_all(&work).unwrap();
        for name in granted {
            fs::create_dir_all(runtime.join("tools").join(name)).unwrap();
            fs::write(runtime.join("tools").join(name).join("murmur.yaml"), "x").unwrap();
        }
        (dir, work, runtime)
    }

    fn run_with(procs: &mut FakeProcs, work: &Path, runtime: &Path, data: &str) -> Reply {
        run_on(procs, &FakeClock::default(), work, runtime, data)
    }

    fn run_on(
        procs: &mut FakeProcs,
        clock: &dyn Clock,
        work: &Path,
        runtime: &Path,
        data: &str,
    ) -> Reply {
        let mut ctx = Ctx {
            procs,
            clock,
            random: &zeros,
            work: work.to_path_buf(),
            runtime: Some(runtime.to_path_buf()),
        };
        call(data, None, &mut ctx)
    }

    #[test]
    fn every_name_not_found_gives_the_install_line_and_leaves_nothing() {
        let (_d, work, runtime) = folders(&["chromium", "chrome"]);
        let not_found = || {
            Err(ProcError::Failed(
                "No such file or directory (os error 2)".into(),
            ))
        };
        let mut procs = FakeProcs {
            starts: VecDeque::from([not_found(), not_found()]),
            ..FakeProcs::default()
        };
        let r = run_with(&mut procs, &work, &runtime, r#"{"action":"snapshot"}"#);
        assert_eq!(
            r.data,
            discovery::not_installed_line(&["chromium", "chrome"])
        );
        assert!(!work.join(".tool-browser").exists());
    }

    #[test]
    fn a_refusal_names_its_rule_and_removes_the_folder() {
        let (_d, work, runtime) = folders(&["chromium"]);
        let mut procs = FakeProcs {
            starts: VecDeque::from([Err(ProcError::Refused {
                rule: "on-shell".into(),
                reason: "denied by hook-command-guard".into(),
            })]),
            ..FakeProcs::default()
        };
        let r = run_with(&mut procs, &work, &runtime, r#"{"action":"snapshot"}"#);
        assert_eq!(
            r.data,
            "tool-browser: murmur refused to start chromium: on-shell: denied by hook-command-guard"
        );
        assert!(!work.join(".tool-browser").exists());
    }

    #[test]
    fn a_sandbox_failure_restarts_once_without_the_sandbox() {
        let (_d, work, runtime) = folders(&["chromium"]);
        let mut procs = FakeProcs {
            starts: VecDeque::from([Ok(()), Ok(())]),
            stderr: b"[1105/074242.297415:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:128] No usable sandbox! If you are running on Ubuntu 23.10+\n".to_vec(),
            exit: 133,
            ..FakeProcs::default()
        };
        let r = run_with(&mut procs, &work, &runtime, r#"{"action":"snapshot"}"#);
        assert_eq!(procs.started.len(), 2, "one retry, no more");
        assert!(!procs.started[0].1.iter().any(|a| a == "--no-sandbox"));
        assert!(procs.started[1].1.iter().any(|a| a == "--no-sandbox"));
        assert!(
            r.data.starts_with(
                "tool-browser: chromium exited with 133 while starting: [1105/074242.297415:FATAL:"
            ),
            "{}",
            r.data
        );
        let dirs: Vec<_> = fs::read_dir(work.join(".tool-browser")).unwrap().collect();
        assert_eq!(dirs.len(), 1, "both attempts share one folder");
    }

    const CLOSED_LINE: &str =
        "the browser connection closed during Page.navigate (the WebSocket closed)";
    const NSS_STDERR: &[u8] = b"[1107/150512.1:ERROR:crypto/nss_util.cc:256] Error initializing NSS: libsoftokn3.so: cannot open shared object file\n[1107/150512.2:FATAL:crypto/nss_util.cc:146] nss_error=-5925\n";

    fn closed_failure(during: Option<&str>) -> Failure<'_> {
        Failure {
            line: CLOSED_LINE.into(),
            during,
            closed: true,
        }
    }

    /// `explain_exit` on a fresh browser folder; the line and the clock's advance.
    fn explain(
        procs: &mut FakeProcs,
        work: &Path,
        deadline: u64,
        failure: Failure,
    ) -> (String, u64) {
        let clock = FakeClock::default();
        let mut ctx = Ctx {
            procs,
            clock: &clock,
            random: &zeros,
            work: work.to_path_buf(),
            runtime: None,
        };
        let dir = StateDir::new(work, "00");
        fs::create_dir_all(&dir.root).unwrap();
        let line = explain_exit(&mut ctx, "prc_01", &dir, "chromium", deadline, failure);
        (line, clock.now_ms())
    }

    #[test]
    fn a_closed_connection_on_a_running_browser_keeps_its_line() {
        let (_d, work, _) = folders(&[]);
        let mut procs = FakeProcs {
            running_reads: u32::MAX,
            ..FakeProcs::default()
        };
        let (line, waited) = explain(&mut procs, &work, 90_000, closed_failure(Some("open")));
        assert_eq!(line, CLOSED_LINE);
        assert_eq!(waited, EXIT_WAIT_MS, "polled for at most 2 s");
        assert_eq!(procs.status_reads as u64, EXIT_WAIT_MS / POLL_MS + 1);
        // Within a call deadline nearer than 2 s, the poll stops there.
        let mut procs = FakeProcs {
            running_reads: u32::MAX,
            ..FakeProcs::default()
        };
        let (_, waited) = explain(&mut procs, &work, 500, closed_failure(Some("open")));
        assert_eq!(waited, 500);
        // An open connection reads status once.
        let mut procs = FakeProcs {
            running_reads: u32::MAX,
            ..FakeProcs::default()
        };
        let failure = Failure {
            line: "click needs ref".into(),
            during: Some("click"),
            closed: false,
        };
        let (line, waited) = explain(&mut procs, &work, 90_000, failure);
        assert_eq!(
            (line.as_str(), waited, procs.status_reads),
            ("click needs ref", 0, 1)
        );
    }

    #[test]
    fn an_exited_browser_names_its_code_method_fatal_line_and_hint() {
        let (_d, work, _) = folders(&[]);
        let mut procs = FakeProcs {
            stderr: NSS_STDERR.to_vec(),
            unread: true,
            exit: 133,
            running_reads: 3,
            ..FakeProcs::default()
        };
        let (line, waited) = explain(
            &mut procs,
            &work,
            90_000,
            closed_failure(Some("Page.navigate")),
        );
        assert_eq!(
            line,
            format!(
                "tool-browser: chromium exited with 133 during Page.navigate: [1107/150512.2:FATAL:crypto/nss_util.cc:146] nss_error=-5925{}",
                launch::nss_hint("chromium")
            )
        );
        assert_eq!(waited, 3 * POLL_MS + EXIT_SETTLE_MS);
        let log = fs::read(StateDir::new(&work, "00").file("browser.log")).unwrap();
        assert_eq!(log, NSS_STDERR, "the drained output is kept in browser.log");
        // While starting, the same exit takes the start wording.
        let mut procs = FakeProcs {
            stderr: NSS_STDERR.to_vec(),
            unread: true,
            exit: 133,
            ..FakeProcs::default()
        };
        let (_d2, work2, _) = folders(&[]);
        let (line, _) = explain(&mut procs, &work2, 90_000, closed_failure(None));
        assert!(
            line.starts_with(
                "tool-browser: chromium exited with 133 while starting: [1107/150512.2:FATAL:"
            ) && line.ends_with(&launch::nss_hint("chromium")),
            "{line}"
        );
    }

    #[test]
    fn a_fatal_line_already_in_browser_log_is_used() {
        let (_d, work, _) = folders(&[]);
        let dir = StateDir::new(&work, "00");
        fs::create_dir_all(&dir.root).unwrap();
        fs::write(dir.file("browser.log"), NSS_STDERR).unwrap();
        let mut procs = FakeProcs {
            exit: 133,
            ..FakeProcs::default()
        };
        let (line, _) = explain(
            &mut procs,
            &work,
            90_000,
            closed_failure(Some("Page.navigate")),
        );
        assert!(
            line.contains("during Page.navigate: [1107/150512.2:FATAL:crypto/nss_util.cc:146] nss_error=-5925; Chrome loads NSS"),
            "{line}"
        );
    }

    /// The sandbox retry starts Chrome again in the same folder, so `browser.log` opens with the
    /// first attempt's FATAL line; a later death that prints nothing must not quote it.
    #[test]
    fn a_fatal_line_from_before_devtools_opened_is_not_quoted() {
        let (_d, work, _) = folders(&[]);
        let dir = StateDir::new(&work, "00");
        fs::create_dir_all(&dir.root).unwrap();
        let sandbox = "[1108/205352.595302:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:128] No usable sandbox!";
        fs::write(
            dir.file("browser.log"),
            format!("{sandbox}\n\nDevTools listening on ws://127.0.0.1:39275/devtools/browser/x\n[1108/205353.1:WARNING:x.cc] later\n"),
        )
        .unwrap();
        let mut procs = FakeProcs {
            exit: 137,
            ..FakeProcs::default()
        };
        let (line, _) = explain(&mut procs, &work, 90_000, closed_failure(Some("click")));
        assert_eq!(
            line,
            "tool-browser: chromium exited with 137 during click: [1108/205353.1:WARNING:x.cc] later"
        );
    }

    #[test]
    fn close_with_no_browser_starts_nothing() {
        let (_d, work, runtime) = folders(&[]);
        let mut procs = FakeProcs::default();
        let r = run_with(&mut procs, &work, &runtime, r#"{"action":"close"}"#);
        assert_eq!((r.passed, r.data.as_str()), (true, "browser closed"));
        assert!(procs.started.is_empty());
    }

    #[test]
    fn a_start_that_outruns_its_limit_stops_its_browser() {
        let (_d, work, runtime) = folders(&["chromium"]);
        let clock = Rc::new(Cell::new(0));
        let mut procs = FakeProcs {
            starts: VecDeque::from([Ok(())]),
            stderr: b"DevTools listening on ws://127.0.0.1:9222/devtools/browser/x\n".to_vec(),
            running: true,
            hang: Some(clock.clone()),
            ..FakeProcs::default()
        };
        let r = run_on(
            &mut procs,
            &SharedClock(clock.clone()),
            &work,
            &runtime,
            r#"{"action":"snapshot"}"#,
        );
        assert_eq!(
            (r.passed, r.data.as_str()),
            (
                false,
                "tool-browser: chromium did not finish starting within 30 s, so it was stopped; see .tool-browser/0000000000000000/browser.log"
            )
        );
        assert_eq!(procs.stopped, ["prc_01"]);
        assert!(
            (launch::START_LIMIT_MS..launch::START_LIMIT_MS + 2_000).contains(&clock.get()),
            "the start ends at its own limit, not the call's: {} ms",
            clock.get()
        );
        assert!(!work
            .join(".tool-browser/0000000000000000/state.json")
            .exists());
    }

    fn running(record: Record) -> Seen {
        Seen::Running {
            handle: "prc_01".into(),
            record,
        }
    }

    fn exited(record: Record) -> Seen {
        Seen::Exited {
            handle: "prc_01".into(),
            code: 137,
            record,
        }
    }

    #[test]
    fn nothing_seen_starts_without_a_note() {
        assert_eq!(next_step(&Seen::Nothing, 0), Next::Start { note: None });
    }

    #[test]
    fn a_recorded_running_browser_is_attached() {
        for record in [Record::Open, Record::Closed] {
            assert_eq!(
                next_step(&running(record), 0),
                Next::Attach("prc_01".into())
            );
        }
    }

    #[test]
    fn an_unrecorded_running_browser_is_waited_for_until_it_is_an_orphan() {
        let seen = running(Record::Missing);
        assert_eq!(next_step(&seen, 0), Next::Wait);
        assert_eq!(next_step(&seen, ORPHAN_MS - 1), Next::Wait);
        assert_eq!(
            next_step(&seen, ORPHAN_MS),
            Next::StopOrphan("prc_01".into())
        );
    }

    #[test]
    fn an_unrecorded_exited_browser_is_left_for_the_restart_gap() {
        let seen = exited(Record::Missing);
        assert_eq!(next_step(&seen, 0), Next::Wait);
        assert_eq!(next_step(&seen, RESTART_GAP_MS - 1), Next::Wait);
        assert_eq!(next_step(&seen, RESTART_GAP_MS), Next::Start { note: None });
    }

    #[test]
    fn a_recorded_exited_browser_is_replaced_with_a_note() {
        assert_eq!(
            next_step(&exited(Record::Open), 0),
            Next::Start {
                note: Some(launch::replaced_note(137))
            }
        );
    }

    #[test]
    fn a_closed_exited_browser_is_replaced_without_a_note() {
        assert_eq!(
            next_step(&exited(Record::Closed), 0),
            Next::Start { note: None }
        );
    }

    #[test]
    fn a_change_of_handle_or_condition_restarts_the_hold() {
        let mut held = Held::default();
        let attempt = exited(Record::Missing);
        assert_eq!(held.update(&attempt, 500), 0);
        assert_eq!(
            held.update(&attempt, 500 + RESTART_GAP_MS - 1),
            RESTART_GAP_MS - 1
        );
        // The retry runs under a new handle.
        let retry = Seen::Running {
            handle: "prc_02".into(),
            record: Record::Missing,
        };
        assert_eq!(held.update(&retry, 2_000), 0);
        assert_eq!(
            next_step(&retry, held.update(&retry, 2_000 + ORPHAN_MS - 1)),
            Next::Wait
        );
        assert_eq!(
            next_step(&retry, held.update(&retry, 2_000 + ORPHAN_MS)),
            Next::StopOrphan("prc_02".into())
        );
        // The same handle in a new condition: its state.json appeared.
        let recorded = Seen::Running {
            handle: "prc_02".into(),
            record: Record::Open,
        };
        assert_eq!(held.update(&recorded, 40_000), 0);
    }

    #[test]
    fn a_look_reads_the_latest_handle_and_its_record() {
        let (_d, work, runtime) = folders(&["chromium"]);
        let clock = FakeClock::default();
        let mut procs = FakeProcs {
            started: vec![("chromium".into(), Vec::new())],
            running: true,
            ..FakeProcs::default()
        };
        let mut ctx = Ctx {
            procs: &mut procs,
            clock: &clock,
            random: &zeros,
            work: work.clone(),
            runtime: Some(runtime),
        };
        assert_eq!(look(&mut ctx), running(Record::Missing));
        let dir = StateDir::new(&work, "ab");
        fs::create_dir_all(&dir.root).unwrap();
        let mut state = crate::state::tests::sample_state();
        state.handle = "prc_01".into();
        dir.save(&state).unwrap();
        assert_eq!(look(&mut ctx), running(Record::Open));
        ctx.procs.stop("prc_01").unwrap();
        assert_eq!(
            look(&mut ctx),
            Seen::Exited {
                handle: "prc_01".into(),
                code: 0,
                record: Record::Open
            }
        );
    }
}
