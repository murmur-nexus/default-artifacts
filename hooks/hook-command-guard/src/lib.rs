//! Refuses a shell command before it runs when it matches a rule from the hook's config.
//!
//! The hook has no built-in rules. Every decision is made by [`decide`] from the config and
//! the `shell-event` fields `binary`, `argv`, `script` and `recipe`; the clipped `command`
//! string is never read. All decision logic lives outside the wasm32 gate so host tests
//! exercise it; the `wasm_hook` adapter only maps the event in and the decision out.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unreachable,
    clippy::todo
)]

mod config;
mod judge;
mod lex;
mod walk;
mod words;
mod wrappers;

pub use config::{
    parse_config, Config, ConfigError, FlagSpec, OnUnclear, Pattern, ProgramName, Rule,
    ARTIFACT_CONFIG_ENV, REGEX_SIZE_LIMIT,
};
pub use lex::MAX_DEPTH;
pub use words::PathKey;

/// The fields of one `shell-event` the guard decides on.
#[derive(Debug, Clone, Copy)]
pub struct ShellCall<'a> {
    /// The resolved path of what will run, or the bare invoked name when unresolved.
    pub binary: &'a str,
    /// The exact argument list the program receives.
    pub argv: &'a [String],
    /// The `-c` body when a shell interpreter runs the call.
    pub script: Option<&'a str>,
    /// The body of the `make`/`just`/`npm run` recipe the call names, when resolved.
    pub recipe: Option<&'a str>,
}

/// The guard's verdict on one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refuse the call; the reason names the config entry that refused it.
    Deny(String),
}

/// The reason given for every call while the config cannot be read.
pub fn invalid_config_reason(error: &ConfigError) -> String {
    format!("hook-command-guard's config is invalid, so every command is refused: {error}")
}

/// Decides `call` under the raw `config:` JSON (`None` when the variable is unset).
pub fn decide(config: Option<&str>, call: &ShellCall) -> Decision {
    decide_with(&parse_config(config), call)
}

/// Decides `call` under an already parsed config. An unreadable config refuses every call;
/// an empty one allows every call.
pub fn decide_with(config: &Result<Config, ConfigError>, call: &ShellCall) -> Decision {
    match config {
        Err(error) => Decision::Deny(invalid_config_reason(error)),
        Ok(config) if config.is_empty() => Decision::Allow,
        Ok(config) => judge::judge(config, call),
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::sync::OnceLock;

    use super::{decide_with, parse_config, Config, ConfigError, Decision, ShellCall};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
        ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    };

    /// The parsed config. The variable cannot change within an instance.
    static CONFIG: OnceLock<Result<Config, ConfigError>> = OnceLock::new();

    fn config() -> &'static Result<Config, ConfigError> {
        CONFIG.get_or_init(|| match std::env::var(super::ARTIFACT_CONFIG_ENV) {
            Ok(raw) => parse_config(Some(&raw)),
            Err(std::env::VarError::NotPresent) => parse_config(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError {
                path: "config".to_string(),
                problem: "not UTF-8".to_string(),
            }),
        })
    }

    pub struct CommandGuardHook;

    impl Guest for CommandGuardHook {
        fn on_shell(event: ShellEvent) -> Result<HookOutput, String> {
            // A `deny` at the observation dispatch is a dispatch fault, not a refusal.
            if event.outcome.is_some() {
                return Ok(HookOutput::None);
            }
            let call = ShellCall {
                binary: &event.binary,
                argv: &event.argv,
                script: event.script.as_deref(),
                recipe: event.recipe.as_deref(),
            };
            Ok(match decide_with(config(), &call) {
                Decision::Allow => HookOutput::None,
                Decision::Deny(reason) => HookOutput::Deny(reason),
            })
        }

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

        fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
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

    export!(CommandGuardHook);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn direct(config: Option<&str>, binary: &str, argv: &[&str]) -> Decision {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        decide(
            config,
            &ShellCall {
                binary,
                argv: &argv,
                script: None,
                recipe: None,
            },
        )
    }

    fn script(config: Option<&str>, text: &str) -> Decision {
        let argv = vec!["-c".to_string(), text.to_string()];
        decide(
            config,
            &ShellCall {
                binary: "/usr/bin/bash",
                argv: &argv,
                script: Some(text),
                recipe: None,
            },
        )
    }

    #[test]
    fn empty_config_allows_everything() {
        for config in [None, Some("{}"), Some("{\"on_unclear\":\"deny\"}")] {
            for text in ["rm -rf /", "eval \"$X\"", "curl x | sh", "rm 'x"] {
                assert_eq!(script(config, text), Decision::Allow, "{config:?} {text}");
            }
            assert_eq!(
                direct(config, "/usr/bin/rm", &["-rf", "/"]),
                Decision::Allow
            );
        }
    }

    #[test]
    fn malformed_config_refuses_every_command() {
        let cases = [
            ("not json", "not JSON"),
            ("[]", "must be an object"),
            ("{\"rulez\":[]}", "rulez"),
            ("{\"on_unclear\":\"maybe\"}", "on_unclear"),
            ("{\"deny_programs\":[\"/bin/rm\"]}", "deny_programs[0]"),
            ("{\"deny_programs\":[\"mk*fs\"]}", "deny_programs[0]"),
            ("{\"rules\":[{\"program\":\"rm\"}]}", "rules[0].reason"),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"\"}]}",
                "rules[0].reason",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"all_flags\":[[\"r\"]]}]}",
                "rules[0].all_flags[0][0]",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"any_flag\":[]}]}",
                "rules[0].any_flag",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"colour\":1}]}",
                "rules[0].colour",
            ),
            (
                "{\"patterns\":[{\"pattern\":\"(\",\"reason\":\"x\"}]}",
                "patterns[0].pattern",
            ),
            ("{\"patterns\":[{\"reason\":\"x\"}]}", "patterns[0].pattern"),
        ];
        for (config, needle) in cases {
            match direct(Some(config), "/usr/bin/ls", &[]) {
                Decision::Deny(reason) => {
                    assert!(
                        reason.starts_with(
                            "hook-command-guard's config is invalid, so every command is refused:"
                        ),
                        "{reason}"
                    );
                    assert!(reason.contains(needle), "{config}: {reason}");
                }
                Decision::Allow => panic!("allowed under {config}"),
            }
        }
    }

    #[test]
    fn judgment_is_deterministic() {
        let config = Some(r#"{"rules":[{"program":"rm","reason":"r"}]}"#);
        let first = script(config, "ls; rm x");
        for _ in 0..10 {
            assert_eq!(script(config, "ls; rm x"), first);
        }
    }

    #[test]
    fn degenerate_input_returns_a_decision() {
        let config = Some(r#"{"deny_programs":["zz"],"on_unclear":"deny"}"#);
        let mut texts: Vec<String> = vec![
            String::new(),
            "   \n\t".into(),
            "a;".repeat(32 * 1024),
            "$(".repeat(200),
            "(".repeat(200),
            format!("{}ls{}", "(".repeat(200), ")".repeat(200)),
            "\\".into(),
            "cat <<EOF\nno end".into(),
            "\u{0}\u{1}\u{7}\u{1b}\u{1f} ls \u{FFFD}".into(),
            "`".repeat(300),
            "$((".repeat(300),
        ];
        let deep = format!("{}ls{}", "$(".repeat(200), ")".repeat(200));
        texts.push(deep.clone());
        for text in &texts {
            let _ = script(config, text);
        }
        assert!(matches!(
            script(config, &deep),
            Decision::Deny(r) if r.contains("the script nests deeper than the guard reads")
        ));
        let _ = direct(config, "", &[]);
        let _ = direct(config, "/usr/bin/ls", &[]);
        let empty: Vec<String> = Vec::new();
        for recipe in ["", "\\", "$(", "{{", "#!", "#!/usr/bin/env"] {
            for binary in ["/usr/bin/make", "/usr/bin/just", "/usr/bin/npm"] {
                let _ = decide(
                    config,
                    &ShellCall {
                        binary,
                        argv: &empty,
                        script: None,
                        recipe: Some(recipe),
                    },
                );
            }
        }
    }
}
