//! Compaction hook: when the session token threshold is reached, replace the earlier
//! conversation history with a summary header and a pointer to the on-disk checkpoint
//! files, keeping the most recent turns verbatim.
//!
//! The logic that builds the `replace-context` output is deliberately split into a
//! `cfg`-independent [`logic`] module so it can be unit-tested on the host without the
//! `wasm32` target. The `wasm_hook` module (compiled only for `wasm32`) is a thin
//! adapter: it converts the WIT `Message` bindings to/from the plain [`logic::PlainMessage`]
//! type, calls the pure functions, and performs the checkpoint file I/O.

// ── Pure, host-testable logic (no WASM bindings, no `cfg`) ────────────────────
pub mod logic {
    use serde_json::Value;

    /// Minimum number of recent messages kept verbatim after the summary header.
    pub const PRESERVE_LAST_N: usize = 8;

    /// The fixed pointer prepended to the replaced context. Keeping this as a plain
    /// `&str` (rather than a `format!` with no arguments) is both correct and clippy-clean.
    pub const CHECKPOINT_NOTICE: &str = "Previous session summary:\n\n\
         [Checkpoints written to workdir/checkpoints/: summary.md, plan.json, decisions.json]";

    /// A role-tagged message, independent of the WIT bindings so it can be constructed
    /// and asserted on in host tests.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct PlainMessage {
        pub role: String,
        pub content: String,
    }

    impl PlainMessage {
        pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
            PlainMessage { role: role.into(), content: content.into() }
        }
    }

    /// Extract readable text from a message's content field. The content is stored as its
    /// JSON serialization (either an array of content blocks or a plain string).
    pub fn extract_text(content: &str) -> String {
        if let Ok(Value::Array(blocks)) = serde_json::from_str(content) {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        b.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect();
            if !parts.is_empty() {
                return parts.join(" ");
            }
        }
        if let Ok(Value::String(s)) = serde_json::from_str(content) {
            return s;
        }
        content.to_string()
    }

    /// Build the text summary of the session — message counts, threshold, and a snippet
    /// of the last assistant response. Included both in `checkpoints/summary.md` and as
    /// the summary message of the replace-context output.
    pub fn build_summary(messages: &[PlainMessage], session_tokens: u64, threshold: f64) -> String {
        let user_count = messages.iter().filter(|m| m.role == "user").count();
        let assistant_count = messages.iter().filter(|m| m.role == "assistant").count();

        let last_assistant_snippet = messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| {
                let text = extract_text(&m.content);
                if text.len() > 300 {
                    // Slice on a char boundary so multi-byte text can't panic.
                    let end = text
                        .char_indices()
                        .take_while(|(i, _)| *i <= 300)
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    format!("{}…", &text[..end])
                } else {
                    text
                }
            })
            .unwrap_or_default();

        let mut summary = format!(
            "Session compacted at {pct:.0}% of context window ({session_tokens} tokens).\n\
             Original history: {total} messages ({user} user, {asst} assistant).\n\
             The {keep} most recent turns are preserved verbatim below.",
            pct = threshold * 100.0,
            total = messages.len(),
            user = user_count,
            asst = assistant_count,
            keep = PRESERVE_LAST_N.min(messages.len()),
        );

        if !last_assistant_snippet.is_empty() {
            summary.push_str("\n\nLast assistant response:\n");
            summary.push_str(&last_assistant_snippet);
        }

        summary
    }

    /// Build the full replace-context message list that seeds the next context window:
    ///
    /// 1. a `user` message pointing at the checkpoint files ([`CHECKPOINT_NOTICE`]),
    /// 2. an `assistant` message carrying the session summary, then
    /// 3. the last [`PRESERVE_LAST_N`] messages, verbatim.
    ///
    /// This is the exact content committed via `HookOutput::ReplaceContext`.
    pub fn build_replace_context(
        messages: &[PlainMessage],
        session_tokens: u64,
        threshold: f64,
    ) -> Vec<PlainMessage> {
        let summary_text = build_summary(messages, session_tokens, threshold);
        let preserve_from = messages.len().saturating_sub(PRESERVE_LAST_N);

        let mut out = Vec::with_capacity(2 + (messages.len() - preserve_from));
        // The checkpoint pointer is the first message, so re-orientation to the
        // checkpoint files is the natural first move after compaction.
        out.push(PlainMessage::new("user", CHECKPOINT_NOTICE));
        out.push(PlainMessage::new("assistant", summary_text));
        out.extend(messages[preserve_from..].iter().cloned());
        out
    }

    /// Render the `checkpoints/summary.md` file body for a given summary.
    pub fn checkpoint_markdown(summary: &str, original_count: usize) -> String {
        format!(
            "# Compaction Summary\n\n\
             {summary}\n\n\
             ## Original Message Count\n\n\
             {original_count} messages\n"
        )
    }
}

// ── WASM adapter: WIT bindings + checkpoint file I/O (wasm32 only) ─────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::fs;

    use crate::logic::{self, PlainMessage};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    pub struct MurmurCompact;

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, Message, SessionContext,
        SessionEndEvent, ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    };

    impl Guest for MurmurCompact {
        fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_inference(_event: InferenceEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_tool_call(_event: ToolEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_shell(_event: ShellEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_compaction(event: CompactionEvent) -> Result<HookOutput, String> {
            compact(event)
                .map(HookOutput::ReplaceContext)
                .map_err(|e| format!("compact failed: {e}"))
        }

        fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_start(_event: TaskStartEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_end(_event: TaskEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    fn compact(event: CompactionEvent) -> Result<Vec<Message>, String> {
        // Convert WIT messages to the plain, host-testable representation.
        let plain: Vec<PlainMessage> = event
            .messages
            .iter()
            .map(|m| PlainMessage::new(m.role.clone(), m.content.clone()))
            .collect();
        let original_count = plain.len();

        let summary_text = logic::build_summary(&plain, event.session_tokens, event.threshold);
        write_checkpoints(&summary_text, original_count)
            .map_err(|e| format!("checkpoint write failed: {e}"))?;

        // Build the replace-context list with the pure logic, then map back to WIT.
        let new_messages = logic::build_replace_context(&plain, event.session_tokens, event.threshold);
        Ok(new_messages
            .into_iter()
            .map(|m| Message { role: m.role, content: m.content })
            .collect())
    }

    // Writes checkpoint files to workdir/checkpoints/. Files: summary.md (compaction
    // summary), plan.json and decisions.json (populated by LLM-powered compaction once
    // the runtime supports wasi:http in hooks; stubs written now for schema compliance).
    // These are the files referenced by the replace-context pointer.
    fn write_checkpoints(summary: &str, original_count: usize) -> Result<(), std::io::Error> {
        fs::create_dir_all("checkpoints")?;
        fs::write("checkpoints/summary.md", logic::checkpoint_markdown(summary, original_count))?;
        fs::write("checkpoints/plan.json", "{\"tasks\":[]}")?;
        fs::write("checkpoints/decisions.json", "{\"decisions\":[]}")?;
        Ok(())
    }

    export!(MurmurCompact);
}

// ── Host-runnable unit tests for the pure replace-context logic ───────────────
#[cfg(test)]
mod tests {
    use super::logic::{
        self, build_replace_context, build_summary, extract_text, PlainMessage, CHECKPOINT_NOTICE,
        PRESERVE_LAST_N,
    };

    fn msg(role: &str, content: &str) -> PlainMessage {
        PlainMessage::new(role, content)
    }

    #[test]
    fn checkpoint_pointer_is_first_message_and_points_at_checkpoint_files() {
        let messages = vec![
            msg("user", "\"first question\""),
            msg("assistant", "\"first answer\""),
            msg("user", "\"second question\""),
            msg("assistant", "\"final answer\""),
        ];

        let out = build_replace_context(&messages, 42_000, 0.85);

        // 1. The first message is the user-role checkpoint pointer, verbatim.
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, CHECKPOINT_NOTICE);
        // The pointer names all three checkpoint files under workdir/checkpoints/.
        assert!(out[0].content.contains("workdir/checkpoints/"));
        assert!(out[0].content.contains("summary.md"));
        assert!(out[0].content.contains("plan.json"));
        assert!(out[0].content.contains("decisions.json"));

        // 2. The second message is the assistant-role summary.
        assert_eq!(out[1].role, "assistant");
        assert!(out[1].content.contains("Session compacted at 85% of context window"));
        assert!(out[1].content.contains("42000 tokens"));
        assert!(out[1].content.contains("4 messages (2 user, 2 assistant)"));
    }

    #[test]
    fn short_history_is_preserved_verbatim_after_the_header_pair() {
        // Fewer than PRESERVE_LAST_N messages: all are kept verbatim after the 2-message
        // header pair, in order.
        let messages = vec![
            msg("user", "\"q1\""),
            msg("assistant", "\"a1\""),
            msg("user", "\"q2\""),
        ];

        let out = build_replace_context(&messages, 10, 0.5);

        assert_eq!(out.len(), 2 + messages.len());
        assert_eq!(out[0].content, CHECKPOINT_NOTICE);
        assert_eq!(&out[2..], &messages[..]);
    }

    #[test]
    fn only_last_n_messages_are_preserved_when_history_is_long() {
        // 20 messages -> header pair + exactly the last PRESERVE_LAST_N verbatim.
        let messages: Vec<PlainMessage> = (0..20)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                msg(role, &format!("\"turn {i}\""))
            })
            .collect();

        let out = build_replace_context(&messages, 100, 0.9);

        assert_eq!(out.len(), 2 + PRESERVE_LAST_N);
        assert_eq!(out[0].content, CHECKPOINT_NOTICE);
        // The preserved tail is exactly the last PRESERVE_LAST_N source messages, in order.
        assert_eq!(&out[2..], &messages[messages.len() - PRESERVE_LAST_N..]);
    }

    #[test]
    fn summary_includes_last_assistant_snippet_extracted_from_content_blocks() {
        // Content stored as a JSON array of content blocks: extract_text pulls the text.
        let messages = vec![
            msg("user", "\"hi\""),
            msg(
                "assistant",
                r#"[{"type":"text","text":"the decisive final answer"}]"#,
            ),
        ];

        let out = build_replace_context(&messages, 1, 0.1);
        assert!(out[1].content.contains("Last assistant response:"));
        assert!(out[1].content.contains("the decisive final answer"));
    }

    #[test]
    fn extract_text_handles_blocks_plain_string_and_raw() {
        assert_eq!(
            extract_text(r#"[{"type":"text","text":"hello"},{"type":"text","text":"world"}]"#),
            "hello world"
        );
        assert_eq!(extract_text(r#""just a string""#), "just a string");
        // Not valid JSON: falls back to the raw content.
        assert_eq!(extract_text("raw text"), "raw text");
    }

    #[test]
    fn summary_snippet_truncation_is_char_boundary_safe() {
        // A long multi-byte assistant message must not panic when truncated at ~300 bytes.
        let long = format!("\"{}\"", "é".repeat(400));
        let messages = vec![msg("assistant", &long)];
        let summary = build_summary(&messages, 5, 0.6);
        assert!(summary.contains('…'));
    }

    #[test]
    fn checkpoint_markdown_embeds_summary_and_count() {
        let md = logic::checkpoint_markdown("SUMMARY BODY", 12);
        assert!(md.starts_with("# Compaction Summary"));
        assert!(md.contains("SUMMARY BODY"));
        assert!(md.contains("12 messages"));
    }
}
