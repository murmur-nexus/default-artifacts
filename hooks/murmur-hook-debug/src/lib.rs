#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::{fs::OpenOptions, io::Write};

    use serde_json::json;

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
            write_event(json!({
                "event": "stage",
                "shell_allow": event.shell_allow,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_session_start(
            ctx: exports::murmur::hook::lifecycle::SessionContext,
        ) -> Result<HookOutput, String> {
            write_event(json!({
                "event": "session-start",
                "capsule_name": ctx.capsule_name,
                "capsule_version": ctx.capsule_version,
                "session_id": ctx.session_id,
                "model": ctx.model,
                "capabilities": ctx.capabilities,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_inference(
            event: exports::murmur::hook::lifecycle::InferenceEvent,
        ) -> Result<HookOutput, String> {
            write_event(json!({
                "event": "inference",
                "turn": event.turn,
                "input_tokens": event.input_tokens,
                "output_tokens": event.output_tokens,
                "decision": event.decision,
                "tool_name": event.tool_name,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_tool_call(
            event: exports::murmur::hook::lifecycle::ToolEvent,
        ) -> Result<HookOutput, String> {
            // `outcome: none` is the decision-point dispatch — the call has not run, so
            // three of the seven keys below have no value. This hook is an observer,
            // not a policy: it writes one line per *completed* call and nothing here, so
            // hook-debug.jsonl never carries a second line for the same call, nor a line
            // for a call that was refused or never made.
            let Some(outcome) = event.outcome else {
                return Ok(HookOutput::None);
            };
            write_event(json!({
                "event": "tool-call",
                "turn": event.turn,
                "tool_name": event.tool_name,
                "input_bytes": event.input_bytes,
                "output_bytes": outcome.output_bytes,
                "duration_ms": outcome.duration_ms,
                "status": outcome.status,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_shell(
            event: exports::murmur::hook::lifecycle::ShellEvent,
        ) -> Result<HookOutput, String> {
            // `outcome: none` is the decision-point dispatch — the command has not run,
            // so four of the seven keys below have no value. Same choice as
            // `on_tool_call`: record only the completed call, so one shell call is one
            // line in hook-debug.jsonl rather than two.
            let Some(outcome) = event.outcome else {
                return Ok(HookOutput::None);
            };
            write_event(json!({
                "event": "shell",
                "turn": event.turn,
                "command": event.command,
                "exit_code": outcome.exit_code,
                "stdout_bytes": outcome.stdout_bytes,
                "stderr_bytes": outcome.stderr_bytes,
                "duration_ms": outcome.duration_ms,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_compaction(
            event: exports::murmur::hook::lifecycle::CompactionEvent,
        ) -> Result<HookOutput, String> {
            write_event(json!({
                "event": "compaction",
                "message_count": event.messages.len(),
                "session_tokens": event.session_tokens,
                "threshold": event.threshold,
            }))?;
            Ok(HookOutput::None)
        }

        fn on_session_end(
            event: exports::murmur::hook::lifecycle::SessionEndEvent,
        ) -> Result<HookOutput, String> {
            write_event(json!({
                "event": "session-end",
                "total_turns": event.total_turns,
                "total_input_tokens": event.total_input_tokens,
                "total_output_tokens": event.total_output_tokens,
                "total_tool_calls": event.total_tool_calls,
                "total_shell_calls": event.total_shell_calls,
                "duration_ms": event.duration_ms,
                "exit_status": event.exit_status,
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
