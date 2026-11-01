//! The call process: find this session's supervisor, start one when there is none, and relay
//! the request to it over loopback.

use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::{
    discovery::Launch,
    envelope,
    state::{cmdline_contains, kill, StateDir, SupervisorInfo},
    supervisor::{LaunchSpec, LAUNCH_ENV},
};

/// Every call is bounded at this.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a call waits for a new supervisor to write `supervisor.json`.
const START_WAIT: Duration = Duration::from_secs(30);
/// A `start.lock` older than this belongs to a call that died.
const STALE_LOCK: Duration = Duration::from_secs(30);

enum Sent {
    Answer(Value),
    /// Nobody is listening, or the listener does not know the token: the record is stale.
    Stale,
}

fn send(info: &SupervisorInfo, request: &Value, state: &StateDir) -> Result<Sent, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], info.port));
    let stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(s) => s,
        Err(_) => return Ok(Sent::Stale),
    };
    stream.set_read_timeout(Some(CALL_TIMEOUT)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let line = json!({"token": info.token, "request": request});
    if writeln!(&stream, "{line}").is_err() {
        return Ok(Sent::Stale);
    }
    let mut answer = String::new();
    match BufReader::new(&stream).read_line(&mut answer) {
        Ok(0) => Err(format!(
            "tool-browser: the browser supervisor ended during the call; see {}/supervisor.log",
            state.rel
        )),
        Ok(_) => {
            let value: Value = serde_json::from_str(answer.trim()).map_err(|_| {
                "tool-browser: the browser supervisor sent an unreadable answer".to_string()
            })?;
            if value.get("rejected").is_some() {
                Ok(Sent::Stale)
            } else {
                Ok(Sent::Answer(value))
            }
        }
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Err(format!(
            "tool-browser: the browser call did not finish within {} s",
            CALL_TIMEOUT.as_secs()
        )),
        Err(e) => Err(format!(
            "tool-browser: reading the supervisor's answer: {e}"
        )),
    }
}

/// Remove a stale record, first killing what it names when it is provably this session's.
fn clear_stale(state: &StateDir, info: &SupervisorInfo) {
    if let Some(pid) = info.browser.pid {
        if cmdline_contains(pid, &state.profile()) {
            kill(pid);
        }
    }
    if cmdline_contains(info.pid, &state.root) {
        crate::state::kill_group(info.pid);
        kill(info.pid);
    }
    state.remove_live_files();
}

/// Relay `request` to this session's supervisor, starting one through `discover` when there is
/// none. `discover` runs only then, so a session with a live browser never repeats it.
pub fn call(
    state: &StateDir,
    request: &Value,
    discover: impl FnOnce() -> Result<Launch, String>,
    workdir: &Path,
    runtime_dir: Option<&Path>,
) -> Value {
    let closing = request.get("action").and_then(Value::as_str) == Some("close");
    if let Some(info) = state.read_info() {
        match send(&info, request, state) {
            Ok(Sent::Answer(v)) => return v,
            Ok(Sent::Stale) => clear_stale(state, &info),
            Err(e) => return envelope::error(&e),
        }
    }
    if closing {
        return envelope::passed("close", "browser closed", None);
    }
    let launch = match discover() {
        Ok(l) => l,
        Err(line) => return envelope::error(&line),
    };
    let info = match start(state, &launch, workdir, runtime_dir) {
        Ok(info) => info,
        Err(line) => return envelope::error(&line),
    };
    match send(&info, request, state) {
        Ok(Sent::Answer(v)) => v,
        Ok(Sent::Stale) => envelope::error(&format!(
            "tool-browser: the browser supervisor started but does not answer; see {}/supervisor.log",
            state.rel
        )),
        Err(e) => envelope::error(&e),
    }
}

/// Start the supervisor, or wait for the one another call is starting.
fn start(
    state: &StateDir,
    launch: &Launch,
    workdir: &Path,
    runtime_dir: Option<&Path>,
) -> Result<SupervisorInfo, String> {
    fs::create_dir_all(&state.root)
        .map_err(|e| format!("tool-browser: cannot create {}: {e}", state.rel))?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(info) = state.read_info() {
            return Ok(info);
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(state.start_lock())
        {
            Ok(_) => {
                let result = spawn_and_wait(state, launch, workdir, runtime_dir);
                let _ = fs::remove_file(state.start_lock());
                return result;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let age = fs::metadata(state.start_lock())
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .unwrap_or_default();
                if age > STALE_LOCK {
                    let _ = fs::remove_file(state.start_lock());
                    continue;
                }
            }
            Err(e) => {
                return Err(format!(
                    "tool-browser: cannot create {}/start.lock: {e}",
                    state.rel
                ))
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "tool-browser: another call has been starting the browser for 30 s; see {}/supervisor.log",
                state.rel
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_and_wait(
    state: &StateDir,
    launch: &Launch,
    workdir: &Path,
    runtime_dir: Option<&Path>,
) -> Result<SupervisorInfo, String> {
    let _ = fs::remove_file(state.start_failed());
    let spec = match launch {
        Launch::Local { name, program } => LaunchSpec {
            kind: "local".into(),
            name: Some(name.clone()),
            program: Some(program.clone()),
            endpoint: None,
            workdir: workdir.to_path_buf(),
            runtime_dir: runtime_dir.map(Path::to_path_buf),
        },
        Launch::Remote { endpoint } => LaunchSpec {
            kind: "remote".into(),
            name: None,
            program: None,
            endpoint: Some(endpoint.clone()),
            workdir: workdir.to_path_buf(),
            runtime_dir: runtime_dir.map(Path::to_path_buf),
        },
    };
    if fs::metadata(state.log()).is_ok_and(|m| m.len() >= crate::supervisor::LOG_MAX_BYTES) {
        let _ = fs::remove_file(state.log());
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.log())
        .map_err(|e| {
            format!(
                "tool-browser: cannot open {}/supervisor.log: {e}",
                state.rel
            )
        })?;
    let exe = std::env::current_exe()
        .map_err(|e| format!("tool-browser: cannot find its own executable: {e}"))?;
    let mut command = Command::new(exe);
    command
        .arg("--supervise")
        .arg(&state.root)
        .env(LAUNCH_ENV, serde_json::to_string(&spec).unwrap_or_default())
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("tool-browser: cannot start its supervisor: {e}"))?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(info) = state.read_info() {
            return Ok(info);
        }
        if let Ok(line) = fs::read_to_string(state.start_failed()) {
            let _ = fs::remove_file(state.start_failed());
            return Err(line.trim().to_string());
        }
        if let Ok(Some(status)) = child.try_wait() {
            if let Ok(line) = fs::read_to_string(state.start_failed()) {
                let _ = fs::remove_file(state.start_failed());
                return Err(line.trim().to_string());
            }
            return Err(format!(
                "tool-browser: its supervisor exited ({status}) before the browser started; see {}/supervisor.log",
                state.rel
            ));
        }
        if Instant::now() >= deadline {
            // The supervisor leads its own process group, and the browser it started is in it.
            crate::state::kill_group(child.id());
            let _ = child.kill();
            return Err(format!(
                "tool-browser: the browser did not start within 30 s; see {}/supervisor.log",
                state.rel
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
