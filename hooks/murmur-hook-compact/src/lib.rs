//! Compaction hook: when the session token threshold is reached, ask the model to
//! summarise the conversation and hand that summary back as the replacement context.
//!
//! There is no deterministic fallback. The hook makes one `murmur:runtime/inference`
//! call with the model the host resolved for compaction (`event.model`); if that call
//! fails *and* it named a distinct model, it retries exactly once with `model: none`
//! (the capsule's primary model). If that also fails, compaction fails hard — the same
//! observable outcome as any other driver inference failure.
//!
//! The logic is deliberately split into a `cfg`-independent [`logic`] module so the
//! whole control flow (including the retry) is unit-testable on the host without the
//! `wasm32` target: [`logic::compact_with`] takes the "call inference" step as a
//! closure. The `wasm_hook` module (compiled only for `wasm32`) is a thin adapter that
//! converts the WIT bindings to/from [`logic::PlainMessage`] and supplies the real
//! `run-inference` import as that closure.

// ── Pure, host-testable logic (no WASM bindings, no `cfg`) ────────────────────
pub mod logic {
    use serde_json::Value;

    /// The hook's own summarisation system prompt, used when the host supplies no
    /// `event.system-prompt` override. When an override *is* present, [`build_request`]
    /// uses it verbatim in place of this default (a full replacement, not a
    /// concatenation).
    pub const DEFAULT_SYSTEM_PROMPT: &str = "You are compacting an agent session so it can \
        continue in a fresh, smaller context window. Read the conversation transcript and \
        write a summary that lets the agent resume without re-reading any of it.\n\n\
        Cover, in this order and only where the transcript supports it:\n\
        1. The task the agent was given, in the user's own terms.\n\
        2. What has already been done, including files created or modified and commands run.\n\
        3. Decisions taken and the reasons for them, so they are not re-litigated.\n\
        4. Facts discovered about the codebase or environment that were expensive to find.\n\
        5. What is still outstanding, and the immediate next step.\n\n\
        Be specific: name files, symbols, commands and identifiers exactly as they appeared. \
        Do not invent anything the transcript does not state, and do not address the user — \
        write it as notes for the agent's own future self.";

    /// The instruction appended as the final transcript message, so the request always
    /// ends on a `user` turn and always names the task explicitly.
    pub const SUMMARY_INSTRUCTION: &str =
        "The conversation above is the session to compact. Write the summary now.";

    /// Marker the host folds into a `"tool"`-role message's `content` before dispatching
    /// a compaction event, because the WIT `message` record has no room for the sibling
    /// `tool_call_id`/`is_error` fields every inference driver requires. Kept in sync with
    /// `TOOL_MARKER` in murmur's `capsule-runtime/src/agent.rs`.
    pub const TOOL_MARKER: &str = "__murmur_tool_msg__";

    /// A role-tagged message, independent of the WIT bindings so it can be constructed
    /// and asserted on in host tests.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct PlainMessage {
        pub role: String,
        pub content: String,
    }

    impl PlainMessage {
        pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
            PlainMessage {
                role: role.into(),
                content: content.into(),
            }
        }
    }

    /// One `run-inference` request, in bindings-free form. The adapter maps this
    /// field-for-field onto the generated `InferenceRequest`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct InferenceCall {
        pub messages: Vec<PlainMessage>,
        pub system_prompt: String,
        pub model: Option<String>,
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

    /// Render one incoming event message into a transcript message safe to forward to a
    /// driver.
    ///
    /// A `"tool"`-role message arrives as the host's [`TOOL_MARKER`] envelope — its real
    /// `tool_call_id`/`is_error`/`body` live inside the JSON, and forwarding it as-is
    /// would reach the driver as a `"tool"` message with no `tool_call_id` (a hard driver
    /// error). It is therefore unwrapped into readable text, tagged with its
    /// `tool_call_id` so the model can still tell tool results apart, and re-roled to
    /// `"user"`. Anything that is not an `assistant` turn also becomes `"user"`, so the
    /// transcript only ever contains the two roles every driver accepts.
    pub fn render_message(message: &PlainMessage) -> PlainMessage {
        let role = if message.role == "assistant" {
            "assistant"
        } else {
            "user"
        };

        if message.role == "tool" {
            if let Some(text) = render_tool_envelope(&message.content) {
                return PlainMessage::new(role, text);
            }
        }

        PlainMessage::new(role, extract_text(&message.content))
    }

    /// Unwrap a [`TOOL_MARKER`] envelope into readable text. Returns `None` when the
    /// content is not an envelope (not JSON, not an object, or marker absent/false), so
    /// the caller falls back to plain [`extract_text`] handling.
    fn render_tool_envelope(content: &str) -> Option<String> {
        let parsed: Value = serde_json::from_str(content).ok()?;
        if parsed.get(TOOL_MARKER).and_then(Value::as_bool) != Some(true) {
            return None;
        }

        let tool_call_id = parsed
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let failed = parsed.get("is_error").and_then(Value::as_bool) == Some(true);
        let body = match parsed.get("body") {
            None | Some(Value::Null) => String::new(),
            Some(body) => extract_text(&body.to_string()),
        };

        let status = if failed { " (error)" } else { "" };
        Some(format!(
            "[tool result for call {tool_call_id}{status}]\n{body}"
        ))
    }

    /// Build the transcript handed to `run-inference`: every event message rendered by
    /// [`render_message`], then [`SUMMARY_INSTRUCTION`] as a final `user` turn.
    ///
    /// Content here is plain text, *not* JSON-encoded: the host forwards a hook's
    /// `run-inference` messages to the driver with `{"role": role, "content": content}`
    /// verbatim, with no parse step. (The replace-context path is the opposite — see
    /// [`build_replace_context`].)
    pub fn build_transcript(messages: &[PlainMessage]) -> Vec<PlainMessage> {
        let mut out: Vec<PlainMessage> = messages.iter().map(render_message).collect();
        out.push(PlainMessage::new("user", SUMMARY_INSTRUCTION));
        out
    }

    /// Build the request for one summarisation attempt. `system_prompt` is the host's
    /// `event.system-prompt` override when present; when `None`, it falls back to
    /// [`DEFAULT_SYSTEM_PROMPT`]. This `unwrap_or` is the single source of truth for the
    /// default — callers pass the override through unresolved.
    pub fn build_request(
        messages: &[PlainMessage],
        model: Option<String>,
        system_prompt: Option<&str>,
    ) -> InferenceCall {
        InferenceCall {
            messages: build_transcript(messages),
            system_prompt: system_prompt.unwrap_or(DEFAULT_SYSTEM_PROMPT).to_string(),
            model,
        }
    }

    /// Build the replace-context output: a single `user` message carrying the model's
    /// summary.
    ///
    /// One message, not a header/tail pair — the summary *is* the new context, and
    /// keeping recent turns verbatim is precisely the deterministic strategy this slice
    /// removes. `user` rather than `assistant` because the replacement seeds the next
    /// window and must not read as something the model already said.
    ///
    /// The content is the JSON serialization of the summary text, because the host parses
    /// a returned message's content back out of JSON (falling back to a raw text block).
    /// Encoding it means a summary that happens to look like JSON (`42`, `[...]`) still
    /// arrives as text.
    pub fn build_replace_context(summary: &str) -> Vec<PlainMessage> {
        let content = serde_json::to_string(summary).unwrap_or_else(|_| summary.to_string());
        vec![PlainMessage::new("user", content)]
    }

    /// The whole compaction control flow, with the inference call injected so it is
    /// testable on the host.
    ///
    /// * one call with `model`;
    /// * on failure, exactly one retry with `model: none` — but only when `model` was
    ///   `Some`, since retrying a `None` request would be a byte-identical repeat;
    /// * both failed (or the only attempt failed) → `Err`. No deterministic fallback.
    ///
    /// `system_prompt` is the host's `event.system-prompt` override (or `None` to use
    /// [`DEFAULT_SYSTEM_PROMPT`]). It is threaded into *both* [`build_request`] calls, so
    /// the primary attempt and the `model: none` fallback carry the identical prompt.
    pub fn compact_with<F>(
        messages: &[PlainMessage],
        model: Option<String>,
        system_prompt: Option<&str>,
        mut call: F,
    ) -> Result<Vec<PlainMessage>, String>
    where
        F: FnMut(InferenceCall) -> Result<String, String>,
    {
        let first = call(build_request(messages, model.clone(), system_prompt));

        let summary = match first {
            Ok(text) => text,
            Err(first_err) => {
                let Some(requested) = model else {
                    return Err(format!("compaction inference failed: {first_err}"));
                };
                // The failed attempt named a distinct model, so falling back to the
                // capsule's primary model is a genuinely different request.
                match call(build_request(messages, None, system_prompt)) {
                    Ok(text) => text,
                    Err(fallback_err) => {
                        return Err(format!(
                            "compaction inference failed with model '{requested}' \
                             ({first_err}) and with the capsule's primary model \
                             ({fallback_err})"
                        ))
                    }
                }
            }
        };

        Ok(build_replace_context(&summary))
    }
}

// ── WASM adapter: WIT bindings ↔ pure logic (wasm32 only) ─────────────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use crate::logic::{self, InferenceCall, PlainMessage};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    use murmur::runtime::inference::{
        run_inference, InferenceRequest, Message as InferenceMessage,
    };

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
            let plain: Vec<PlainMessage> = event
                .messages
                .iter()
                .map(|m| PlainMessage::new(m.role.clone(), m.content.clone()))
                .collect();

            // Forward the host's `event.system-prompt` override verbatim; `build_request`
            // is the single place that falls back to `DEFAULT_SYSTEM_PROMPT` when it is
            // `None`, so the adapter must not resolve it here.
            let new_messages =
                logic::compact_with(&plain, event.model, event.system_prompt.as_deref(), dispatch)?;

            Ok(HookOutput::ReplaceContext(
                new_messages
                    .into_iter()
                    .map(|m| Message {
                        role: m.role,
                        content: m.content,
                    })
                    .collect(),
            ))
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

    /// The one impure step of the control flow: hand a built request to the host's
    /// `murmur:runtime/inference` import and return just the completion text.
    fn dispatch(call: InferenceCall) -> Result<String, String> {
        let request = InferenceRequest {
            messages: call
                .messages
                .into_iter()
                .map(|m| InferenceMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
            system_prompt: Some(call.system_prompt),
            model: call.model,
        };
        run_inference(&request).map(|response| response.text)
    }

    export!(MurmurCompact);
}

// ── Host-runnable unit tests for the pure compaction logic ────────────────────
#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::logic::{
        build_replace_context, build_request, build_transcript, compact_with, extract_text,
        render_message, InferenceCall, PlainMessage, DEFAULT_SYSTEM_PROMPT, SUMMARY_INSTRUCTION,
        TOOL_MARKER,
    };

    fn msg(role: &str, content: &str) -> PlainMessage {
        PlainMessage::new(role, content)
    }

    fn history() -> Vec<PlainMessage> {
        vec![
            msg("user", "\"do the thing\""),
            msg("assistant", "\"done\""),
        ]
    }

    /// Records every request it is handed and replies from a scripted list of outcomes.
    struct FakeInference {
        calls: RefCell<Vec<InferenceCall>>,
        replies: RefCell<Vec<Result<String, String>>>,
    }

    impl FakeInference {
        fn new(replies: Vec<Result<String, String>>) -> Self {
            FakeInference {
                calls: RefCell::new(Vec::new()),
                replies: RefCell::new(replies),
            }
        }

        fn call(&self, request: InferenceCall) -> Result<String, String> {
            self.calls.borrow_mut().push(request);
            self.replies.borrow_mut().remove(0)
        }

        fn calls(self) -> Vec<InferenceCall> {
            self.calls.into_inner()
        }
    }

    // ── control flow ──────────────────────────────────────────────────────────

    #[test]
    fn happy_path_makes_exactly_one_call_with_the_requested_model() {
        let fake = FakeInference::new(vec![Ok("THE SUMMARY".to_string())]);

        let out = compact_with(&history(), Some("haiku-compact".to_string()), None, |r| {
            fake.call(r)
        })
        .expect("summarisation succeeded");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model.as_deref(), Some("haiku-compact"));
        assert_eq!(out, build_replace_context("THE SUMMARY"));
    }

    #[test]
    fn failure_retries_once_with_the_primary_model_and_uses_its_answer() {
        let fake = FakeInference::new(vec![
            Err("model not available".to_string()),
            Ok("FALLBACK SUMMARY".to_string()),
        ]);

        let out = compact_with(&history(), Some("haiku-compact".to_string()), None, |r| {
            fake.call(r)
        })
        .expect("fallback succeeded");

        let calls = fake.calls();
        assert_eq!(
            calls.len(),
            2,
            "one attempt per model, so each gets its own trace span"
        );
        assert_eq!(calls[0].model.as_deref(), Some("haiku-compact"));
        assert_eq!(
            calls[1].model, None,
            "the fallback asks for the capsule's primary model"
        );
        // Same transcript and system prompt both times — only the model differs.
        assert_eq!(calls[0].messages, calls[1].messages);
        assert_eq!(calls[0].system_prompt, calls[1].system_prompt);
        assert_eq!(out, build_replace_context("FALLBACK SUMMARY"));
    }

    #[test]
    fn no_redundant_retry_when_no_compaction_model_was_requested() {
        let fake = FakeInference::new(vec![Err("driver exploded".to_string())]);

        let err = compact_with(&history(), None, None, |r| fake.call(r)).unwrap_err();

        assert_eq!(
            fake.calls().len(),
            1,
            "a second `model: none` call would be identical"
        );
        assert!(err.contains("driver exploded"), "{err}");
    }

    #[test]
    fn both_attempts_failing_is_a_hard_error_naming_both_causes() {
        let fake = FakeInference::new(vec![
            Err("first boom".to_string()),
            Err("second boom".to_string()),
        ]);

        let err = compact_with(&history(), Some("haiku-compact".to_string()), None, |r| {
            fake.call(r)
        })
        .unwrap_err();

        assert_eq!(fake.calls().len(), 2);
        assert!(err.contains("haiku-compact"), "{err}");
        assert!(err.contains("first boom"), "{err}");
        assert!(err.contains("second boom"), "{err}");
    }

    // ── request shape ─────────────────────────────────────────────────────────

    #[test]
    fn system_prompt_override_absent_uses_the_built_in_default_on_both_attempts() {
        // No `event.system-prompt`: both the primary and fallback attempts fall back to
        // `DEFAULT_SYSTEM_PROMPT` — the pre-existing behaviour, unchanged.
        let fake = FakeInference::new(vec![Err("nope".to_string()), Ok("s".to_string())]);
        compact_with(&history(), Some("m".to_string()), None, |r| fake.call(r)).unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(call.system_prompt, DEFAULT_SYSTEM_PROMPT);
        }
    }

    #[test]
    fn system_prompt_override_present_replaces_the_default_on_both_attempts() {
        // The override reaches both the primary attempt and the `model: none` fallback,
        // identically — never just the first (mirrors the model-field assertion in
        // `failure_retries_once_with_the_primary_model_and_uses_its_answer`).
        let override_prompt = "task = X, currently editing Y, already tried Z";
        let fake = FakeInference::new(vec![
            Err("model not available".to_string()),
            Ok("FALLBACK SUMMARY".to_string()),
        ]);

        compact_with(
            &history(),
            Some("haiku-compact".to_string()),
            Some(override_prompt),
            |r| fake.call(r),
        )
        .expect("fallback succeeded");

        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].system_prompt, override_prompt);
        assert_eq!(calls[1].system_prompt, override_prompt);
        // Both attempts carry the *same* prompt — a regression that threads the override
        // into only the first call would fail here.
        assert_eq!(calls[0].system_prompt, calls[1].system_prompt);
    }

    #[test]
    fn system_prompt_override_present_without_fallback_yields_one_overridden_call() {
        // A double that succeeds on the first attempt: exactly one recorded call, and its
        // system prompt is the override verbatim.
        let fake = FakeInference::new(vec![Ok("SUMMARY".to_string())]);

        compact_with(
            &history(),
            Some("haiku-compact".to_string()),
            Some("custom prompt"),
            |r| fake.call(r),
        )
        .expect("summarisation succeeded");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].system_prompt, "custom prompt");
    }

    #[test]
    fn system_prompt_override_is_a_full_replacement_not_a_concatenation() {
        // The recorded prompt is byte-identical to the override, and neither string
        // contains the other as a substring — ruling out `format!("{default}\n{over}")`.
        let override_prompt = "task = X, currently editing Y, already tried Z";
        let request = build_request(&history(), None, Some(override_prompt));

        assert_eq!(request.system_prompt, override_prompt);
        assert!(!request.system_prompt.contains(DEFAULT_SYSTEM_PROMPT));
        assert!(!DEFAULT_SYSTEM_PROMPT.contains(override_prompt));
    }

    #[test]
    fn transcript_carries_every_message_plus_a_final_user_instruction() {
        let transcript = build_transcript(&history());

        assert_eq!(transcript.len(), history().len() + 1);
        assert_eq!(transcript[0], msg("user", "do the thing"));
        assert_eq!(transcript[1], msg("assistant", "done"));
        assert_eq!(transcript[2], msg("user", SUMMARY_INSTRUCTION));
    }

    #[test]
    fn replace_context_is_one_user_message_holding_the_json_encoded_summary() {
        let out = build_replace_context("summary body");

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, "\"summary body\"");
        // The host parses this back out of JSON, so a JSON-looking summary stays text.
        assert_eq!(build_replace_context("42")[0].content, "\"42\"");
    }

    #[test]
    fn request_model_is_passed_through_verbatim() {
        assert_eq!(build_request(&history(), None, None).model, None);
        assert_eq!(
            build_request(&history(), Some("x".to_string()), None)
                .model
                .as_deref(),
            Some("x")
        );
    }

    // ── tool-marker rendering ─────────────────────────────────────────────────

    fn envelope(
        tool_call_id: serde_json::Value,
        is_error: serde_json::Value,
        body: serde_json::Value,
    ) -> String {
        serde_json::json!({
            TOOL_MARKER: true,
            "tool_call_id": tool_call_id,
            "is_error": is_error,
            "body": body,
        })
        .to_string()
    }

    #[test]
    fn tool_marker_becomes_readable_text_under_a_non_tool_role() {
        let wrapped = envelope(
            serde_json::json!("call_42"),
            serde_json::Value::Null,
            serde_json::json!("3 tests passed"),
        );

        let rendered = render_message(&msg("tool", &wrapped));

        assert_ne!(
            rendered.role, "tool",
            "a tool role with no tool_call_id breaks every driver"
        );
        assert_eq!(rendered.role, "user");
        assert!(rendered.content.contains("call_42"), "{}", rendered.content);
        assert!(
            rendered.content.contains("3 tests passed"),
            "{}",
            rendered.content
        );
        // No raw wrapper JSON reaches the model.
        assert!(
            !rendered.content.contains(TOOL_MARKER),
            "{}",
            rendered.content
        );
        assert!(
            !rendered.content.contains("\"body\""),
            "{}",
            rendered.content
        );
    }

    #[test]
    fn tool_marker_body_content_blocks_are_flattened_and_errors_are_flagged() {
        let wrapped = envelope(
            serde_json::json!("call_7"),
            serde_json::json!(true),
            serde_json::json!([{"type": "text", "text": "exit code 1"}]),
        );

        let rendered = render_message(&msg("tool", &wrapped));

        assert!(rendered.content.contains("(error)"), "{}", rendered.content);
        assert!(
            rendered.content.contains("exit code 1"),
            "{}",
            rendered.content
        );
    }

    #[test]
    fn tool_marker_with_missing_fields_still_renders() {
        let wrapped = envelope(
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );

        let rendered = render_message(&msg("tool", &wrapped));

        assert_eq!(rendered.role, "user");
        assert!(rendered.content.contains("unknown"), "{}", rendered.content);
    }

    #[test]
    fn tool_message_without_a_valid_marker_falls_back_to_extract_text() {
        // Not JSON at all.
        let raw = render_message(&msg("tool", "just some output"));
        assert_eq!(raw, msg("user", "just some output"));

        // JSON, but not our envelope: treated as ordinary content.
        let other = render_message(&msg("tool", r#"{"__murmur_tool_msg__":false,"body":"x"}"#));
        assert_eq!(other.role, "user");
        assert_eq!(other.content, r#"{"__murmur_tool_msg__":false,"body":"x"}"#);

        // A plain JSON string content.
        let quoted = render_message(&msg("tool", "\"quoted output\""));
        assert_eq!(quoted, msg("user", "quoted output"));
    }

    #[test]
    fn non_tool_messages_keep_the_existing_extract_text_handling() {
        assert_eq!(
            render_message(&msg("assistant", r#"[{"type":"text","text":"hello"}]"#)),
            msg("assistant", "hello")
        );
        assert_eq!(
            render_message(&msg("user", "\"plain\"")),
            msg("user", "plain")
        );
        assert_eq!(render_message(&msg("user", "raw")), msg("user", "raw"));
        // An unknown role is normalised to `user` rather than forwarded as-is.
        assert_eq!(
            render_message(&msg("system", "\"note\"")),
            msg("user", "note")
        );
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
}
