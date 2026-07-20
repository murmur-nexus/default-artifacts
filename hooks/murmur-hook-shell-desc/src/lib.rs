// Binary profiles are declared at module root so they are accessible from tests
// compiled for the host target, even though the hook WIT impl is wasm32-only.
const BASH: &str = "\
name: bash
runtime: tool
implementation: native
description: |
  GNU Bourne-Again Shell. Run single commands with -c '<cmd>'.
  Strict mode: bash -c 'set -euo pipefail; ...'.
  Sequential: cmd1 && cmd2. Fallback: cmd1 || cmd2.
  Capture stderr: cmd 2>&1. Background: cmd &.
input:
  type: object
  properties:
    command: { type: string, description: \"Shell command string.\" }
  required: [command]
";

const GIT: &str = "\
name: git
runtime: tool
implementation: native
description: |
  Git version control. Common subcommands: status, log --oneline [-n N],
  diff [--staged], add <path>, commit -m '<msg>', checkout -b <branch>,
  push [origin <branch>], pull [--rebase], stash / stash pop.
  Use --no-pager for non-interactive output.
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const CARGO: &str = "\
name: cargo
runtime: tool
implementation: native
description: |
  Rust build tool. Subcommands: build [--release], test [--lib] [-- <filter>],
  check (fast typecheck), clippy [-- -D warnings], fmt [--check],
  run [--release], add <crate>. Output goes to stderr; capture with 2>&1.
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const RUSTC: &str = "\
name: rustc
runtime: tool
implementation: native
description: |
  Rust compiler. Prefer cargo build for projects.
  Direct: rustc <file.rs> -o <out> [--edition 2021].
  Useful flags: --emit=asm, --explain <E-code>.
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const GREP: &str = "\
name: grep
runtime: tool
implementation: native
description: |
  Search file contents. Key flags: -r (recursive), -n (line numbers),
  -i (case-insensitive), -l (filenames only), -E (extended regex),
  -F (fixed string), --include='*.rs', -A/-B/-C N (context lines).
  Example: grep -rn 'fn main' --include='*.rs' .
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const FIND: &str = "\
name: find
runtime: tool
implementation: native
description: |
  Locate files. find <dir> -name '*.rs' -type f -not -path '*/target/*'.
  Flags: -type d (dirs), -mtime -1 (modified today), -size +1M,
  -exec <cmd> {} \\;
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const CURL: &str = "\
name: curl
runtime: tool
implementation: native
description: |
  HTTP client. Key flags: -s (silent), -S (show errors), -X <METHOD>,
  -H '<k>: <v>', -d '<body>', -o <file>, -L (follow redirects), -f (fail on error).
  Pattern: curl -sSf -X POST -H 'Content-Type: application/json' -d '{\"k\":\"v\"}' <url>
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const JQ: &str = "\
name: jq
runtime: tool
implementation: native
description: |
  JSON processor. echo '<json>' | jq '<filter>'.
  Common filters: . (identity), .field, .[N], .[], select(.x == \"v\"),
  {k: .v}, keys, length, type.
  Flags: -r (raw string), -c (compact), -e (exit 1 if null).
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const PYTHON3: &str = "\
name: python3
runtime: tool
implementation: native
description: |
  Python 3. One-liners: python3 -c '<code>'. JSON: python3 -m json.tool.
  Example: python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"key\"])'
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

const NODE: &str = "\
name: node
runtime: tool
implementation: native
description: |
  Node.js runtime. One-liners: node -e '<code>'. Scripts: node <file.js>.
input:
  type: object
  properties:
    command: { type: string }
  required: [command]
";

pub fn profile(binary: &str) -> Option<&'static str> {
    match binary {
        "bash" => Some(BASH),
        "git" => Some(GIT),
        "cargo" => Some(CARGO),
        "rustc" => Some(RUSTC),
        "grep" => Some(GREP),
        "find" => Some(FIND),
        "curl" => Some(CURL),
        "jq" => Some(JQ),
        "python3" => Some(PYTHON3),
        "node" => Some(NODE),
        _ => None,
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use super::profile;

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
            let manifests: Vec<ToolManifest> = event
                .shell_allow
                .iter()
                .filter_map(|binary| {
                    profile(binary).map(|content| ToolManifest {
                        binary_name: binary.clone(),
                        content: content.to_string(),
                    })
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
    use super::profile;

    #[test]
    fn known_binaries_have_profiles() {
        for binary in ["bash", "git", "cargo", "rustc", "grep", "find", "curl", "jq", "python3", "node"] {
            assert!(profile(binary).is_some(), "missing profile for {binary}");
        }
    }

    #[test]
    fn unknown_binary_returns_none() {
        assert!(profile("my-tool").is_none());
        assert!(profile("").is_none());
    }

    #[test]
    fn profiles_use_tool_runtime_not_native() {
        for binary in ["bash", "git", "cargo", "rustc", "grep", "find", "curl", "jq", "python3", "node"] {
            let content = profile(binary).unwrap();
            assert!(content.contains("runtime: tool"), "{binary}: should use runtime: tool");
            assert!(content.contains("implementation: native"), "{binary}: should declare native implementation");
            assert!(!content.contains("runtime: native"), "{binary}: must not use deprecated runtime: native");
        }
    }

    #[test]
    fn git_profile_mentions_subcommands() {
        let content = profile("git").unwrap();
        assert!(content.contains("log --oneline"), "git profile should mention log --oneline");
    }

    #[test]
    fn cargo_profile_mentions_check() {
        let content = profile("cargo").unwrap();
        assert!(content.contains("check"), "cargo profile should mention check");
    }
}
