//! The single `parse` operation: read a raw test-runner output file, dispatch to
//! the format parser, resolve `stable_id` for cargo failures, and shape the
//! dual-envelope result (with `summary` + `data_path` capping for large lists).

use std::path::Path;

use serde_json::{json, Value};

use crate::out::{self, failed, Meta};
use crate::parse::{self, Failure};
use crate::resolve;

/// Maximum number of failures embedded inline in `data.failures`. Above this,
/// the inline array is capped, `data.truncated` is set, and the full array is
/// written to disk and referenced by `data_path`.
const INLINE_CAP: usize = 50;

pub fn op_parse(op: &Value) -> Value {
    let input_path = match op.get("input_path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return failed("missing required field 'input_path'"),
    };
    let format = op.get("format").and_then(|v| v.as_str()).unwrap_or("auto");
    let repo_path = op.get("repo_path").and_then(|v| v.as_str());

    let raw = match std::fs::read_to_string(input_path) {
        Ok(s) => s,
        Err(e) => return failed(format!("could not read input_path '{input_path}': {e}")),
    };

    let fmt = if format == "auto" {
        match parse::detect_format(&raw) {
            Some(f) => f,
            None => {
                return failed(
                    "could not auto-detect test format from input; pass 'format' explicitly as one of: cargo_test, pytest, go_test, jest",
                )
            }
        }
    } else {
        match format {
            "cargo_test" | "pytest" | "go_test" | "jest" => format.to_string(),
            other => {
                return failed(format!(
                    "unknown format '{other}'; expected one of: auto, cargo_test, pytest, go_test, jest"
                ))
            }
        }
    };

    let (mut failures, passed) = match fmt.as_str() {
        "cargo_test" => parse::parse_cargo(&raw),
        "pytest" => parse::parse_pytest(&raw),
        "go_test" => parse::parse_go(&raw),
        "jest" => parse::parse_jest(&raw),
        _ => unreachable!("format validated above"),
    };

    // stable_id is Rust-only and code-graph-backed: cargo_test + repo_path only.
    if fmt == "cargo_test" {
        if let Some(rp) = repo_path {
            resolve::resolve_stable_ids(rp, &mut failures);
        }
    }

    let failed_n = failures.len() as i64;
    let total = passed + failed_n;
    let any_failed = failed_n > 0;

    let all_vals: Vec<Value> = failures.iter().map(Failure::to_json).collect();
    let truncated = all_vals.len() > INLINE_CAP;

    let (inline_vals, data_path) = if truncated {
        let full = json!({
            "format_used": fmt,
            "total": total,
            "passed": passed,
            "failed": failed_n,
            "truncated": false,
            "failures": all_vals,
        });
        let dp = write_full(input_path, &full);
        (all_vals[..INLINE_CAP].to_vec(), dp)
    } else {
        (all_vals, None)
    };

    let data = json!({
        "format_used": fmt,
        "total": total,
        "passed": passed,
        "failed": failed_n,
        "truncated": truncated,
        "failures": inline_vals,
        "data_path": data_path,
    });

    let summary = format!("{passed} passed, {failed_n} failed");
    out::parse_result(
        summary,
        data,
        data_path,
        Some(Meta::read(input_path)),
        any_failed,
    )
}

/// Write the complete, untruncated failures payload next to the input file, as
/// `<input-stem>.failures.json`. Returns the path, or `None` if the write fails
/// (in which case the caller simply omits `data_path`).
fn write_full(input_path: &str, full: &Value) -> Option<String> {
    let p = Path::new(input_path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("test-report");
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
    let out = match dir {
        Some(d) => d.join(format!("{stem}.failures.json")),
        None => Path::new(&format!("{stem}.failures.json")).to_path_buf(),
    };
    let body = serde_json::to_string_pretty(full).ok()?;
    std::fs::write(&out, body).ok()?;
    Some(out.display().to_string())
}
