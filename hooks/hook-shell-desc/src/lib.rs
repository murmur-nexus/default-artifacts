// Profiles live at module root, outside the wasm32 gate, so host tests can read them.
//
// The hook profiles only binaries the runtime runs without a shell: it splits `command` into
// an argument list and executes the binary with it. Shell interpreters (`bash`, `sh`, ...) get
// no profile, so the runtime's own manifest — which states how `command` reaches the
// interpreter — is the one the model reads.

/// Builds one `(binary, manifest)` row from the binary's identity phrase and an optional source
/// sentence. Every manifest shares the same body, which states what the runtime does with `command`
/// (`split_shell_words` and the shell result format in murmur's capsule-runtime).
macro_rules! profile_row {
    ($b:literal, $what:literal $(, $src:literal)?) => {
        ($b, concat!(
            "name: ", $b, "\n",
            "runtime: tool\n",
            "implementation: native\n",
            "description: |\n",
            "  Runs ", $b, ", ", $what, ", in the capsule workdir.\n",
            "  `command` is the argument list ", $b, " receives, without the leading `", $b, "`.",
            $($src,)?
            "\n",
            "  The runtime splits `command` into arguments at whitespace. Text inside '...' or \"...\" stays one argument and loses its quotes, and a backslash makes the next character literal, inside quotes too.\n",
            "  No shell runs the command: |, >, <, &&, ;, globs, ~ and $VAR reach ", $b, " as literal arguments.\n",
            "  Returns ", $b, "'s exit code, stdout and stderr. Output past the runtime's size limit is cut short, and the result says so.\n",
            "input:\n",
            "  type: object\n",
            "  properties:\n",
            "    command: { type: string, description: \"Arguments passed to ", $b, ", without the binary name.\" }\n",
            "  required: [command]\n",
        ))
    };
}

/// Binary name and its manifest, for every binary the hook profiles.
const PROFILES: &[(&str, &str)] = &[
    profile_row!("cargo", "the Rust package manager and build tool"),
    profile_row!("curl", "which transfers data to and from URLs"),
    profile_row!(
        "find",
        "which walks directory trees and lists the files that match its tests"
    ),
    profile_row!("git", "the Git version control client"),
    profile_row!(
        "grep",
        "which prints the lines of files that match a pattern"
    ),
    profile_row!("jq", "a command-line JSON processor"),
    profile_row!(
        "node",
        "the Node.js JavaScript runtime",
        " It is not JavaScript source."
    ),
    profile_row!(
        "python3",
        "the Python 3 interpreter",
        " It is not Python source."
    ),
    profile_row!("rustc", "the Rust compiler"),
];

/// The manifest the hook writes for `binary`, or `None` when the runtime's own manifest stands.
pub fn profile(binary: &str) -> Option<&'static str> {
    PROFILES
        .iter()
        .find(|(name, _)| *name == binary)
        .map(|(_, text)| *text)
}

/// One `(binary, manifest)` per `shell_allow` entry that has a profile, in input order.
pub fn manifests_for(shell_allow: &[String]) -> Vec<(String, &'static str)> {
    shell_allow
        .iter()
        .filter_map(|binary| profile(binary).map(|text| (binary.clone(), text)))
        .collect()
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use super::manifests_for;

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    pub struct ShellDescHook;

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
        ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent, ToolManifest,
    };

    impl Guest for ShellDescHook {
        fn on_stage(event: StageEvent) -> Result<HookOutput, String> {
            let manifests = manifests_for(&event.shell_allow)
                .into_iter()
                .map(|(binary_name, content)| ToolManifest {
                    binary_name,
                    content: content.to_string(),
                })
                .collect();
            Ok(HookOutput::WriteManifests(manifests))
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

    export!(ShellDescHook);
}

#[cfg(test)]
mod tests {
    use super::{manifests_for, profile};

    /// Every binary the hook profiles.
    const PROFILED: [&str; 9] = [
        "cargo", "curl", "find", "git", "grep", "jq", "node", "python3", "rustc",
    ];

    const PYTHON3_MANIFEST: &str = "\
name: python3
runtime: tool
implementation: native
description: |
  Runs python3, the Python 3 interpreter, in the capsule workdir.
  `command` is the argument list python3 receives, without the leading `python3`. It is not Python source.
  The runtime splits `command` into arguments at whitespace. Text inside '...' or \"...\" stays one argument and loses its quotes, and a backslash makes the next character literal, inside quotes too.
  No shell runs the command: |, >, <, &&, ;, globs, ~ and $VAR reach python3 as literal arguments.
  Returns python3's exit code, stdout and stderr. Output past the runtime's size limit is cut short, and the result says so.
input:
  type: object
  properties:
    command: { type: string, description: \"Arguments passed to python3, without the binary name.\" }
  required: [command]
";

    /// True when `text` would have the model put `-c` into `command`.
    fn tells_model_to_add_dash_c(text: &str) -> bool {
        let is_separator = |c: char| {
            c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '(' | ')' | ',' | ';' | ':')
        };
        text.split(is_separator).any(|token| token == "-c")
            || text.contains("bash -c")
            || text.contains("sh -c")
    }

    #[test]
    fn known_binaries_have_profiles() {
        for binary in PROFILED {
            assert!(profile(binary).is_some(), "missing profile for {binary}");
        }
    }

    #[test]
    fn unknown_binary_returns_none() {
        assert!(profile("my-tool").is_none());
        assert!(profile("").is_none());
    }

    #[test]
    fn interpreters_have_no_profile() {
        // Mirrors `SHELL_INTERPRETERS` in murmur's crates/capsule-runtime/src/shell.rs.
        for binary in ["bash", "sh", "zsh", "fish", "dash", "ksh"] {
            assert!(
                profile(binary).is_none(),
                "{binary} must keep the runtime's manifest"
            );
        }
    }

    #[test]
    fn manifests_for_skips_interpreters_and_unknown_binaries() {
        let allow = ["bash", "python3", "sh", "my-tool"].map(String::from);
        assert_eq!(
            manifests_for(&allow),
            vec![("python3".to_string(), PYTHON3_MANIFEST)]
        );
        assert!(manifests_for(&[]).is_empty());
    }

    #[test]
    fn python3_manifest_is_pinned() {
        assert_eq!(profile("python3"), Some(PYTHON3_MANIFEST));
    }

    #[test]
    fn no_profile_tells_the_model_to_add_dash_c() {
        for binary in PROFILED {
            let text = profile(binary).unwrap();
            assert!(
                !tells_model_to_add_dash_c(text),
                "{binary}: profile tells the model to add -c"
            );
        }
        // The 0.7.0 descriptions, which the checker must catch.
        assert!(tells_model_to_add_dash_c(
            "GNU Bourne-Again Shell. Run single commands with -c '<cmd>'.\nStrict mode: bash -c 'set -euo pipefail; ...'."
        ));
        assert!(tells_model_to_add_dash_c(
            "Python 3. One-liners: python3 -c '<code>'. JSON: python3 -m json.tool."
        ));
    }

    #[test]
    fn no_profile_puts_its_binary_name_in_front_of_arguments() {
        for binary in PROFILED {
            let text = profile(binary).unwrap();
            assert!(
                !text.contains(&format!("{binary} -")),
                "{binary}: profile shows `{binary} -...`"
            );
        }
    }

    #[test]
    fn every_profile_states_input_output_and_limits() {
        for b in PROFILED {
            let text = profile(b).unwrap();
            assert!(text.starts_with(&format!("name: {b}\n")), "{b}: name");
            for expected in [
                "runtime: tool".to_string(),
                "implementation: native".to_string(),
                format!("`command` is the argument list {b} receives, without the leading `{b}`."),
                "No shell runs the command".to_string(),
                format!("Returns {b}'s exit code, stdout and stderr."),
                "required: [command]".to_string(),
            ] {
                assert!(text.contains(&expected), "{b}: missing {expected:?}");
            }
            assert!(
                !text.contains("runtime: native"),
                "{b}: must not use runtime: native"
            );
        }
    }
}
