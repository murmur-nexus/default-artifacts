//! `tool-write`: create or replace a file and any missing directories.

use serde_json::Value;

use crate::{err_result, fail_msg, io_error_kind, ok_summary_only, reject_absolute, required_path};

/// Write `content` to the file `path` names, creating any missing parent directories.
pub(crate) fn write(op: &Value) -> Value {
    let path = match required_path(op, "path") {
        Ok(p) => p,
        Err(failure) => return failure,
    };
    // Before `content` is read and before `create_dir_all` below, so an absolute
    // destination never manufactures the directory tree it names.
    if let Some(refusal) = reject_absolute("path", &path) {
        return refusal;
    }
    let content = match op.get("content").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return fail_msg("missing required field: content"),
    };

    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return err_result(
                    io_error_kind(&e),
                    format!("failed to create directories for {path}: {e}"),
                );
            }
        }
    }

    let byte_count = content.len();
    match std::fs::write(&path, content) {
        Ok(()) => ok_summary_only(
            format!("wrote {path}"),
            format!("{byte_count} bytes written"),
        ),
        Err(e) => err_result(io_error_kind(&e), format!("{path}: {e}")),
    }
}
