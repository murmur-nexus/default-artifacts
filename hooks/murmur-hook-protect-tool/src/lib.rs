//! murmur-hook-protect-tool — refuse tool calls that write a protected path.
//!
//! `binding: on-tool-call`, `commit_policy: deny`. At the decision-point dispatch
//! — the one whose `tool-event.outcome` is `none` — it reads the exact tool input
//! JSON, extracts the write target(s) a configured tool rule names, and returns
//! `deny(reason)` when a target matches a protected-path pattern. At the
//! post-call observation dispatch (`outcome: some(...)`) there is nothing left to
//! refuse, so it returns `none` and does nothing.
//!
//! All of the deciding lives in `murmur-path-policy`, at the crate root and with
//! no `cfg` gate, so `cargo test` runs it on the host. This crate is the wiring:
//! which side of the policy it is, where the config comes from, and the `Guest`
//! impl. The `#[cfg(target_arch = "wasm32")] mod wasm_hook` block below holds the
//! component itself.
//!
//! It is independent of `murmur-hook-protect-shell` at runtime: neither requires
//! the other to be installed and neither reads the other's config. Either alone
//! is a complete gate for its own event.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unreachable,
    clippy::todo
)]

use murmur_path_policy::{PolicySide, PolicyState};

/// The side of the policy this artifact decides for. It reads `protect`, `allow`
/// and `tools`; `shell_write_binaries` is the other half's key and is an unknown
/// key here.
pub const SIDE: PolicySide = PolicySide::Tool;

/// Parse the policy from the raw `MURMUR_ARTIFACT_CONFIG` value. `None` is what
/// an artifact entry with no `config:` block delivers, and means "the defaults".
pub fn load_policy(raw: Option<&str>) -> PolicyState {
    PolicyState::load(SIDE, raw)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use murmur_path_policy::{decide_tool_call, Decision};

    fn editor(operation: &str, path: &str) -> String {
        format!(r#"{{"operation":"{operation}","path":"{path}"}}"#)
    }

    #[test]
    fn the_default_policy_refuses_a_test_edit_and_allows_a_source_edit() {
        let policy = load_policy(None);
        assert!(matches!(
            decide_tool_call(
                &policy,
                "murmur-tool-editor",
                &editor("write_file", "tests/test_x.py")
            ),
            Decision::Deny(_)
        ));
        assert_eq!(
            decide_tool_call(
                &policy,
                "murmur-tool-editor",
                &editor("write_file", "src/x.py")
            ),
            Decision::Allow
        );
    }

    #[test]
    fn this_side_does_not_read_the_shell_halfs_key() {
        let policy = load_policy(Some(r#"{"shell_write_binaries":["python3"]}"#));
        assert!(policy.config().is_none());
    }

    #[test]
    fn a_config_this_side_owns_is_accepted() {
        let policy = load_policy(Some(r#"{"protect":["fixtures/"],"tools":[]}"#));
        assert!(policy.config().is_some());
        // With no tool rules, nothing is gated.
        assert_eq!(
            decide_tool_call(
                &policy,
                "murmur-tool-editor",
                &editor("write_file", "fixtures/a.json")
            ),
            Decision::Allow
        );
    }
}

// ── the actual WASM hook component ──────────────────────────────────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::cell::OnceCell;

    use murmur_path_policy::{decide_tool_call, Decision, ARTIFACT_CONFIG_ENV};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
        ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    };

    thread_local! {
        /// Parsed once per component instance: the config cannot change mid-session,
        /// and a policy hook stands in front of every gated call.
        static POLICY: OnceCell<super::PolicyState> = OnceCell::new();
    }

    fn with_policy<R>(f: impl FnOnce(&super::PolicyState) -> R) -> R {
        POLICY.with(|cell| {
            f(cell.get_or_init(|| {
                super::load_policy(std::env::var(ARTIFACT_CONFIG_ENV).ok().as_deref())
            }))
        })
    }

    pub struct ProtectTool;

    impl Guest for ProtectTool {
        fn on_tool_call(event: ToolEvent) -> Result<HookOutput, String> {
            // `outcome: some(...)` is the post-call observation: the call has already
            // happened, so a `deny` returned here refuses nothing and is recorded as a
            // dispatch fault. Decide only at the decision point.
            if event.outcome.is_some() {
                return Ok(HookOutput::None);
            }
            match with_policy(|policy| decide_tool_call(policy, &event.tool_name, &event.input)) {
                Decision::Allow => Ok(HookOutput::None),
                Decision::Deny(reason) => Ok(HookOutput::Deny(reason)),
            }
        }

        // The eight events this hook is not bound to. `binding: on-tool-call` means the
        // runtime never dispatches them here, and an `on-stage` refusal is logged and
        // discarded rather than honored — so there is nothing for any of them to do.
        fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_start(_event: TaskStartEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_inference(_event: InferenceEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_shell(_event: ShellEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_end(_event: TaskEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    export!(ProtectTool);
}
