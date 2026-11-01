//! The keeper: `tool-computer --keep <state folder>`, detached from the call that started it.
//!
//! It writes the cookie, starts Xvfb, the window manager and every `launch` program, writes
//! `display.json`, then watches once a second: it records child exits, stops everything when
//! Xvfb exits, and stops everything when the session's runtime folder is gone. The watch runs
//! behind [`Clock`] and [`ProcessTable`] so unit tests drive it.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config;
use crate::grants::{self, Grants};
use crate::state::{
    self, CappedLog, DisplayInfo, Exit, Launched, StateDir, Stopped, WindowManager, KEEPER_LOG,
    XAUTHORITY,
};
use crate::xbackend::X11;
use crate::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

const TICK: Duration = Duration::from_secs(1);
const TERM_GRACE: Duration = Duration::from_secs(3);
const DISPLAY_NUMBER_WAIT: Duration = Duration::from_secs(10);
const WINDOWS_STABLE: Duration = Duration::from_millis(500);
const WINDOWS_CAP: Duration = Duration::from_secs(5);

pub trait Clock {
    fn sleep(&mut self, duration: Duration);
}

/// The keeper's children, by pid.
pub trait ProcessTable {
    /// The exit status once `pid` has exited (reaping it), and again on every later ask.
    fn try_wait(&mut self, pid: i32) -> Option<i32>;
    /// Sends `signal` to the process group `pid` leads.
    fn signal_group(&mut self, pid: i32, signal: i32);
}

/// A child the keeper watches besides Xvfb.
#[derive(Debug, Clone, PartialEq)]
pub struct Watched {
    pub pid: i32,
    /// The `launch` entry, or `None` for the window manager.
    pub index: Option<usize>,
    pub argv0: String,
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    XvfbExited(i32),
    SessionEnded,
}

pub struct Watch {
    pub state: StateDir,
    pub runtime_dir: PathBuf,
    pub keeper_pid: i32,
    pub xvfb_pid: i32,
    pub children: Vec<Watched>,
}

impl Watch {
    /// Records every child that exited since the last look in `exits.jsonl`.
    pub fn reap<P: ProcessTable>(&mut self, procs: &mut P) {
        for child in self.children.iter_mut().filter(|c| !c.done) {
            if let Some(status) = procs.try_wait(child.pid) {
                child.done = true;
                let _ = self.state.append_exit(&Exit {
                    index: child.index,
                    argv0: child.argv0.clone(),
                    status,
                });
            }
        }
    }

    fn check<P: ProcessTable>(&mut self, procs: &mut P) -> Option<Stop> {
        self.reap(procs);
        if let Some(status) = procs.try_wait(self.xvfb_pid) {
            return Some(Stop::XvfbExited(status));
        }
        if !self.runtime_dir.is_dir() {
            return Some(Stop::SessionEnded);
        }
        None
    }

    /// Watches until Xvfb exits or the session ends, stops every child, and records why.
    pub fn run<C: Clock, P: ProcessTable>(&mut self, clock: &mut C, procs: &mut P) -> Stop {
        loop {
            if let Some(stop) = self.check(procs) {
                self.stop(clock, procs, stop);
                return stop;
            }
            clock.sleep(TICK);
        }
    }

    fn stop<C: Clock, P: ProcessTable>(&mut self, clock: &mut C, procs: &mut P, stop: Stop) {
        let mut live: Vec<i32> = self
            .children
            .iter()
            .filter(|c| !c.done)
            .map(|c| c.pid)
            .collect();
        if stop == Stop::SessionEnded {
            live.insert(0, self.xvfb_pid);
        }
        terminate(clock, procs, live);
        let stopped = match stop {
            Stop::XvfbExited(status) => Stopped {
                reason: format!("Xvfb exited with status {status}"),
                was_running: true,
                xvfb_status: Some(status),
            },
            Stop::SessionEnded => Stopped {
                reason: "the session's runtime folder is gone".into(),
                was_running: true,
                xvfb_status: None,
            },
        };
        let _ = self.state.write_stopped(&stopped);
        self.state.remove_display_if_kept_by(self.keeper_pid);
    }
}

/// SIGTERM to each group, SIGKILL to any still running after 3 s.
fn terminate<C: Clock, P: ProcessTable>(clock: &mut C, procs: &mut P, mut live: Vec<i32>) {
    live.retain(|pid| *pid > 0);
    for pid in &live {
        procs.signal_group(*pid, libc::SIGTERM);
    }
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while !live.is_empty() && waited < TERM_GRACE {
        clock.sleep(step);
        waited += step;
        live.retain(|pid| procs.try_wait(*pid).is_none());
    }
    for pid in &live {
        procs.signal_group(*pid, libc::SIGKILL);
    }
    for pid in live {
        for _ in 0..20 {
            if procs.try_wait(pid).is_some() {
                break;
            }
            clock.sleep(Duration::from_millis(50));
        }
    }
}

struct RealClock;

impl Clock for RealClock {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// `waitpid` on the keeper's own children, remembering each status once reaped.
#[derive(Default)]
struct RealProcesses {
    reaped: HashMap<i32, i32>,
}

/// A wait status as one number: the exit code, or 128 plus the signal.
pub fn decode_status(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        status
    }
}

impl ProcessTable for RealProcesses {
    fn try_wait(&mut self, pid: i32) -> Option<i32> {
        if let Some(status) = self.reaped.get(&pid) {
            return Some(*status);
        }
        let mut status = 0;
        // SAFETY: waitpid with WNOHANG on a pid; `status` is a valid out pointer.
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let code = if got == pid {
            decode_status(status)
        } else if got == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            -1
        } else {
            return None;
        };
        self.reaped.insert(pid, code);
        Some(code)
    }

    fn signal_group(&mut self, pid: i32, signal: i32) {
        // SAFETY: kill(2) has no memory effects; a gone group answers ESRCH.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

/// Why the display did not start, recorded in `stopped.json`.
struct StartFailure {
    reason: String,
    xvfb_status: Option<i32>,
}

impl From<String> for StartFailure {
    fn from(reason: String) -> Self {
        Self {
            reason,
            xvfb_status: None,
        }
    }
}

fn io_failure(what: &str) -> impl FnOnce(io::Error) -> StartFailure + '_ {
    move |e| StartFailure::from(format!("{what}: {e}"))
}

/// The keeper's entry point; returns the process exit code.
pub fn run(state_dir: &Path) -> i32 {
    let state = StateDir::from_abs(state_dir);
    let keeper_pid = std::process::id() as i32;
    let mut procs = RealProcesses::default();
    match start(&state, keeper_pid, &mut procs) {
        Ok(mut watch) => {
            let stop = watch.run(&mut RealClock, &mut procs);
            eprintln!("tool-computer keeper: stopped: {stop:?}");
            0
        }
        Err(failure) => {
            eprintln!("tool-computer keeper: {}", failure.reason);
            let _ = state.write_stopped(&Stopped {
                reason: failure.reason,
                was_running: false,
                xvfb_status: failure.xvfb_status,
            });
            1
        }
    }
}

/// Sets what every child of the keeper shares: its own process group, and SIGTERM when the
/// keeper dies, so no program outlives the keeper that would stop it.
fn detach_with_parent(command: &mut Command) {
    command.process_group(0);
    // SAFETY: prctl is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

/// A program on the display: the work folder as its directory, and `DISPLAY` and `XAUTHORITY`
/// naming this display alone.
fn display_command(
    program: &Path,
    args: &[String],
    workdir: &Path,
    display: u16,
    xauth: &Path,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(workdir)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .env_remove("WAYLAND_DISPLAY")
        .env("DISPLAY", format!("127.0.0.1:{display}"))
        .env("XAUTHORITY", xauth)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    detach_with_parent(&mut command);
    command
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn start(
    state: &StateDir,
    keeper_pid: i32,
    procs: &mut RealProcesses,
) -> Result<Watch, StartFailure> {
    let workdir = std::env::current_dir().map_err(io_failure("cannot read the work folder"))?;
    let runtime_dir = std::env::var_os("MURMUR_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| StartFailure::from("MURMUR_RUNTIME_DIR is not set".to_string()))?;
    let grants = Grants::from_env();
    let config = config::parse(
        std::env::var("MURMUR_ARTIFACT_CONFIG").ok().as_deref(),
        &grants,
    )?;
    let resolved = grants::resolve(&grants, &config.launch, std::env::var_os("PATH").as_ref())?;
    let keeper_log =
        CappedLog::open(&state.path(KEEPER_LOG)).map_err(io_failure("cannot open keeper.log"))?;

    let mut cookie = [0u8; 16];
    getrandom::getrandom(&mut cookie)
        .map_err(|e| StartFailure::from(format!("cannot draw a cookie: {e}")))?;
    let xauth = state.path(XAUTHORITY);
    state::write_atomic(&xauth, &state::xauthority_bytes(None, &cookie), 0o600)
        .map_err(io_failure("cannot write Xauthority"))?;

    let (xvfb_pid, display) = start_xvfb(&resolved.xvfb, &xauth, &workdir, &keeper_log, procs)?;
    state::write_atomic(
        &xauth,
        &state::xauthority_bytes(Some(display), &cookie),
        0o600,
    )
    .map_err(io_failure("cannot rewrite Xauthority"))?;

    let x = connect_with_retry(display, &cookie).map_err(|reason| {
        procs.signal_group(xvfb_pid, libc::SIGKILL);
        StartFailure::from(format!("Xvfb started display {display}, but {reason}"))
    })?;

    let mut children = Vec::new();
    let mut window_manager = None;
    if let Some((name, path)) = &resolved.window_manager {
        match display_command(path, &[], &workdir, display, &xauth).spawn() {
            Ok(mut child) => {
                keeper_log.drain(child.stdout.take().expect("piped"));
                keeper_log.drain(child.stderr.take().expect("piped"));
                let pid = child.id() as i32;
                children.push(Watched {
                    pid,
                    index: None,
                    argv0: name.clone(),
                    done: false,
                });
                window_manager = Some(WindowManager {
                    name: name.clone(),
                    pid,
                });
            }
            Err(e) => eprintln!("tool-computer keeper: cannot start {name}: {e}"),
        }
    }

    let mut launched = Vec::new();
    for (index, (argv, path)) in config.launch.iter().zip(&resolved.launch).enumerate() {
        let log_name = format!("launch-{index}-{}.log", argv[0]);
        let log = CappedLog::open(&state.path(&log_name))
            .map_err(io_failure("cannot open a launch log"))?;
        let pid = match display_command(path, &argv[1..], &workdir, display, &xauth).spawn() {
            Ok(mut child) => {
                log.drain(child.stdout.take().expect("piped"));
                log.drain(child.stderr.take().expect("piped"));
                let pid = child.id() as i32;
                children.push(Watched {
                    pid,
                    index: Some(index),
                    argv0: argv[0].clone(),
                    done: false,
                });
                pid
            }
            Err(e) => {
                log.write(format!("tool-computer: cannot start {}: {e}\n", argv[0]).as_bytes());
                let _ = state.append_exit(&Exit {
                    index: Some(index),
                    argv0: argv[0].clone(),
                    status: 127,
                });
                0
            }
        };
        launched.push(Launched {
            index,
            argv: argv.clone(),
            pid,
            log: state.rel_path(&log_name),
        });
    }

    if !children.is_empty() {
        wait_for_windows(&x);
    }
    drop(x);

    let mut watch = Watch {
        state: state.clone(),
        runtime_dir,
        keeper_pid,
        xvfb_pid,
        children,
    };
    watch.reap(procs);
    let info = DisplayInfo {
        display,
        address: format!("127.0.0.1:{display}"),
        width: DISPLAY_WIDTH,
        height: DISPLAY_HEIGHT,
        keeper_pid,
        xvfb_pid,
        window_manager,
        launched,
        started_at: now_unix(),
    };
    state
        .write_display(&info)
        .map_err(io_failure("cannot write display.json"))?;
    Ok(watch)
}

/// Starts Xvfb and reads the display number it picked from `-displayfd`.
fn start_xvfb(
    xvfb: &Path,
    xauth: &Path,
    workdir: &Path,
    keeper_log: &CappedLog,
    procs: &mut RealProcesses,
) -> Result<(i32, u16), StartFailure> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 writes two descriptors into `fds`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io_failure("cannot make the -displayfd pipe")(
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: both descriptors were just created and are owned here alone.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let write_fd = fds[1];

    let mut command = Command::new(xvfb);
    command
        .args(["-displayfd", &write_fd.to_string()])
        .args([
            "-screen",
            "0",
            &format!("{DISPLAY_WIDTH}x{DISPLAY_HEIGHT}x24"),
        ])
        .arg("-auth")
        .arg(xauth)
        .args(["-nolisten", "unix", "-nolisten", "local", "-listen", "tcp"])
        .args(["-noreset", "-dpi", "96", "+extension", "XTEST"])
        .current_dir(workdir)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: fcntl is async-signal-safe; it clears close-on-exec on the pipe's write end so
    // Xvfb inherits it as its -displayfd.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(write_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    detach_with_parent(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| StartFailure::from(format!("Xvfb could not be started: {e}")))?;
    drop(write_end);
    keeper_log.drain(child.stdout.take().expect("piped"));
    keeper_log.drain(child.stderr.take().expect("piped"));
    let pid = child.id() as i32;
    drop(child);

    let mut reader = std::fs::File::from(read_end);
    let deadline = Instant::now() + DISPLAY_NUMBER_WAIT;
    let mut got = Vec::new();
    loop {
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&reader),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready > 0 {
            let mut buf = [0u8; 32];
            let n = reader.read(&mut buf).unwrap_or(0);
            got.extend_from_slice(&buf[..n]);
            if let Some(line) = got
                .split(|b| *b == b'\n')
                .next()
                .filter(|_| got.contains(&b'\n'))
            {
                if let Some(display) = std::str::from_utf8(line)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                {
                    return Ok((pid, display));
                }
            }
            if n == 0 {
                // End of file with no number: Xvfb is exiting. Give it a moment to be reaped.
                for _ in 0..20 {
                    if let Some(status) = procs.try_wait(pid) {
                        return Err(StartFailure {
                            reason: format!("Xvfb exited with status {status}"),
                            xvfb_status: Some(status),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                procs.signal_group(pid, libc::SIGKILL);
                return Err(StartFailure::from(
                    "Xvfb closed -displayfd without a display number".to_string(),
                ));
            }
        }
        if let Some(status) = procs.try_wait(pid) {
            return Err(StartFailure {
                reason: format!("Xvfb exited with status {status}"),
                xvfb_status: Some(status),
            });
        }
        if Instant::now() >= deadline {
            procs.signal_group(pid, libc::SIGKILL);
            return Err(StartFailure::from(format!(
                "Xvfb gave no display number within {} s",
                DISPLAY_NUMBER_WAIT.as_secs()
            )));
        }
    }
}

fn connect_with_retry(display: u16, cookie: &[u8]) -> Result<X11, String> {
    let mut last = String::new();
    for _ in 0..30 {
        match X11::connect(display, cookie) {
            Ok(x) => return Ok(x),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

/// Waits until the count of viewable top-level windows has held for 500 ms, at most 5 s.
fn wait_for_windows(x: &X11) {
    let started = Instant::now();
    let mut count = x.viewable_top_level_windows().unwrap_or(0);
    let mut since = Instant::now();
    while started.elapsed() < WINDOWS_CAP {
        std::thread::sleep(Duration::from_millis(100));
        let now = x.viewable_top_level_windows().unwrap_or(count);
        if now != count {
            count = now;
            since = Instant::now();
        } else if since.elapsed() >= WINDOWS_STABLE && count > 0 {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeClock {
        slept: Duration,
    }

    impl Clock for FakeClock {
        fn sleep(&mut self, duration: Duration) {
            self.slept += duration;
        }
    }

    /// Each pid exits with its status once the given number of `try_wait` asks have passed.
    #[derive(Default)]
    struct FakeProcesses {
        exits: HashMap<i32, (usize, i32)>,
        asks: HashMap<i32, usize>,
        signals: Vec<(i32, i32)>,
        /// Pids that exit as soon as they get SIGTERM.
        obey_term: Vec<i32>,
    }

    impl ProcessTable for FakeProcesses {
        fn try_wait(&mut self, pid: i32) -> Option<i32> {
            let asked = self.asks.entry(pid).or_default();
            *asked += 1;
            match self.exits.get(&pid) {
                Some((after, status)) if *asked > *after => Some(*status),
                _ => None,
            }
        }

        fn signal_group(&mut self, pid: i32, signal: i32) {
            self.signals.push((pid, signal));
            if signal == libc::SIGTERM && self.obey_term.contains(&pid) {
                self.exits.insert(pid, (0, 128 + libc::SIGTERM));
            }
            if signal == libc::SIGKILL {
                self.exits.insert(pid, (0, 128 + libc::SIGKILL));
            }
        }
    }

    fn watch(dir: &Path) -> (Watch, PathBuf) {
        let runtime = dir.join("sessions/ses_t/runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let state = StateDir::new(&dir.join("work"), "ses_t");
        state.create().unwrap();
        let info = DisplayInfo {
            display: 5,
            address: "127.0.0.1:5".into(),
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
            keeper_pid: 1,
            xvfb_pid: 100,
            window_manager: None,
            launched: Vec::new(),
            started_at: 0,
        };
        state.write_display(&info).unwrap();
        let watch = Watch {
            state,
            runtime_dir: runtime.clone(),
            keeper_pid: 1,
            xvfb_pid: 100,
            children: vec![Watched {
                pid: 200,
                index: Some(0),
                argv0: "xmessage".into(),
                done: false,
            }],
        };
        (watch, runtime)
    }

    #[test]
    fn a_gone_runtime_folder_stops_everything_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watch, runtime) = watch(dir.path());
        std::fs::remove_dir_all(&runtime).unwrap();
        let mut procs = FakeProcesses {
            obey_term: vec![100, 200],
            ..Default::default()
        };
        let stop = watch.run(&mut FakeClock::default(), &mut procs);
        assert_eq!(stop, Stop::SessionEnded);
        assert_eq!(
            procs.signals,
            vec![(100, libc::SIGTERM), (200, libc::SIGTERM)],
            "one SIGTERM per group, and no SIGKILL for groups that obeyed"
        );
        assert!(watch.state.read_display().is_none());
        assert!(watch.state.read_stopped().unwrap().was_running);
    }

    #[test]
    fn a_child_ignoring_sigterm_gets_sigkill_after_three_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watch, runtime) = watch(dir.path());
        std::fs::remove_dir_all(&runtime).unwrap();
        let mut procs = FakeProcesses {
            obey_term: vec![100],
            ..Default::default()
        };
        let mut clock = FakeClock::default();
        watch.run(&mut clock, &mut procs);
        assert_eq!(
            procs.signals,
            vec![
                (100, libc::SIGTERM),
                (200, libc::SIGTERM),
                (200, libc::SIGKILL)
            ]
        );
        assert!(clock.slept >= TERM_GRACE);
    }

    #[test]
    fn xvfb_exiting_writes_stopped_json_and_ends_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watch, _runtime) = watch(dir.path());
        let mut procs = FakeProcesses {
            obey_term: vec![200],
            ..Default::default()
        };
        procs.exits.insert(100, (2, 1));
        let mut clock = FakeClock::default();
        let stop = watch.run(&mut clock, &mut procs);
        assert_eq!(stop, Stop::XvfbExited(1));
        assert_eq!(
            clock.slept,
            Duration::from_secs(2) + Duration::from_millis(100)
        );
        let stopped = watch.state.read_stopped().unwrap();
        assert_eq!(stopped.reason, "Xvfb exited with status 1");
        assert_eq!(stopped.xvfb_status, Some(1));
        assert!(watch.state.read_display().is_none());
        assert_eq!(procs.signals, vec![(200, libc::SIGTERM)]);
    }

    #[test]
    fn child_exits_become_exit_lines() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watch, runtime) = watch(dir.path());
        watch.children.push(Watched {
            pid: 300,
            index: None,
            argv0: "twm".into(),
            done: false,
        });
        let mut procs = FakeProcesses::default();
        procs.exits.insert(200, (1, 0));
        procs.exits.insert(300, (0, 137));
        watch.reap(&mut procs);
        watch.reap(&mut procs);
        watch.reap(&mut procs);
        let exits = watch.state.take_new_exits();
        assert_eq!(
            exits,
            vec![
                Exit {
                    index: None,
                    argv0: "twm".into(),
                    status: 137
                },
                Exit {
                    index: Some(0),
                    argv0: "xmessage".into(),
                    status: 0
                },
            ]
        );
        drop(runtime);
    }

    #[test]
    fn wait_statuses_decode_to_codes() {
        assert_eq!(decode_status(0), 0);
        assert_eq!(decode_status(3 << 8), 3);
        assert_eq!(decode_status(libc::SIGKILL), 128 + libc::SIGKILL);
    }
}
