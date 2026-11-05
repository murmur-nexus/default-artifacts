//! What one call runs git and touches files through: the [`GitEnv`] the caller provides, and the
//! [`Git`] wrapper the handlers call, which turns each `murmur:process` outcome into the
//! [`GitOut`] the native tool's `Command::new("git")` gave and remembers what ends the call.

use std::{
    cell::{Cell, RefCell},
    fs,
    io::Write,
    path::PathBuf,
};

use murmur_host_roots::{locate, HostRoot, Located};
use serde_json::Value;

use crate::out::{err, err_result};

/// How one `git` run ended, as `murmur:process` `run` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRun {
    /// git ran to its end. A non-zero `exit_code` is git's own failure, not an error here; a
    /// death by signal `n` is `128 + n`. Each stream holds at most its first 1 MiB, and
    /// `*_truncated` says the rest was discarded.
    Ran {
        exit_code: i32,
        timed_out: bool,
        stdout: Vec<u8>,
        stdout_truncated: bool,
        stderr: Vec<u8>,
        stderr_truncated: bool,
    },
    /// A rule refused the command before it started: `rule` is the rule's name
    /// (`capabilities.shell.allow`, `on-shell`, `capabilities.filesystem.read_only`, …) and
    /// `reason` what it found.
    Refused { rule: String, reason: String },
    /// The tool call's deadline passed before git finished.
    TimedOut,
    /// git could not be started; the text says why.
    Failed(String),
}

/// Everything outside the tool's own logic that a call reaches: git, and the files it opens.
///
/// The component implements it over the `murmur:process` import and its WASI preopens; host
/// tests implement it over `std::process::Command` and a scratch work folder.
pub trait GitEnv {
    /// Run `git` with `args` in the work folder: `murmur:process` `run` with the program `git`,
    /// exactly these arguments, no extra environment, no stdin and no time limit of its own.
    fn run_git(&self, args: &[&str]) -> GitRun;

    /// The host folders the session opened to the tool, in preopen order; empty when it opened
    /// none.
    fn roots(&self) -> &[HostRoot];

    /// The filesystem path that opens `open`: a path relative to the work folder, or a
    /// `murmur-host…` path [`murmur_host_roots::locate`] returned.
    fn fs_path(&self, open: &str) -> PathBuf;
}

/// What one git command printed, as the native tool's runner kept it: exit 0 is `success`, and
/// each stream is lossily decoded with its trailing newlines removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitOut {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// What ended a call before its handler's own result: the first refusal or time-out.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
    Refused { rule: String, reason: String },
    TimedOut { subcommand: String },
}

/// The git runner one call's handlers share.
///
/// A refusal or time-out on any command ends the call: every later [`Git::git`] runs nothing and
/// returns a failed [`GitOut`], and [`Git::finish`] puts the stop's line in place of whatever the
/// handler returned. A stream cut at 1 MiB marks the call as truncated.
pub struct Git<'a> {
    env: &'a dyn GitEnv,
    stop: RefCell<Option<Stop>>,
    truncated: Cell<bool>,
}

impl<'a> Git<'a> {
    pub fn new(env: &'a dyn GitEnv) -> Self {
        Git {
            env,
            stop: RefCell::new(None),
            truncated: Cell::new(false),
        }
    }

    pub fn env(&self) -> &'a dyn GitEnv {
        self.env
    }

    /// Run `git <args>` in the work folder.
    ///
    /// - A run that ends gives `success` for exit 0 and the streams it printed. A cut stream
    ///   keeps its bytes up to the last newline, so a reader of lines sees only complete ones.
    /// - `failed(text)` gives a failed run whose stderr is `failed to execute git: <text>`, the
    ///   native runner's text for a spawn error.
    /// - A refusal or time-out stops the call (see [`Git`]) and gives a failed, empty run.
    pub fn git(&self, args: &[&str]) -> GitOut {
        let stopped = GitOut::default();
        if self.stop.borrow().is_some() {
            return stopped;
        }
        match self.env.run_git(args) {
            GitRun::Ran {
                exit_code,
                timed_out,
                stdout,
                stdout_truncated,
                stderr,
                stderr_truncated,
            } => {
                if timed_out {
                    self.stop_with(Stop::TimedOut {
                        subcommand: subcommand_of(args),
                    });
                    return stopped;
                }
                GitOut {
                    success: exit_code == 0,
                    stdout: self.text(stdout, stdout_truncated),
                    stderr: self.text(stderr, stderr_truncated),
                }
            }
            GitRun::Refused { rule, reason } => {
                self.stop_with(Stop::Refused { rule, reason });
                stopped
            }
            GitRun::TimedOut => {
                self.stop_with(Stop::TimedOut {
                    subcommand: subcommand_of(args),
                });
                stopped
            }
            GitRun::Failed(text) => GitOut {
                success: false,
                stdout: String::new(),
                stderr: format!("failed to execute git: {text}"),
            },
        }
    }

    fn stop_with(&self, stop: Stop) {
        self.stop.borrow_mut().get_or_insert(stop);
    }

    fn text(&self, mut bytes: Vec<u8>, truncated: bool) -> String {
        if truncated {
            self.truncated.set(true);
            let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            bytes.truncate(keep);
        }
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\n')
            .to_string()
    }

    /// The call's result: the stop's line when a git command was refused or timed out, else the
    /// handler's `result`. When a stream was cut, the envelope gains `"git_output_truncated":
    /// true`, which [`crate::to_tool_result`] turns into the tool result's `truncated` flag.
    pub fn finish(self, result: Value) -> Value {
        let mut result = match self.stop.into_inner() {
            Some(Stop::Refused { rule, reason }) => err_result(
                err::REFUSED,
                format!("tool-git: murmur refused to run git: {rule}: {reason}"),
            ),
            Some(Stop::TimedOut { subcommand }) => {
                err_result(err::TIMED_OUT, format!("git {subcommand} timed out"))
            }
            None => result,
        };
        if self.truncated.get() {
            result[crate::TRUNCATED_KEY] = Value::Bool(true);
        }
        result
    }
}

/// The git subcommand `args` run: the first argument after any `-C <dir>`, `--git-dir=…` and
/// `--work-tree=…`.
fn subcommand_of(args: &[&str]) -> String {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "-C" => {
                rest.next();
            }
            a if a.starts_with("--git-dir=") || a.starts_with("--work-tree=") => {}
            a => return a.to_string(),
        }
    }
    String::new()
}

/// The call's `log_path`, where the handlers append a line per step, best effort.
pub struct Log<'a> {
    path: Option<&'a str>,
    env: &'a dyn GitEnv,
}

impl<'a> Log<'a> {
    pub fn new(path: Option<&'a str>, env: &'a dyn GitEnv) -> Self {
        Log { path, env }
    }
}

/// Append `message` to the call's `log_path`. A relative path opens in the work folder and an
/// absolute one through the host folder covering it; one that opens nowhere, or any write error,
/// is skipped silently and never fails the call.
pub fn log_write(log: &Log, message: &str) {
    let Some(path) = log.path else { return };
    if path.is_empty() {
        return;
    }
    let open = match locate(path, log.env.roots()) {
        Ok(Located::Relative(open)) => open,
        Ok(Located::Host { open, .. }) => open,
        Err(_) => return,
    };
    let mut file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log.env.fs_path(&open))
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = file.write_all(message.as_bytes());
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A `GitEnv` that answers each run with the next scripted outcome and records the argv.
    struct Scripted {
        answers: RefCell<VecDeque<GitRun>>,
        argvs: RefCell<Vec<Vec<String>>>,
    }

    impl Scripted {
        fn new(answers: Vec<GitRun>) -> Self {
            Scripted {
                answers: RefCell::new(answers.into()),
                argvs: RefCell::new(Vec::new()),
            }
        }
    }

    impl GitEnv for Scripted {
        fn run_git(&self, args: &[&str]) -> GitRun {
            self.argvs
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect());
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("no scripted answer left")
        }
        fn roots(&self) -> &[HostRoot] {
            &[]
        }
        fn fs_path(&self, open: &str) -> PathBuf {
            PathBuf::from(open)
        }
    }

    fn ran(code: i32, stdout: &[u8], stdout_truncated: bool) -> GitRun {
        GitRun::Ran {
            exit_code: code,
            timed_out: false,
            stdout: stdout.to_vec(),
            stdout_truncated,
            stderr: Vec::new(),
            stderr_truncated: false,
        }
    }

    #[test]
    fn a_run_keeps_the_native_runners_text() {
        let env = Scripted::new(vec![ran(0, b"a\nb\n\n", false), ran(1, b"", false)]);
        let git = Git::new(&env);
        assert_eq!(
            git.git(&["status"]),
            GitOut {
                success: true,
                stdout: "a\nb".into(),
                stderr: String::new()
            }
        );
        assert!(!git.git(&["status"]).success);
        assert_eq!(git.finish(Value::Null), Value::Null);
    }

    #[test]
    fn a_cut_stream_keeps_only_complete_lines_and_marks_the_call() {
        let env = Scripted::new(vec![ran(0, b"one\ntwo\nthr", true)]);
        let git = Git::new(&env);
        assert_eq!(git.git(&["log"]).stdout, "one\ntwo");
        let out = git.finish(serde_json::json!({"summary": "x"}));
        assert_eq!(out[crate::TRUNCATED_KEY], true);
    }

    #[test]
    fn a_refusal_ends_the_call_and_runs_nothing_more() {
        let env = Scripted::new(vec![
            ran(0, b"", false),
            GitRun::Refused {
                rule: "on-shell".into(),
                reason: "no force push".into(),
            },
        ]);
        let git = Git::new(&env);
        assert!(git.git(&["-C", "r", "status"]).success);
        assert!(!git.git(&["-C", "r", "push", "--force"]).success);
        assert_eq!(git.git(&["-C", "r", "status"]), GitOut::default());
        assert_eq!(env.argvs.borrow().len(), 2);
        let out = git.finish(serde_json::json!({"status": "passed"}));
        assert_eq!(out["error_kind"], err::REFUSED);
        assert_eq!(
            out["summary"],
            "tool-git: murmur refused to run git: on-shell: no force push"
        );
    }

    #[test]
    fn a_time_out_names_the_subcommand() {
        for (answer, args, line) in [
            (
                GitRun::TimedOut,
                vec!["-C", "/r", "fetch", "origin"],
                "git fetch timed out",
            ),
            (
                GitRun::Ran {
                    exit_code: 137,
                    timed_out: true,
                    stdout: Vec::new(),
                    stdout_truncated: false,
                    stderr: Vec::new(),
                    stderr_truncated: false,
                },
                vec!["clone", "u", "d"],
                "git clone timed out",
            ),
            (
                GitRun::TimedOut,
                vec!["--git-dir=/g", "--work-tree=/t", "rev-parse"],
                "git rev-parse timed out",
            ),
        ] {
            let env = Scripted::new(vec![answer]);
            let git = Git::new(&env);
            git.git(&args);
            let out = git.finish(Value::Null);
            assert_eq!(out["error_kind"], err::TIMED_OUT);
            assert_eq!(out["summary"], line);
        }
    }

    #[test]
    fn a_failed_start_reads_as_the_native_spawn_error() {
        let env = Scripted::new(vec![GitRun::Failed("No such file or directory".into())]);
        let git = Git::new(&env);
        assert_eq!(
            git.git(&["status"]).stderr,
            "failed to execute git: No such file or directory"
        );
        assert_eq!(git.finish(Value::Null), Value::Null);
    }
}
