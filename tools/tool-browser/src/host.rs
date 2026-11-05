//! What the tool needs from its host, as two traits: the session's processes
//! (`murmur:process/process@0.1.0` in the component, a fake in unit tests) and a clock.
//!
//! Everything else in the crate reaches the browser only through these, so the call logic runs
//! and is tested on the host target.

/// `process-error`, as the runtime returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    /// A rule refused the call; nothing ran.
    Refused {
        rule: String,
        reason: String,
    },
    UnknownHandle,
    NotRunning,
    /// Nothing arrived, or the call's time ran out.
    TimedOut,
    /// The program could not be started or reached; the text says why.
    Failed(String),
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { rule, reason } => write!(f, "refused by {rule}: {reason}"),
            Self::UnknownHandle => f.write_str("unknown handle"),
            Self::NotRunning => f.write_str("the process is not running"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Failed(text) => f.write_str(text),
        }
    }
}

/// A session process's state: running, or its exit code (`128 + n` for a death by signal `n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    Running,
    Exited(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub handle: String,
    /// A process under the name was already running; nothing new ran.
    pub already_running: bool,
}

/// What a process printed since the last read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A TCP connection to a port a session process listens on, lasting until the call returns.
pub trait Conn {
    /// Up to `max` bytes, waiting at most `timeout_ms` for the first. An empty list means the
    /// peer closed; [`ProcError::TimedOut`] means nothing arrived in time.
    fn read(&mut self, max: u32, timeout_ms: u32) -> Result<Vec<u8>, ProcError>;
    /// Write all of `bytes`.
    fn write(&mut self, bytes: &[u8]) -> Result<(), ProcError>;
}

/// The session's processes, as `murmur:process` serves them to this tool.
pub trait Processes {
    fn start(
        &mut self,
        name: &str,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Started, ProcError>;
    /// This tool's latest process under `name`, running or exited.
    fn find(&mut self, name: &str) -> Option<String>;
    fn status(&mut self, handle: &str) -> Result<ProcState, ProcError>;
    fn read(&mut self, handle: &str, max: u32) -> Result<Output, ProcError>;
    /// `SIGTERM` to the process group, `SIGKILL` 2 s later; the final state.
    fn stop(&mut self, handle: &str) -> Result<ProcState, ProcError>;
    fn connect(&mut self, handle: &str, port: u16) -> Result<Box<dyn Conn>, ProcError>;
}

/// Time, as the call measures it.
pub trait Clock {
    /// Milliseconds on a monotonic clock.
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64);
    /// Seconds since the Unix epoch.
    fn unix_secs(&self) -> u64;
}

/// The real clock: `Instant` and `thread::sleep`, which both work on `wasm32-wasip2`.
pub struct SystemClock {
    origin: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    fn unix_secs(&self) -> u64 {
        crate::util::unix_now()
    }
}
