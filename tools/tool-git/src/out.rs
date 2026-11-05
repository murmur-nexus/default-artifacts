//! The result envelope every call returns, its error kinds, and the helpers the handlers share.
//!
//! The envelope is the native 0.6.0 tool's stdout object: `{ok, message, status, summary, data,
//! data_path, metadata}`, `error_kind` on a typed failure, and every key of a successful call's
//! `data` also at the top level. Only `status`, `summary` and `data` reach the model
//! ([`crate::to_tool_result`]); `ok`, `message`, `error_kind` and the flattened copies are what
//! host tests of the core assert on.

use serde_json::{json, Value};

pub mod err {
    pub const NOTHING_TO_STASH: &str = "nothing_to_stash";
    pub const NOTHING_TO_COMMIT: &str = "nothing_to_commit";
    pub const NOT_FOUND: &str = "not_found";
    pub const CONFLICT: &str = "conflict";
    pub const ALREADY_EXISTS: &str = "already_exists";
    pub const NOT_MERGED: &str = "not_merged";
    pub const DIRTY_WORKING_TREE: &str = "dirty_working_tree";
    // ── Remote operation error kinds ──────────────────────────────────────────
    pub const REMOTE_NOT_FOUND: &str = "remote_not_found";
    pub const NO_TRACKING_BRANCH: &str = "no_tracking_branch";
    pub const NON_FAST_FORWARD: &str = "non_fast_forward";
    pub const NO_UPSTREAM: &str = "no_upstream";
    pub const DEST_EXISTS: &str = "dest_exists";
    pub const CLONE_FAILED: &str = "clone_failed";
    // ── Merge / worktree error kinds ──────────────────────────────────────────
    pub const FF_ONLY_FAILED: &str = "ff_only_failed";
    pub const BRANCH_CONFLICT: &str = "branch_conflict";
    pub const DIRTY: &str = "dirty";
    // ── symbol_history error kinds ────────────────────────────────────────────
    // Distinct from NOT_FOUND: the repo was never indexed at all (no
    // .murmur/code-graph.db), so no symbol id could ever resolve. The remedy is
    // to run index_repository (tool-code-graph), not to fix the id.
    pub const NOT_INDEXED: &str = "not_indexed";
    // The repository's code-graph.db is neither in the work folder nor under a host folder the
    // session opens to the tool, so the component cannot open it.
    pub const OUTSIDE_ROOTS: &str = "outside_roots";
    // A relative `repo` whose first segment is `murmur-host`, the name the host folders are
    // opened under, while the tool holds at least one.
    pub const RESERVED_NAME: &str = "reserved_name";
    // ── murmur:process error kinds ────────────────────────────────────────────
    // murmur refused to run a git command: a rule named in the summary.
    pub const REFUSED: &str = "refused";
    // A git command ran past the tool call's deadline and was killed.
    pub const TIMED_OUT: &str = "timed_out";
}

/// Merge `extra` (a JSON object) with the base ok envelope and return the combined Value.
/// `extra` is also stored under `"data"`, which is what reaches the model.
pub fn ok_with(message: impl Into<String>, extra: Value) -> Value {
    let msg = message.into();
    let data = extra.clone();
    let mut obj = json!({
        "ok": true,
        "message": &msg,
        "status": "passed",
        "summary": &msg,
        "data": data,
        "data_path": null,
        "metadata": null,
    });
    // Also flatten extra fields at the top level for envelope readers.
    if let Value::Object(map) = extra {
        for (k, v) in map {
            obj[k] = v;
        }
    }
    obj
}

pub fn ok_simple(message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": true,
        "message": &msg,
        "status": "passed",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

pub fn fail_msg(message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "message": &msg,
        "status": "error",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

pub fn err_result(error_kind: &str, message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "error_kind": error_kind,
        "message": &msg,
        "status": "error",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

/// The last line of git's stderr without its `fatal: ` prefix.
pub fn strip_fatal(s: &str) -> String {
    s.lines()
        .last()
        .unwrap_or(s)
        .trim_start_matches("fatal: ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_msg_returns_ok_false() {
        let out = fail_msg("something went wrong");
        assert_eq!(out["ok"], false);
        assert_eq!(out["message"], "something went wrong");
        assert_eq!(out["status"], "error");
        assert_eq!(out["data"], Value::Null);
    }

    #[test]
    fn err_result_includes_error_kind() {
        let out = err_result(err::NOTHING_TO_STASH, "clean tree");
        assert_eq!(out["ok"], false);
        assert_eq!(out["error_kind"], err::NOTHING_TO_STASH);
        assert_eq!(out["message"], "clean tree");
        assert_eq!(out["summary"], "clean tree");
    }

    #[test]
    fn err_constants_are_distinct() {
        let kinds = [
            err::NOTHING_TO_STASH,
            err::NOTHING_TO_COMMIT,
            err::NOT_FOUND,
            err::CONFLICT,
            err::ALREADY_EXISTS,
            err::NOT_MERGED,
            err::DIRTY_WORKING_TREE,
            err::REMOTE_NOT_FOUND,
            err::NO_TRACKING_BRANCH,
            err::NON_FAST_FORWARD,
            err::NO_UPSTREAM,
            err::DEST_EXISTS,
            err::CLONE_FAILED,
            err::FF_ONLY_FAILED,
            err::BRANCH_CONFLICT,
            err::DIRTY,
            err::NOT_INDEXED,
            err::OUTSIDE_ROOTS,
            err::RESERVED_NAME,
            err::REFUSED,
            err::TIMED_OUT,
        ];
        for (i, a) in kinds.iter().enumerate() {
            for (j, b) in kinds.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "error kind constants must be unique");
                }
            }
        }
    }

    #[test]
    fn ok_with_merges_extra_fields() {
        let out = ok_with("done", json!({ "entries": [] }));
        assert_eq!(out["ok"], true);
        assert_eq!(out["message"], "done");
        assert!(out["entries"].is_array());
        assert_eq!(out["data"], json!({ "entries": [] }));
    }

    #[test]
    fn strip_fatal_removes_prefix() {
        assert_eq!(
            strip_fatal("fatal: not a git repository"),
            "not a git repository"
        );
        assert_eq!(strip_fatal("error: branch exists"), "error: branch exists");
        assert_eq!(strip_fatal(""), "");
    }
}
