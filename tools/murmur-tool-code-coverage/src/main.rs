//! murmur-tool-code-coverage — a native Murmur tool that performs spectrum-based
//! fault localization (Ochiai / Tarantula) over a Rust repository already indexed
//! by `murmur-tool-code-graph`. It reads a directory of per-test LCOV `.info`
//! coverage reports (which the agent produced via its own `cargo llvm-cov` shell
//! calls — this tool never runs coverage instrumentation itself) and the list of
//! failing test names, computes `ef`/`ep` and Ochiai/Tarantula suspicion per
//! symbol, writes those four scores back onto `murmur-tool-code-graph`'s existing
//! `symbols` table, and returns a ranked suspect list. One operation: `localize`.
//!
//! I/O contract (identical to `murmur-tool-git`/`murmur-tool-code-graph`/
//! `murmur-tool-test-report`): read one JSON envelope `{"data": ..., "log_path":
//! ...}` from stdin, print one JSON object to stdout. `data` may be a JSON object
//! or a JSON-encoded string; both are handled.

mod db;
mod lcov;
mod ops;
mod out;
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
        "localize" => ops::op_localize(&op),
        "" => failed("missing required field 'operation'"),
        other => failed(format!("unknown operation: {other}")),
    }
}
