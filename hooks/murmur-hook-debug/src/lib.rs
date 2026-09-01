//! murmur-hook-debug: append one JSON line per lifecycle dispatch to `hook-debug.jsonl`.
//!
//! The crate's whole job is serialising the events it is handed, so the line builders live
//! here at the crate root over plain mirrors of the WIT records rather than inside
//! `wasm_hook`. Everything under `wasm_hook` is gated on `target_arch = "wasm32"` and so is
//! unreachable from `cargo test`; this seam is what makes the key set of each line
//! assertable natively. The adapter converts the bindgen types into these mirrors and does
//! the one thing the mirrors cannot: open the file.

use serde_json::{json, Value};

/// Plain mirror of `murmur:hook/lifecycle`'s `message`. Only the count reaches
/// `hook-debug.jsonl`, but the mirror carries the collection so the derivation stays in
/// [`compaction_line`] rather than in the adapter.
pub struct Message {
    pub role: String,
    pub content: String,
    pub id: Option<String>,
    pub source_id: Option<String>,
}

/// Plain mirror of `stage-event`.
pub struct StageEvent {
    pub shell_allow: Vec<String>,
}

/// Plain mirror of `session-context`.
pub struct SessionContext {
    pub capsule_name: String,
    pub capsule_version: String,
    pub session_id: String,
    pub model: String,
    pub capabilities: Vec<String>,
}

/// Plain mirror of `inference-event`, narrowed to the fields this hook records. The
/// prompt, completion and tool manifest are deliberately absent: they are unbounded and
/// this file is a dispatch log, not a transcript.
pub struct InferenceEvent {
    pub turn: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub decision: String,
    pub tool_name: Option<String>,
}

/// Plain mirror of `tool-outcome`.
pub struct ToolOutcome {
    pub output_bytes: u64,
    pub duration_ms: u64,
    pub status: String,
}

/// Plain mirror of `tool-event`, narrowed to the fields this hook records.
pub struct ToolEvent {
    pub turn: u32,
    pub tool_name: String,
    pub input_bytes: u64,
    /// `none` is the decision-point dispatch — the call has not run.
    pub outcome: Option<ToolOutcome>,
}

/// Plain mirror of `shell-outcome`.
pub struct ShellOutcome {
    pub exit_code: i32,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub duration_ms: u64,
}

/// Plain mirror of `shell-event`, narrowed to the fields this hook records.
pub struct ShellEvent {
    pub turn: u32,
    pub command: String,
    /// `none` is the decision-point dispatch — the command has not run.
    pub outcome: Option<ShellOutcome>,
}

/// Plain mirror of `compaction-event`, narrowed to the fields this hook records.
pub struct CompactionEvent {
    pub messages: Vec<Message>,
    pub session_tokens: u64,
    pub threshold: f64,
}

/// Plain mirror of `session-end-event`.
pub struct SessionEndEvent {
    pub total_turns: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tool_calls: u32,
    pub total_shell_calls: u32,
    pub duration_ms: u64,
    pub exit_status: String,
}

pub fn stage_line(event: &StageEvent) -> Value {
    json!({
        "event": "stage",
        "shell_allow": event.shell_allow,
    })
}

pub fn session_start_line(ctx: &SessionContext) -> Value {
    json!({
        "event": "session-start",
        "capsule_name": ctx.capsule_name,
        "capsule_version": ctx.capsule_version,
        "session_id": ctx.session_id,
        "model": ctx.model,
        "capabilities": ctx.capabilities,
    })
}

pub fn inference_line(event: &InferenceEvent) -> Value {
    json!({
        "event": "inference",
        "turn": event.turn,
        "input_tokens": event.input_tokens,
        "output_tokens": event.output_tokens,
        "decision": event.decision,
        "tool_name": event.tool_name,
    })
}

/// `None` at the decision-point dispatch — the call has not run, so three of the seven
/// keys would have no value. This hook is an observer, not a policy: it writes one line
/// per *completed* call, so `hook-debug.jsonl` never carries a second line for the same
/// call, nor a line for a call that was refused or never made.
pub fn tool_call_line(event: &ToolEvent) -> Option<Value> {
    let outcome = event.outcome.as_ref()?;
    Some(json!({
        "event": "tool-call",
        "turn": event.turn,
        "tool_name": event.tool_name,
        "input_bytes": event.input_bytes,
        "output_bytes": outcome.output_bytes,
        "duration_ms": outcome.duration_ms,
        "status": outcome.status,
    }))
}

/// `None` at the decision-point dispatch — the command has not run, so four of the seven
/// keys would have no value. Same choice as [`tool_call_line`]: record only the completed
/// call, so one shell call is one line in `hook-debug.jsonl` rather than two.
pub fn shell_line(event: &ShellEvent) -> Option<Value> {
    let outcome = event.outcome.as_ref()?;
    Some(json!({
        "event": "shell",
        "turn": event.turn,
        "command": event.command,
        "exit_code": outcome.exit_code,
        "stdout_bytes": outcome.stdout_bytes,
        "stderr_bytes": outcome.stderr_bytes,
        "duration_ms": outcome.duration_ms,
    }))
}

pub fn compaction_line(event: &CompactionEvent) -> Value {
    json!({
        "event": "compaction",
        "message_count": event.messages.len(),
        "session_tokens": event.session_tokens,
        "threshold": event.threshold,
    })
}

pub fn session_end_line(event: &SessionEndEvent) -> Value {
    json!({
        "event": "session-end",
        "total_turns": event.total_turns,
        "total_input_tokens": event.total_input_tokens,
        "total_output_tokens": event.total_output_tokens,
        "total_tool_calls": event.total_tool_calls,
        "total_shell_calls": event.total_shell_calls,
        "duration_ms": event.duration_ms,
        "exit_status": event.exit_status,
    })
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::{fs::OpenOptions, io::Write};

    use super::{
        compaction_line, inference_line, session_end_line, session_start_line, shell_line,
        stage_line, tool_call_line,
    };

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    pub struct MurmurHookDebug;

    use exports::murmur::hook::lifecycle::HookOutput;

    impl exports::murmur::hook::lifecycle::Guest for MurmurHookDebug {
        fn on_stage(
            event: exports::murmur::hook::lifecycle::StageEvent,
        ) -> Result<HookOutput, String> {
            write_event(stage_line(&super::StageEvent {
                shell_allow: event.shell_allow,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_session_start(
            ctx: exports::murmur::hook::lifecycle::SessionContext,
        ) -> Result<HookOutput, String> {
            write_event(session_start_line(&super::SessionContext {
                capsule_name: ctx.capsule_name,
                capsule_version: ctx.capsule_version,
                session_id: ctx.session_id,
                model: ctx.model,
                capabilities: ctx.capabilities,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_inference(
            event: exports::murmur::hook::lifecycle::InferenceEvent,
        ) -> Result<HookOutput, String> {
            write_event(inference_line(&super::InferenceEvent {
                turn: event.turn,
                input_tokens: event.input_tokens,
                output_tokens: event.output_tokens,
                decision: event.decision,
                tool_name: event.tool_name,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_tool_call(
            event: exports::murmur::hook::lifecycle::ToolEvent,
        ) -> Result<HookOutput, String> {
            let line = tool_call_line(&super::ToolEvent {
                turn: event.turn,
                tool_name: event.tool_name,
                input_bytes: event.input_bytes,
                outcome: event.outcome.map(|o| super::ToolOutcome {
                    output_bytes: o.output_bytes,
                    duration_ms: o.duration_ms,
                    status: o.status,
                }),
            });
            let Some(line) = line else {
                return Ok(HookOutput::None);
            };
            write_event(line)?;
            Ok(HookOutput::None)
        }

        fn on_shell(
            event: exports::murmur::hook::lifecycle::ShellEvent,
        ) -> Result<HookOutput, String> {
            let line = shell_line(&super::ShellEvent {
                turn: event.turn,
                command: event.command,
                outcome: event.outcome.map(|o| super::ShellOutcome {
                    exit_code: o.exit_code,
                    stdout_bytes: o.stdout_bytes,
                    stderr_bytes: o.stderr_bytes,
                    duration_ms: o.duration_ms,
                }),
            });
            let Some(line) = line else {
                return Ok(HookOutput::None);
            };
            write_event(line)?;
            Ok(HookOutput::None)
        }

        fn on_compaction(
            event: exports::murmur::hook::lifecycle::CompactionEvent,
        ) -> Result<HookOutput, String> {
            write_event(compaction_line(&super::CompactionEvent {
                messages: event
                    .messages
                    .into_iter()
                    .map(|m| super::Message {
                        role: m.role,
                        content: m.content,
                        id: m.id,
                        source_id: m.source_id,
                    })
                    .collect(),
                session_tokens: event.session_tokens,
                threshold: event.threshold,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_session_end(
            event: exports::murmur::hook::lifecycle::SessionEndEvent,
        ) -> Result<HookOutput, String> {
            write_event(session_end_line(&super::SessionEndEvent {
                total_turns: event.total_turns,
                total_input_tokens: event.total_input_tokens,
                total_output_tokens: event.total_output_tokens,
                total_tool_calls: event.total_tool_calls,
                total_shell_calls: event.total_shell_calls,
                duration_ms: event.duration_ms,
                exit_status: event.exit_status,
            }))?;
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

    fn write_event(value: serde_json::Value) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open("hook-debug.jsonl")
            .map_err(|error| format!("failed to open hook-debug.jsonl: {error}"))?;
        writeln!(file, "{value}").map_err(|error| format!("failed to write hook event: {error}"))
    }

    export!(MurmurHookDebug);
}

// ── native unit tests of the line-building seam ─────────────────────────────
//
// Two tests, deliberately: the crate serialises its input and does nothing else, so
// there is exactly one line shape per dispatch to pin and one branch — the decision-point
// dispatch — to pin as writing nothing.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Round-trip each line through the JSONL encoding the hook actually writes, so the
    /// assertion covers what lands on disk rather than the in-memory `Value`.
    fn written(line: &Value) -> Value {
        serde_json::from_str(&line.to_string()).expect("each written line is valid JSON")
    }

    fn keys(line: &Value) -> Vec<&str> {
        line.as_object()
            .expect("every line is a JSON object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn every_dispatch_writes_its_documented_keys_and_values() {
        let stage = written(&stage_line(&StageEvent {
            shell_allow: vec!["git".to_string(), "cargo".to_string()],
        }));
        assert_eq!(keys(&stage), ["event", "shell_allow"]);
        assert_eq!(
            stage,
            json!({"event": "stage", "shell_allow": ["git", "cargo"]})
        );

        let session_start = written(&session_start_line(&SessionContext {
            capsule_name: "demo".to_string(),
            capsule_version: "1.2.3".to_string(),
            session_id: "sess_1".to_string(),
            model: "claude".to_string(),
            capabilities: vec!["filesystem".to_string()],
        }));
        assert_eq!(
            keys(&session_start),
            [
                "capabilities",
                "capsule_name",
                "capsule_version",
                "event",
                "model",
                "session_id"
            ]
        );
        assert_eq!(
            session_start,
            json!({
                "event": "session-start",
                "capsule_name": "demo",
                "capsule_version": "1.2.3",
                "session_id": "sess_1",
                "model": "claude",
                "capabilities": ["filesystem"],
            })
        );

        let inference = written(&inference_line(&InferenceEvent {
            turn: 3,
            input_tokens: 120,
            output_tokens: 45,
            decision: "tool_use".to_string(),
            tool_name: Some("editor".to_string()),
        }));
        assert_eq!(
            keys(&inference),
            [
                "decision",
                "event",
                "input_tokens",
                "output_tokens",
                "tool_name",
                "turn"
            ]
        );
        assert_eq!(
            inference,
            json!({
                "event": "inference",
                "turn": 3,
                "input_tokens": 120,
                "output_tokens": 45,
                "decision": "tool_use",
                "tool_name": "editor",
            })
        );

        // `tool_name: none` is an end_turn inference — the key stays, carrying null.
        let no_tool = written(&inference_line(&InferenceEvent {
            turn: 4,
            input_tokens: 1,
            output_tokens: 2,
            decision: "end_turn".to_string(),
            tool_name: None,
        }));
        assert_eq!(no_tool["tool_name"], Value::Null);

        let tool_call = written(
            &tool_call_line(&ToolEvent {
                turn: 3,
                tool_name: "editor".to_string(),
                input_bytes: 64,
                outcome: Some(ToolOutcome {
                    output_bytes: 128,
                    duration_ms: 17,
                    status: "ok".to_string(),
                }),
            })
            .expect("the observation dispatch writes a line"),
        );
        assert_eq!(
            keys(&tool_call),
            [
                "duration_ms",
                "event",
                "input_bytes",
                "output_bytes",
                "status",
                "tool_name",
                "turn"
            ]
        );
        assert_eq!(
            tool_call,
            json!({
                "event": "tool-call",
                "turn": 3,
                "tool_name": "editor",
                "input_bytes": 64,
                "output_bytes": 128,
                "duration_ms": 17,
                "status": "ok",
            })
        );

        let shell = written(
            &shell_line(&ShellEvent {
                turn: 5,
                command: "-c pytest -q".to_string(),
                outcome: Some(ShellOutcome {
                    exit_code: -1,
                    stdout_bytes: 6,
                    stderr_bytes: 0,
                    duration_ms: 42,
                }),
            })
            .expect("the observation dispatch writes a line"),
        );
        assert_eq!(
            keys(&shell),
            [
                "command",
                "duration_ms",
                "event",
                "exit_code",
                "stderr_bytes",
                "stdout_bytes",
                "turn"
            ]
        );
        assert_eq!(
            shell,
            json!({
                "event": "shell",
                "turn": 5,
                "command": "-c pytest -q",
                "exit_code": -1,
                "stdout_bytes": 6,
                "stderr_bytes": 0,
                "duration_ms": 42,
            })
        );

        let message = |role: &str| Message {
            role: role.to_string(),
            content: "hi".to_string(),
            id: None,
            source_id: None,
        };
        let compaction = written(&compaction_line(&CompactionEvent {
            messages: vec![message("user"), message("assistant"), message("user")],
            session_tokens: 90_000,
            threshold: 0.8,
        }));
        assert_eq!(
            keys(&compaction),
            ["event", "message_count", "session_tokens", "threshold"]
        );
        assert_eq!(
            compaction,
            json!({
                "event": "compaction",
                "message_count": 3,
                "session_tokens": 90_000,
                "threshold": 0.8,
            })
        );

        let session_end = written(&session_end_line(&SessionEndEvent {
            total_turns: 7,
            total_input_tokens: 1_000,
            total_output_tokens: 200,
            total_tool_calls: 4,
            total_shell_calls: 2,
            duration_ms: 12_345,
            exit_status: "ok".to_string(),
        }));
        assert_eq!(
            keys(&session_end),
            [
                "duration_ms",
                "event",
                "exit_status",
                "total_input_tokens",
                "total_output_tokens",
                "total_shell_calls",
                "total_tool_calls",
                "total_turns"
            ]
        );
        assert_eq!(
            session_end,
            json!({
                "event": "session-end",
                "total_turns": 7,
                "total_input_tokens": 1_000,
                "total_output_tokens": 200,
                "total_tool_calls": 4,
                "total_shell_calls": 2,
                "duration_ms": 12_345,
                "exit_status": "ok",
            })
        );
    }

    /// The decision-point dispatch carries `outcome: none` and leaves no line behind, so
    /// one call is one line rather than two.
    #[test]
    fn decision_point_dispatch_writes_no_line() {
        assert!(tool_call_line(&ToolEvent {
            turn: 1,
            tool_name: "editor".to_string(),
            input_bytes: 64,
            outcome: None,
        })
        .is_none());

        assert!(shell_line(&ShellEvent {
            turn: 1,
            command: "-c rm -rf /".to_string(),
            outcome: None,
        })
        .is_none());
    }
}
