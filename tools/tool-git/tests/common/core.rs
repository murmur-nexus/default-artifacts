//! The portable core run on the host: `tool_git::run` with a `GitEnv` over the same test host
//! for `murmur:process` the component gets, and the work folder and host folders as real paths.
//! It sees the whole envelope, `error_kind` included, which the component never hands the model.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::Value;
use tool_git::{GitEnv, GitRun, HostRoot};

use super::process::{self, ProcessHost};

pub struct HostEnv {
    pub process: Arc<ProcessHost>,
    pub work: PathBuf,
    pub roots: Vec<HostRoot>,
}

impl HostEnv {
    pub fn new(process: &Arc<ProcessHost>, work: &Path, roots: &[(PathBuf, bool)]) -> Self {
        HostEnv {
            process: Arc::clone(process),
            work: work.to_path_buf(),
            roots: roots
                .iter()
                .map(|(path, writable)| {
                    HostRoot::from_preopen(&format!("murmur-host{}", path.display()), *writable)
                        .expect("a host root")
                })
                .collect(),
        }
    }
}

impl GitEnv for HostEnv {
    fn run_git(&self, args: &[&str]) -> GitRun {
        let request = process::Request {
            program: "git".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: Vec::new(),
            stdin: None,
            timeout_ms: None,
        };
        match self.process.run(request) {
            Ok(o) => GitRun::Ran {
                exit_code: o.exit_code,
                timed_out: o.timed_out,
                stdout: o.stdout,
                stdout_truncated: o.stdout_truncated,
                stderr: o.stderr,
                stderr_truncated: o.stderr_truncated,
            },
            Err(process::Error::Refused { rule, reason }) => GitRun::Refused { rule, reason },
            Err(process::Error::TimedOut) => GitRun::TimedOut,
            Err(process::Error::Failed(text)) => GitRun::Failed(text),
        }
    }

    fn roots(&self) -> &[HostRoot] {
        &self.roots
    }

    /// A `murmur-host…` path is the host path after the prefix; anything else is relative to
    /// the work folder.
    fn fs_path(&self, open: &str) -> PathBuf {
        match open.strip_prefix("murmur-host") {
            Some("") => PathBuf::from("/"),
            Some(rest) if rest.starts_with('/') => PathBuf::from(rest),
            _ => self.work.join(open),
        }
    }
}

/// Run one call through the core and return its whole envelope.
pub fn call(env: &HostEnv, data: String, log_path: Option<String>) -> Value {
    tool_git::run(Some(&data), log_path.as_deref(), env)
}
