//! The two decision functions the hooks call, and the reason strings they emit.
//!
//! A reason is pinned verbatim into the message the model is shown:
//!
//! ```text
//! Refused by policy hook '{hook_name}': {reason}
//!
//! This call did not run. Retrying it unchanged will be refused again.
//! ```
//!
//! So every reason here names the path, names the pattern that refused it, and
//! ends with something the agent can act on rather than retry.

use crate::config::{parse_config, ConfigError, PolicyConfig, PolicySide};
use crate::glob::Pattern;
use crate::path::{normalize, PathTarget};
use crate::shell::{shell_write_targets, ShellTarget};
use crate::tool::{tool_write_targets, ToolTargets};

/// The artifact name the tool half's reasons are prefixed with.
pub const HOOK_PROTECT_TOOL: &str = "murmur-hook-protect-tool";
/// The artifact name the shell half's reasons are prefixed with.
pub const HOOK_PROTECT_SHELL: &str = "murmur-hook-protect-shell";

/// What the hook returns for one gated call. There is no permit arm: this policy
/// can only narrow what the capsule manifest already granted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing here refuses the call; the hook returns `hook-output::none`.
    Allow,
    /// Refuse, with this reason shown to the model verbatim.
    Deny(String),
}

/// The parsed policy, or the fault that made it unusable.
///
/// Built once per component instance and cached, because parsing per call would
/// pay the cost on every gated call for a value that cannot change mid-session.
#[derive(Clone, Debug)]
pub enum PolicyState {
    /// A usable policy.
    Ready(PolicyConfig),
    /// The config could not be parsed. Every gated call is refused, with a reason
    /// that names the offending key and cannot be mistaken for a path match —
    /// see the README for why this fails at the first decision point rather than
    /// at stage time.
    Unusable(ConfigError),
}

impl PolicyState {
    /// Parse the raw `MURMUR_ARTIFACT_CONFIG` value. `None` means the artifact's
    /// manifest entry carried no `config:` block, which is the defaults.
    pub fn load(side: PolicySide, raw: Option<&str>) -> Self {
        match parse_config(side, raw) {
            Ok(config) => Self::Ready(config),
            Err(error) => Self::Unusable(error),
        }
    }

    /// The parsed config, when there is one.
    pub fn config(&self) -> Option<&PolicyConfig> {
        match self {
            Self::Ready(config) => Some(config),
            Self::Unusable(_) => None,
        }
    }
}

/// What the matcher made of one write target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No protected pattern matches, or an `allow` pattern matched first.
    Allow,
    /// The path popped above the workdir root, so the policy cannot anchor it.
    Escaping,
    /// A protected pattern matched. Carries that pattern's source text.
    Protected(String),
}

/// Judge one raw write target against the policy.
///
/// An escaping path is decided before `allow` is consulted: with no anchor there
/// is no path to match an `allow` pattern against.
pub fn judge_path(config: &PolicyConfig, raw: &str) -> Verdict {
    let target = normalize(raw);
    match target {
        PathTarget::Escaping => return Verdict::Escaping,
        // Names no file, so there is nothing to protect.
        PathTarget::Empty => return Verdict::Allow,
        PathTarget::Relative(_) | PathTarget::Absolute(_) => {}
    }
    let components = target.components();
    let absolute = target.is_absolute();

    let matches = |pattern: &Pattern| pattern.matches_path(components, absolute);
    if config.allow.iter().any(matches) {
        return Verdict::Allow;
    }
    match config.protect.iter().find(|pattern| matches(pattern)) {
        Some(pattern) => Verdict::Protected(pattern.source().to_string()),
        None => Verdict::Allow,
    }
}

fn config_error_reason(hook: &str, error: &ConfigError) -> String {
    format!(
        "{hook}: configuration error — {error}. Every gated call is refused until this \
         artifact's config: block is fixed. This is a configuration fault, not a protected-path \
         match."
    )
}

fn protected_reason(hook: &str, actor: &str, path: &str, pattern: &str) -> String {
    format!(
        "{hook}: '{actor}' would write '{path}', which the protected-path pattern '{pattern}' \
         refuses. Change the code under test, not the test."
    )
}

fn escaping_reason(hook: &str, actor: &str, path: &str) -> String {
    format!(
        "{hook}: '{actor}' would write '{path}', which escapes the capsule workdir. A path the \
         policy cannot anchor is one it cannot judge, so it is refused. Use a path inside the \
         workdir."
    )
}

/// Decide one `on-tool-call` decision-point dispatch.
///
/// `input` is `tool-event.input`: the exact tool input JSON, never truncated.
pub fn decide_tool_call(state: &PolicyState, tool_name: &str, input: &str) -> Decision {
    let config = match state {
        PolicyState::Unusable(error) => {
            return Decision::Deny(config_error_reason(HOOK_PROTECT_TOOL, error))
        }
        PolicyState::Ready(config) => config,
    };

    match tool_write_targets(config, tool_name, input) {
        // A tool no rule names is not gated at all, and a read is not a write:
        // refusing an agent's reads of a protected file would break every capsule
        // that installs this and protect nothing.
        ToolTargets::NotGated | ToolTargets::NotAWrite => Decision::Allow,
        ToolTargets::Unreadable(detail) => Decision::Deny(format!(
            "{HOOK_PROTECT_TOOL}: '{tool_name}' {detail}. A call whose write target the policy \
             cannot read is refused."
        )),
        ToolTargets::Targets(targets) => {
            for target in &targets {
                match judge_path(config, &target.path) {
                    Verdict::Allow => {}
                    Verdict::Escaping => {
                        return Decision::Deny(escaping_reason(
                            HOOK_PROTECT_TOOL,
                            tool_name,
                            &target.path,
                        ))
                    }
                    Verdict::Protected(pattern) => {
                        return Decision::Deny(protected_reason(
                            HOOK_PROTECT_TOOL,
                            tool_name,
                            &target.path,
                            &pattern,
                        ))
                    }
                }
            }
            Decision::Allow
        }
    }
}

/// Decide one `on-shell` decision-point dispatch.
///
/// Decided on `binary`, `argv` and `script`; `command` is clipped to 200
/// characters and is never read.
pub fn decide_shell_call(
    state: &PolicyState,
    binary: &str,
    argv: &[String],
    script: Option<&str>,
) -> Decision {
    let config = match state {
        PolicyState::Unusable(error) => {
            return Decision::Deny(config_error_reason(HOOK_PROTECT_SHELL, error))
        }
        PolicyState::Ready(config) => config,
    };

    for target in shell_write_targets(config, binary, argv, script) {
        match target {
            ShellTarget::Path { form, path } => match judge_path(config, &path) {
                Verdict::Allow => {}
                Verdict::Escaping => {
                    return Decision::Deny(escaping_reason(HOOK_PROTECT_SHELL, &form, &path))
                }
                Verdict::Protected(pattern) => {
                    return Decision::Deny(protected_reason(
                        HOOK_PROTECT_SHELL,
                        &form,
                        &path,
                        &pattern,
                    ))
                }
            },
            ShellTarget::Unreadable { form, note } => {
                return Decision::Deny(format!(
                    "{HOOK_PROTECT_SHELL}: '{form}' writes files this hook cannot name — {note}. \
                     A write whose target the policy cannot read is refused. Make the edit with \
                     the editor tool, or name the file on the command line."
                ))
            }
        }
    }
    Decision::Allow
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn tool_state(raw: Option<&str>) -> PolicyState {
        PolicyState::load(PolicySide::Tool, raw)
    }

    fn shell_state(raw: Option<&str>) -> PolicyState {
        PolicyState::load(PolicySide::Shell, raw)
    }

    fn editor(operation: &str, path: &str) -> String {
        format!(r#"{{"operation":"{operation}","path":"{path}"}}"#)
    }

    fn deny_reason(decision: &Decision) -> String {
        match decision {
            Decision::Deny(reason) => reason.clone(),
            Decision::Allow => panic!("expected a refusal, got Allow"),
        }
    }

    fn run_script(state: &PolicyState, text: &str) -> Decision {
        decide_shell_call(
            state,
            "/bin/bash",
            &["-c".to_string(), text.to_string()],
            Some(text),
        )
    }

    #[test]
    fn a_protected_write_is_refused_with_the_path_and_the_pattern() {
        let decision = decide_tool_call(
            &tool_state(None),
            "murmur-tool-editor",
            &editor("write_file", "tests/test_auth.py"),
        );
        assert_eq!(
            deny_reason(&decision),
            "murmur-hook-protect-tool: 'murmur-tool-editor' would write 'tests/test_auth.py', \
             which the protected-path pattern 'tests/' refuses. Change the code under test, not \
             the test."
        );
    }

    #[test]
    fn an_unprotected_write_is_allowed() {
        assert_eq!(
            decide_tool_call(
                &tool_state(None),
                "murmur-tool-editor",
                &editor("write_file", "src/x.py")
            ),
            Decision::Allow
        );
    }

    #[test]
    fn the_editors_reads_are_never_refused() {
        for operation in ["read_file", "find_in_files"] {
            assert_eq!(
                decide_tool_call(
                    &tool_state(None),
                    "murmur-tool-editor",
                    &editor(operation, "tests/test_auth.py")
                ),
                Decision::Allow,
                "{operation}"
            );
        }
    }

    #[test]
    fn a_tool_with_no_rule_is_not_gated() {
        assert_eq!(
            decide_tool_call(
                &tool_state(None),
                "murmur-tool-git",
                r#"{"operation":"write_file","path":"tests/test_auth.py"}"#
            ),
            Decision::Allow
        );
    }

    #[test]
    fn an_escaping_path_is_refused_with_a_distinct_reason() {
        let decision = decide_tool_call(
            &tool_state(None),
            "murmur-tool-editor",
            &editor("write_file", "../../etc/passwd"),
        );
        assert_eq!(
            deny_reason(&decision),
            "murmur-hook-protect-tool: 'murmur-tool-editor' would write '../../etc/passwd', \
             which escapes the capsule workdir. A path the policy cannot anchor is one it cannot \
             judge, so it is refused. Use a path inside the workdir."
        );
        // A path whose `..` cancels out inside the workdir is judged normally.
        assert_eq!(
            decide_tool_call(
                &tool_state(None),
                "murmur-tool-editor",
                &editor("write_file", "./tests/../src/x.py")
            ),
            Decision::Allow
        );
        assert!(matches!(
            decide_tool_call(
                &tool_state(None),
                "murmur-tool-editor",
                &editor("write_file", "a/../../b")
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn an_absolute_path_is_judged_on_basename_and_directory_patterns_only() {
        // A root-anchored pattern misses it — the named limit.
        let state = tool_state(Some(r#"{"protect":["tests/a.py"]}"#));
        assert_eq!(
            decide_tool_call(
                &state,
                "murmur-tool-editor",
                &editor("write_file", "/w/tests/a.py")
            ),
            Decision::Allow
        );
        // Basename and directory-component patterns still apply.
        let state = tool_state(None);
        assert!(matches!(
            decide_tool_call(
                &state,
                "murmur-tool-editor",
                &editor("write_file", "/w/tests/a.py")
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn an_empty_path_and_a_root_path_are_decided_without_panicking() {
        for path in ["", "/"] {
            assert_eq!(
                decide_tool_call(
                    &tool_state(None),
                    "murmur-tool-editor",
                    &editor("write_file", path)
                ),
                Decision::Allow,
                "{path}"
            );
        }
    }

    #[test]
    fn allow_is_checked_before_protect() {
        let state = tool_state(Some(
            r#"{"protect":["tests/"],"allow":["tests/fixtures/"]}"#,
        ));
        assert_eq!(
            decide_tool_call(
                &state,
                "murmur-tool-editor",
                &editor("write_file", "tests/fixtures/data.json")
            ),
            Decision::Allow
        );
        assert!(matches!(
            decide_tool_call(
                &state,
                "murmur-tool-editor",
                &editor("write_file", "tests/a.py")
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn an_unusable_config_refuses_every_gated_call_with_a_config_reason() {
        for raw in [
            r#"{"protect": []}"#,
            r#"{"protect": ["tests/", 7]}"#,
            r#"{"protect": ["a**b"]}"#,
            r#"{"protet": ["tests/"]}"#,
            r#"{"protect": "tests/"}"#,
            "{not json",
        ] {
            let state = tool_state(Some(raw));
            // Even a call naming no protected path at all is refused.
            let reason = deny_reason(&decide_tool_call(
                &state,
                "murmur-tool-editor",
                &editor("write_file", "src/x.py"),
            ));
            assert!(reason.contains("configuration error"), "{raw}: {reason}");
            assert!(
                reason.contains("not a protected-path match"),
                "{raw}: {reason}"
            );
            assert!(
                !reason.contains("protected-path pattern"),
                "{raw}: {reason}"
            );
            // A read is refused too: an unusable config gates everything.
            assert!(matches!(
                decide_tool_call(
                    &state,
                    "murmur-tool-editor",
                    &editor("read_file", "src/x.py")
                ),
                Decision::Deny(_)
            ));
            // And so is every shell call.
            let shell = shell_state(Some(r#"{"protect": []}"#));
            assert!(matches!(run_script(&shell, "echo hi"), Decision::Deny(_)));
        }
    }

    #[test]
    fn the_config_error_reason_names_the_offending_key() {
        let reason = deny_reason(&decide_tool_call(
            &tool_state(Some(r#"{"protet": ["tests/"]}"#)),
            "murmur-tool-editor",
            &editor("write_file", "src/x.py"),
        ));
        assert!(reason.contains("unknown key 'protet'"), "{reason}");

        let reason = deny_reason(&decide_tool_call(
            &tool_state(Some(r#"{"protect": ["a**b"]}"#)),
            "murmur-tool-editor",
            &editor("write_file", "src/x.py"),
        ));
        assert!(reason.contains("pattern 0"), "{reason}");
        assert!(reason.contains("a**b"), "{reason}");
    }

    #[test]
    fn an_unreadable_tool_input_is_refused_with_a_distinct_reason() {
        let reason = deny_reason(&decide_tool_call(
            &tool_state(None),
            "murmur-tool-editor",
            "{not json",
        ));
        assert!(reason.contains("not valid JSON"), "{reason}");
        assert!(reason.contains("cannot read is refused"), "{reason}");

        let reason = deny_reason(&decide_tool_call(
            &tool_state(None),
            "murmur-tool-editor",
            r#"["write_file","tests/a.py"]"#,
        ));
        assert!(reason.contains("rather than a JSON object"), "{reason}");
    }

    #[test]
    fn the_shell_reason_names_the_write_form() {
        let reason = deny_reason(&run_script(
            &shell_state(None),
            "sed -i s/a/b/ tests/test_auth.py",
        ));
        assert_eq!(
            reason,
            "murmur-hook-protect-shell: 'sed -i' would write 'tests/test_auth.py', which the \
             protected-path pattern 'tests/' refuses. Change the code under test, not the test."
        );

        let reason = deny_reason(&run_script(
            &shell_state(None),
            "echo x > tests/test_auth.py",
        ));
        assert!(reason.contains("'> redirect' would write"), "{reason}");

        let reason = deny_reason(&run_script(&shell_state(None), "pytest | tee conftest.py"));
        assert!(
            reason.contains("'tee' would write 'conftest.py'"),
            "{reason}"
        );
        assert!(reason.contains("pattern 'conftest.py'"), "{reason}");
    }

    #[test]
    fn every_shell_write_form_refuses_a_protected_path_and_passes_an_unprotected_one() {
        let state = shell_state(None);
        let refused = [
            "sed -i s/a/b/ tests/a.py",
            "echo x > tests/a.py",
            "echo x >> tests/a.py",
            "echo x 2> tests/a.py",
            "echo x &> tests/a.py",
            "echo x >| tests/a.py",
            "pytest | tee tests/a.py",
            "patch tests/a.py",
            "cp a.py tests/a.py",
            "mv a.py tests/a.py",
            "install a.py tests/a.py",
            "ln -s a.py tests/a.py",
            "rm tests/a.py",
            "truncate -s 0 tests/a.py",
            "dd if=/dev/zero of=tests/a.py",
            "git checkout -- tests/a.py",
            "git restore tests/a.py",
            "git apply changes.diff",
        ];
        for command in refused {
            assert!(
                matches!(run_script(&state, command), Decision::Deny(_)),
                "expected a refusal for: {command}"
            );
        }

        let allowed = [
            "sed -i s/a/b/ src/a.py",
            "sed s/a/b/ tests/a.py",
            "echo x > src/a.py",
            "cat < tests/a.py",
            "pytest | tee src/a.py",
            "patch src/a.py",
            "cp a.py src/a.py",
            "mv a.py src/a.py",
            "install a.py src/a.py",
            "ln -s a.py src/a.py",
            "rm src/a.py",
            "truncate -s 0 src/a.py",
            "dd if=tests/a.py of=src/a.py",
            "git checkout main",
            "git checkout -- src/a.py",
            "git restore src/a.py",
            "git apply --check changes.diff",
            "git status",
            "pytest tests/",
            "ls tests/",
            "cat tests/a.py",
            "grep -r x tests/",
            "python3 -m pytest tests/test_a.py",
        ];
        for command in allowed {
            assert_eq!(
                run_script(&state, command),
                Decision::Allow,
                "expected no refusal for: {command}"
            );
        }
    }

    #[test]
    fn a_write_form_whose_targets_live_in_a_patch_is_refused_as_unreadable() {
        let reason = deny_reason(&run_script(&shell_state(None), "git apply changes.diff"));
        assert!(
            reason.contains("'git apply' writes files this hook cannot name"),
            "{reason}"
        );
        assert!(reason.contains("inside the patch"), "{reason}");

        let reason = deny_reason(&run_script(&shell_state(None), "patch -p1 < changes.diff"));
        assert!(
            reason.contains("'patch' writes files this hook cannot name"),
            "{reason}"
        );
    }

    #[test]
    fn an_escaping_shell_target_is_refused() {
        let reason = deny_reason(&run_script(&shell_state(None), "rm ../../etc/passwd"));
        assert!(reason.contains("escapes the capsule workdir"), "{reason}");
        assert!(reason.contains("'rm' would write"), "{reason}");
    }

    #[test]
    fn a_configured_extra_binary_is_gated_and_an_unconfigured_one_is_not() {
        let state = shell_state(Some(r#"{"shell_write_binaries":["my-writer"]}"#));
        assert!(matches!(
            run_script(&state, "my-writer tests/a.py"),
            Decision::Deny(_)
        ));
        assert_eq!(
            run_script(&state, "other-writer tests/a.py"),
            Decision::Allow
        );
    }

    #[test]
    fn an_empty_argv_and_an_empty_script_are_allowed_without_panicking() {
        let state = shell_state(None);
        assert_eq!(
            decide_shell_call(&state, "/bin/true", &[], None),
            Decision::Allow
        );
        assert_eq!(
            decide_shell_call(&state, "/bin/bash", &["-c".to_string()], Some("")),
            Decision::Allow
        );
        assert_eq!(run_script(&state, "echo x >"), Decision::Allow);
        assert_eq!(
            decide_shell_call(&state, "/bin/rm", &["tests/a.py".to_string()], None),
            {
                let reason = protected_reason(HOOK_PROTECT_SHELL, "rm", "tests/a.py", "tests/");
                Decision::Deny(reason)
            }
        );
    }

    #[test]
    fn an_unterminated_quote_still_produces_a_decision() {
        let state = shell_state(None);
        assert!(matches!(
            run_script(&state, "tee 'tests/a.py"),
            Decision::Deny(_)
        ));
        assert_eq!(
            run_script(&state, "sed -i 's/a/b/ src/a.py"),
            Decision::Allow
        );
    }
}
