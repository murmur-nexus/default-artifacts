//! tool-git — a Murmur tool, packaged as a `wasm32-wasip2` component exporting
//! `murmur:tool/run@0.2.0` and importing `murmur:process/process@0.1.0` (world `tool`), that gives
//! a capsule typed git operations.
//!
//! Every git command runs through `murmur:process` `run`: the program `git`, the handler's
//! arguments, the work folder as its working directory and no extra environment. So murmur puts
//! each one to the capsule's decision point — `capabilities.shell.allow`, the
//! `capabilities.filesystem.read_only` check and every `on-shell` deny hook — exactly as it puts
//! a shell `git` command. `symbol_history` reads tool-code-graph's `.murmur/code-graph.db` with
//! SQLite compiled into the component, read-only.
//!
//! Everything outside `mod wasm_tool` is target-independent and host-tested: [`run`] takes the
//! call's `data` and `log_path` and a [`GitEnv`] and returns the native 0.6.0 tool's result
//! envelope, and [`to_tool_result`] reduces it to what the model receives, as murmur's native
//! dispatch reduced that tool's stdout. The component only gathers its host roots, implements
//! [`GitEnv`] over the `murmur:process` import and its WASI preopens, and maps the outcome onto
//! the WIT `tool-result`.

mod env;
mod ops;
mod out;
mod paths;
mod resolve;

pub use env::{GitEnv, GitOut, GitRun};
pub use murmur_file_tools::{NativeOutcome, OutcomeStatus};
pub use murmur_host_roots::{HostRoot, RootAccess};
use serde_json::Value;

use env::{Git, Log};
use out::fail_msg;

/// The envelope key [`run`] sets to `true` when a git stream was cut at 1 MiB.
pub const TRUNCATED_KEY: &str = "git_output_truncated";

/// What the summary of a call whose git output was cut ends with.
pub const TRUNCATED_SUFFIX: &str = "; git output cut at 1 MiB";

/// Run one call. `data` is the call's `tool-input.data`: the arguments as a JSON string;
/// `log_path` its `tool-input.log-path`, where each handler appends a line, best effort.
///
/// Returns the result envelope the native 0.6.0 tool printed for the same input, message for
/// message: `{ok, message, status, summary, data, data_path, metadata}`, `error_kind` on a typed
/// failure, and `data`'s keys also at the top level. When murmur refused or timed out a git
/// command, the envelope is that line instead (`error_kind` `refused` or `timed_out`); when a git
/// stream was cut, it also holds [`TRUNCATED_KEY`].
pub fn run(data: Option<&str>, log_path: Option<&str>, env: &dyn GitEnv) -> Value {
    let Some(raw) = data else {
        return fail_msg("missing data field");
    };
    let op: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return fail_msg(format!("invalid data JSON string: {e}")),
    };
    let git = Git::new(env);
    let result = ops::dispatch(&git, &op, &Log::new(log_path, env));
    git.finish(result)
}

/// What the model receives for a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: OutcomeStatus,
    pub summary: Option<String>,
    pub data: Option<String>,
    /// A git stream was cut at 1 MiB.
    pub truncated: bool,
}

/// `envelope` as the model receives it: murmur's native-tool mapping ([`NativeOutcome::of`]:
/// `status`, `summary`, and `data` as a string or compact JSON), then the truncation flag, which
/// sets `truncated` and ends the summary with [`TRUNCATED_SUFFIX`].
pub fn to_tool_result(envelope: &Value) -> ToolOutcome {
    let NativeOutcome {
        status,
        mut summary,
        data,
    } = NativeOutcome::of(envelope);
    let truncated = envelope.get(TRUNCATED_KEY) == Some(&Value::Bool(true));
    if truncated {
        summary = Some(format!("{}{TRUNCATED_SUFFIX}", summary.unwrap_or_default()));
    }
    ToolOutcome {
        status,
        summary,
        data,
        truncated,
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::PathBuf;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use murmur::process::process::{self, ProcessError, RunRequest};
    use murmur_file_tools::OutcomeStatus;
    use murmur_host_roots::HostRoot;
    use wasip2::filesystem::{preopens, types::DescriptorFlags};

    use crate::{GitEnv, GitRun};

    struct Component;

    /// The host folders murmur opened for this call, in preopen order. A root is writable when
    /// its descriptor's flags hold `mutate-directory`; a flags error counts as read-only. Each
    /// descriptor is dropped here, so none outlives the call.
    fn host_roots() -> Vec<HostRoot> {
        preopens::get_directories()
            .into_iter()
            .filter_map(|(descriptor, name)| {
                let writable = descriptor
                    .get_flags()
                    .is_ok_and(|flags| flags.contains(DescriptorFlags::MUTATE_DIRECTORY));
                HostRoot::from_preopen(&name, writable)
            })
            .collect()
    }

    /// git through the `murmur:process` import, files through the WASI preopens.
    struct WasmEnv {
        roots: Vec<HostRoot>,
    }

    impl GitEnv for WasmEnv {
        fn run_git(&self, args: &[&str]) -> GitRun {
            let request = RunRequest {
                program: "git".to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: Vec::new(),
                stdin: None,
                timeout_ms: None,
            };
            match process::run(&request) {
                Ok(out) => GitRun::Ran {
                    exit_code: out.exit_code,
                    timed_out: out.timed_out,
                    stdout: out.stdout,
                    stdout_truncated: out.stdout_truncated,
                    stderr: out.stderr,
                    stderr_truncated: out.stderr_truncated,
                },
                Err(ProcessError::Refused(refusal)) => GitRun::Refused {
                    rule: refusal.rule,
                    reason: refusal.reason,
                },
                Err(ProcessError::TimedOut) => GitRun::TimedOut,
                Err(ProcessError::Failed(text)) => GitRun::Failed(text),
                // `run` names no handle, so murmur never answers it with these two.
                Err(ProcessError::UnknownHandle) => GitRun::Failed("unknown-handle".into()),
                Err(ProcessError::NotRunning) => GitRun::Failed("not-running".into()),
            }
        }

        fn roots(&self) -> &[HostRoot] {
            &self.roots
        }

        /// A path relative to the work folder opens through the preopen `.`, and a
        /// `murmur-host…` path through the host root it names, so each is opened as written.
        fn fs_path(&self, open: &str) -> PathBuf {
            PathBuf::from(open)
        }
    }

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            let env = WasmEnv {
                roots: host_roots(),
            };
            let envelope = crate::run(input.data.as_deref(), input.log_path.as_deref(), &env);
            let outcome = crate::to_tool_result(&envelope);
            ToolResult {
                status: match outcome.status {
                    OutcomeStatus::Passed => Status::Passed,
                    OutcomeStatus::Failed => Status::Failed,
                    OutcomeStatus::Error => Status::Error,
                },
                summary: outcome.summary,
                data: outcome.data,
                data_path: None,
                truncated: outcome.truncated,
                metadata: Vec::new(),
                attachments: Vec::new(),
            }
        }
    }

    export!(Component);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    /// A `GitEnv` for calls that must not reach git.
    struct NoGit;

    impl GitEnv for NoGit {
        fn run_git(&self, args: &[&str]) -> GitRun {
            panic!("unexpected git run: {args:?}")
        }
        fn roots(&self) -> &[HostRoot] {
            &[]
        }
        fn fs_path(&self, open: &str) -> PathBuf {
            PathBuf::from(open)
        }
    }

    #[test]
    fn run_refuses_what_is_not_a_call() {
        for (data, needle) in [
            (None, "missing data field"),
            (Some(""), "invalid data JSON string: "),
            (Some("{oops"), "invalid data JSON string: "),
            (Some("{}"), "unknown operation: "),
            (Some("[1]"), "unknown operation: "),
            (
                Some(r#"{"operation":"bogus_op_xyz"}"#),
                "unknown operation: bogus_op_xyz",
            ),
        ] {
            let out = run(data, None, &NoGit);
            assert_eq!(out["ok"], false, "{data:?}");
            assert_eq!(out["status"], "error", "{data:?}");
            let summary = out["summary"].as_str().unwrap();
            assert!(summary.starts_with(needle), "{data:?}: {summary}");
            assert_eq!(out["message"], out["summary"]);
            assert_eq!(out["data"], Value::Null);
        }
    }

    #[test]
    fn a_missing_required_field_runs_no_git() {
        let out = run(Some(r#"{"operation":"clone","dest":"d"}"#), None, &NoGit);
        assert_eq!(out["summary"], "missing required field: url");
    }

    #[test]
    fn the_model_sees_status_summary_and_compact_data() {
        let envelope = out::ok_with("1 commit(s)", json!({"commits": [{"hash": "h"}]}));
        assert_eq!(
            to_tool_result(&envelope),
            ToolOutcome {
                status: OutcomeStatus::Passed,
                summary: Some("1 commit(s)".into()),
                data: Some(r#"{"commits":[{"hash":"h"}]}"#.into()),
                truncated: false,
            }
        );
        assert_eq!(
            to_tool_result(&out::err_result(out::err::NOT_FOUND, "gone")),
            ToolOutcome {
                status: OutcomeStatus::Error,
                summary: Some("gone".into()),
                data: None,
                truncated: false,
            }
        );
    }

    #[test]
    fn a_cut_output_sets_truncated_and_says_so() {
        let mut envelope = out::ok_with("diff complete", json!({"diff": "x"}));
        envelope[TRUNCATED_KEY] = Value::Bool(true);
        let outcome = to_tool_result(&envelope);
        assert!(outcome.truncated);
        assert_eq!(
            outcome.summary.as_deref(),
            Some("diff complete; git output cut at 1 MiB")
        );
        assert_eq!(outcome.data.as_deref(), Some(r#"{"diff":"x"}"#));
    }
}
