//! tool-code-coverage — a Murmur tool, packaged as a `wasm32-wasip2` component exporting
//! `murmur:tool/run@0.2.0` (world `tool`), that performs spectrum-based fault localization
//! (Ochiai / Tarantula) over a Rust repository already indexed by `tool-code-graph`. It reads a
//! directory of per-test LCOV `.info` coverage reports (which the agent produced via its own
//! `cargo llvm-cov` shell calls — this tool never runs coverage instrumentation itself) and the
//! list of failing test names, computes `ef`/`ep` and Ochiai/Tarantula suspicion per symbol,
//! writes those four scores back onto `tool-code-graph`'s existing `symbols` table in
//! `<repo_path>/.murmur/code-graph.db`, and returns a ranked suspect list. One operation:
//! `localize`.
//!
//! Everything outside `mod wasm_tool` is target-independent and host-tested: [`run`] takes the
//! call's `data` and the host roots as values and returns the result envelope, which
//! [`NativeOutcome::of`] reduces to what the model receives, as murmur's native dispatch reduced
//! the 0.5.0 tool's stdout. The component only gathers the roots from its WASI preopens and maps
//! that outcome onto the WIT `tool-result`.

mod db;
mod lcov;
mod ops;
mod out;
mod resolve;
mod sf_paths;

pub use murmur_file_tools::NativeOutcome;
pub use murmur_host_roots::{HostRoot, RootAccess};
use serde_json::Value;

use out::failed;

/// Run one call. `data` is the call's `tool-input.data`: the arguments as a JSON object, or as
/// a JSON string holding one. `roots` are the host folders the tool may open an absolute path
/// in, in preopen order; empty when the session opened none.
///
/// Returns the result envelope the native 0.5.0 tool printed, message for message:
/// `{ok, message, status, summary, data, data_path, metadata}` with `data`'s keys also at the
/// top level. Only `status`, `summary` and `data` reach the model ([`NativeOutcome::of`]).
pub fn run(data: Option<&str>, roots: &[HostRoot]) -> Value {
    let raw = match data {
        Some(raw) if !raw.trim().is_empty() => raw,
        _ => return failed("missing data field"),
    };
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(e) => return failed(format!("invalid stdin JSON: {e}")),
    };

    // The arguments may arrive double-encoded: a JSON string holding the object.
    let op = match parsed {
        Value::Object(_) => parsed,
        Value::String(inner) => match serde_json::from_str(&inner) {
            Ok(value) => value,
            Err(e) => return failed(format!("invalid data JSON string: {e}")),
        },
        _ => return failed("data must be a JSON string or object"),
    };

    let operation = op.get("operation").and_then(|v| v.as_str()).unwrap_or("");

    match operation {
        "localize" => ops::op_localize(&op, roots),
        "" => failed("missing required field 'operation'"),
        other => failed(format!("unknown operation: {other}")),
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use murmur_file_tools::{NativeOutcome, OutcomeStatus};
    use murmur_host_roots::HostRoot;
    use wasip2::filesystem::{preopens, types::DescriptorFlags};

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

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            let roots = host_roots();
            let envelope = crate::run(input.data.as_deref(), &roots);
            let parts = NativeOutcome::of(&envelope);
            ToolResult {
                status: match parts.status {
                    OutcomeStatus::Passed => Status::Passed,
                    OutcomeStatus::Failed => Status::Failed,
                    OutcomeStatus::Error => Status::Error,
                },
                summary: parts.summary,
                data: parts.data,
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: Vec::new(),
            }
        }
    }

    export!(Component);
}

#[cfg(test)]
mod tests {
    use murmur_file_tools::OutcomeStatus;
    use serde_json::json;

    use super::*;

    #[test]
    fn each_envelope_reaches_the_model_as_status_summary_and_compact_data() {
        let data = json!({"scored_symbols": 1, "top_suspects": [], "data_path": null});
        let ok = out::passed("localize: 1 symbol(s)", data.clone(), None, None);
        assert_eq!(
            NativeOutcome::of(&ok),
            NativeOutcome {
                status: OutcomeStatus::Passed,
                summary: Some("localize: 1 symbol(s)".to_string()),
                data: Some(serde_json::to_string(&data).unwrap()),
            }
        );
        for (envelope, status) in [
            (failed("localize: nope"), OutcomeStatus::Failed),
            (out::errored("localize: nope"), OutcomeStatus::Error),
        ] {
            assert_eq!(
                NativeOutcome::of(&envelope),
                NativeOutcome {
                    status,
                    summary: Some("localize: nope".to_string()),
                    data: None,
                }
            );
        }
    }

    #[test]
    fn run_refuses_what_is_not_a_call() {
        for (data, needle) in [
            (None, "missing data field"),
            (Some("  "), "missing data field"),
            (Some("{oops"), "invalid stdin JSON: "),
            (Some("\"{oops\""), "invalid data JSON string: "),
            (Some("[1]"), "data must be a JSON string or object"),
            (Some("7"), "data must be a JSON string or object"),
            (Some("{}"), "missing required field 'operation'"),
            (Some(r#"{"operation":"x"}"#), "unknown operation: x"),
        ] {
            let out = run(data, &[]);
            assert_eq!(out["status"], "failed", "{data:?}");
            let summary = out["summary"].as_str().unwrap();
            assert!(summary.starts_with(needle), "{data:?}: {summary}");
            assert_eq!(out["data"], Value::Null);
        }
    }

    #[test]
    fn run_accepts_a_double_encoded_call() {
        let call = json!({"operation": "localize"}).to_string();
        let out = run(Some(&Value::String(call).to_string()), &[]);
        // `repo_path` defaults to the work folder, then `coverage_dir` is required.
        assert_eq!(
            out["summary"],
            "localize: missing required field 'coverage_dir'"
        );
    }
}
