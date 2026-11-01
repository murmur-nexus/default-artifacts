//! The call side of the display: reuse the keeper's display, or start a keeper and wait for it.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::grants::{Grants, WINDOW_MANAGERS};
use crate::input::and_list;
use crate::state::{
    self, DisplayInfo, StateDir, DISPLAY_JSON, EXITS, EXITS_SEEN, KEEPER_LOG, START_LOCK,
    STOPPED_JSON, XAUTHORITY,
};
use crate::xbackend::X11;

const START_WAIT: Duration = Duration::from_secs(15);
const STALE_LOCK: Duration = Duration::from_secs(30);

pub const RESTART_LINE: &str = "the previous display stopped; programs on it are gone";

/// A display ready for this call.
pub struct Ready {
    pub x: X11,
    pub info: DisplayInfo,
    /// Lines that open the result: the restart line and the banner, when this call started the
    /// display.
    pub preface: Vec<String>,
}

/// Whether `pid` is a running (not zombie) process.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists {
        return false;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .is_none_or(|state| state != 'Z'),
        // No /proc to read: trust kill(2).
        Err(_) => true,
    }
}

fn connect(state: &StateDir, info: &DisplayInfo) -> Result<X11, String> {
    let cookie = state::read_cookie(&state.path(XAUTHORITY))
        .ok_or_else(|| "the display's Xauthority holds no cookie".to_string())?;
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        match X11::connect(info.display, &cookie) {
            Ok(x) => return Ok(x),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The line that opens the first result of a new display.
pub fn banner(info: &DisplayInfo, state: &StateDir) -> String {
    let wm = match &info.window_manager {
        Some(wm) => wm.name.clone(),
        None => format!("none (grant one of {})", and_list_or(&WINDOW_MANAGERS)),
    };
    let launched = if info.launched.is_empty() {
        "nothing".to_string()
    } else {
        info.launched
            .iter()
            .map(|l| l.argv[0].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "display: Xvfb {address}, {w}x{h}x24; window manager {wm}; launched {launched}; shell \
         commands reach it with DISPLAY={address} XAUTHORITY={xauth}",
        address = info.address,
        w = info.width,
        h = info.height,
        xauth = state.path(XAUTHORITY).display(),
    )
}

/// `a, b or c`.
fn and_list_or(items: &[&str]) -> String {
    and_list(items).replacen(" and ", " or ", 1)
}

fn stop_stale_keeper(pid: i32) {
    if !pid_alive(pid) {
        return;
    }
    // SAFETY: kill(2) on a pid this tool's keeper recorded.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if pid_alive(pid) {
        // SAFETY: as above.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// `start.lock`, held while this call starts a keeper; removed on drop.
struct StartLock<'a> {
    path: &'a Path,
}

impl Drop for StartLock<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path);
    }
}

enum Lock<'a> {
    Taken(StartLock<'a>),
    /// Another call held the lock and has since let it go.
    Released,
}

fn take_lock(path: &Path) -> Result<Lock<'_>, String> {
    let deadline = Instant::now() + START_WAIT;
    let mut waited = false;
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(if waited {
                    drop(file);
                    let _ = fs::remove_file(path);
                    Lock::Released
                } else {
                    Lock::Taken(StartLock { path })
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let age = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .unwrap_or_default();
                if age > STALE_LOCK {
                    let _ = fs::remove_file(path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "tool-computer: another call has been starting the display for {} s; try \
                         again",
                        age.as_secs()
                    ));
                }
                waited = true;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(format!(
                    "tool-computer: cannot create {}: {e}",
                    path.display()
                ))
            }
        }
    }
}

/// Connects to this session's display, starting one when none runs or the last one stopped.
pub fn ensure(state: &StateDir, workdir: &Path) -> Result<Ready, String> {
    state
        .create()
        .map_err(|e| format!("tool-computer: cannot create {}: {e}", state.rel.display()))?;
    for _ in 0..2 {
        let mut restarted = false;
        if let Some(info) = state.read_display() {
            if pid_alive(info.xvfb_pid) {
                if let Ok(x) = connect(state, &info) {
                    return Ok(Ready {
                        x,
                        info,
                        preface: Vec::new(),
                    });
                }
            }
            stop_stale_keeper(info.keeper_pid);
            let _ = fs::remove_file(state.path(DISPLAY_JSON));
            restarted = true;
        } else if state.read_stopped().is_some_and(|s| s.was_running) {
            restarted = true;
        }

        let lock_path = state.path(START_LOCK);
        let _lock = match take_lock(&lock_path)? {
            Lock::Taken(lock) => lock,
            Lock::Released => continue,
        };
        let _ = fs::remove_file(state.path(STOPPED_JSON));
        if restarted {
            let _ = fs::remove_file(state.path(EXITS));
            let _ = fs::remove_file(state.path(EXITS_SEEN));
        }
        let info = start_keeper(state, workdir)?;
        let x = connect(state, &info)
            .map_err(|e| format!("tool-computer: the new display does not answer: {e}"))?;
        let mut preface = Vec::new();
        if restarted {
            preface.push(RESTART_LINE.to_string());
        }
        preface.push(banner(&info, state));
        return Ok(Ready { x, info, preface });
    }
    Err("tool-computer: another call started the display, but it does not answer".to_string())
}

fn start_failure_line(state: &StateDir, reason: &str) -> String {
    let tail = state.keeper_log_tail(3);
    let tail = if tail.is_empty() {
        "nothing".to_string()
    } else {
        tail.join(" | ")
    };
    let mut line = format!(
        "tool-computer: the display did not start: {reason}; the last lines of {}: {tail}",
        state.rel_path(KEEPER_LOG)
    );
    if let Some(hint) = keymap_hint(state, &Grants::from_env()) {
        line.push_str("; ");
        line.push_str(hint);
    }
    line
}

/// Xvfb compiles its keymap by running `xkbcomp` through `/bin/sh -c`; inside a sealed capsule
/// that exec needs its own grant, and without it Xvfb exits naming only the keyboard.
fn keymap_hint(state: &StateDir, grants: &Grants) -> Option<&'static str> {
    let log = fs::read_to_string(state.path(KEEPER_LOG)).unwrap_or_default();
    (log.contains("Failed to compile keymap") && !grants.granted("sh")).then_some(
        "Xvfb compiles its keymap by running xkbcomp through sh, so add sh to \
         capabilities.shell.allow",
    )
}

/// Re-executes this binary as a detached keeper and waits for its `display.json`.
fn start_keeper(state: &StateDir, workdir: &Path) -> Result<DisplayInfo, String> {
    let exe = std::env::current_exe().map_err(|e| {
        format!("tool-computer: cannot find its own binary to start the keeper: {e}")
    })?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(state.path(KEEPER_LOG))
        .map_err(|e| format!("tool-computer: cannot open keeper.log: {e}"))?;
    let mut command = Command::new(exe);
    command
        .arg("--keep")
        .arg(&state.abs)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // SAFETY: setsid is async-signal-safe; the keeper leads a new session, away from the call's
    // process group and terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut keeper = command
        .spawn()
        .map_err(|e| format!("tool-computer: cannot start the keeper: {e}"))?;

    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(info) = state.read_display() {
            return Ok(info);
        }
        if let Some(stopped) = state.read_stopped() {
            if !stopped.was_running {
                return Err(start_failure_line(state, &stopped.reason));
            }
        }
        if let Ok(Some(status)) = keeper.try_wait() {
            if let Some(info) = state.read_display() {
                return Ok(info);
            }
            let reason = state
                .read_stopped()
                .map(|s| s.reason)
                .unwrap_or_else(|| format!("the keeper exited with {status}"));
            return Err(start_failure_line(state, &reason));
        }
        if Instant::now() >= deadline {
            // SAFETY: the keeper is this call's own child.
            unsafe {
                libc::kill(keeper.id() as i32, libc::SIGTERM);
            }
            return Err(start_failure_line(
                state,
                &format!(
                    "the keeper wrote no display.json within {} s",
                    START_WAIT.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Launched, WindowManager};

    #[test]
    fn the_banner_names_display_wm_launches_and_shell_access() {
        let state = StateDir::new(Path::new("/w"), "ses_x");
        let mut info = DisplayInfo {
            display: 3,
            address: "127.0.0.1:3".into(),
            width: 1440,
            height: 900,
            keeper_pid: 1,
            xvfb_pid: 2,
            window_manager: None,
            launched: Vec::new(),
            started_at: 0,
        };
        assert_eq!(
            banner(&info, &state),
            "display: Xvfb 127.0.0.1:3, 1440x900x24; window manager none (grant one of openbox, \
             fluxbox, icewm, twm or matchbox-window-manager); launched nothing; shell commands \
             reach it with DISPLAY=127.0.0.1:3 XAUTHORITY=/w/.tool-computer/ses_x/Xauthority"
        );
        info.window_manager = Some(WindowManager {
            name: "openbox".into(),
            pid: 4,
        });
        info.launched = vec![Launched {
            index: 0,
            argv: vec!["xmessage".into(), "hi".into()],
            pid: 5,
            log: "x".into(),
        }];
        let line = banner(&info, &state);
        assert!(
            line.contains("window manager openbox; launched xmessage;"),
            "{line}"
        );
    }

    #[test]
    fn a_keymap_failure_without_sh_names_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "ses_x");
        state.create().unwrap();
        fs::write(
            state.path(KEEPER_LOG),
            "XKB: Failed to compile keymap\n(EE) Failed to activate virtual core keyboard: 2\n",
        )
        .unwrap();
        assert!(
            keymap_hint(&state, &Grants::from_names(&["Xvfb", "xkbcomp"]))
                .unwrap()
                .contains("add sh to capabilities.shell.allow")
        );
        assert_eq!(keymap_hint(&state, &Grants::from_names(&["sh"])), None);
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0));
    }
}
