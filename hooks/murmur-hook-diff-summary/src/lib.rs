#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::{cell::RefCell, path::Path};

    use serde_json::{json, Value};
    use similar::TextDiff;

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    const CONTEXT_LINES: usize = 2;
    const TRUNCATE_THRESHOLD: usize = 300;
    const MAX_FILE_BYTES: usize = 512 * 1024;

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
        static STATE: RefCell<Option<HookState>> = RefCell::new(None);
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
                        .map(|(path, snap)| build_file_entry(path, snap))
                        .collect();
                    state.snapshots.clear();
                    let session_id = state.session_id.clone();
                    let json_str = serde_json::to_string(&serde_json::json!({
                        "session_id": session_id,
                        "files": files,
                    }))
                    .ok()?;
                    Some(json_str)
                });
                // Always emit an artifact at end_turn (even empty files list) so
                // the pipeline can be verified end-to-end.
                let payload = output.unwrap_or_else(|| {
                    r#"{"session_id":"unknown","files":[]}"#.to_string()
                });
                return Ok(HookOutput::Artifact(payload));
            }

            // Before-state snapshot: only for editor write/replace operations.
            if event.tool_name.as_deref() != Some("murmur-tool-editor") {
                return Ok(HookOutput::None);
            }

            let path = match extract_write_path(
                event.tools.as_deref(),
                event.output.as_deref(),
            ) {
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

    // ── path extraction ───────────────────────────────────────────────────────
    //
    // The inference event carries the tool call data in `tools` and/or `output`.
    // We try multiple JSON layouts because the runtime may deliver the data as a
    // raw operation object, an Anthropic tool_use block, or a wrapping envelope.

    fn extract_write_path(tools: Option<&str>, output: Option<&str>) -> Option<String> {
        [tools, output]
            .into_iter()
            .flatten()
            .find_map(|src| try_extract_write_path(src))
    }

    fn try_extract_write_path(s: &str) -> Option<String> {
        let v: Value = serde_json::from_str(s).ok()?;
        extract_from_value(&v)
    }

    fn extract_from_value(v: &Value) -> Option<String> {
        match v {
            Value::Object(map) => {
                let op = map.get("operation").and_then(|o| o.as_str());
                let path_str = map.get("path").and_then(|p| p.as_str());

                // Direct tool input: {"operation":"write_file","path":"..."}
                if let Some(op_name) = op {
                    if is_write_op(op_name) {
                        return path_str.map(str::to_string);
                    }
                    return None; // read_file / find_in_files — not a write
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

    fn is_write_op(op: &str) -> bool {
        matches!(op, "write_file" | "replace_in_file")
    }

    fn normalize_path(path: &str) -> String {
        path.strip_prefix("./").unwrap_or(path).to_string()
    }

    // ── diff computation ──────────────────────────────────────────────────────

    fn build_file_entry(path: &str, snap: &Snapshot) -> Value {
        let lang = language_for(path);

        match std::fs::read(path) {
            Ok(after) => {
                let before = snap.before.as_deref().unwrap_or(&[]);
                diff_entry(path, lang, before, &after)
            }
            Err(_) => match &snap.before {
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

    fn diff_entry(path: &str, lang: &'static str, before: &[u8], after: &[u8]) -> Value {
        if is_binary(before) || is_binary(after) {
            return json!({"path": path, "language": lang, "binary": true});
        }

        let (before_str, after_str) = match (
            std::str::from_utf8(before),
            std::str::from_utf8(after),
        ) {
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

    // ── diff text helpers ─────────────────────────────────────────────────────

    fn parse_diff_stats(diff: &str) -> (Vec<Value>, usize, usize) {
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

    fn parse_hunk_header(line: &str) -> Option<Value> {
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

    fn parse_range(s: &str) -> Option<(u32, u32)> {
        if let Some((a, b)) = s.split_once(',') {
            Some((a.parse().ok()?, b.parse().ok()?))
        } else {
            Some((s.parse().ok()?, 1))
        }
    }

    // Keeps diff lines until `max` change lines (+/-) have been emitted, then
    // stops mid-hunk. The result is a syntactically incomplete diff but contains
    // exactly the first `max` changed lines, which is all the renderer needs.
    fn truncate_diff(diff: &str, max: usize) -> String {
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

    // ── misc helpers ──────────────────────────────────────────────────────────

    fn is_binary(data: &[u8]) -> bool {
        data.contains(&0u8)
    }

    fn language_for(path: &str) -> &'static str {
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

    export!(MurmurHookDiffSummary);
}
