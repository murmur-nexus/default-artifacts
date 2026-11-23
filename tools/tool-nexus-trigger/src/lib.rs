//! A writer for murmur-nexus's trigger inbox (protocol v1), packaged as a `wasm32-wasip2`
//! component exporting `murmur:tool/run` (world `tool`) and importing no `murmur:*` interface.
//!
//! A capsule uses it to ask Nexus to wait: `add`, `list`, `show` and `cancel`, the four
//! structured ops of the inbox. Each call writes one request into the inbox, waits a fixed time
//! for a Nexus window's answer, reads and removes it, and returns it. The tool holds no trigger
//! policy: approval, ranges, the live and awaiting limits and the cancel rule stay in Nexus. It
//! checks only the request's size and the label's length, both numbers Nexus publishes.
//!
//! The operator grants the entry `capabilities.filesystem.scope: .nexus/triggers/inbox`, so the
//! tool's `.` is the inbox and nothing else. An entry with no scope reaches the whole work
//! folder; the tool refuses it before writing anything.
//!
//! Every module but the `wasm_tool` adapter compiles for the host, so `cargo test` exercises it
//! natively: the grant, the clock and the randomness arrive as parameters.

pub mod answer;
pub mod inbox;
pub mod ops;
pub mod request;

use ops::Grant;

/// The inbox, as the guest sees it: the scope the operator granted is mounted at `.`.
pub const GUEST_ROOT: &str = ".";

/// The read-only preopen the runtime hands only a tool entry without a scope.
pub const RUNTIME_PREOPEN: &str = "murmur-runtime";

/// What the guest's preopen names say the entry reaches: a scoped entry holds only its subtree
/// (and `state` when it has a store), so a `murmur-runtime` preopen or a host root
/// ([`murmur_host_roots::HostRoot::from_preopen`]) means it declared no scope.
pub fn grant_of<'a>(preopen_names: impl IntoIterator<Item = &'a str>) -> Grant {
    if preopen_names.into_iter().any(|name| {
        name == RUNTIME_PREOPEN || murmur_host_roots::HostRoot::from_preopen(name, false).is_some()
    }) {
        Grant::WholeWorkdir
    } else {
        Grant::Inbox
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::Path;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

    use crate::inbox::{Inbox, SystemClock, RID_BYTES};
    use crate::ops::{self, OpStatus};

    struct Component;

    /// The preopens' names. Each descriptor is dropped here, so none outlives the call.
    fn preopen_names() -> Vec<String> {
        wasip2::filesystem::preopens::get_directories()
            .into_iter()
            .map(|(_descriptor, name)| name)
            .collect()
    }

    /// [`RID_BYTES`] bytes from `wasi:random/random`.
    fn random_rid_bytes() -> [u8; RID_BYTES] {
        let mut bytes = [0u8; RID_BYTES];
        let random = wasip2::random::random::get_random_bytes(RID_BYTES as u64);
        for (slot, byte) in bytes.iter_mut().zip(random) {
            *slot = byte;
        }
        bytes
    }

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            let names = preopen_names();
            let grant = crate::grant_of(names.iter().map(String::as_str));
            let mut random = random_rid_bytes;
            let mut clock = SystemClock::new();
            let mut inbox = Inbox::new(Path::new(crate::GUEST_ROOT), &mut random, &mut clock);
            let response = ops::run(grant, input.data.as_deref(), &mut inbox);

            ToolResult {
                status: match response.status {
                    OpStatus::Passed => Status::Passed,
                    OpStatus::Failed => Status::Failed,
                    OpStatus::Error => Status::Error,
                },
                summary: Some(response.summary),
                data: Some(response.data),
                data_path: response.data_path,
                truncated: response.truncated,
                metadata: response.metadata,
                attachments: Vec::new(),
            }
        }
    }

    export!(Component);
}

#[cfg(test)]
mod tests {
    //! Source-level invariants: the no-policy and inbox-only properties the behaviour tests
    //! cannot see. Every needle is assembled with `concat!` so that this module is not its own
    //! evidence, and every file is cut at its own test module, if it has one.

    /// Every `src/*.rs` file, cut at `"\nmod tests {"` when present.
    fn raw_non_test_sources() -> Vec<(&'static str, &'static str)> {
        [
            ("lib.rs", include_str!("lib.rs")),
            ("answer.rs", include_str!("answer.rs")),
            ("inbox.rs", include_str!("inbox.rs")),
            ("ops.rs", include_str!("ops.rs")),
            ("request.rs", include_str!("request.rs")),
        ]
        .into_iter()
        .map(|(name, source)| {
            let cut = source
                .find("\nmod tests {")
                .map_or(source, |at| &source[..at]);
            (name, cut)
        })
        .collect()
    }

    /// Source with `//` comments removed line by line, so a doc comment explaining what the tool
    /// never does is not counted as doing it.
    fn code_only(source: &str) -> String {
        source
            .lines()
            .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_source_file_is_covered() {
        let lib = include_str!("lib.rs");
        let declared = lib
            .lines()
            .filter(|line| line.starts_with("pub mod "))
            .count();
        assert_eq!(declared + 1, raw_non_test_sources().len());
    }

    #[test]
    fn no_target_cfg_outside_the_adapter() {
        let needle = concat!("cfg(target", "_arch");
        for (name, source) in raw_non_test_sources() {
            let expected = usize::from(name == "lib.rs");
            assert_eq!(
                source.matches(needle).count(),
                expected,
                "src/{name}: only the wasm_tool adapter is target-gated"
            );
        }
        assert!(include_str!("lib.rs").contains(concat!(
            "#[cfg(target",
            "_arch = \"wasm32\")]\nmod wasm_tool {"
        )));
    }

    #[test]
    fn source_never_names_the_store_or_a_process() {
        // The trigger store is Nexus's alone (its SQLite lock and record format), and spawning
        // `nexus` would need the node grant back.
        for (name, source) in raw_non_test_sources() {
            let source = source.to_lowercase();
            for needle in [
                concat!("leases", ".sqlite"),
                concat!(".to", "ml"),
                concat!("murmur:pro", "cess"),
                concat!("murmur::pro", "cess"),
                concat!("std::pro", "cess"),
                concat!("command::", "new"),
            ] {
                assert!(
                    !source.contains(needle),
                    "src/{name}: non-test source must not contain {needle}"
                );
            }
        }
    }

    #[test]
    fn source_never_lists_creates_folders_or_opens_other_names() {
        for (name, source) in raw_non_test_sources() {
            let code = code_only(source);
            for needle in [
                concat!("read", "_dir"),
                concat!("create", "_dir"),
                concat!("Dir", "Builder"),
                concat!(".tak", "en"),
                concat!(".answer", ".tmp"),
            ] {
                assert!(
                    !code.contains(needle),
                    "src/{name}: non-test code must not contain {needle}"
                );
            }
        }
        // Every path is built in one place, from the root and the three suffixes.
        let inbox = code_only(include_str!("inbox.rs"));
        assert_eq!(inbox.matches(concat!("root", ".join(")).count(), 3);
        assert_eq!(inbox.matches(concat!(".jo", "in(")).count(), 3);
        assert_eq!(inbox.matches(concat!("Path", "Buf")).count(), 4);
        for (name, source) in raw_non_test_sources() {
            let code = code_only(source);
            if name != "inbox.rs" {
                assert!(
                    !code.contains(concat!("Path", "Buf")),
                    "src/{name}: only inbox.rs builds a path"
                );
            }
            let roots = code.matches(concat!("Path", "::new(")).count();
            let expected = usize::from(name == "lib.rs");
            assert_eq!(roots, expected, "src/{name}: the one root is GUEST_ROOT");
        }
        assert!(include_str!("lib.rs").contains(concat!("Path::new(crate::", "GUEST_ROOT)")));
    }

    #[test]
    fn no_trigger_policy_number_but_the_label_limit() {
        // TRIGGER_LIMITS other than labelMaxChars: labelDefaultChars 40, commandMaxBytes 4000,
        // wakePromptMaxBytes 4000, liveMax 16, awaitingMax 4, and the duration bounds in ms.
        for (name, source) in raw_non_test_sources() {
            let code = code_only(source);
            for needle in [
                "4000", "4_000", "3600", "86400", "86_400", "1800", "28800", "30000", "30_000",
                "60000", "60_000",
            ] {
                assert!(
                    !code.contains(needle),
                    "src/{name}: non-test code must not hold the policy number {needle}"
                );
            }
            for needle in [
                concat!("live", "max"),
                concat!("awaiting", "max"),
                concat!("live_", "max"),
                concat!("awaiting_", "max"),
                concat!("parse_", "duration"),
                concat!("cr", "on::"),
            ] {
                assert!(
                    !code.to_lowercase().contains(needle),
                    "src/{name}: non-test code must not hold {needle}"
                );
            }
        }
        assert_eq!(crate::request::LABEL_MAX_CHARS, 80);
        assert_eq!(crate::request::REQUEST_MAX_BYTES, 16_384);
    }

    #[test]
    fn manifest_schema_root_is_opaque_and_writes_nothing() {
        let manifest = include_str!("../murmur.yaml");
        assert!(manifest.contains(
            "input_schema:\n  type: object\n  format: murmur-opaque\n  murmur-destinations: []\n"
        ));
        assert!(manifest.contains("\n  required: [op]\n"));
        assert!(manifest.contains("requires_files:\n  - tool_nexus_trigger.wasm\n"));
    }

    #[test]
    fn manifest_declares_no_capabilities_block() {
        // Only the operator's entry grants the scope; a bundled block would grant nothing.
        let manifest = include_str!("../murmur.yaml");
        assert!(!manifest
            .lines()
            .any(|line| line.starts_with("capabilities:")));
    }

    #[test]
    fn manifest_fields_match_the_protocol() {
        let manifest: serde_json::Value =
            serde_norway::from_str(include_str!("../murmur.yaml")).expect("murmur.yaml parses");
        let properties = manifest["input_schema"]["properties"]
            .as_object()
            .expect("input_schema.properties");
        let mut expected: Vec<&str> = vec!["op", "id", "yes"];
        expected.extend(crate::request::ADD_FIELDS);
        let mut got: Vec<&str> = properties.keys().map(String::as_str).collect();
        got.sort_unstable();
        expected.sort_unstable();
        assert_eq!(got, expected);
        for (name, property) in properties {
            let want = if name == "yes" { "boolean" } else { "string" };
            assert_eq!(property["type"], want, "{name}");
            assert!(
                property["description"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()),
                "{name} has a description"
            );
        }
        assert_eq!(
            properties["op"]["enum"],
            serde_json::json!(["add", "list", "show", "cancel"])
        );
        let description = manifest["description"].as_str().unwrap();
        for needle in [
            "waits for a person's approval in a Nexus window",
            "shell command Nexus runs on the host, as the person",
            "Call list before you add",
            "approve, refuse and renew are a person's",
            "Operators:",
            "scope: .nexus/triggers/inbox",
        ] {
            assert!(description.contains(needle), "description: {needle}");
        }
    }

    #[test]
    fn grant_reads_the_preopen_names() {
        use crate::ops::Grant;
        assert_eq!(crate::grant_of(["."]), Grant::Inbox);
        assert_eq!(crate::grant_of([".", "state"]), Grant::Inbox);
        assert_eq!(crate::grant_of([]), Grant::Inbox);
        assert_eq!(
            crate::grant_of([".", "murmur-runtime"]),
            Grant::WholeWorkdir
        );
        assert_eq!(
            crate::grant_of([".", "murmur-runtime", "murmur-host"]),
            Grant::WholeWorkdir
        );
        assert_eq!(
            crate::grant_of([".", "murmur-host/data"]),
            Grant::WholeWorkdir
        );
        assert_eq!(crate::grant_of([".", "murmur-hostile"]), Grant::Inbox);
    }
}
