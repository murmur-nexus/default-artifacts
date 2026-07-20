//! murmur-tool-test-report — a native Murmur tool that parses a raw test-runner
//! output file (already produced by the agent's own shell call — this tool never
//! spawns a test process) into a structured list of failures.
//!
//! Supported formats: `cargo_test`, `pytest`, `go_test`, `jest` — selected
//! explicitly or auto-detected. `cargo_test` failures may additionally carry a
//! `stable_id` resolved from `<repo_path>/.murmur/code-graph.db` (best-effort,
//! read-only).
//!
//! I/O contract (identical to `murmur-tool-git`/`murmur-tool-code-graph`): read
//! one JSON envelope `{"data": ..., "log_path": ...}` from stdin, print one JSON
//! object to stdout. `data` may be a JSON object or a JSON-encoded string.

mod ops;
mod out;
mod parse;
mod resolve;

use std::io::Read;

use serde_json::Value;

use out::failed;

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("fatal: failed to read stdin");
        std::process::exit(1);
    }
    let result = run(&raw);
    let json = serde_json::to_string(&result).unwrap_or_else(|_| {
        r#"{"ok":false,"status":"error","message":"failed to serialize output"}"#.to_string()
    });
    println!("{json}");
}

fn run(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return failed("missing input on stdin");
    }

    let envelope: Value = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => return failed(format!("invalid stdin JSON: {e}")),
    };

    let data_value = match envelope.get("data") {
        None | Some(Value::Null) => return failed("missing data field"),
        Some(v) => v.clone(),
    };

    // `data` may be a JSON-encoded string (double-encoded) or a JSON object.
    let op: Value = match &data_value {
        Value::String(s) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => return failed(format!("invalid data JSON string: {e}")),
        },
        Value::Object(_) => data_value.clone(),
        _ => return failed("data must be a JSON string or object"),
    };

    let operation = op.get("operation").and_then(|v| v.as_str()).unwrap_or("");

    match operation {
        "parse" => ops::op_parse(&op),
        "" => failed("missing required field 'operation'"),
        other => failed(format!("unknown operation: {other}")),
    }
}
