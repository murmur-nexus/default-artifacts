//! murmur-tool-code-graph — a native Murmur tool that indexes a Rust repository
//! into a SQLite-backed symbol/edge graph and exposes it through six structured
//! operations: `index_repository`, `find_symbol`, `get_symbol`, `slice_symbol`,
//! `explain_path`, `impact_analysis`.
//!
//! I/O contract (identical to `murmur-tool-git`): read one JSON envelope
//! `{"data": ..., "log_path": ...}` from stdin, print one JSON object to stdout.
//! `data` may be a JSON object or a JSON-encoded string; both are handled.

mod db;
mod ops;
mod out;
mod parse;

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
    let json = serde_json::to_string(&result)
        .unwrap_or_else(|_| r#"{"ok":false,"status":"error","message":"failed to serialize output"}"#.to_string());
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
        "index_repository" => ops::op_index_repository(&op),
        "find_symbol" => ops::op_find_symbol(&op),
        "get_symbol" => ops::op_get_symbol(&op),
        "slice_symbol" => ops::op_slice_symbol(&op),
        "explain_path" => ops::op_explain_path(&op),
        "impact_analysis" => ops::op_impact_analysis(&op),
        "" => failed("missing required field 'operation'"),
        other => failed(format!("unknown operation: {other}")),
    }
}
