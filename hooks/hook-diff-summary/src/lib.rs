//! hook-diff-summary: emit a unified diff of every file the editor tool wrote
//! during a turn, as an artifact on `end_turn`.
//!
//! Everything except the filesystem read and the `Guest` impl lives here at the crate
//! root. Everything under `wasm_hook` is gated on `target_arch = "wasm32"` and so is
//! unreachable from `cargo test`; hoisting the diff shaping out of it is what makes the
//! truncation thresholds, the binary detection and the create/modify/delete matrix
//! assertable natively. [`build_file_entry`] takes the before and after bytes rather than
//! reading them, and the adapter supplies `std::fs::read(path).ok()` as the "after".

use std::path::Path;

use serde_json::{json, Value};
use similar::TextDiff;

/// Unchanged lines kept either side of a change in the unified diff.
pub const CONTEXT_LINES: usize = 2;

/// Changed lines (`+` and `-` together) a diff may carry before it is truncated. Past
/// this the entry reports the full count in `total_changed_lines` and a `diff` holding
/// exactly this many changed lines.
pub const TRUNCATE_THRESHOLD: usize = 300;

/// Size either side of a diff may reach before the entry is emitted with null stats.
pub const MAX_FILE_BYTES: usize = 512 * 1024;

// ── path extraction ───────────────────────────────────────────────────────────
//
// The inference event carries the tool call data in `tools` and/or `output`.
// We try multiple JSON layouts because the runtime may deliver the data as a
// raw operation object, an Anthropic tool_use block, or a wrapping envelope.

pub fn extract_write_path(tools: Option<&str>, output: Option<&str>) -> Option<String> {
    [tools, output]
        .into_iter()
        .flatten()
        .find_map(try_extract_write_path)
}

pub fn try_extract_write_path(s: &str) -> Option<String> {
    let v: Value = serde_json::from_str(s).ok()?;
    extract_from_value(&v)
}

pub fn extract_from_value(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            // Direct tool input: {"write":{"path":"..."}} or {"edit":{"path":"..."}}. Those two
            // locations are what the editor's input schema annotates `format:
            // murmur-destination`. A `read` or `find` call writes nothing, and an object
            // carrying `operation`/`dest_path` is the pre-0.12.0 shape, which the editor refuses
            // outright, so neither yields a path.
            if let Some(op) = write_op(map) {
                return op.get("path").and_then(Value::as_str).map(str::to_string);
            }
            if map.contains_key("read") || map.contains_key("find") {
                return None;
            }

            // Anthropic tool_use block: {"type":"tool_use","input":{...}}
            if let Some(input) = map.get("input") {
                if let Some(p) = extract_from_value(input) {
                    return Some(p);
                }
            }

            // Runtime envelope: {"data":"<inner-json>","log_path":...}
            if let Some(data) = map.get("data") {
                let inner: Option<Value> = match data {
                    Value::String(s) => serde_json::from_str(s).ok(),
                    other @ (Value::Object(_) | Value::Array(_)) => Some(other.clone()),
                    _ => None,
                };
                if let Some(p) = inner.as_ref().and_then(extract_from_value) {
                    return Some(p);
                }
            }

            None
        }
        Value::Array(items) => items.iter().find_map(extract_from_value),
        // Double-encoded JSON string
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .as_ref()
            .and_then(extract_from_value),
        _ => None,
    }
}

/// The tool whose writes are snapshotted: the bare name the model calls it by, which is
/// what the runtime reports as an event's `tool_name`.
pub const EDITOR_TOOL: &str = "tool-editor";

/// Whether a tool call's `tool_name` is the editor's, and so a write this hook diffs.
pub fn is_editor_call(tool_name: Option<&str>) -> bool {
    tool_name == Some(EDITOR_TOOL)
}

/// The editor's operation keys that write the file their object's `path` names.
pub const WRITE_OPS: [&str; 2] = ["write", "edit"];

/// The operation object of an editor call whose key is `write` or `edit`, or `None` for any
/// other call.
pub fn write_op(call: &serde_json::Map<String, Value>) -> Option<&serde_json::Map<String, Value>> {
    WRITE_OPS
        .iter()
        .find_map(|key| call.get(*key).and_then(Value::as_object))
}

pub fn normalize_path(path: &str) -> String {
    path.strip_prefix("./").unwrap_or(path).to_string()
}

// ── diff computation ──────────────────────────────────────────────────────────

/// The artifact payload emitted at `end_turn`.
pub fn build_summary(session_id: &str, entries: &[Value]) -> Value {
    json!({
        "session_id": session_id,
        "files": entries,
    })
}

/// One file's entry in the summary.
///
/// `before` is `None` when the file did not exist when the write was announced, `after`
/// is `None` when it is not on disk now. Both `None` is a file created and then deleted
/// within the turn: no net change, and the entry reports zeros rather than being omitted.
pub fn build_file_entry(path: &str, before: Option<&[u8]>, after: Option<&[u8]>) -> Value {
    let lang = language_for(path);

    match after {
        Some(after) => diff_entry(path, lang, before.unwrap_or(&[]), after),
        None => match before {
            // File was created then deleted — no net change.
            None => json!({
                "path": path, "language": lang,
                "diff": "", "hunks": [],
                "added_lines": 0, "removed_lines": 0,
                "truncated": false, "total_changed_lines": 0,
            }),
            // File existed before and is now gone — all-deletions diff.
            Some(before) => diff_entry(path, lang, before, &[]),
        },
    }
}

/// Checks run in this order, and the order is observable: a file over
/// [`MAX_FILE_BYTES`] that contains a NUL byte reports `binary: true` and never
/// `truncated`.
pub fn diff_entry(path: &str, lang: &'static str, before: &[u8], after: &[u8]) -> Value {
    if is_binary(before) || is_binary(after) {
        return json!({"path": path, "language": lang, "binary": true});
    }

    let (before_str, after_str) = match (std::str::from_utf8(before), std::str::from_utf8(after)) {
        (Ok(b), Ok(a)) => (b, a),
        _ => return json!({"path": path, "language": lang, "binary": true}),
    };

    if before.len() > MAX_FILE_BYTES || after.len() > MAX_FILE_BYTES {
        return json!({
            "path": path, "language": lang,
            "diff": null, "hunks": [],
            "added_lines": null, "removed_lines": null,
            "truncated": true, "total_changed_lines": null,
        });
    }

    if before_str == after_str {
        return json!({
            "path": path, "language": lang,
            "diff": "", "hunks": [],
            "added_lines": 0, "removed_lines": 0,
            "truncated": false, "total_changed_lines": 0,
        });
    }

    let text_diff = TextDiff::from_lines(before_str, after_str);
    let mut unified = text_diff.unified_diff();
    let hunk_body = unified.context_radius(CONTEXT_LINES).to_string();

    if hunk_body.is_empty() {
        return json!({
            "path": path, "language": lang,
            "diff": "", "hunks": [],
            "added_lines": 0, "removed_lines": 0,
            "truncated": false, "total_changed_lines": 0,
        });
    }

    let full_diff = format!("--- a/{path}\n+++ b/{path}\n{hunk_body}");
    let (hunks, added, removed) = parse_diff_stats(&full_diff);
    let total = added + removed;

    if total > TRUNCATE_THRESHOLD {
        let trunc = truncate_diff(&full_diff, TRUNCATE_THRESHOLD);
        let (trunc_hunks, trunc_added, trunc_removed) = parse_diff_stats(&trunc);
        return json!({
            "path": path, "language": lang,
            "diff": trunc, "hunks": trunc_hunks,
            "added_lines": trunc_added, "removed_lines": trunc_removed,
            "truncated": true, "total_changed_lines": total,
        });
    }

    json!({
        "path": path, "language": lang,
        "diff": full_diff, "hunks": hunks,
        "added_lines": added, "removed_lines": removed,
        "truncated": false, "total_changed_lines": total,
    })
}

// ── diff text helpers ─────────────────────────────────────────────────────────

pub fn parse_diff_stats(diff: &str) -> (Vec<Value>, usize, usize) {
    let mut hunks = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(h) = parse_hunk_header(line) {
                hunks.push(h);
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }

    (hunks, added, removed)
}

pub fn parse_hunk_header(line: &str) -> Option<Value> {
    // "@@ -A,B +C,D @@ optional-context" → {old_start, old_count, new_start, new_count}
    let inner = line.strip_prefix("@@ ")?.split(" @@").next()?;
    let mut parts = inner.split_whitespace();
    let (old_start, old_count) = parse_range(parts.next()?.strip_prefix('-')?)?;
    let (new_start, new_count) = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some(json!({
        "old_start": old_start,
        "old_count": old_count,
        "new_start": new_start,
        "new_count": new_count,
    }))
}

pub fn parse_range(s: &str) -> Option<(u32, u32)> {
    if let Some((a, b)) = s.split_once(',') {
        Some((a.parse().ok()?, b.parse().ok()?))
    } else {
        Some((s.parse().ok()?, 1))
    }
}

/// Keeps diff lines until `max` change lines (+/-) have been emitted, then
/// stops mid-hunk. The result is a syntactically incomplete diff but contains
/// exactly the first `max` changed lines, which is all the renderer needs.
pub fn truncate_diff(diff: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;

    for line in diff.lines() {
        let is_add = line.starts_with('+') && !line.starts_with("+++");
        let is_del = line.starts_with('-') && !line.starts_with("---");

        if (is_add || is_del) && count >= max {
            break;
        }
        out.push_str(line);
        out.push('\n');
        if is_add || is_del {
            count += 1;
        }
    }

    out
}

// ── misc helpers ──────────────────────────────────────────────────────────────

pub fn is_binary(data: &[u8]) -> bool {
    data.contains(&0u8)
}

pub fn language_for(path: &str) -> &'static str {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let name_lc = file_name.to_lowercase();
    match name_lc.as_str() {
        "dockerfile" | "containerfile" => return "dockerfile",
        "makefile" | "gnumakefile" | "bsdmakefile" => return "makefile",
        _ => {}
    }

    match ext.to_lowercase().as_str() {
        "rs" => "rust",
        "py" | "pyw" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "cc" | "cxx" | "c++" => "cpp",
        "h" | "hpp" | "hxx" => "c",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdx" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "xml" => "xml",
        "sql" => "sql",
        "proto" => "protobuf",
        "wit" => "wit",
        "wat" | "wasm" => "webassembly",
        "tf" | "tfvars" => "hcl",
        "lua" => "lua",
        "r" | "rmd" => "r",
        "scala" => "scala",
        "clj" | "cljs" | "cljc" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" | "lhs" => "haskell",
        "ml" | "mli" => "ocaml",
        "tex" | "sty" | "cls" => "latex",
        "vue" => "vue",
        "svelte" => "svelte",
        "graphql" | "gql" => "graphql",
        _ => "text",
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::cell::RefCell;

    use super::{
        build_file_entry, build_summary, extract_write_path, is_editor_call, normalize_path,
    };

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    // ── per-session state ─────────────────────────────────────────────────────

    struct Snapshot {
        // None when the file did not exist at snapshot time (new-file creation).
        before: Option<Vec<u8>>,
    }

    struct HookState {
        session_id: String,
        // Insertion-ordered list keeps files in the order they were first touched.
        snapshots: Vec<(String, Snapshot)>,
    }

    thread_local! {
        static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
    }

    pub struct MurmurHookDiffSummary;

    use exports::murmur::hook::lifecycle::HookOutput;

    impl exports::murmur::hook::lifecycle::Guest for MurmurHookDiffSummary {
        fn on_stage(
            _event: exports::murmur::hook::lifecycle::StageEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(
            ctx: exports::murmur::hook::lifecycle::SessionContext,
        ) -> Result<HookOutput, String> {
            STATE.with(|s| {
                *s.borrow_mut() = Some(HookState {
                    session_id: ctx.session_id,
                    snapshots: Vec::new(),
                });
            });
            Ok(HookOutput::None)
        }

        // on_inference fires before the tool runs, so this is the right place
        // to snapshot the "before" state of any file about to be modified.
        fn on_inference(
            event: exports::murmur::hook::lifecycle::InferenceEvent,
        ) -> Result<HookOutput, String> {
            // When the agent finishes its turn, flush the accumulated diff so
            // the client can read it before (or immediately after) the task
            // completion event arrives.  Clear snapshots so the next task
            // starts fresh.
            if event.decision == "end_turn" {
                let output = STATE.with(|s| {
                    let mut guard = s.borrow_mut();
                    let state = guard.as_mut()?;
                    let files: Vec<_> = state
                        .snapshots
                        .iter()
                        .map(|(path, snap)| {
                            let after = std::fs::read(path).ok();
                            build_file_entry(path, snap.before.as_deref(), after.as_deref())
                        })
                        .collect();
                    state.snapshots.clear();
                    let session_id = state.session_id.clone();
                    let json_str =
                        serde_json::to_string(&build_summary(&session_id, &files)).ok()?;
                    Some(json_str)
                });
                // Always emit an artifact at end_turn (even empty files list) so
                // the pipeline can be verified end-to-end.
                let payload =
                    output.unwrap_or_else(|| r#"{"session_id":"unknown","files":[]}"#.to_string());
                return Ok(HookOutput::Artifact(payload));
            }

            // Before-state snapshot: only for editor `write`/`edit` calls.
            if !is_editor_call(event.tool_name.as_deref()) {
                return Ok(HookOutput::None);
            }

            let path = match extract_write_path(event.tools.as_deref(), event.output.as_deref()) {
                Some(p) if !p.is_empty() => normalize_path(&p),
                _ => return Ok(HookOutput::None),
            };

            STATE.with(|s| {
                let mut guard = s.borrow_mut();
                let state = match guard.as_mut() {
                    Some(s) => s,
                    None => return,
                };
                if state.snapshots.iter().any(|(p, _)| p == &path) {
                    return; // already captured for this task
                }
                let before = std::fs::read(&path).ok();
                state.snapshots.push((path, Snapshot { before }));
            });

            Ok(HookOutput::None)
        }

        fn on_tool_call(
            _event: exports::murmur::hook::lifecycle::ToolEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_shell(
            _event: exports::murmur::hook::lifecycle::ShellEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_compaction(
            _event: exports::murmur::hook::lifecycle::CompactionEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_end(
            _event: exports::murmur::hook::lifecycle::SessionEndEvent,
        ) -> Result<HookOutput, String> {
            // Clear any residual state (end_turn already handled output).
            STATE.with(|s| s.borrow_mut().take());
            Ok(HookOutput::None)
        }

        fn on_task_start(
            _event: exports::murmur::hook::lifecycle::TaskStartEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_end(
            _event: exports::murmur::hook::lifecycle::TaskEndEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    export!(MurmurHookDiffSummary);
}

// ── native unit tests of the diff-shaping seam ──────────────────────────────
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn lines(range: std::ops::Range<usize>) -> Vec<u8> {
        range
            .map(|i| format!("line {i}\n"))
            .collect::<String>()
            .into_bytes()
    }

    // ── the create / modify / delete matrix ─────────────────────────────────

    #[test]
    fn modified_file_reports_its_added_and_removed_lines() {
        let before = b"alpha\nbeta\ngamma\n";
        let after = b"alpha\nBETA\ngamma\n";
        let entry = build_file_entry("src/main.rs", Some(before), Some(after));

        assert_eq!(entry["path"], json!("src/main.rs"));
        assert_eq!(entry["language"], json!("rust"));
        assert_eq!(entry["added_lines"], json!(1));
        assert_eq!(entry["removed_lines"], json!(1));
        assert_eq!(entry["total_changed_lines"], json!(2));
        assert_eq!(entry["truncated"], json!(false));
        let diff = entry["diff"]
            .as_str()
            .expect("a modification carries a diff");
        assert!(diff.starts_with("--- a/src/main.rs\n+++ b/src/main.rs\n"));
        assert!(diff.contains("-beta\n"));
        assert!(diff.contains("+BETA\n"));
        assert_eq!(entry["hunks"].as_array().expect("one hunk").len(), 1);
    }

    #[test]
    fn created_file_is_all_additions() {
        let entry = build_file_entry("new.py", None, Some(b"one\ntwo\n"));

        assert_eq!(entry["language"], json!("python"));
        assert_eq!(entry["added_lines"], json!(2));
        assert_eq!(entry["removed_lines"], json!(0));
        assert_eq!(entry["total_changed_lines"], json!(2));
        assert_eq!(entry["truncated"], json!(false));
    }

    /// A file that existed and is gone now must diff against an empty "after" — not
    /// collapse into the same zero entry a created-then-deleted file produces.
    #[test]
    fn deleted_file_is_all_deletions_not_a_no_op() {
        let entry = build_file_entry("gone.rs", Some(b"one\ntwo\nthree\n"), None);

        assert_eq!(entry["added_lines"], json!(0));
        assert_eq!(entry["removed_lines"], json!(3));
        assert_eq!(entry["total_changed_lines"], json!(3));
        let diff = entry["diff"].as_str().expect("a deletion carries a diff");
        assert!(diff.contains("-one\n"));
        assert!(diff.contains("-three\n"));
    }

    #[test]
    fn created_then_deleted_file_reports_no_net_change() {
        let entry = build_file_entry("scratch.txt", None, None);

        assert_eq!(
            entry,
            json!({
                "path": "scratch.txt", "language": "text",
                "diff": "", "hunks": [],
                "added_lines": 0, "removed_lines": 0,
                "truncated": false, "total_changed_lines": 0,
            })
        );
    }

    /// Unchanged content lands on the same zero entry as created-then-deleted.
    #[test]
    fn unchanged_file_reports_the_same_zero_entry() {
        let same = b"alpha\nbeta\n";
        let unchanged = build_file_entry("scratch.txt", Some(same), Some(same));
        let created_then_deleted = build_file_entry("scratch.txt", None, None);

        assert_eq!(unchanged, created_then_deleted);
    }

    // ── the 300-changed-line truncation threshold ───────────────────────────

    #[test]
    fn a_diff_at_the_threshold_is_not_truncated() {
        let entry = build_file_entry("big.rs", None, Some(&lines(0..TRUNCATE_THRESHOLD)));

        assert_eq!(entry["truncated"], json!(false));
        assert_eq!(entry["added_lines"], json!(TRUNCATE_THRESHOLD));
        assert_eq!(entry["removed_lines"], json!(0));
        assert_eq!(entry["total_changed_lines"], json!(TRUNCATE_THRESHOLD));
    }

    #[test]
    fn a_diff_over_the_threshold_keeps_the_full_count_and_trims_the_text() {
        let over = TRUNCATE_THRESHOLD + 51;
        let entry = build_file_entry("big.rs", None, Some(&lines(0..over)));

        assert_eq!(entry["truncated"], json!(true));
        // The full count survives even though the text does not.
        assert_eq!(entry["total_changed_lines"], json!(over));
        let added = entry["added_lines"].as_u64().expect("a count");
        let removed = entry["removed_lines"].as_u64().expect("a count");
        assert_eq!(added + removed, TRUNCATE_THRESHOLD as u64);
        // Re-parsing the emitted text yields exactly those counts.
        let diff = entry["diff"]
            .as_str()
            .expect("a truncated diff is still text");
        let (_, reparsed_added, reparsed_removed) = parse_diff_stats(diff);
        assert_eq!(reparsed_added as u64, added);
        assert_eq!(reparsed_removed as u64, removed);
    }

    // ── the 512 KiB cap ─────────────────────────────────────────────────────

    #[test]
    fn a_file_over_the_byte_cap_reports_null_stats() {
        let huge = vec![b'a'; MAX_FILE_BYTES + 1];
        let entry = build_file_entry("huge.txt", Some(b"small\n"), Some(&huge));

        assert_eq!(
            entry,
            json!({
                "path": "huge.txt", "language": "text",
                "diff": null, "hunks": [],
                "added_lines": null, "removed_lines": null,
                "truncated": true, "total_changed_lines": null,
            })
        );
    }

    // ── binary detection, and the order it runs in ──────────────────────────

    #[test]
    fn a_nul_byte_on_either_side_is_binary() {
        let expected = json!({"path": "blob.bin", "language": "text", "binary": true});
        assert_eq!(
            build_file_entry("blob.bin", Some(b"\x00\x01"), Some(b"text\n")),
            expected
        );
        assert_eq!(
            build_file_entry("blob.bin", Some(b"text\n"), Some(b"\x00\x01")),
            expected
        );
    }

    /// Invalid UTF-8 with no NUL byte in it is reported as binary too — the entry has no
    /// `diff` key at all, rather than a diff of replacement characters.
    #[test]
    fn invalid_utf8_without_a_nul_is_binary() {
        let entry = build_file_entry("blob.dat", Some(b"text\n"), Some(&[0xff, 0xfe, 0x41]));

        assert_eq!(
            entry,
            json!({"path": "blob.dat", "language": "text", "binary": true})
        );
        assert!(entry.get("diff").is_none());
    }

    /// Binary is decided before the size cap: a file over [`MAX_FILE_BYTES`] carrying a
    /// NUL reports `binary` and never `truncated`.
    #[test]
    fn binary_is_decided_before_the_byte_cap() {
        let mut huge = vec![b'a'; MAX_FILE_BYTES + 1];
        huge[0] = 0u8;
        let entry = build_file_entry("huge.bin", Some(b"small\n"), Some(&huge));

        assert_eq!(entry["binary"], json!(true));
        assert!(entry.get("truncated").is_none());
    }

    // ── the summary wrapper ─────────────────────────────────────────────────

    #[test]
    fn build_summary_wraps_the_entries_in_order() {
        let entries = vec![
            build_file_entry("a.rs", None, Some(b"x\n")),
            build_file_entry("b.rs", None, Some(b"y\n")),
        ];
        let summary = build_summary("sess_1", &entries);

        assert_eq!(summary["session_id"], json!("sess_1"));
        let files = summary["files"].as_array().expect("a files array");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], json!("a.rs"));
        assert_eq!(files[1]["path"], json!("b.rs"));
        assert_eq!(build_summary("sess_1", &[])["files"], json!([]));
    }

    // ── path extraction ─────────────────────────────────────────────────────

    #[test]
    fn extract_write_path_reads_every_layout_the_runtime_delivers() {
        // Direct operation object, for each writing operation.
        assert_eq!(
            try_extract_write_path(r#"{"write":{"path":"src/a.rs","content":"x"}}"#),
            Some("src/a.rs".to_string())
        );
        assert_eq!(
            try_extract_write_path(
                r#"{"edit":{"path":"src/e.rs","edits":[{"old":"a","new":"b"}]}}"#
            ),
            Some("src/e.rs".to_string())
        );
        // Anthropic tool_use block.
        assert_eq!(
            try_extract_write_path(
                r#"{"type":"tool_use","input":{"edit":{"path":"src/b.rs","edits":[]}}}"#
            ),
            Some("src/b.rs".to_string())
        );
        // Runtime envelope carrying the inner JSON as a string, and as an object.
        assert_eq!(
            try_extract_write_path(r#"{"data":"{\"write\":{\"path\":\"src/c.rs\"}}"}"#),
            Some("src/c.rs".to_string())
        );
        assert_eq!(
            try_extract_write_path(r#"{"data":{"edit":{"path":"src/c2.rs"}},"log_path":null}"#),
            Some("src/c2.rs".to_string())
        );
        // Double-encoded JSON string.
        assert_eq!(
            try_extract_write_path(r#""{\"write\":{\"path\":\"src/d.rs\"}}""#),
            Some("src/d.rs".to_string())
        );
        // An array of calls yields the first write.
        assert_eq!(
            try_extract_write_path(
                r#"[{"read":{"path":"r.rs"}},{"edit":{"path":"src/f.rs","edits":[]}}]"#
            ),
            Some("src/f.rs".to_string())
        );
        // `tools` is consulted before `output`.
        assert_eq!(
            extract_write_path(
                Some(r#"{"write":{"path":"from-tools.rs"}}"#),
                Some(r#"{"write":{"path":"from-output.rs"}}"#)
            ),
            Some("from-tools.rs".to_string())
        );
        // Nothing to find.
        assert_eq!(extract_write_path(None, None), None);
        assert_eq!(try_extract_write_path("not json"), None);
        assert_eq!(try_extract_write_path(r#"{"write":{"content":"x"}}"#), None);
        assert_eq!(try_extract_write_path(r#"{"write":"src/a.rs"}"#), None);
    }

    #[test]
    fn only_write_and_edit_calls_are_snapshotted() {
        let call = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        for key in ["write", "edit"] {
            let v = call(&format!(r#"{{"{key}":{{"path":"a"}}}}"#));
            assert!(write_op(v.as_object().unwrap()).is_some(), "{key}");
        }
        for s in [
            r#"{"read":{"path":"a"}}"#,
            r#"{"find":{"pattern":"a"}}"#,
            r#"{"edit":"a"}"#,
        ] {
            assert!(write_op(call(s).as_object().unwrap()).is_none(), "{s}");
        }
        for s in [
            r#"{"read":{"path":"src/a.rs"}}"#,
            r#"{"find":{"pattern":"x","dir":"src"}}"#,
            r#"{"type":"tool_use","input":{"read":{"path":"src/a.rs"}}}"#,
            r#"{"data":"{\"find\":{\"pattern\":\"x\"}}"}"#,
        ] {
            assert_eq!(try_extract_write_path(s), None, "{s}");
        }
    }

    #[test]
    fn the_pre_0_12_0_shape_yields_no_path_in_any_wrapping() {
        // No fallback: the editor refuses this shape, so a call carrying it wrote nothing.
        for s in [
            r#"{"operation":"write_file","dest_path":"src/a.rs","content":"x"}"#,
            r#"{"operation":"replace_in_file","dest_path":"src/a.rs"}"#,
            r#"{"type":"tool_use","input":{"operation":"write_file","dest_path":"src/b.rs"}}"#,
            r#"{"data":"{\"operation\":\"write_file\",\"dest_path\":\"src/c.rs\"}"}"#,
            r#""{\"operation\":\"write_file\",\"dest_path\":\"src/d.rs\"}""#,
            r#"{"operation":"read_file","path":"src/a.rs"}"#,
        ] {
            assert_eq!(try_extract_write_path(s), None, "{s}");
        }
    }

    #[test]
    fn only_the_editor_tool_is_snapshotted_under_its_bare_name() {
        assert!(is_editor_call(Some("tool-editor")));
        // The name it went by before the default artifacts dropped their `murmur-` prefix, and
        // its scoped install name: neither is what the model calls it.
        assert!(!is_editor_call(Some(&format!("murmur-{EDITOR_TOOL}"))));
        assert!(!is_editor_call(Some(&format!("murmur/{EDITOR_TOOL}"))));
        assert!(!is_editor_call(Some("tool-create")));
        assert!(!is_editor_call(None));
    }

    #[test]
    fn normalize_path_strips_only_a_leading_dot_slash() {
        assert_eq!(normalize_path("./x.rs"), "x.rs");
        assert_eq!(normalize_path("x.rs"), "x.rs");
    }
}
