//! The supervisor: the one long-lived process per session that owns the browser.
//!
//! The first call process starts it as `tool-browser --supervise <state folder>` in its own
//! process group, with no handle on the call's stdout or stderr. It starts (or connects to) the
//! browser, attaches one page, writes `supervisor.json`, and then serves one request at a time
//! on a loopback port, each a line `{"token", "request"}` answered by a line holding the
//! envelope. Between requests it ticks: it stops the browser after [`IDLE_LIMIT`] without a
//! request, when the session's runtime directory disappears, or when the browser exits.

use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    cdp::Cdp,
    envelope,
    page::Page,
    request::{self, Action},
    state::{BrowserInfo, StateDir, SupervisorInfo},
};

/// How long the supervisor keeps an unused browser.
pub const IDLE_LIMIT: Duration = Duration::from_secs(15 * 60);
/// How often it checks that the session's runtime directory still exists.
pub const RUNTIME_DIR_CHECK: Duration = Duration::from_secs(5);
/// How long a local browser has to write `DevToolsActivePort`.
const DEVTOOLS_WAIT: Duration = Duration::from_secs(20);
/// How long each command may take while the page is being attached.
const ATTACH_COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
/// How long `Browser.close` has before the browser is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// `supervisor.log` is cut at this size.
pub const LOG_MAX_BYTES: u64 = 1024 * 1024;
/// Below this `RLIMIT_AS` no Chrome measured for the README started: chrome-headless-shell 145
/// needed 80 GiB inside a capsule, Chrome 156 104 GiB outside one. V8 reserves its address space
/// up front, so the number is reservations, not memory used.
pub const MIN_ADDRESS_SPACE: u64 = 80 * 1024 * 1024 * 1024;

/// What the call process hands the supervisor, as `TOOL_BROWSER_LAUNCH`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchSpec {
    /// `local` or `remote`.
    pub kind: String,
    /// The granted name, for a local browser.
    pub name: Option<String>,
    /// The program to run, for a local browser.
    pub program: Option<PathBuf>,
    /// The DevTools endpoint, for a remote browser.
    pub endpoint: Option<String>,
    pub workdir: PathBuf,
    pub runtime_dir: Option<PathBuf>,
}

pub const LAUNCH_ENV: &str = "TOOL_BROWSER_LAUNCH";

// ── the stop decision ────────────────────────────────────────────────────────

/// Monotonic time since an arbitrary origin.
pub trait Clock {
    fn now(&self) -> Duration;
}

pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// The browser as the stop decision sees it.
pub trait BrowserHandle {
    /// Why the browser is gone, once it is.
    fn exited(&mut self) -> Option<String>;
    /// Stop it: close it and, for a local browser, kill it if it lingers.
    fn shutdown(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Idle,
    SessionEnded,
    BrowserExited(String),
}

/// When the supervisor stops. [`Lifetime::tick`] stops at most once.
pub struct Lifetime {
    last_request: Duration,
    last_dir_check: Duration,
    runtime_dir: Option<PathBuf>,
    stopped: bool,
}

impl Lifetime {
    pub fn new(now: Duration, runtime_dir: Option<PathBuf>) -> Self {
        Self {
            last_request: now,
            last_dir_check: now,
            runtime_dir,
            stopped: false,
        }
    }

    pub fn request_finished(&mut self, now: Duration) {
        self.last_request = now;
    }

    pub fn stopped(&self) -> bool {
        self.stopped
    }

    fn due(&mut self, now: Duration, browser: &mut dyn BrowserHandle) -> Option<StopReason> {
        if let Some(why) = browser.exited() {
            return Some(StopReason::BrowserExited(why));
        }
        if now.saturating_sub(self.last_request) >= IDLE_LIMIT {
            return Some(StopReason::Idle);
        }
        if let Some(dir) = &self.runtime_dir {
            if now.saturating_sub(self.last_dir_check) >= RUNTIME_DIR_CHECK {
                self.last_dir_check = now;
                if !dir.exists() {
                    return Some(StopReason::SessionEnded);
                }
            }
        }
        None
    }

    /// Decide, and stop when it is time: shut the browser down and remove the state files.
    pub fn tick(
        &mut self,
        clock: &dyn Clock,
        browser: &mut dyn BrowserHandle,
        state: &StateDir,
    ) -> Option<StopReason> {
        if self.stopped {
            return None;
        }
        let reason = self.due(clock.now(), browser)?;
        self.stop(browser, state);
        Some(reason)
    }

    /// Stop now, for `close`.
    pub fn stop(&mut self, browser: &mut dyn BrowserHandle, state: &StateDir) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        browser.shutdown();
        state.remove_live_files();
    }
}

// ── the real browser ─────────────────────────────────────────────────────────

struct RealBrowser {
    cdp: Arc<Cdp>,
    child: Option<Child>,
    target_id: String,
}

impl BrowserHandle for RealBrowser {
    fn exited(&mut self) -> Option<String> {
        if let Some(child) = &mut self.child {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(format!("the browser exited ({status})"));
            }
        }
        self.cdp
            .state()
            .closed
            .clone()
            .map(|why| format!("the browser connection closed ({why})"))
    }

    fn shutdown(&mut self) {
        match &mut self.child {
            Some(child) => {
                let _ = self
                    .cdp
                    .command(None, "Browser.close", json!({}), CLOSE_GRACE);
                let deadline = Instant::now() + CLOSE_GRACE;
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                if let Ok(None) = child.try_wait() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            None => {
                let _ = self.cdp.command(
                    None,
                    "Target.closeTarget",
                    json!({"targetId": self.target_id}),
                    CLOSE_GRACE,
                );
            }
        }
        self.cdp.disconnect();
    }
}

// ── starting ─────────────────────────────────────────────────────────────────

/// Whether a seccomp filter is on this process, which on Linux means murmur's.
///
/// `/proc/self/status` says so with `Seccomp:\t2`. On the Full tier Landlock refuses the read
/// itself, and Landlock is only ever applied there together with murmur's seccomp filter, so an
/// unreadable status file on Linux counts as a filter too.
pub fn seccomp_filter_on() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    match fs::read_to_string("/proc/self/status") {
        Ok(status) => status_shows_filter(&status),
        Err(_) => true,
    }
}

fn status_shows_filter(status: &str) -> bool {
    status
        .lines()
        .any(|l| l.starts_with("Seccomp:") && l["Seccomp:".len()..].trim() == "2")
}

/// The soft `RLIMIT_AS` from `/proc/self/limits`, when one is set.
fn address_space_limit() -> Option<u64> {
    let limits = fs::read_to_string("/proc/self/limits").ok()?;
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max address space"))?;
    line["Max address space".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Full Chrome and Chromium bind a unix socket file for their ProcessSingleton, and murmur's
/// Landlock ruleset withholds the right to create one on every path.
const SINGLETON_HINT: &str = "; this Chrome binds a unix socket file for its ProcessSingleton, which murmur's Full and Sealed tiers refuse on every path: grant chrome-headless-shell under one of the four names instead, as the tool-browser README shows";

/// `; <program> is a #! script …` when `program` starts with `#!`.
fn script_hint(program: &Path) -> String {
    let mut head = [0u8; 256];
    let n = fs::File::open(program)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let first = String::from_utf8_lossy(&head[..n]);
    match first.strip_prefix("#!") {
        Some(rest) => format!(
            "; it is a #! script run by {}, which capabilities.shell.allow must grant too, with the programs the script runs (see the tool-browser README)",
            rest.lines().next().unwrap_or("").trim()
        ),
        None => String::new(),
    }
}

/// `; Chrome needs more address space …` when `RLIMIT_AS` is below what Chrome was measured to
/// need, else nothing.
fn memory_hint() -> String {
    match address_space_limit().filter(|l| *l < MIN_ADDRESS_SPACE) {
        Some(limit) => format!(
            "; Chrome needs more address space than capabilities.resources.memory_bytes allows ({} GiB here, at least {} GiB needed): set it to 274877906944 (256 GiB) as the tool-browser README says",
            limit >> 30,
            MIN_ADDRESS_SPACE >> 30
        ),
        None => String::new(),
    }
}

/// The arguments a local browser starts with.
pub fn browser_args(profile: &Path, sandbox_off: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "--headless",
        "--remote-debugging-address=127.0.0.1",
        "--remote-debugging-port=0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("--user-data-dir={}", profile.display()));
    args.extend(
        [
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--mute-audio",
            "--password-store=basic",
            "--use-mock-keychain",
            "--window-size=1280,800",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    if sandbox_off {
        args.push("--no-sandbox".into());
        args.push("--disable-dev-shm-usage".into());
    }
    args.push("about:blank".into());
    args
}

/// Copies the browser's stderr into `supervisor.log`, up to [`LOG_MAX_BYTES`], keeping the
/// last lines for a start-failure message.
fn pipe_stderr(stderr: impl Read + Send + 'static, log: PathBuf) -> Arc<Mutex<VecDeque<String>>> {
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let keep = Arc::clone(&tail);
    thread::spawn(move || {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log)
            .ok();
        let mut cut = false;
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            {
                let mut t = keep.lock().unwrap_or_else(|p| p.into_inner());
                if !line.trim().is_empty() {
                    t.push_back(line.clone());
                    if t.len() > 3 {
                        t.pop_front();
                    }
                }
            }
            if let Some(f) = &mut file {
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                if size < LOG_MAX_BYTES {
                    let _ = writeln!(f, "{line}");
                } else if !cut {
                    let _ = writeln!(f, "… supervisor.log cut at 1 MiB");
                    cut = true;
                }
            }
        }
    });
    tail
}

struct Started {
    cdp: Arc<Cdp>,
    child: Option<Child>,
    info: BrowserInfo,
    banner: String,
}

fn start_local(
    spec: &LaunchSpec,
    name: &str,
    program: &Path,
    state: &StateDir,
) -> Result<Started, String> {
    let profile = state.profile();
    let _ = fs::remove_dir_all(&profile);
    fs::create_dir_all(&profile)
        .map_err(|e| format!("tool-browser: cannot create {}: {e}", profile.display()))?;
    let sandbox_off = seccomp_filter_on();
    let mut child = Command::new(program)
        .args(browser_args(&profile, sandbox_off))
        .current_dir(&spec.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "tool-browser: cannot run {}: {e}{}",
                program.display(),
                script_hint(program)
            )
        })?;
    let tail = pipe_stderr(child.stderr.take().expect("stderr is piped"), state.log());
    let port_file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + DEVTOOLS_WAIT;
    let failure = |what: String, child: &mut Child| -> String {
        // Let the stderr thread catch up with the last lines.
        thread::sleep(Duration::from_millis(300));
        let _ = child.kill();
        let _ = child.wait();
        let last: Vec<String> = tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|l| crate::util::one_line(l, 300))
            .collect();
        let mut line = format!("tool-browser: {} {what}", program.display());
        if !last.is_empty() {
            line.push_str(&format!("; last stderr: {}", last.join(" | ")));
        }
        if last
            .iter()
            .any(|l| l.contains("SingletonSocket") || l.contains("ProcessSingleton"))
        {
            line.push_str(SINGLETON_HINT);
        }
        line.push_str(&memory_hint());
        line
    };
    let (port, path) = loop {
        if let Ok(text) = fs::read_to_string(&port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.trim().parse::<u16>() {
                    break (port, path.trim().to_string());
                }
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(failure(
                format!("exited with {status} before DevTools started"),
                &mut child,
            ));
        }
        if Instant::now() >= deadline {
            return Err(failure(
                "did not open DevTools within 20 s".into(),
                &mut child,
            ));
        }
        thread::sleep(Duration::from_millis(100));
    };
    let cdp = match Cdp::connect(&format!("ws://127.0.0.1:{port}{path}")) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            return Err(failure(
                format!("opened DevTools, but connecting failed: {e}"),
                &mut child,
            ))
        }
    };
    let version = browser_version(&cdp);
    let sandbox = if sandbox_off {
        "Chrome sandbox off (murmur's seccomp filter is on, so the capsule's containment holds the browser)"
    } else {
        "Chrome sandbox on (no murmur seccomp filter on this tier)"
    };
    Ok(Started {
        info: BrowserInfo {
            kind: "local".into(),
            program: program.display().to_string(),
            version: version.clone(),
            pid: Some(child.id()),
        },
        banner: format!("browser: {name} {version}, local, headless; {sandbox}"),
        cdp,
        child: Some(child),
    })
}

fn browser_version(cdp: &Cdp) -> String {
    cdp.browser("Browser.getVersion", json!({}))
        .ok()
        .and_then(|v| v["product"].as_str().map(str::to_string))
        .map(|p| p.split_once('/').map(|(_, v)| v.to_string()).unwrap_or(p))
        .unwrap_or_else(|| "(unknown version)".into())
}

/// The browser WebSocket behind `ws://host:port` with no path, from `/json/version`.
fn discover_ws_path(url: &crate::ws::WsUrl) -> Result<String, String> {
    let addr = url.authority();
    let mut stream = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(
        stream,
        "GET /json/version HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    // DevTools keeps the connection open whatever the request says, so read the headers, then
    // exactly the body they announce.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (header_end, length) = loop {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("it closed the connection before answering /json/version".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let length = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.trim() == "content-length")
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (i + 4, length);
        }
        if buf.len() > 1024 * 1024 {
            return Err("its /json/version answer is too large".into());
        }
    };
    while buf.len() < header_end + length {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let v: Value = serde_json::from_slice(&buf[header_end..])
        .map_err(|_| "its /json/version is not JSON".to_string())?;
    v["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "its /json/version names no webSocketDebuggerUrl".into())
}

fn start_remote(endpoint: &str) -> Result<Started, String> {
    let url = crate::ws::WsUrl::parse(endpoint).map_err(|e| format!("tool-browser: {e}"))?;
    let unreachable = |e: String| {
        format!(
            "tool-browser could not reach the browser at {}: {e}; if {} is outside the capsule, add it to capabilities.network.allow",
            url.authority(),
            url.host
        )
    };
    let ws_url = if url.path == "/" && !url.tls {
        discover_ws_path(&url).map_err(unreachable)?
    } else {
        endpoint.to_string()
    };
    let cdp = Arc::new(Cdp::connect(&ws_url).map_err(unreachable)?);
    let version = browser_version(&cdp);
    Ok(Started {
        info: BrowserInfo {
            kind: "remote".into(),
            program: endpoint.to_string(),
            version: version.clone(),
            pid: None,
        },
        banner: format!("browser: remote {} {version}", url.authority()),
        cdp,
        child: None,
    })
}

/// Remove the `profile/` of other sessions in this work folder whose supervisor is gone: a
/// session murmur killed at its end could not remove its own.
fn sweep_dead_profiles(state: &StateDir) {
    let Some(parent) = state.root.parent() else {
        return;
    };
    for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
        let dir = entry.path();
        if dir == state.root || !dir.join("profile").is_dir() || dir.join("start.lock").exists() {
            continue;
        }
        let other = StateDir::from_root(dir.clone());
        let alive = other
            .read_info()
            .is_some_and(|info| TcpStream::connect(("127.0.0.1", info.port)).is_ok());
        if !alive {
            let _ = fs::remove_dir_all(dir.join("profile"));
            let _ = fs::remove_file(other.info());
        }
    }
}

// ── serving ──────────────────────────────────────────────────────────────────

/// The supervisor's entry point. Never returns.
pub fn run(state_root: PathBuf) -> ! {
    let state = StateDir::from_root(state_root);
    let spec: Option<LaunchSpec> = std::env::var(LAUNCH_ENV)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let Some(spec) = spec else {
        fail_start(
            &state,
            "tool-browser: the supervisor was started without its launch spec",
        );
    };
    match serve(&state, &spec) {
        Ok(()) => std::process::exit(0),
        Err(line) => fail_start(&state, &line),
    }
}

fn fail_start(state: &StateDir, line: &str) -> ! {
    eprintln!("{line}");
    let _ = fs::write(state.start_failed(), crate::util::one_line(line, 4000));
    std::process::exit(1);
}

fn serve(state: &StateDir, spec: &LaunchSpec) -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("tool-browser: cannot listen on loopback: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = crate::util::hex(
        &crate::util::random_bytes(24).map_err(|e| format!("tool-browser: /dev/urandom: {e}"))?,
    );
    let started = match (spec.kind.as_str(), &spec.program, &spec.endpoint) {
        ("local", Some(program), _) => {
            sweep_dead_profiles(state);
            start_local(
                spec,
                spec.name.as_deref().unwrap_or("browser"),
                program,
                state,
            )?
        }
        ("remote", _, Some(endpoint)) => start_remote(endpoint)?,
        _ => return Err("tool-browser: the supervisor's launch spec is incomplete".into()),
    };
    let remote = started.child.is_none();
    // The whole start has to finish inside the call process's 30 s wait: at most 20 s for
    // DevTools, then attaching, whose commands get a short timeout until the page is up.
    started.cdp.set_timeout(ATTACH_COMMAND_TIMEOUT);
    let page = match Page::attach(
        Arc::clone(&started.cdp),
        remote,
        spec.workdir.clone(),
        state.clone(),
    ) {
        Ok(p) => p,
        Err(e) => {
            let mut browser = RealBrowser {
                cdp: started.cdp,
                child: started.child,
                target_id: String::new(),
            };
            browser.shutdown();
            state.remove_live_files();
            return Err(format!(
                "tool-browser: the browser started, but attaching a page failed: {e}{}",
                if remote { String::new() } else { memory_hint() }
            ));
        }
    };
    started.cdp.set_timeout(crate::cdp::COMMAND_TIMEOUT);
    let mut browser = RealBrowser {
        cdp: Arc::clone(&started.cdp),
        child: started.child,
        target_id: page.target_id.clone(),
    };
    let info = SupervisorInfo {
        pid: std::process::id(),
        port,
        token: token.clone(),
        browser: started.info,
        started_at: crate::util::unix_now(),
    };
    if let Err(e) = state.write_info(&info) {
        browser.shutdown();
        state.remove_live_files();
        return Err(format!(
            "tool-browser: cannot write {}: {e}",
            state.info().display()
        ));
    }
    eprintln!(
        "tool-browser supervisor: serving on 127.0.0.1:{port}; {}",
        started.banner
    );

    let clock = SystemClock::new();
    let mut lifetime = Lifetime::new(clock.now(), spec.runtime_dir.clone());
    let mut server = Server {
        page,
        banner: Some(started.banner),
        token,
    };
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let close = server.handle(stream, &mut lifetime, &mut browser, state);
                lifetime.request_finished(clock.now());
                if close || lifetime.stopped() {
                    eprintln!("tool-browser supervisor: closed on request");
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100))
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
        if let Some(reason) = lifetime.tick(&clock, &mut browser, state) {
            eprintln!("tool-browser supervisor: stopped: {reason:?}");
            return Ok(());
        }
    }
}

struct Server {
    page: Page,
    banner: Option<String>,
    token: String,
}

impl Server {
    /// Answer the one request on `stream`. True when the request was `close`.
    fn handle(
        &mut self,
        stream: TcpStream,
        lifetime: &mut Lifetime,
        browser: &mut RealBrowser,
        state: &StateDir,
    ) -> bool {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut reader = BufReader::new(&stream).take(4 * 1024 * 1024);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            return false;
        }
        let msg: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
        let given = msg.get("token").and_then(Value::as_str).unwrap_or("");
        if !constant_time_eq(given.as_bytes(), self.token.as_bytes()) {
            reply(&stream, &json!({"rejected": "token"}));
            return false;
        }
        let request_value = msg.get("request").cloned().unwrap_or(Value::Null);
        let (envelope, close) = match request::parse_request(&request_value) {
            Err(e) => (envelope::error(&e), false),
            Ok(req) if req.action == Action::Close => {
                lifetime.stop(browser, state);
                (envelope::passed("close", "browser closed", None), true)
            }
            Ok(req) => {
                let action = req.action.name();
                let mut env = match self.page.run(&req) {
                    Ok(out) => envelope::passed(action, &out.data, out.attachment.as_deref()),
                    Err(e) => envelope::error(&e),
                };
                if let Some(banner) = self.banner.take() {
                    envelope::prepend_banner(&mut env, &banner);
                }
                (env, false)
            }
        };
        reply(&stream, &envelope);
        close
    }
}

fn reply(mut stream: &TcpStream, value: &Value) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let _ = writeln!(stream, "{value}");
    let _ = stream.flush();
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct FakeClock(Cell<Duration>);
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    #[derive(Default)]
    struct FakeBrowser {
        shutdowns: u32,
        gone: Option<String>,
    }
    impl BrowserHandle for FakeBrowser {
        fn exited(&mut self) -> Option<String> {
            self.gone.clone()
        }
        fn shutdown(&mut self) {
            self.shutdowns += 1;
        }
    }

    fn state_with_files() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "ses_test");
        fs::create_dir_all(state.profile()).unwrap();
        fs::write(state.profile().join("Local State"), "{}").unwrap();
        fs::write(state.info(), "{}").unwrap();
        (dir, state)
    }

    #[test]
    fn idle_stop_happens_at_fifteen_minutes_exactly_once() {
        let (_dir, state) = state_with_files();
        let clock = FakeClock(Cell::new(Duration::from_secs(100)));
        let mut lifetime = Lifetime::new(clock.now(), None);
        let mut browser = FakeBrowser::default();
        lifetime.request_finished(clock.now());

        clock.0.set(Duration::from_secs(100 + 14 * 60 + 59));
        assert_eq!(lifetime.tick(&clock, &mut browser, &state), None);
        assert_eq!(browser.shutdowns, 0);
        assert!(state.info().exists() && state.profile().exists());

        clock.0.set(Duration::from_secs(100 + 15 * 60));
        assert_eq!(
            lifetime.tick(&clock, &mut browser, &state),
            Some(StopReason::Idle)
        );
        assert_eq!(browser.shutdowns, 1);
        assert!(!state.info().exists() && !state.profile().exists());

        clock.0.set(Duration::from_secs(100 + 20 * 60));
        assert_eq!(lifetime.tick(&clock, &mut browser, &state), None);
        assert_eq!(browser.shutdowns, 1, "the stop happens once");
    }

    #[test]
    fn a_request_resets_the_idle_clock() {
        let (_dir, state) = state_with_files();
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let mut lifetime = Lifetime::new(clock.now(), None);
        let mut browser = FakeBrowser::default();
        clock.0.set(Duration::from_secs(10 * 60));
        lifetime.request_finished(clock.now());
        clock.0.set(Duration::from_secs(20 * 60));
        assert_eq!(lifetime.tick(&clock, &mut browser, &state), None);
    }

    #[test]
    fn removing_the_runtime_dir_stops_on_the_next_tick() {
        let (dir, state) = state_with_files();
        let runtime = dir.path().join("sessions/ses_test/runtime");
        fs::create_dir_all(&runtime).unwrap();
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let mut lifetime = Lifetime::new(clock.now(), Some(runtime.clone()));
        let mut browser = FakeBrowser::default();

        clock.0.set(RUNTIME_DIR_CHECK);
        assert_eq!(lifetime.tick(&clock, &mut browser, &state), None);

        fs::remove_dir_all(&runtime).unwrap();
        clock.0.set(RUNTIME_DIR_CHECK * 2);
        assert_eq!(
            lifetime.tick(&clock, &mut browser, &state),
            Some(StopReason::SessionEnded)
        );
        assert_eq!(browser.shutdowns, 1);
        assert!(!state.info().exists() && !state.profile().exists());
    }

    #[test]
    fn a_browser_exit_stops_at_once() {
        let (_dir, state) = state_with_files();
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let mut lifetime = Lifetime::new(clock.now(), None);
        let mut browser = FakeBrowser {
            gone: Some("crashed".into()),
            ..Default::default()
        };
        assert_eq!(
            lifetime.tick(&clock, &mut browser, &state),
            Some(StopReason::BrowserExited("crashed".into()))
        );
    }

    #[test]
    fn seccomp_mode_two_is_a_filter() {
        assert!(status_shows_filter(
            "Name:\tx\nSeccomp:\t2\nSeccomp_filters:\t1\n"
        ));
        assert!(!status_shows_filter("Seccomp:\t0\nSeccomp_filters:\t0\n"));
    }

    #[test]
    fn sandbox_flags_follow_the_seccomp_filter() {
        let on = browser_args(Path::new("/w/profile"), true);
        assert!(
            on.contains(&"--no-sandbox".to_string())
                && on.contains(&"--disable-dev-shm-usage".to_string())
        );
        let off = browser_args(Path::new("/w/profile"), false);
        assert!(!off.contains(&"--no-sandbox".to_string()));
        assert_eq!(off.last().map(String::as_str), Some("about:blank"));
        assert!(off.contains(&"--user-data-dir=/w/profile".to_string()));
    }
}
