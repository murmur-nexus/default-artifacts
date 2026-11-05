//! The test host for `murmur:process/process@0.1.0` `run`, shared by the component harness and
//! the host runner of the core.
//!
//! It runs the real program the way murmur's `run` does, minus containment: in the work folder,
//! with an environment cleared down to `PATH`, `HOME=<scratch>/home` and `GIT_CONFIG_NOSYSTEM=1`
//! plus the request's own variables, each stream cut at its first 1 MiB, and killed with its
//! process group when its time runs out. A program outside `shell_allow` is refused with murmur's
//! rule and reason. A test can inject what murmur's own checks would answer — an `on-shell`
//! refusal, a `failed` text, a time-out — or a lower output cap, for the commands it picks.

use std::{
    io::{Read, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

/// What murmur keeps of each stream.
pub const STREAM_CAP: usize = 1 << 20;

/// A `run-request`, as the guest sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout_ms: Option<u64>,
}

/// A `run-output`.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

/// The `process-error`s `run` answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Refused { rule: String, reason: String },
    TimedOut,
    Failed(String),
}

/// What the host answers for a command a test picked, in place of running it normally.
#[derive(Clone, Debug)]
pub enum Inject {
    /// `refused` with this rule and reason; nothing runs.
    Refuse { rule: String, reason: String },
    /// `failed(text)`; nothing runs.
    Fail(String),
    /// The `timed-out` error; nothing runs.
    TimeOut,
    /// A `run-output` with `timed-out: true`, as for a program killed at its deadline.
    KilledAtDeadline,
    /// Run it, keeping only the first `n` bytes of each stream.
    CapAt(usize),
}

impl Inject {
    pub fn on_shell(reason: &str) -> Self {
        Inject::Refuse {
            rule: "on-shell".into(),
            reason: reason.into(),
        }
    }
}

type Matcher = Box<dyn Fn(&[String]) -> bool + Send + Sync>;

/// One recorded `run`: the request, and whether a program ran for it.
#[derive(Clone, Debug)]
pub struct Record {
    pub request: Request,
    pub ran: bool,
}

pub struct ProcessHost {
    pub work: PathBuf,
    pub home: PathBuf,
    shell_allow: Mutex<Vec<String>>,
    injections: Mutex<Vec<(Matcher, Inject)>>,
    records: Mutex<Vec<Record>>,
    /// The tool call's deadline: the limit of a request with `timeout-ms: none`.
    pub deadline: Duration,
}

impl ProcessHost {
    pub fn new(work: PathBuf, home: PathBuf) -> Self {
        ProcessHost {
            work,
            home,
            shell_allow: Mutex::new(vec!["git".into()]),
            injections: Mutex::new(Vec::new()),
            records: Mutex::new(Vec::new()),
            deadline: Duration::from_secs(120),
        }
    }

    pub fn set_shell_allow(&self, allow: &[&str]) {
        *self.shell_allow.lock().unwrap() = allow.iter().map(|s| s.to_string()).collect();
    }

    /// Answer every later command whose args `when` accepts with `what`.
    pub fn inject(&self, when: impl Fn(&[String]) -> bool + Send + Sync + 'static, what: Inject) {
        self.injections.lock().unwrap().push((Box::new(when), what));
    }

    pub fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }

    pub fn run(&self, request: Request) -> Result<Output, Error> {
        let mut record = Record {
            request: request.clone(),
            ran: false,
        };
        let answer = self.answer(&request, &mut record.ran);
        self.records.lock().unwrap().push(record);
        answer
    }

    fn answer(&self, request: &Request, ran: &mut bool) -> Result<Output, Error> {
        if !self.shell_allow.lock().unwrap().contains(&request.program) {
            return Err(Error::Refused {
                rule: "capabilities.shell.allow".into(),
                reason: format!(
                    "binary '{}' is not in capabilities.shell.allow",
                    request.program
                ),
            });
        }
        let mut cap = STREAM_CAP;
        let injected = self
            .injections
            .lock()
            .unwrap()
            .iter()
            .find(|(when, _)| when(&request.args))
            .map(|(_, what)| what.clone());
        match injected {
            Some(Inject::Refuse { rule, reason }) => return Err(Error::Refused { rule, reason }),
            Some(Inject::Fail(text)) => return Err(Error::Failed(text)),
            Some(Inject::TimeOut) => return Err(Error::TimedOut),
            Some(Inject::KilledAtDeadline) => {
                return Ok(Output {
                    exit_code: 128 + 9,
                    timed_out: true,
                    ..Output::default()
                })
            }
            Some(Inject::CapAt(n)) => cap = n,
            None => {}
        }

        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(request.env.iter().map(|(k, v)| (k, v)))
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().map_err(|e| Error::Failed(e.to_string()))?;
        *ran = true;
        if let Some(bytes) = &request.stdin {
            let mut stdin = child.stdin.take().unwrap();
            let bytes = bytes.clone();
            thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            });
        }
        let keep = |mut stream: Box<dyn Read + Send>| {
            thread::spawn(move || {
                let (mut kept, mut cut) = (Vec::new(), false);
                let mut buf = [0u8; 65536];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                    cut |= n > room;
                }
                (kept, cut)
            })
        };
        let stdout = keep(Box::new(child.stdout.take().unwrap()));
        let stderr = keep(Box::new(child.stderr.take().unwrap()));

        let limit = request.timeout_ms.map_or(self.deadline, |ms| {
            Duration::from_millis(ms).min(self.deadline)
        });
        let started = Instant::now();
        let (status, timed_out) = loop {
            if let Some(status) = child.try_wait().map_err(|e| Error::Failed(e.to_string()))? {
                break (status, false);
            }
            if started.elapsed() >= limit {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .status();
                break (
                    child.wait().map_err(|e| Error::Failed(e.to_string()))?,
                    true,
                );
            }
            thread::sleep(Duration::from_millis(2));
        };
        let (stdout, stdout_truncated) = stdout.join().unwrap();
        let (stderr, stderr_truncated) = stderr.join().unwrap();
        Ok(Output {
            exit_code: status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            timed_out,
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
        })
    }
}
