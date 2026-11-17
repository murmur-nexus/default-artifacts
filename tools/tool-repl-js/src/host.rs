//! What the program's globals reach, in plain Rust types: the bus (`murmur:bus/bus@0.1.0`) and
//! `murmur:process/process@0.2.0` `run`. The component implements [`Host`] over its imports; the
//! host tests implement it with a fake.

/// One operation `list()` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub name: String,
    pub description: String,
    /// JSON text.
    pub input_schema: String,
}

/// `tool-result`'s `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    Passed,
    Failed,
    Error,
}

/// One entry of `tool-result`'s `attachments`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub media_type: String,
    pub path: String,
}

/// A bus call's `tool-result`, field for field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub status: ResultStatus,
    pub summary: Option<String>,
    pub data: Option<String>,
    pub data_path: Option<String>,
    pub truncated: bool,
    pub metadata: Vec<(String, String)>,
    pub attachments: Vec<Attachment>,
}

/// The bus's `call-error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The call did not run.
    Refused(String),
    /// The call ran and failed in the runtime.
    Failed(String),
}

/// `run-request`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunRequest {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout_ms: Option<u64>,
}

/// `run-output`: the first 1 MiB of each stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutput {
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

/// `process-error`, shared with the other tools on `murmur:process`.
pub use murmur_process_host::ProcError;

/// The bus and the process runner, as the globals call them.
pub trait Host {
    /// `list()`: every operation a program may call, or the bus's refusal.
    fn list(&mut self) -> Result<Vec<Operation>, String>;
    /// `call(name, input)`, `input` being JSON text.
    fn call(&mut self, name: &str, input: &str) -> Result<Envelope, CallError>;
    /// `call-batch`: `(name, input)` pairs, one result per pair in the same order.
    fn call_batch(&mut self, calls: &[(String, String)]) -> Vec<Result<Envelope, CallError>>;
    /// `murmur:process` `run`.
    fn run(&mut self, request: RunRequest) -> Result<RunOutput, ProcError>;
}
