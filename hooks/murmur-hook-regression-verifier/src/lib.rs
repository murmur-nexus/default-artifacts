//! murmur-hook-regression-verifier — in-the-loop regression enforcement.
//!
//! A single blocking hook instance, bound to all lifecycle events, that both
//! *observes* a task's test activity and *gates* at task end:
//!
//!   • `on-shell`  — recognizes a test-runner invocation (pytest / cargo test /
//!     go test / a jest invocation) from the command string, parses its combined
//!     stdout+stderr with the shared `murmur-test-parse` parsers, and records the
//!     result as a per-command snapshot. The FIRST snapshot for a command seen
//!     *before the task's first source edit* is that command's baseline; every
//!     later snapshot for the same command is its current result.
//!   • `on-tool-call` — a call to `murmur-tool-editor` or `murmur-tool-create`
//!     marks that the task's first source edit has occurred.
//!   • `on-task-end` — diffs each command's latest current snapshot against its
//!     baseline. A previously-passing test that is now failing (or a wholesale
//!     collapse of the baseline-passing set — the "0/644" collection-failure
//!     class) is a regression: the hook returns `ReopenTask(reason)`. Otherwise
//!     it returns `None`. Every verdict, reopen or clean, is appended as one JSON
//!     line to `regression-verifier.jsonl` at the workdir root.
//!
//! The verdict-derivation and observation logic lives at the crate root as pure
//! functions/structs so it is testable under `cargo test` without WASI or
//! wit-bindgen (see the `#[cfg(not(target_arch = "wasm32"))] mod tests` block).
//! The `#[cfg(target_arch = "wasm32")] mod wasm_hook` block holds the actual
//! `wit_bindgen::generate!` component and the `Guest` implementation that drives
//! these functions from real lifecycle dispatches.

use std::collections::{BTreeMap, BTreeSet};

use murmur_test_parse::{detect_format, parse_cargo, parse_go, parse_jest, parse_pytest};
use serde_json::{json, Value};

/// Fraction of a command's baseline-passing tests that must stop passing for the
/// regression to be flagged as a wholesale `collection_failure` (an import- or
/// collection-time break) rather than a handful of isolated regressions. 0.9 =
/// "effectively the entirety of the baseline-passing set". Derived purely from
/// the passed/failed counts the parsers already produce — no per-language
/// collection-error string matching.
const COLLECTION_FAILURE_RATIO: f64 = 0.9;

/// The two tool names that mean "the agent edited a source file this task".
const EDIT_TOOLS: [&str; 2] = ["murmur-tool-editor", "murmur-tool-create"];

/// Substrings in a shell command that mark it as a test-runner invocation. The
/// runner *format* is then resolved from the command's OUTPUT via
/// `detect_format`, exactly as `murmur-tool-test-report` auto-detects it.
const TEST_COMMAND_MARKERS: [&str; 6] = [
    "pytest",
    "cargo test",
    "go test",
    "npx jest",
    "npm test",
    "yarn test",
];

/// One parsed test run: the runner-reported passed count and the set of failing
/// test names. We keep only what the verdict needs (passed count + failure
/// identities), not the full `Failure` structs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub passed: i64,
    pub failed: Vec<String>,
}

/// Per-command diff of the latest current snapshot against its baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandVerdict {
    pub command: String,
    pub baseline_passed: i64,
    pub current_passed: i64,
    /// Named tests failing now that were not failing in the baseline.
    pub regressions: Vec<String>,
    /// Count of baseline-passing tests that no longer pass. Captures collection
    /// failures where individual failures are not enumerated (passed collapses
    /// to 0 with no per-test FAILED lines).
    pub lost_passing: i64,
    pub collection_failure: bool,
    /// Baseline-failing tests that now pass. Informational only — never gates.
    pub newly_passing: Vec<String>,
}

impl CommandVerdict {
    /// True when this command shows a regression that should reopen the task.
    fn has_regression(&self) -> bool {
        !self.regressions.is_empty() || self.lost_passing > 0
    }
}

/// The whole-task verdict aggregated across every command with both a baseline
/// and a current snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub reopen: bool,
    pub collection_failure: bool,
    /// Union of named regressions across all commands (sorted, deduped).
    pub regressions: Vec<String>,
    /// Union of newly-passing test names across all commands (informational).
    pub newly_passing: Vec<String>,
    pub commands: Vec<CommandVerdict>,
}

/// Session/task-scoped observation state. Reset per task in `on_task_start`;
/// NOT reset between reopens of the same task (the host re-fires `on-task-end`
/// on the same instance without an intervening `on-task-start`).
#[derive(Default)]
pub struct RegressionState {
    baselines: BTreeMap<String, Snapshot>,
    currents: BTreeMap<String, Snapshot>,
    first_edit_seen: bool,
}

impl RegressionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all per-task observations. Called at task start so a second task in
    /// the same session does not inherit the first task's baselines.
    pub fn reset(&mut self) {
        self.baselines.clear();
        self.currents.clear();
        self.first_edit_seen = false;
    }

    /// Record a tool call; marks the first source edit if it is an editor tool.
    pub fn observe_tool(&mut self, tool_name: &str) {
        if EDIT_TOOLS.contains(&tool_name) {
            self.first_edit_seen = true;
        }
    }

    /// Record a shell command's output. No-op unless the command matches a
    /// test-runner marker and its output is a recognizable test format.
    pub fn observe_shell(&mut self, command: &str, stdout: &str, stderr: &str) {
        if !is_test_command(command) {
            return;
        }
        let raw = format!("{stdout}\n{stderr}");
        let Some(snap) = snapshot_from_output(&raw) else {
            return;
        };
        let key = command.to_string();
        // The first snapshot for a command seen before the first edit is its
        // baseline; every other snapshot (subsequent runs, or a command first
        // seen post-edit) is a current result.
        if !self.first_edit_seen && !self.baselines.contains_key(&key) {
            self.baselines.insert(key, snap);
        } else {
            self.currents.insert(key, snap);
        }
    }

    /// Derive the whole-task verdict from the accumulated snapshots.
    pub fn derive_verdict(&self) -> Verdict {
        let mut commands = Vec::new();
        let mut any_reopen = false;
        let mut any_collection = false;
        let mut all_regressions: Vec<String> = Vec::new();
        let mut all_newly_passing: Vec<String> = Vec::new();

        for (command, base) in &self.baselines {
            let Some(cur) = self.currents.get(command) else {
                continue;
            };
            let base_failed: BTreeSet<&String> = base.failed.iter().collect();
            let cur_failed: BTreeSet<&String> = cur.failed.iter().collect();

            let mut regressions: Vec<String> = cur
                .failed
                .iter()
                .filter(|n| !base_failed.contains(*n))
                .cloned()
                .collect();
            regressions.sort();
            regressions.dedup();

            let mut newly_passing: Vec<String> = base
                .failed
                .iter()
                .filter(|n| !cur_failed.contains(*n))
                .cloned()
                .collect();
            newly_passing.sort();
            newly_passing.dedup();

            let lost_passing = (base.passed - cur.passed).max(0);
            let collection_failure = base.passed > 0
                && (lost_passing as f64) >= COLLECTION_FAILURE_RATIO * (base.passed as f64);

            let cv = CommandVerdict {
                command: command.clone(),
                baseline_passed: base.passed,
                current_passed: cur.passed,
                regressions,
                lost_passing,
                collection_failure,
                newly_passing,
            };
            if cv.has_regression() {
                any_reopen = true;
            }
            if cv.collection_failure {
                any_collection = true;
            }
            all_regressions.extend(cv.regressions.iter().cloned());
            all_newly_passing.extend(cv.newly_passing.iter().cloned());
            commands.push(cv);
        }

        all_regressions.sort();
        all_regressions.dedup();
        all_newly_passing.sort();
        all_newly_passing.dedup();

        Verdict {
            reopen: any_reopen,
            collection_failure: any_collection,
            regressions: all_regressions,
            newly_passing: all_newly_passing,
            commands,
        }
    }
}

/// True when `cmd` invokes one of the four supported test runners.
pub fn is_test_command(cmd: &str) -> bool {
    TEST_COMMAND_MARKERS.iter().any(|m| cmd.contains(m))
}

/// Parse combined test-runner output into a `Snapshot`, or `None` when the text
/// is not a recognizable test format. Uses the shared `murmur-test-parse`
/// parsers so the format matchers are never duplicated in this hook.
pub fn snapshot_from_output(raw: &str) -> Option<Snapshot> {
    let fmt = detect_format(raw)?;
    let (failures, passed) = match fmt.as_str() {
        "cargo_test" => parse_cargo(raw),
        "pytest" => parse_pytest(raw),
        "go_test" => parse_go(raw),
        "jest" => parse_jest(raw),
        _ => return None,
    };
    Some(Snapshot {
        passed,
        failed: failures.into_iter().map(|f| f.test_name).collect(),
    })
}

/// The reopen feedback message: names every regressed test, states the
/// collection-failure verdict when applicable, and states plainly that breaking
/// previously-passing tests is a failed fix to revise or revert.
pub fn build_reason(verdict: &Verdict) -> String {
    let mut out = String::new();
    out.push_str(
        "Regression check failed: your change broke tests that were passing before you started \
         editing this task. A change that makes a previously-passing test fail is a failed fix, \
         not a partial success — revise it so those tests pass again, or revert it.",
    );
    for c in &verdict.commands {
        if !c.has_regression() {
            continue;
        }
        if c.collection_failure {
            out.push_str(&format!(
                "\n\n• `{}` — COLLECTION FAILURE: essentially the entire module broke at once \
                 ({} of {} previously-passing tests no longer pass). This is the unmistakable \
                 signature of an import- or collection-time break (a syntax error, a bad import, \
                 or a module-load panic), not a handful of isolated assertion failures. Make the \
                 module load again before anything else.",
                c.command, c.lost_passing, c.baseline_passed
            ));
        } else {
            let count = c.regressions.len().max(c.lost_passing as usize);
            out.push_str(&format!(
                "\n\n• `{}` — {} previously-passing test(s) now fail",
                c.command, count
            ));
            if !c.regressions.is_empty() {
                out.push_str(&format!(": {}", c.regressions.join(", ")));
            }
            let unnamed = (c.lost_passing as usize).saturating_sub(c.regressions.len());
            if unnamed > 0 {
                out.push_str(&format!(
                    " (and {unnamed} further previously-passing test(s) no longer appear in the \
                     run's passing set)"
                ));
            }
            out.push('.');
        }
    }
    if !verdict.newly_passing.is_empty() {
        out.push_str(&format!(
            "\n\n(For context only: {} test(s) now pass that were not passing in the baseline — \
             {}. That is good, but it does not offset the regressions above.)",
            verdict.newly_passing.len(),
            verdict.newly_passing.join(", ")
        ));
    }
    out
}

/// The single JSON-line shape appended to `regression-verifier.jsonl`. The shape
/// is identical for reopen and clean verdicts — the `decision`/`reopen` fields
/// distinguish them, and `regressions`/`collection_failure` are simply empty/
/// false on a clean verdict.
pub fn verdict_json(verdict: &Verdict, task_id: &str, task_exit_status: &str) -> Value {
    json!({
        "record_type": "regression_verdict",
        "task_id": task_id,
        "task_exit_status": task_exit_status,
        "decision": if verdict.reopen { "reopen" } else { "clean" },
        "reopen": verdict.reopen,
        "collection_failure": verdict.collection_failure,
        "regressions": verdict.regressions,
        "newly_passing": verdict.newly_passing,
        "commands": verdict.commands.iter().map(|c| json!({
            "command": c.command,
            "baseline_passed": c.baseline_passed,
            "current_passed": c.current_passed,
            "regressions": c.regressions,
            "lost_passing": c.lost_passing,
            "collection_failure": c.collection_failure,
            "newly_passing": c.newly_passing,
        })).collect::<Vec<_>>(),
    })
}

// ── native unit tests of the pure logic ─────────────────────────────────────
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn cargo_output(passed: i64, failed: &[&str]) -> String {
        let mut s = String::from("running tests\n");
        for name in failed {
            s.push_str(&format!(
                "---- {name} stdout ----\nthread '{name}' panicked at src/lib.rs:1:1:\nboom\n\n"
            ));
        }
        s.push_str(&format!(
            "test result: FAILED. {passed} passed; {} failed; 0 ignored\n",
            failed.len()
        ));
        s
    }

    #[test]
    fn scenario_1_no_regression_is_clean() {
        let mut st = RegressionState::new();
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        st.observe_tool("murmur-tool-editor");
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        let v = st.derive_verdict();
        assert!(!v.reopen);
        assert!(v.regressions.is_empty());
        assert!(!v.collection_failure);
    }

    #[test]
    fn scenario_2_single_test_regression_reopens() {
        let mut st = RegressionState::new();
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        st.observe_tool("murmur-tool-editor");
        st.observe_shell("cargo test", &cargo_output(2, &["beta"]), "");
        let v = st.derive_verdict();
        assert!(v.reopen);
        assert_eq!(v.regressions, vec!["beta".to_string()]);
        assert!(!v.collection_failure);
        // reason names the regressed test and states the failed-fix verdict.
        let reason = build_reason(&v);
        assert!(reason.contains("beta"));
        assert!(reason.contains("failed fix"));
    }

    #[test]
    fn scenario_3_collection_failure_module_wide() {
        let mut st = RegressionState::new();
        // baseline: 644 passing, none failing.
        st.observe_shell(
            "pytest",
            "=== test session starts ===\n=== 644 passed in 1.0s ===",
            "",
        );
        st.observe_tool("murmur-tool-create");
        // current: collection error — 0 passing, no enumerated failures.
        st.observe_shell(
            "pytest",
            "=== test session starts ===\nERROR collecting tests\n=== 1 error in 0.1s ===",
            "",
        );
        let v = st.derive_verdict();
        assert!(v.reopen);
        assert!(v.collection_failure);
        let reason = build_reason(&v);
        assert!(reason.contains("COLLECTION FAILURE"));
    }

    #[test]
    fn scenario_4_newly_passing_does_not_gate() {
        let mut st = RegressionState::new();
        // baseline: 2 passing, `beta` failing.
        st.observe_shell("cargo test", &cargo_output(2, &["beta"]), "");
        st.observe_tool("murmur-tool-editor");
        // current: `beta` fixed, now 3 passing, none failing.
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        let v = st.derive_verdict();
        assert!(!v.reopen);
        assert_eq!(v.newly_passing, vec!["beta".to_string()]);
        assert!(v.regressions.is_empty());
    }

    #[test]
    fn scenario_5_no_test_command_observed_is_clean() {
        let mut st = RegressionState::new();
        st.observe_shell("ls -la", "a\nb\nc", "");
        st.observe_tool("murmur-tool-editor");
        st.observe_shell("echo hi", "hi", "");
        let v = st.derive_verdict();
        assert!(!v.reopen);
        assert!(v.commands.is_empty());
    }

    #[test]
    fn scenario_6_task_start_resets_state() {
        let mut st = RegressionState::new();
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        st.observe_tool("murmur-tool-editor");
        st.observe_shell("cargo test", &cargo_output(2, &["beta"]), "");
        assert!(st.derive_verdict().reopen);
        // A new task resets: prior baseline/current/edit flag are gone.
        st.reset();
        assert!(st.derive_verdict().commands.is_empty());
        // Fresh baseline taken pre-edit again (proves first_edit_seen was reset).
        st.observe_shell("cargo test", &cargo_output(5, &[]), "");
        st.observe_tool("murmur-tool-editor");
        st.observe_shell("cargo test", &cargo_output(5, &[]), "");
        assert!(!st.derive_verdict().reopen);
    }

    #[test]
    fn scenario_7_reopen_does_not_reset_state() {
        let mut st = RegressionState::new();
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        st.observe_tool("murmur-tool-editor");
        st.observe_shell("cargo test", &cargo_output(2, &["beta"]), "");
        // First on-task-end: regression → reopen.
        let v1 = st.derive_verdict();
        assert!(v1.reopen);
        // No reset happens between reopens. The agent fixes `beta` and re-runs;
        // the baseline (3 passing) must still be there to re-derive against.
        st.observe_shell("cargo test", &cargo_output(3, &[]), "");
        let v2 = st.derive_verdict();
        assert!(!v2.reopen);
        // Baseline survived across the two derive calls (no on_task_start fired).
        assert_eq!(v2.commands.len(), 1);
        assert_eq!(v2.commands[0].baseline_passed, 3);
    }

    #[test]
    fn command_seen_only_post_edit_has_no_baseline() {
        let mut st = RegressionState::new();
        st.observe_tool("murmur-tool-editor");
        // First sighting of this command is after the edit → no baseline.
        st.observe_shell(
            "go test ./...",
            "--- FAIL: TestX (0.0s)\n    x_test.go:1: boom\nFAIL",
            "",
        );
        let v = st.derive_verdict();
        assert!(!v.reopen);
        assert!(v.commands.is_empty());
    }

    #[test]
    fn verdict_json_shape_is_stable_for_clean_and_reopen() {
        let clean = Verdict {
            reopen: false,
            collection_failure: false,
            regressions: vec![],
            newly_passing: vec![],
            commands: vec![],
        };
        let j = verdict_json(&clean, "task-1", "ok");
        assert_eq!(j["decision"], "clean");
        assert_eq!(j["reopen"], false);
        assert_eq!(j["record_type"], "regression_verdict");

        let reopen = Verdict {
            reopen: true,
            collection_failure: false,
            regressions: vec!["beta".into()],
            newly_passing: vec![],
            commands: vec![],
        };
        let j2 = verdict_json(&reopen, "task-1", "ok");
        assert_eq!(j2["decision"], "reopen");
        // Same key set in both cases.
        assert_eq!(
            j.as_object().unwrap().keys().collect::<Vec<_>>(),
            j2.as_object().unwrap().keys().collect::<Vec<_>>()
        );
    }
}

// ── the actual WASM hook component ──────────────────────────────────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::cell::RefCell;
    use std::io::Write;

    use super::{build_reason, verdict_json, RegressionState};

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
        static STATE: RefCell<RegressionState> = RefCell::new(RegressionState::new());
    }

    /// Append one JSON line to `regression-verifier.jsonl` at the workdir root.
    /// Best-effort: a write failure is logged to stderr but never fails the hook
    /// (the reopen/clean decision has already been made).
    fn append_verdict_line(line: &str) {
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("./regression-verifier.jsonl");
        match opened {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{line}") {
                    eprintln!("[murmur-hook-regression-verifier] failed to write jsonl: {e}");
                }
            }
            Err(e) => {
                eprintln!(
                    "[murmur-hook-regression-verifier] could not open regression-verifier.jsonl: {e}"
                );
            }
        }
    }

    pub struct RegressionVerifier;

    impl Guest for RegressionVerifier {
        fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
            STATE.with(|s| s.borrow_mut().reset());
            Ok(HookOutput::None)
        }

        fn on_task_start(_event: TaskStartEvent) -> Result<HookOutput, String> {
            // Reset per-task state so a second task in the same session starts
            // clean. NOT called between reopens of the same task.
            STATE.with(|s| s.borrow_mut().reset());
            Ok(HookOutput::None)
        }

        fn on_inference(_event: InferenceEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_tool_call(event: ToolEvent) -> Result<HookOutput, String> {
            STATE.with(|s| s.borrow_mut().observe_tool(&event.tool_name));
            Ok(HookOutput::None)
        }

        fn on_shell(event: ShellEvent) -> Result<HookOutput, String> {
            STATE.with(|s| {
                s.borrow_mut()
                    .observe_shell(&event.command, &event.stdout, &event.stderr)
            });
            Ok(HookOutput::None)
        }

        fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_end(event: TaskEndEvent) -> Result<HookOutput, String> {
            let verdict = STATE.with(|s| s.borrow().derive_verdict());
            let line = verdict_json(&verdict, &event.task_id, &event.exit_status).to_string();
            append_verdict_line(&line);
            if verdict.reopen {
                Ok(HookOutput::ReopenTask(build_reason(&verdict)))
            } else {
                Ok(HookOutput::None)
            }
        }

        fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    export!(RegressionVerifier);
}
