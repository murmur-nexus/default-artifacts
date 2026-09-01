//! Extracting the write targets a tool call names, from the exact tool input JSON.
//!
//! `tool-event.input` is the exact JSON the tool will receive, never truncated,
//! which is what a policy has to decide on. A tool matching no configured rule is
//! not gated at all: this hook knows which key of which tool names a file only
//! because a rule said so.

use serde_json::Value;

use crate::config::{json_type, PolicyConfig, ToolRule};

/// One write target a tool call names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolWriteTarget {
    /// The input key the path was read from, e.g. `path`.
    pub key: String,
    /// The path exactly as the tool input carried it, un-normalized.
    pub path: String,
}

/// What a tool call turned out to be, once its rule was found and its input read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolTargets {
    /// No configured rule matches this tool name. The call is not gated at all and
    /// behaves exactly as with the hook uninstalled.
    NotGated,
    /// A rule matched, but its `write_when` says this call does not write —
    /// `read_file` and `find_in_files` land here. Refusing an agent's reads of a
    /// protected file would break every capsule that installs this and protect
    /// nothing.
    NotAWrite,
    /// A rule matched but its input cannot be read, so the target cannot be
    /// determined. Carries the clause naming what was wrong, for the refusal
    /// reason.
    Unreadable(String),
    /// The write targets the rule found. May be empty when the input names none of
    /// the rule's `path_keys`.
    Targets(Vec<ToolWriteTarget>),
}

/// The first rule whose `match` covers this tool name.
pub fn rule_for_tool<'a>(config: &'a PolicyConfig, tool_name: &str) -> Option<&'a ToolRule> {
    config
        .tools
        .iter()
        .find(|rule| rule.tool_match.matches_name(tool_name))
}

/// Read the write targets a tool call names, per the configured rules.
pub fn tool_write_targets(config: &PolicyConfig, tool_name: &str, input: &str) -> ToolTargets {
    let Some(rule) = rule_for_tool(config, tool_name) else {
        return ToolTargets::NotGated;
    };

    let value: Value = match serde_json::from_str(input) {
        Ok(value) => value,
        Err(e) => {
            return ToolTargets::Unreadable(format!(
                "is gated by a protected-path rule, but its input is not valid JSON ({e}), so \
                 the write target cannot be read"
            ))
        }
    };
    let Value::Object(map) = value else {
        return ToolTargets::Unreadable(format!(
            "is gated by a protected-path rule, but its input is {} rather than a JSON object, \
             so the write target cannot be read",
            json_type(&value)
        ));
    };

    if let Some(write_when) = &rule.write_when {
        match map.get(&write_when.key) {
            Some(Value::String(actual)) => {
                if !write_when.any_of.iter().any(|value| value == actual) {
                    return ToolTargets::NotAWrite;
                }
            }
            None | Some(Value::Null) => return ToolTargets::NotAWrite,
            Some(other) => {
                return ToolTargets::Unreadable(format!(
                    "is gated by a protected-path rule, but its '{}' key is {} rather than a \
                     string, so the policy cannot tell whether this call writes",
                    write_when.key,
                    json_type(other)
                ))
            }
        }
    }

    let mut targets = Vec::new();
    for key in &rule.path_keys {
        match map.get(key) {
            None | Some(Value::Null) => continue,
            Some(Value::String(path)) => targets.push(ToolWriteTarget {
                key: key.clone(),
                path: path.clone(),
            }),
            Some(other) => {
                return ToolTargets::Unreadable(format!(
                    "is gated by a protected-path rule, but its '{key}' key is {} rather than a \
                     string, so the write target cannot be read",
                    json_type(other)
                ))
            }
        }
    }
    ToolTargets::Targets(targets)
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
    use crate::config::{PolicyConfig, PolicySide};

    fn defaults() -> PolicyConfig {
        PolicyConfig::defaults(PolicySide::Tool)
    }

    fn paths(targets: &ToolTargets) -> Vec<String> {
        match targets {
            ToolTargets::Targets(list) => list.iter().map(|t| t.path.clone()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn the_editors_two_writing_operations_yield_the_path() {
        for operation in ["write_file", "replace_in_file"] {
            let input = format!(r#"{{"operation":"{operation}","path":"tests/test_x.py"}}"#);
            let targets = tool_write_targets(&defaults(), "murmur-tool-editor", &input);
            assert_eq!(paths(&targets), vec!["tests/test_x.py".to_string()]);
            assert_eq!(
                match &targets {
                    ToolTargets::Targets(list) => list[0].key.clone(),
                    other => panic!("{other:?}"),
                },
                "path"
            );
        }
    }

    #[test]
    fn the_editors_two_reading_operations_are_not_writes() {
        for operation in ["read_file", "find_in_files"] {
            let input = format!(r#"{{"operation":"{operation}","path":"tests/test_x.py"}}"#);
            assert_eq!(
                tool_write_targets(&defaults(), "murmur-tool-editor", &input),
                ToolTargets::NotAWrite,
                "{operation}"
            );
        }
    }

    #[test]
    fn a_tool_with_no_matching_rule_is_not_gated_at_all() {
        assert_eq!(
            tool_write_targets(&defaults(), "murmur-tool-git", r#"{"path":"tests/a.py"}"#),
            ToolTargets::NotGated
        );
    }

    #[test]
    fn a_rule_with_no_write_when_treats_every_path_key_hit_as_a_write() {
        let targets = tool_write_targets(
            &defaults(),
            "murmur-tool-create",
            r#"{"type":"tool","name":"tests","runtime":"wasm"}"#,
        );
        assert_eq!(paths(&targets), vec!["tests".to_string()]);
    }

    #[test]
    fn a_missing_write_when_key_means_the_call_does_not_write() {
        assert_eq!(
            tool_write_targets(
                &defaults(),
                "murmur-tool-editor",
                r#"{"path":"tests/a.py"}"#
            ),
            ToolTargets::NotAWrite
        );
    }

    #[test]
    fn input_that_is_not_valid_json_is_unreadable() {
        let targets = tool_write_targets(&defaults(), "murmur-tool-editor", "{not json");
        match targets {
            ToolTargets::Unreadable(detail) => {
                assert!(detail.contains("not valid JSON"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn input_that_is_a_json_array_is_unreadable() {
        let targets = tool_write_targets(&defaults(), "murmur-tool-editor", r#"["write_file"]"#);
        match targets {
            ToolTargets::Unreadable(detail) => {
                assert!(
                    detail.contains("a list rather than a JSON object"),
                    "{detail}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_non_string_operation_or_path_is_unreadable() {
        let targets = tool_write_targets(
            &defaults(),
            "murmur-tool-editor",
            r#"{"operation":7,"path":"a"}"#,
        );
        match targets {
            ToolTargets::Unreadable(detail) => {
                assert!(detail.contains("'operation'"), "{detail}");
                assert!(detail.contains("whether this call writes"), "{detail}");
            }
            other => panic!("{other:?}"),
        }

        let targets = tool_write_targets(
            &defaults(),
            "murmur-tool-editor",
            r#"{"operation":"write_file","path":["a"]}"#,
        );
        match targets {
            ToolTargets::Unreadable(detail) => assert!(detail.contains("'path'"), "{detail}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_path_and_a_root_path_are_still_targets() {
        let targets = tool_write_targets(
            &defaults(),
            "murmur-tool-editor",
            r#"{"operation":"write_file","path":""}"#,
        );
        assert_eq!(paths(&targets), vec![String::new()]);

        let targets = tool_write_targets(
            &defaults(),
            "murmur-tool-editor",
            r#"{"operation":"write_file","path":"/"}"#,
        );
        assert_eq!(paths(&targets), vec!["/".to_string()]);
    }

    #[test]
    fn a_write_call_naming_no_path_key_yields_no_targets() {
        let targets = tool_write_targets(
            &defaults(),
            "murmur-tool-editor",
            r#"{"operation":"write_file","content":"x"}"#,
        );
        assert_eq!(targets, ToolTargets::Targets(Vec::new()));
    }
}
