//! The session's state folder, `<work folder>/.tool-browser/<session-id>/`, and the
//! `supervisor.json` record the call process finds the supervisor by.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// The folder under the work folder that holds every session's state.
pub const STATE_ROOT: &str = ".tool-browser";

#[derive(Debug, Clone)]
pub struct StateDir {
    /// Absolute path of `<work folder>/.tool-browser/<session-id>`.
    pub root: PathBuf,
    /// The same, relative to the work folder, as results name it.
    pub rel: String,
}

impl StateDir {
    pub fn new(workdir: &Path, session: &str) -> Self {
        Self {
            root: workdir.join(STATE_ROOT).join(session),
            rel: format!("{STATE_ROOT}/{session}"),
        }
    }

    /// For the supervisor, which is handed the absolute folder.
    pub fn from_root(root: PathBuf) -> Self {
        let session = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".into());
        Self {
            root,
            rel: format!("{STATE_ROOT}/{session}"),
        }
    }

    pub fn profile(&self) -> PathBuf {
        self.root.join("profile")
    }
    pub fn info(&self) -> PathBuf {
        self.root.join("supervisor.json")
    }
    pub fn log(&self) -> PathBuf {
        self.root.join("supervisor.log")
    }
    pub fn start_lock(&self) -> PathBuf {
        self.root.join("start.lock")
    }
    pub fn start_failed(&self) -> PathBuf {
        self.root.join("start-failed")
    }
    pub fn downloads(&self) -> PathBuf {
        self.root.join("downloads")
    }

    pub fn read_info(&self) -> Option<SupervisorInfo> {
        serde_json::from_slice(&fs::read(self.info()).ok()?).ok()
    }

    /// Write `supervisor.json` with mode 0600, atomically.
    pub fn write_info(&self, info: &SupervisorInfo) -> std::io::Result<()> {
        let tmp = self.root.join("supervisor.json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(serde_json::to_string_pretty(info)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, self.info())
    }

    /// Remove what a running browser leaves: `profile/` and `supervisor.json`.
    pub fn remove_live_files(&self) {
        let _ = fs::remove_file(self.info());
        let _ = fs::remove_dir_all(self.profile());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserInfo {
    /// `local` or `remote`.
    pub kind: String,
    /// The program run, or the endpoint connected to.
    pub program: String,
    pub version: String,
    /// The browser's pid; absent for a remote browser.
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorInfo {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub browser: BrowserInfo,
    /// Seconds since the Unix epoch.
    pub started_at: u64,
}

/// Whether the process `pid` runs with `needle` in its command line. Linux only: elsewhere
/// there is no `/proc` to ask, and the answer is no.
pub fn cmdline_contains(pid: u32, needle: &Path) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let needle = needle.to_string_lossy();
    raw.split(|b| *b == 0)
        .any(|arg| String::from_utf8_lossy(arg).contains(needle.as_ref()))
}

/// Send SIGKILL to the process group `pgid` leads.
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: kill(2) takes plain integers and touches no memory of ours.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// Send SIGKILL to `pid`.
pub fn kill(pid: u32) {
    if pid > 1 {
        // SAFETY: kill(2) takes plain integers and touches no memory of ours.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}
