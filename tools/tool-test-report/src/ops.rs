//! The single `parse` operation: read a raw test-runner output file, dispatch to
//! the format parser, resolve `stable_id` for cargo failures, and shape the
//! dual-envelope result (with `summary` + `data_path` capping for large lists).
//!
//! Every path input goes through `murmur_host_roots::locate` before anything is opened: a
//! relative value opens in the work folder as written, an absolute one only through the host
//! root covering it (as `murmur-host/…`). An absolute value handed to the filesystem as written
//! would resolve into the work folder, not onto the host.

use std::path::Path;

use murmur_host_roots::{describe_roots, locate, HostRoot, Located, Refusal};
use serde_json::{json, Value};

use crate::out::{self, failed, Meta};
use crate::parse::{self, Failure};
use crate::resolve;

/// Maximum number of failures embedded inline in `data.failures`. Above this,
/// the inline array is capped, `data.truncated` is set, and the full array is
/// written to disk and referenced by `data_path`.
const INLINE_CAP: usize = 50;

/// The tool's name, as refusals name it.
const TOOL_NAME: &str = "tool-test-report";

pub fn op_parse(op: &Value, roots: &[HostRoot]) -> Value {
    let input_path = match op.get("input_path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return failed("missing required field 'input_path'"),
    };
    let format = op.get("format").and_then(|v| v.as_str()).unwrap_or("auto");
    let repo_path = op.get("repo_path").and_then(|v| v.as_str());

    let input_open = match locate(input_path, roots) {
        Ok(located) => open_path(located),
        Err(refusal) => {
            return failed(refusal_message(
                "input_path",
                "could not read input_path",
                "",
                input_path,
                refusal,
                roots,
            ))
        }
    };

    let raw = match std::fs::read_to_string(&input_open) {
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

    // stable_id is Rust-only and code-graph-backed: cargo_test + repo_path only. Other
    // formats never look at repo_path, so they never refuse it either.
    if fmt == "cargo_test" {
        if let Some(rp) = repo_path {
            let repo_open = match locate(rp, roots) {
                Ok(located) => open_path(located),
                Err(refusal) => {
                    return failed(refusal_message(
                        "repo_path",
                        "could not open repo_path",
                        " ('.' is the work folder itself)",
                        rp,
                        refusal,
                        roots,
                    ))
                }
            };
            let db = Path::new(&repo_open).join(".murmur").join("code-graph.db");
            if let Some(db) = db.to_str() {
                resolve::resolve_stable_ids(db, &mut failures);
            }
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
        let dp = write_full(input_path, &input_open, &full);
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
/// `<input-stem>.failures.json`: in the directory of `input_open`, the path the input was read
/// from (`murmur-host/…` for a host value). Returns the same file named from `input_path`, the
/// value as written — so an absolute input gives an absolute host path — or `None` if the write
/// fails, as it does under a `read` host root (the caller then reports `data_path: null`).
fn write_full(input_path: &str, input_open: &str, full: &Value) -> Option<String> {
    let body = serde_json::to_string_pretty(full).ok()?;
    std::fs::write(sibling_failures_file(input_open), body).ok()?;
    Some(sibling_failures_file(input_path).display().to_string())
}

/// `<dir of path>/<stem of path>.failures.json`, or the bare file name when `path` has no
/// parent directory. The stem falls back to `test-report`.
fn sibling_failures_file(path: &str) -> std::path::PathBuf {
    let p = Path::new(path);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("test-report");
    let name = format!("{stem}.failures.json");
    match p.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.join(name),
        None => Path::new(&name).to_path_buf(),
    }
}

/// The path to open for a located value: a relative value as written, or the `murmur-host/…`
/// path of an absolute one.
fn open_path(located: Located<'_>) -> String {
    match located {
        Located::Relative(value) => value,
        Located::Host { open, .. } => open,
    }
}

/// The message refusing `value`, the `field` input, before anything is opened. `prefix` opens
/// the two "nothing covers it" messages (`could not read input_path`), and `relative_note`
/// follows their closing "a relative path opens in the work folder".
fn refusal_message(
    field: &str,
    prefix: &str,
    relative_note: &str,
    value: &str,
    refusal: Refusal,
    roots: &[HostRoot],
) -> String {
    match refusal {
        Refusal::NoRoots => format!(
            "{prefix} '{value}': this session opens no host folder to {TOOL_NAME}; a relative \
             path opens in the work folder{relative_note}"
        ),
        Refusal::Uncovered => format!(
            "{prefix} '{value}': it is outside every host folder {TOOL_NAME} may open: {}. A \
             relative path opens in the work folder{relative_note}",
            describe_roots(roots)
        ),
        Refusal::ReservedName { suggestion } => format!(
            "'{field}' starts with murmur-host, where this tool's host folders are opened; name \
             the host path absolutely: '{suggestion}'"
        ),
    }
}
