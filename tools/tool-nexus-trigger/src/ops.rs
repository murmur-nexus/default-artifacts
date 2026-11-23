//! One call, end to end: check the grant, check and encode the input, make one exchange, and
//! map the answer.
//!
//! Every refusal is decided before the exchange, so a refused call writes nothing. There is one
//! exchange per call and no retry: a second request could add a second trigger.

use crate::answer;
use crate::request::{self, Op};

/// What the entry's preopens say it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// The entry declares a `capabilities.filesystem.scope`: its `.` is that folder alone.
    Inbox,
    /// The entry declares no scope, so its `.` is the whole work folder; the runtime then also
    /// hands it `murmur-runtime` (and `murmur-host…` roots when unconfined or given `host_paths`).
    WholeWorkdir,
}

/// What one exchange came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exchanged {
    /// The answer file's bytes, read and removed.
    Answer(Vec<u8>),
    /// Something was at the answer's name that is not an answer this tool reads: a link, a folder,
    /// a file over the cap, or a read error. The reason, for the message.
    Unreadable(String),
    /// Nothing answered in time and the request was removed untaken: nothing was created.
    NoAnswer,
    /// Nothing answered in time, and the request was gone: a window took it.
    TakenNoAnswer,
    /// Nothing answered in time, and removing the request failed with this error.
    NotRemoved(String),
    /// The request could not be written; this error. Its temporary file was removed.
    CannotWrite(String),
}

/// Sends one request and returns what came of it. [`crate::inbox::Inbox`] implements it over
/// a folder; host tests implement it with a recording fake.
pub trait Exchange {
    fn exchange(&mut self, request: &[u8]) -> Exchanged;
}

/// Mirrors the WIT `status` enum without depending on the generated bindings, so this module
/// stays host-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpStatus {
    Passed,
    Failed,
    Error,
}

/// The tool result, field for field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: OpStatus,
    pub summary: String,
    pub data: String,
    pub data_path: Option<String>,
    pub truncated: bool,
    pub metadata: Vec<(String, String)>,
}

/// Run one call. `data` is the model's input, `None` when the runtime passed none.
pub fn run(grant: Grant, data: Option<&str>, io: &mut dyn Exchange) -> Response {
    let op = request::op_of(data);
    if grant == Grant::WholeWorkdir {
        return answer::tool_line(OpStatus::Error, op, answer::WHOLE_WORKDIR.to_string());
    }
    let request = match request::build(data) {
        Ok(request) => request,
        Err(refusal) => return answer::refused(op, &refusal),
    };
    let op: Op = request.op;
    match io.exchange(&request.bytes) {
        Exchanged::Answer(bytes) => answer::from_nexus(op, &bytes),
        Exchanged::Unreadable(reason) => {
            answer::tool_line(OpStatus::Error, Some(op), answer::malformed_answer(&reason))
        }
        Exchanged::NoAnswer => {
            answer::tool_line(OpStatus::Error, Some(op), answer::NO_ANSWER.to_string())
        }
        Exchanged::TakenNoAnswer => answer::tool_line(
            OpStatus::Error,
            Some(op),
            answer::TAKEN_NO_ANSWER.to_string(),
        ),
        Exchanged::NotRemoved(error) => {
            answer::tool_line(OpStatus::Error, Some(op), answer::not_removed(&error))
        }
        Exchanged::CannotWrite(error) => {
            answer::tool_line(OpStatus::Error, Some(op), answer::cannot_write(&error))
        }
    }
}
