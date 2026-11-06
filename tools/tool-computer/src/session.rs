//! One call, from its input to its reply: the checks, then the session's display found by its
//! process name or started, one action, the screenshot, and what the display's programs printed
//! and how they ended.

use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use crate::config::{self, held_line, Config};
use crate::exec;
use crate::grants::{self, Grants, SCOPE_LINE, WINDOW_MANAGERS};
use crate::host::{Clock, ProcError, ProcState, Processes};
use crate::input::{self, and_list};
use crate::png;
use crate::state::{
    self, Launched, State, StateDir, WindowManager, DISPLAY_LOG, STATE_ROOT, XAUTHORITY,
};
use crate::x11::XClient;
use crate::xbackend::XBackend;
use crate::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

/// The longest a call runs, by its own clock: a 60 s `wait` and its screenshot fit.
pub const CALL_LIMIT_MS: u64 = 120_000;
/// The session-process name of Xvfb.
pub const DISPLAY_NAME: &str = "display";
/// The session-process name of the window manager.
pub const WM_NAME: &str = "wm";
pub const RESTART_LINE: &str = "the previous display stopped; programs on it are gone";

/// How long a call waits for the `state.json` of a display another call is starting.
const JOIN_WAIT_MS: u64 = 30_000;
const POLL_MS: u64 = 100;
/// How often, and how long, a start reads Xvfb's output for its display number.
const NUMBER_POLL_MS: u64 = 50;
const NUMBER_WAIT_MS: u64 = 15_000;
/// How long a start retries `connect` while murmur does not yet see Xvfb's listener.
const CONNECT_RETRY_MS: u64 = 2_000;
/// Output can still be on its way through the runtime's reader when an exit is seen.
const EXIT_GRACE_MS: u64 = 300;
const WINDOWS_STABLE_MS: u64 = 500;
const WINDOWS_CAP_MS: u64 = 5_000;
/// The most one `read` of a process's output asks for.
const READ_MAX: u32 = 65_536;

/// `n` random bytes.
pub type Random<'a> = &'a dyn Fn(usize) -> Vec<u8>;

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

/// What a call answers, before the WASM boundary turns it into a `ToolResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub passed: bool,
    pub summary: String,
    pub data: String,
    /// The screenshot, relative to the work folder; on every passed call.
    pub attachment: Option<String>,
}

impl Reply {
    /// A refusal: `summary` and `data` both carry `line`, and nothing is attached.
    pub fn error(line: String) -> Self {
        Self {
            passed: false,
            summary: line.clone(),
            data: line,
            attachment: None,
        }
    }
}

/// Runs one call. `data` is the call's input and `config` the entry's `config:` JSON.
pub fn call(data: &str, config: Option<&str>, ctx: &mut Ctx) -> Reply {
    run(data, config, ctx).unwrap_or_else(Reply::error)
}

/// The call's input, an object or a JSON-encoded object.
fn data_of(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Err("tool-computer: no input".into());
    }
    let data: Value = serde_json::from_str(raw)
        .map_err(|e| format!("tool-computer: the input is not JSON: {e}"))?;
    match data {
        Value::String(s) => serde_json::from_str(&s)
            .map_err(|e| format!("tool-computer: data is a string that is not JSON: {e}")),
        other => Ok(other),
    }
}

fn run(data: &str, config: Option<&str>, ctx: &mut Ctx) -> Result<Reply, String> {
    let data = data_of(data)?;
    let (action, command) = input::checked(&data)?;
    let grants = Grants::new(ctx.runtime.clone());
    let config = config::parse(config, &grants)?;
    if config.confirm.contains(&action) {
        return Err(held_line(action));
    }
    if !grants.known() {
        return Err(SCOPE_LINE.into());
    }
    if let Some(line) = grants::missing_display_grants(&grants) {
        return Err(line);
    }
    let deadline = ctx.clock.now_ms() + CALL_LIMIT_MS;

    let mut display = attach(ctx, &grants, &config, deadline)?;
    let action_line = exec::perform(&mut display.x, &command)?;
    let frame = exec::capture(&mut display.x, &command)?;
    let (cx, cy) = display.x.query_pointer()?;
    display.x.sync()?;
    let encoded = png::encode_rgb(frame.width, frame.height, &frame.rgb);
    let screen = display
        .dir
        .write_screen(&encoded)
        .map_err(|e| format!("tool-computer: cannot write the screenshot: {e}"))?;

    let mut lines = std::mem::take(&mut display.preface);
    lines.push(action_line.clone());
    lines.push(format!("cursor: {cx},{cy}"));
    lines.push(format!(
        "screenshot: {screen} ({}x{})",
        frame.width, frame.height
    ));
    lines.append(&mut display.notes);
    lines.extend(drain_and_report(ctx, &display.dir, &mut display.state));
    Ok(Reply {
        passed: true,
        summary: action_line,
        data: lines.join("\n"),
        attachment: Some(screen),
    })
}

/// A display this call is connected to.
struct Attached<'a> {
    dir: StateDir,
    state: State,
    x: XClient<'a>,
    /// The lines that open the result: the restart line and the banner.
    preface: Vec<String>,
    /// Lines that follow the screenshot line on the call that started the display.
    notes: Vec<String>,
}

enum Started<'a> {
    Fresh(Box<Attached<'a>>),
    /// Another call started it first.
    Joined(String),
}

/// The session's display: the running one, or a new one.
fn attach<'a>(
    ctx: &mut Ctx<'a>,
    grants: &Grants,
    config: &Config,
    deadline: u64,
) -> Result<Attached<'a>, String> {
    let found = ctx.procs.find(DISPLAY_NAME);
    let status = found.as_ref().map(|h| ctx.procs.status(h));
    if let (Some(handle), Some(Ok(ProcState::Running))) = (&found, &status) {
        return join(ctx, handle, deadline);
    }
    let mut preface = Vec::new();
    if let Some(handle) = &found {
        // Only a display that ran gets the restart line: a start that failed left no state.json.
        if let Some((_, old)) = state::find_by_handle(&ctx.work, handle) {
            stop_programs(ctx, &old);
            preface.push(RESTART_LINE.to_string());
        }
    }
    match start(ctx, grants, config, deadline)? {
        Started::Fresh(attached) => {
            let mut attached = *attached;
            preface.append(&mut attached.preface);
            attached.preface = preface;
            Ok(attached)
        }
        Started::Joined(handle) => join(ctx, &handle, deadline),
    }
}

/// Stops a stopped display's window manager and programs, so a new start under their names
/// starts them afresh.
fn stop_programs(ctx: &mut Ctx, old: &State) {
    let handles = old
        .window_manager
        .iter()
        .map(|wm| &wm.handle)
        .chain(old.launched.iter().filter_map(|l| l.handle.as_ref()));
    for handle in handles {
        let _ = ctx.procs.stop(handle);
    }
}

/// Connects to the running display `handle` through its `state.json`.
fn join<'a>(ctx: &mut Ctx<'a>, handle: &str, deadline: u64) -> Result<Attached<'a>, String> {
    let wait_until = (ctx.clock.now_ms() + JOIN_WAIT_MS).min(deadline);
    let (dir, state) = loop {
        if let Some(found) = state::find_by_handle(&ctx.work, handle) {
            break found;
        }
        if ctx.clock.now_ms() >= wait_until {
            let _ = ctx.procs.stop(handle);
            return Err(format!(
                "tool-computer: the running display ({handle}) has no state.json under \
                 {STATE_ROOT}/, so it was stopped; the next call starts a new one"
            ));
        }
        ctx.clock.sleep_ms(POLL_MS);
    };
    let cookie = state::read_cookie(&dir.file(XAUTHORITY))
        .ok_or_else(|| format!("tool-computer: {}/{XAUTHORITY} holds no cookie", dir.rel))?;
    let x = connect_x(ctx, handle, state.port, &cookie, deadline, 0)?;
    Ok(Attached {
        dir,
        state,
        x,
        preface: Vec::new(),
        notes: Vec::new(),
    })
}

/// Opens a connection to the display's port and completes the X setup on it, retrying for
/// `retry_ms` while murmur does not yet see the listener.
fn connect_x<'a>(
    ctx: &mut Ctx<'a>,
    handle: &str,
    port: u16,
    cookie: &[u8],
    deadline: u64,
    retry_ms: u64,
) -> Result<XClient<'a>, String> {
    let retry_until = ctx.clock.now_ms() + retry_ms;
    let conn = loop {
        let error = match ctx.procs.connect(handle, port) {
            Ok(conn) => break conn,
            Err(e) => e,
        };
        let retry = match &error {
            ProcError::Refused { rule, .. } => rule == "connect",
            ProcError::Failed(_) => true,
            _ => false,
        };
        if retry && ctx.clock.now_ms() < retry_until {
            ctx.clock.sleep_ms(POLL_MS);
            continue;
        }
        return Err(match error {
            ProcError::Refused { rule, reason } => format!(
                "tool-computer: murmur refused the connection to the display: {rule}: {reason}"
            ),
            other => format!("tool-computer: cannot reach the display on port {port}: {other}"),
        });
    };
    XClient::setup(conn, cookie, ctx.clock, deadline)
}

/// Xvfb's arguments: the display number on stdout, TCP on loopback only by `connect`, the
/// cookie file, and XTEST.
pub fn xvfb_args(state_rel: &str) -> Vec<String> {
    [
        "-displayfd",
        "1",
        "-screen",
        "0",
        &format!("{DISPLAY_WIDTH}x{DISPLAY_HEIGHT}x24"),
        "-auth",
        &format!("{state_rel}/{XAUTHORITY}"),
        "-nolisten",
        "unix",
        "-nolisten",
        "local",
        "-listen",
        "tcp",
        "-noreset",
        "-dpi",
        "96",
        "+extension",
        "XTEST",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The log of `config.launch[index]`.
pub fn launch_log_name(index: usize, argv0: &str) -> String {
    format!("launch-{index}-{argv0}.log")
}

/// Starts Xvfb as `display`, then the window manager and the `launch` programs on it.
fn start<'a>(
    ctx: &mut Ctx<'a>,
    grants: &Grants,
    config: &Config,
    deadline: u64,
) -> Result<Started<'a>, String> {
    let id = hex(&(ctx.random)(8));
    let dir = StateDir::new(&ctx.work, &id);
    fs::create_dir_all(&dir.root)
        .map_err(|e| format!("tool-computer: cannot create {}: {e}", dir.rel))?;
    let cookie = (ctx.random)(16);
    if let Err(e) = fs::write(dir.file(XAUTHORITY), state::xauthority_bytes(None, &cookie)) {
        dir.discard();
        return Err(format!(
            "tool-computer: cannot write {}: {e}",
            dir.rel_file(XAUTHORITY)
        ));
    }
    let handle = match ctx
        .procs
        .start(DISPLAY_NAME, "Xvfb", &xvfb_args(&dir.rel), &[])
    {
        Ok(s) if s.already_running => {
            dir.discard();
            return Ok(Started::Joined(s.handle));
        }
        Ok(s) => s.handle,
        Err(e) => {
            dir.discard();
            return Err(match e {
                ProcError::Refused { rule, reason } => {
                    format!("tool-computer: murmur refused to start Xvfb: {rule}: {reason}")
                }
                ProcError::Failed(text) if grants::is_not_found(&text) => {
                    grants::not_installed_line("Xvfb", Some("xvfb"))
                }
                other => format!("tool-computer: Xvfb could not be started: {other}"),
            });
        }
    };
    let display = display_number(ctx, &handle, &dir, grants)?;

    let mut started = vec![handle.clone()];
    match bring_up(
        ctx,
        grants,
        config,
        deadline,
        &dir,
        &handle,
        display,
        &cookie,
        &mut started,
    ) {
        Ok(attached) => Ok(Started::Fresh(Box::new(attached))),
        Err(line) => {
            for handle in started.iter().rev() {
                let _ = ctx.procs.stop(handle);
            }
            Err(line)
        }
    }
}

/// Reads Xvfb's output into `display.log` until its first stdout line, the display number.
fn display_number(
    ctx: &mut Ctx,
    handle: &str,
    dir: &StateDir,
    grants: &Grants,
) -> Result<u16, String> {
    let until = ctx.clock.now_ms() + NUMBER_WAIT_MS;
    let mut stdout = Vec::new();
    loop {
        stdout.extend(drain(ctx.procs, handle, dir, DISPLAY_LOG));
        if let Some(number) = parse_display_number(&stdout) {
            return Ok(number);
        }
        let exited = match ctx.procs.status(handle) {
            Ok(ProcState::Running) => None,
            Ok(ProcState::Exited(code)) => Some(code.to_string()),
            Err(e) => Some(format!("unknown ({e})")),
        };
        if let Some(code) = exited {
            ctx.clock.sleep_ms(EXIT_GRACE_MS);
            drain(ctx.procs, handle, dir, DISPLAY_LOG);
            return Err(start_failure_line(
                dir,
                &format!("Xvfb exited with status {code}"),
                grants.granted("sh"),
            ));
        }
        if ctx.clock.now_ms() >= until {
            let _ = ctx.procs.stop(handle);
            drain(ctx.procs, handle, dir, DISPLAY_LOG);
            return Err(start_failure_line(
                dir,
                &format!(
                    "Xvfb gave no display number within {} s",
                    NUMBER_WAIT_MS / 1000
                ),
                grants.granted("sh"),
            ));
        }
        ctx.clock.sleep_ms(NUMBER_POLL_MS);
    }
}

/// The display number on the first complete line of Xvfb's stdout.
fn parse_display_number(stdout: &[u8]) -> Option<u16> {
    let end = stdout.iter().position(|b| *b == b'\n')?;
    std::str::from_utf8(&stdout[..end])
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The line for a display that did not start, with `display.log`'s last lines and the keymap
/// hints.
pub fn start_failure_line(dir: &StateDir, reason: &str, sh_granted: bool) -> String {
    let tail = dir.log_tail(DISPLAY_LOG, 3);
    let tail = if tail.is_empty() {
        "nothing".to_string()
    } else {
        tail.join(" | ")
    };
    let mut line = format!(
        "tool-computer: the display did not start: {reason}; the last lines of {}: {tail}",
        dir.rel_file(DISPLAY_LOG)
    );
    let log = fs::read(dir.file(DISPLAY_LOG))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    line.push_str(&keymap_hints(&log, sh_granted));
    line
}

/// Xvfb compiles its keymap by running `xkbcomp` through `/bin/sh -c`; when that fails it exits
/// naming only the keyboard. Inside a sealed capsule the `sh` exec needs its own grant, and a
/// host without xkbcomp makes `sh` say so.
fn keymap_hints(log: &str, sh_granted: bool) -> String {
    if !log.contains("Failed to compile keymap") {
        return String::new();
    }
    let mut hints = String::new();
    if !sh_granted {
        hints.push_str(
            "; Xvfb compiles its keymap by running xkbcomp through sh, so add sh to \
             capabilities.shell.allow",
        );
    }
    let xkbcomp_missing = log
        .lines()
        .any(|l| l.contains("xkbcomp") && (l.contains("not found") || l.contains("No such file")));
    if xkbcomp_missing {
        hints.push_str("; install the x11-xkb-utils package on this host first");
    }
    hints
}

/// Starts `program` under `name`; a process already running under the name is stopped first.
fn start_fresh(
    procs: &mut dyn Processes,
    name: &str,
    program: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<String, ProcError> {
    let started = procs.start(name, program, args, env)?;
    if !started.already_running {
        return Ok(started.handle);
    }
    let _ = procs.stop(&started.handle);
    procs.start(name, program, args, env).map(|s| s.handle)
}

/// Everything after Xvfb gave its number: the cookie, the X connection, the window manager, the
/// `launch` programs, the wait for their windows and `state.json`. Each handle it starts is
/// pushed to `started`, so a failure stops them.
#[allow(clippy::too_many_arguments)]
fn bring_up<'a>(
    ctx: &mut Ctx<'a>,
    grants: &Grants,
    config: &Config,
    deadline: u64,
    dir: &StateDir,
    handle: &str,
    display: u16,
    cookie: &[u8],
    started: &mut Vec<String>,
) -> Result<Attached<'a>, String> {
    fs::write(
        dir.file(XAUTHORITY),
        state::xauthority_bytes(Some(display), cookie),
    )
    .map_err(|e| {
        format!(
            "tool-computer: cannot rewrite {}: {e}",
            dir.rel_file(XAUTHORITY)
        )
    })?;
    let port = 6000 + display;
    let mut x = connect_x(ctx, handle, port, cookie, deadline, CONNECT_RETRY_MS)?;

    let address = format!("127.0.0.1:{display}");
    let env = vec![
        ("DISPLAY".to_string(), address.clone()),
        ("XAUTHORITY".to_string(), dir.rel_file(XAUTHORITY)),
    ];
    let mut window_manager = None;
    for name in WINDOW_MANAGERS.iter().filter(|n| grants.granted(n)) {
        // Not installed, refused or failing: the next granted one is tried.
        if let Ok(wm) = start_fresh(ctx.procs, WM_NAME, name, &[], &env) {
            started.push(wm.clone());
            window_manager = Some(WindowManager {
                name: name.to_string(),
                handle: wm,
            });
            break;
        }
    }

    let mut launched = Vec::new();
    let mut notes = Vec::new();
    let mut exits = Vec::new();
    for (index, argv) in config.launch.iter().enumerate() {
        let argv0 = &argv[0];
        let log_name = launch_log_name(index, argv0);
        let log = dir.rel_file(&log_name);
        let _ = fs::write(dir.file(&log_name), b"");
        let mut entry = Launched {
            index,
            argv: argv.clone(),
            handle: None,
            log: log.clone(),
            refused: None,
        };
        match start_fresh(
            ctx.procs,
            &format!("launch-{index}"),
            argv0,
            &argv[1..],
            &env,
        ) {
            Ok(h) => {
                started.push(h.clone());
                entry.handle = Some(h);
            }
            Err(ProcError::Refused { rule, reason }) => {
                notes.push(format!(
                    "launched {argv0} (launch[{index}]) was not started: murmur refused it: \
                     {rule}: {reason}"
                ));
                entry.refused = Some(format!("{rule}: {reason}"));
            }
            Err(other) => {
                let text = match other {
                    ProcError::Failed(text) => text,
                    other => other.to_string(),
                };
                dir.append_log(
                    &log_name,
                    format!("tool-computer: cannot start {argv0}: {text}\n").as_bytes(),
                );
                exits.push(format!(
                    "launched {argv0} (launch[{index}]) exited with status 127; output in {log}"
                ));
            }
        }
        launched.push(entry);
    }
    notes.append(&mut exits);

    if window_manager.is_some() || launched.iter().any(|l| l.handle.is_some()) {
        wait_for_windows(&mut x, ctx.clock);
    }

    let state = State {
        handle: handle.to_string(),
        display,
        port,
        address,
        width: DISPLAY_WIDTH,
        height: DISPLAY_HEIGHT,
        window_manager,
        launched,
        reported_exits: Vec::new(),
        started_at: ctx.clock.unix_secs(),
    };
    dir.save(&state)?;
    let banner = banner(&state, dir);
    Ok(Attached {
        dir: dir.clone(),
        state,
        x,
        preface: vec![banner],
        notes,
    })
}

/// Waits until the count of viewable top-level windows has held for 500 ms, at most 5 s.
fn wait_for_windows(x: &mut XClient, clock: &dyn Clock) {
    let started = clock.now_ms();
    let mut count = x.viewable_top_level_windows().unwrap_or(0);
    let mut since = clock.now_ms();
    while clock.now_ms() - started < WINDOWS_CAP_MS {
        clock.sleep_ms(POLL_MS);
        let now = x.viewable_top_level_windows().unwrap_or(count);
        if now != count {
            count = now;
            since = clock.now_ms();
        } else if clock.now_ms() - since >= WINDOWS_STABLE_MS && count > 0 {
            return;
        }
    }
}

/// The line that opens the first result of a new display.
pub fn banner(state: &State, dir: &StateDir) -> String {
    let wm = match &state.window_manager {
        Some(wm) => wm.name.clone(),
        None => format!(
            "none (grant one of {})",
            and_list(&WINDOW_MANAGERS).replacen(" and ", " or ", 1)
        ),
    };
    let names: Vec<&str> = state
        .launched
        .iter()
        .filter(|l| l.refused.is_none())
        .map(|l| l.argv[0].as_str())
        .collect();
    let launched = if names.is_empty() {
        "nothing".to_string()
    } else {
        names.join(", ")
    };
    format!(
        "display: Xvfb {address}, {w}x{h}x24; window manager {wm}; launched {launched}; shell \
         commands reach it with DISPLAY={address} XAUTHORITY={xauth}",
        address = state.address,
        w = state.width,
        h = state.height,
        xauth = dir.rel_file(XAUTHORITY),
    )
}

/// Reads everything `handle` printed since the last read into the log `name`; the stdout bytes.
fn drain(procs: &mut dyn Processes, handle: &str, dir: &StateDir, name: &str) -> Vec<u8> {
    let mut stdout = Vec::new();
    for _ in 0..64 {
        match procs.read(handle, READ_MAX) {
            Ok(out) if out.stdout.is_empty() && out.stderr.is_empty() => break,
            Ok(out) => {
                dir.append_log(name, &out.stdout);
                dir.append_log(name, &out.stderr);
                stdout.extend_from_slice(&out.stdout);
            }
            Err(_) => break,
        }
    }
    stdout
}

/// Drains every program's output into its log, and the exit line of each program that exited
/// since the last report; `state.json` records each one reported.
fn drain_and_report(ctx: &mut Ctx, dir: &StateDir, state: &mut State) -> Vec<String> {
    let mut lines = Vec::new();
    let mut newly = Vec::new();
    if let Some(wm) = &state.window_manager {
        if !state.reported_exits.contains(&wm.handle) {
            if let Ok(ProcState::Exited(code)) = ctx.procs.status(&wm.handle) {
                newly.push(wm.handle.clone());
                lines.push(format!(
                    "window manager {} exited with status {code}; output in {}",
                    wm.name,
                    dir.rel_file(DISPLAY_LOG)
                ));
            }
        }
    }
    for l in &state.launched {
        let Some(handle) = &l.handle else { continue };
        if state.reported_exits.contains(handle) {
            continue;
        }
        if let Ok(ProcState::Exited(code)) = ctx.procs.status(handle) {
            newly.push(handle.clone());
            lines.push(format!(
                "launched {} (launch[{}]) exited with status {code}; output in {}",
                l.argv[0], l.index, l.log
            ));
        }
    }
    if !newly.is_empty() {
        ctx.clock.sleep_ms(EXIT_GRACE_MS);
    }

    drain(ctx.procs, &state.handle, dir, DISPLAY_LOG);
    if let Some(wm) = &state.window_manager {
        drain(ctx.procs, &wm.handle, dir, DISPLAY_LOG);
    }
    for l in &state.launched {
        if let Some(handle) = &l.handle {
            drain(
                ctx.procs,
                handle,
                dir,
                &launch_log_name(l.index, &l.argv[0]),
            );
        }
    }

    if !newly.is_empty() {
        state.reported_exits.append(&mut newly);
        let _ = dir.save(state);
    }
    lines
}

#[cfg(test)]
mod tests;
