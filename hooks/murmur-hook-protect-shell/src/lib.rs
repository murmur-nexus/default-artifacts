//! murmur-hook-protect-shell — refuse shell calls that write a protected path.
//!
//! `binding: on-shell`, `commit_policy: deny`. At the decision-point dispatch —
//! the one whose `shell-event.outcome` is `none` — it reads `binary`, `argv` and
//! `script`, recognizes the shell write forms it knows, and returns
//! `deny(reason)` when a write target matches a protected-path pattern. It never
//! reads `command`: that field is clipped to 200 characters and is display only.
//! At the post-call observation dispatch (`outcome: some(...)`) there is nothing
//! left to refuse, so it returns `none` and does nothing.
//!
//! All of the deciding lives in `murmur-path-policy`, at the crate root and with
//! no `cfg` gate, so `cargo test` runs it on the host. This crate is the wiring:
//! which side of the policy it is, where the config comes from, and the `Guest`
//! impl. The `#[cfg(target_arch = "wasm32")] mod wasm_hook` block below holds the
//! component itself.
//!
//! It is independent of `murmur-hook-protect-tool` at runtime: neither requires
//! the other to be installed and neither reads the other's config. Either alone
//! is a complete gate for its own event.
//!
//! This half is best-effort and cannot be made airtight — see the README.

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
/// and `shell_write_binaries`; `tools` is the other half's key and is an unknown
/// key here.
pub const SIDE: PolicySide = PolicySide::Shell;

/// Parse the policy from the raw `MURMUR_ARTIFACT_CONFIG` value. `None` is what
/// an artifact entry with no `config:` block delivers, and means "the defaults".
pub fn load_policy(raw: Option<&str>) -> PolicyState {
    PolicyState::load(SIDE, raw)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use murmur_path_policy::{decide_shell_call, Decision};

    fn run(policy: &PolicyState, script: &str) -> Decision {
        decide_shell_call(
            policy,
            "/bin/bash",
            &["-c".to_string(), script.to_string()],
            Some(script),
        )
    }

    #[test]
    fn the_default_policy_refuses_a_test_edit_and_allows_a_source_edit() {
        let policy = load_policy(None);
        assert!(matches!(
            run(&policy, "sed -i s/a/b/ tests/test_x.py"),
            Decision::Deny(_)
        ));
        assert_eq!(run(&policy, "sed -i s/a/b/ src/x.py"), Decision::Allow);
    }

    #[test]
    fn this_side_does_not_read_the_tool_halfs_key() {
        let policy = load_policy(Some(r#"{"tools":[]}"#));
        assert!(policy.config().is_none());
    }

    #[test]
    fn a_config_this_side_owns_is_accepted() {
        let policy = load_policy(Some(
            r#"{"protect":["fixtures/"],"shell_write_binaries":["jq"]}"#,
        ));
        assert!(policy.config().is_some());
        assert!(matches!(
            run(&policy, "jq . fixtures/a.json"),
            Decision::Deny(_)
        ));
        assert_eq!(run(&policy, "jq . src/a.json"), Decision::Allow);
    }
}

// ── the actual WASM hook component ──────────────────────────────────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::cell::OnceCell;

    use murmur_path_policy::{decide_shell_call, Decision, ARTIFACT_CONFIG_ENV};

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

    pub struct ProtectShell;

    impl Guest for ProtectShell {
        fn on_shell(event: ShellEvent) -> Result<HookOutput, String> {
            // `outcome: some(...)` is the post-call observation: the command has already
            // run, so a `deny` returned here refuses nothing and is recorded as a
            // dispatch fault. Decide only at the decision point.
            if event.outcome.is_some() {
                return Ok(HookOutput::None);
            }
            let decision = with_policy(|policy| {
                decide_shell_call(policy, &event.binary, &event.argv, event.script.as_deref())
            });
            match decision {
                Decision::Allow => Ok(HookOutput::None),
                Decision::Deny(reason) => Ok(HookOutput::Deny(reason)),
            }
        }

        // The eight events this hook is not bound to. `binding: on-shell` means the
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

        fn on_tool_call(_event: ToolEvent) -> Result<HookOutput, String> {
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

    export!(ProtectShell);
}
