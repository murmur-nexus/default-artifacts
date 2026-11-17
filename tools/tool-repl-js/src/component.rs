//! The wasm32 adapter: [`Host`] over the `murmur:bus` and `murmur:process` imports, and the
//! `murmur:tool/run` export over one [`Engine`] kept for the life of the instance.

use std::{cell::RefCell, rc::Rc};

use crate::{
    Attachment, CallError, Engine, Envelope, Host, Operation, ProcError, ResultStatus, RunOutput,
    RunRequest, ARTIFACT_CONFIG_ENV,
};

wit_bindgen::generate!({
    path: "../../wit/guest",
    world: "code-mode",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use murmur::bus::bus;
use murmur::process::process;
use murmur::tool::run as imported;

thread_local! {
    /// The session's one context: murmur keeps this instance from the model's first call until
    /// the instance ends, and every call runs against it.
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

struct Imports;

fn envelope(result: imported::ToolResult) -> Envelope {
    Envelope {
        status: match result.status {
            imported::Status::Passed => ResultStatus::Passed,
            imported::Status::Failed => ResultStatus::Failed,
            imported::Status::Error => ResultStatus::Error,
        },
        summary: result.summary,
        data: result.data,
        data_path: result.data_path,
        truncated: result.truncated,
        metadata: result.metadata,
        attachments: result
            .attachments
            .into_iter()
            .map(|a| Attachment {
                media_type: a.media_type,
                path: a.path,
            })
            .collect(),
    }
}

fn call_error(e: bus::CallError) -> CallError {
    match e {
        bus::CallError::Refused(text) => CallError::Refused(text),
        bus::CallError::Failed(text) => CallError::Failed(text),
    }
}

impl Host for Imports {
    fn list(&mut self) -> Result<Vec<Operation>, String> {
        bus::list().map(|ops| {
            ops.into_iter()
                .map(|op| Operation {
                    name: op.name,
                    description: op.description,
                    input_schema: op.input_schema,
                })
                .collect()
        })
    }

    fn call(&mut self, name: &str, input: &str) -> Result<Envelope, CallError> {
        bus::call(name, input).map(envelope).map_err(call_error)
    }

    fn call_batch(&mut self, calls: &[(String, String)]) -> Vec<Result<Envelope, CallError>> {
        let requests: Vec<bus::CallRequest> = calls
            .iter()
            .map(|(name, input)| bus::CallRequest {
                name: name.clone(),
                input: input.clone(),
            })
            .collect();
        bus::call_batch(&requests)
            .into_iter()
            .map(|r| r.map(envelope).map_err(call_error))
            .collect()
    }

    fn run(&mut self, request: RunRequest) -> Result<RunOutput, ProcError> {
        let request = process::RunRequest {
            program: request.program,
            args: request.args,
            env: request.env,
            stdin: request.stdin,
            timeout_ms: request.timeout_ms,
        };
        match process::run(&request) {
            Ok(o) => Ok(RunOutput {
                exit_code: o.exit_code,
                timed_out: o.timed_out,
                stdout: o.stdout,
                stdout_truncated: o.stdout_truncated,
                stderr: o.stderr,
                stderr_truncated: o.stderr_truncated,
            }),
            Err(process::ProcessError::Refused(r)) => Err(ProcError::Refused {
                rule: r.rule,
                reason: r.reason,
            }),
            Err(process::ProcessError::UnknownHandle) => Err(ProcError::UnknownHandle),
            Err(process::ProcessError::NotRunning) => Err(ProcError::NotRunning),
            Err(process::ProcessError::TimedOut) => Err(ProcError::TimedOut),
            Err(process::ProcessError::Failed(text)) => Err(ProcError::Failed(text)),
        }
    }
}

fn result(status: ResultStatus, text: String, truncated: bool) -> ToolResult {
    ToolResult {
        status: match status {
            ResultStatus::Passed => Status::Passed,
            ResultStatus::Failed => Status::Failed,
            ResultStatus::Error => Status::Error,
        },
        summary: None,
        data: Some(text),
        data_path: None,
        truncated,
        metadata: Vec::new(),
        attachments: Vec::new(),
    }
}

struct Component;

impl Guest for Component {
    fn run(input: ToolInput) -> ToolResult {
        let config = std::env::var(ARTIFACT_CONFIG_ENV).ok();
        ENGINE.with(|cell| {
            let mut cell = cell.borrow_mut();
            if cell.is_none() {
                match Engine::new(Rc::new(RefCell::new(Imports))) {
                    Ok(engine) => *cell = Some(engine),
                    Err(e) => return result(ResultStatus::Error, e, false),
                }
            }
            let engine = cell.as_mut().expect("the engine was just made");
            let outcome = engine.handle(input.data.as_deref(), config.as_deref());
            result(outcome.status, outcome.text, outcome.truncated)
        })
    }
}

export!(Component);
