//! The `MURMUR_ARTIFACT_CONFIG` parser.
//!
//! Config reaches a hook as one environment variable holding the compact JSON of
//! the `config:` block on the artifact's entry in the *capsule operator's*
//! manifest. An entry with no `config:` key delivers no variable at all — that
//! means "use the defaults", never "an empty object".
//!
//! The runtime validates only shape at launch (a mapping with string keys, at
//! most 65536 bytes). Meaning is entirely this module's business, and anything it
//! cannot make sense of becomes a [`ConfigError`] that refuses every gated call
//! rather than a silently-permissive default.

use std::fmt;

use serde_json::{Map, Value};

use crate::glob::Pattern;

/// The environment variable the runtime injects an artifact's `config:` block
/// into, as compact JSON.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Which of the two artifacts is reading the config. The two share `protect` and
/// `allow` and each owns one further key, so a key belonging to the other half is
/// an unknown key here — the two hooks never read each other's config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicySide {
    /// `murmur-hook-protect-tool`, which also reads `tools`.
    Tool,
    /// `murmur-hook-protect-shell`, which also reads `shell_write_binaries`.
    Shell,
}

impl PolicySide {
    /// Every top-level key this side accepts, in the order the "expected one of"
    /// message lists them.
    fn known_keys(self) -> &'static [&'static str] {
        match self {
            Self::Tool => &["allow", "protect", "tools"],
            Self::Shell => &["allow", "protect", "shell_write_binaries"],
        }
    }
}

/// The default `protect` list, used when the key is absent.
pub const DEFAULT_PROTECT: [&str; 5] = ["tests/", "test_*", "*_test.*", "spec/", "conftest.py"];

/// The condition on a tool's input that means "this call writes". With no
/// `write_when`, every `path_keys` hit is a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteWhen {
    /// The input key to read, e.g. `operation`.
    pub key: String,
    /// The values of that key that mean the call writes.
    pub any_of: Vec<String>,
}

/// One `tools[]` rule: which tool it gates, which input keys name write targets,
/// and when the call counts as a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRule {
    /// The tool name to gate, exact or glob. A tool name has no path components,
    /// so this pattern must not contain a `/`.
    pub tool_match: Pattern,
    /// The input keys whose string values are write targets.
    pub path_keys: Vec<String>,
    /// When present, the call is a write only if this condition holds.
    pub write_when: Option<WriteWhen>,
}

/// The whole usable policy, after parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyConfig {
    /// Patterns a write target is refused for. Never empty.
    pub protect: Vec<Pattern>,
    /// Patterns checked before `protect`; a match here is never refused.
    pub allow: Vec<Pattern>,
    /// Tool rules. Read by `murmur-hook-protect-tool` only; a tool matching no
    /// rule is not gated at all.
    pub tools: Vec<ToolRule>,
    /// Extra binary basenames whose non-flag argv entries are write targets. Read
    /// by `murmur-hook-protect-shell` only.
    pub shell_write_binaries: Vec<String>,
}

/// Why the config is unusable. `key` is the offending top-level key (or
/// [`ARTIFACT_CONFIG_ENV`] when the whole value is at fault); `detail` is the
/// operator-facing sentence, which always names that key and, for a bad pattern,
/// its index and text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// The top-level config key the fault belongs to.
    pub key: String,
    /// The full message, naming the key and enough of the value to fix it.
    pub detail: String,
}

impl ConfigError {
    fn new(key: &str, detail: String) -> Self {
        Self {
            key: key.to_string(),
            detail,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

/// The JSON type of a value, phrased to drop into "found ...".
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

impl PolicyConfig {
    /// The policy an artifact with no `config:` block runs: the default protected
    /// paths, nothing allowed back, and — for the tool side — the two default
    /// rules for this repo's own editing tools.
    pub fn defaults(side: PolicySide) -> Self {
        // DEFAULT_PROTECT and the default rules are compiled from literals that
        // are covered by `default_patterns_all_compile`; a pattern that failed to
        // compile is dropped rather than panicking, which the test forbids.
        let protect = DEFAULT_PROTECT
            .iter()
            .filter_map(|p| Pattern::parse(p).ok())
            .collect();
        let tools = match side {
            PolicySide::Tool => default_tool_rules(),
            PolicySide::Shell => Vec::new(),
        };
        Self {
            protect,
            allow: Vec::new(),
            tools,
            shell_write_binaries: Vec::new(),
        }
    }
}

/// The two default `tools` rules: this repo's editor tool gated on its two
/// writing operations, and its scaffolding tool gated on the directory it creates.
fn default_tool_rules() -> Vec<ToolRule> {
    let mut rules = Vec::new();
    if let Ok(tool_match) = Pattern::parse("murmur-tool-editor") {
        rules.push(ToolRule {
            tool_match,
            path_keys: vec!["path".to_string()],
            write_when: Some(WriteWhen {
                key: "operation".to_string(),
                any_of: vec!["write_file".to_string(), "replace_in_file".to_string()],
            }),
        });
    }
    if let Ok(tool_match) = Pattern::parse("murmur-tool-create") {
        rules.push(ToolRule {
            tool_match,
            path_keys: vec!["name".to_string(), "path".to_string()],
            write_when: None,
        });
    }
    rules
}

/// Parse the raw `MURMUR_ARTIFACT_CONFIG` value for one side of the policy.
///
/// `None` (and a blank value) means the artifact's manifest entry carried no
/// `config:` block, which is the defaults.
pub fn parse_config(side: PolicySide, raw: Option<&str>) -> Result<PolicyConfig, ConfigError> {
    let Some(raw) = raw else {
        return Ok(PolicyConfig::defaults(side));
    };
    if raw.trim().is_empty() {
        return Ok(PolicyConfig::defaults(side));
    }

    let value: Value = serde_json::from_str(raw).map_err(|e| {
        ConfigError::new(
            ARTIFACT_CONFIG_ENV,
            format!("{ARTIFACT_CONFIG_ENV} is not valid JSON: {e}"),
        )
    })?;
    let Value::Object(map) = value else {
        return Err(ConfigError::new(
            ARTIFACT_CONFIG_ENV,
            format!(
                "{ARTIFACT_CONFIG_ENV} must be a JSON object, found {}",
                json_type(&value)
            ),
        ));
    };

    let known = side.known_keys();
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            return Err(ConfigError::new(
                key,
                format!("unknown key '{key}'; expected one of: {}", known.join(", ")),
            ));
        }
    }

    let protect = match pattern_list(&map, "protect")? {
        Some(patterns) => {
            if patterns.is_empty() {
                return Err(ConfigError::new(
                    "protect",
                    "key 'protect' must list at least one pattern; an empty 'protect' list \
                     protects nothing"
                        .to_string(),
                ));
            }
            patterns
        }
        None => PolicyConfig::defaults(side).protect,
    };
    let allow = pattern_list(&map, "allow")?.unwrap_or_default();

    let tools = match map.get("tools") {
        Some(value) => parse_tool_rules(value)?,
        None => PolicyConfig::defaults(side).tools,
    };
    let shell_write_binaries = string_list(&map, "shell_write_binaries")?.unwrap_or_default();
    for (index, binary) in shell_write_binaries.iter().enumerate() {
        if binary.trim().is_empty() {
            return Err(ConfigError::new(
                "shell_write_binaries",
                format!("key 'shell_write_binaries' entry {index} must not be empty"),
            ));
        }
    }

    Ok(PolicyConfig {
        protect,
        allow,
        tools,
        shell_write_binaries,
    })
}

/// A list-of-strings value, or `None` when the key is absent.
fn string_list(map: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, ConfigError> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let Value::Array(items) = value else {
        return Err(ConfigError::new(
            key,
            format!(
                "key '{key}' must be a list of strings, found {}",
                json_type(value)
            ),
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Value::String(text) = item else {
            return Err(ConfigError::new(
                key,
                format!(
                    "key '{key}' entry {index} must be a string, found {}",
                    json_type(item)
                ),
            ));
        };
        out.push(text.clone());
    }
    Ok(Some(out))
}

/// A list-of-globs value, compiled. `None` when the key is absent.
fn pattern_list(map: &Map<String, Value>, key: &str) -> Result<Option<Vec<Pattern>>, ConfigError> {
    let Some(sources) = string_list(map, key)? else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        match Pattern::parse(source) {
            Ok(pattern) => out.push(pattern),
            Err(e) => {
                return Err(ConfigError::new(
                    key,
                    format!("key '{key}' pattern {index} '{source}' is malformed: {e}"),
                ))
            }
        }
    }
    Ok(Some(out))
}

/// Parse the `tools` list into rules.
fn parse_tool_rules(value: &Value) -> Result<Vec<ToolRule>, ConfigError> {
    let Value::Array(entries) = value else {
        return Err(ConfigError::new(
            "tools",
            format!(
                "key 'tools' must be a list of rule objects, found {}",
                json_type(value)
            ),
        ));
    };
    let mut rules = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        rules.push(parse_tool_rule(index, entry)?);
    }
    Ok(rules)
}

const TOOL_RULE_KEYS: [&str; 3] = ["match", "path_keys", "write_when"];
const WRITE_WHEN_KEYS: [&str; 2] = ["any_of", "key"];

fn parse_tool_rule(index: usize, entry: &Value) -> Result<ToolRule, ConfigError> {
    let bad = |detail: String| ConfigError::new("tools", detail);

    let Value::Object(map) = entry else {
        return Err(bad(format!(
            "key 'tools' entry {index} must be an object, found {}",
            json_type(entry)
        )));
    };
    for key in map.keys() {
        if !TOOL_RULE_KEYS.contains(&key.as_str()) {
            return Err(bad(format!(
                "key 'tools' entry {index}: unknown key '{key}'; expected one of: {}",
                TOOL_RULE_KEYS.join(", ")
            )));
        }
    }

    let Some(match_value) = map.get("match") else {
        return Err(bad(format!(
            "key 'tools' entry {index} is missing required key 'match'"
        )));
    };
    let Value::String(match_source) = match_value else {
        return Err(bad(format!(
            "key 'tools' entry {index} key 'match' must be a string, found {}",
            json_type(match_value)
        )));
    };
    if match_source.contains('/') {
        return Err(bad(format!(
            "key 'tools' entry {index} key 'match' '{match_source}' must not contain '/'; a tool \
             name has no path components"
        )));
    }
    let tool_match = Pattern::parse(match_source).map_err(|e| {
        bad(format!(
            "key 'tools' entry {index} key 'match' '{match_source}' is malformed: {e}"
        ))
    })?;

    let Some(path_keys_value) = map.get("path_keys") else {
        return Err(bad(format!(
            "key 'tools' entry {index} is missing required key 'path_keys'"
        )));
    };
    let path_keys = nested_string_list(path_keys_value, index, "path_keys")?;
    if path_keys.is_empty() {
        return Err(bad(format!(
            "key 'tools' entry {index} key 'path_keys' must list at least one input key; a rule \
             naming no path key can gate nothing"
        )));
    }

    let write_when = match map.get("write_when") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_write_when(index, value)?),
    };

    Ok(ToolRule {
        tool_match,
        path_keys,
        write_when,
    })
}

fn parse_write_when(index: usize, value: &Value) -> Result<WriteWhen, ConfigError> {
    let bad = |detail: String| ConfigError::new("tools", detail);

    let Value::Object(map) = value else {
        return Err(bad(format!(
            "key 'tools' entry {index} key 'write_when' must be an object, found {}",
            json_type(value)
        )));
    };
    for key in map.keys() {
        if !WRITE_WHEN_KEYS.contains(&key.as_str()) {
            return Err(bad(format!(
                "key 'tools' entry {index} key 'write_when': unknown key '{key}'; expected one \
                 of: {}",
                WRITE_WHEN_KEYS.join(", ")
            )));
        }
    }
    let Some(Value::String(key)) = map.get("key") else {
        return Err(bad(match map.get("key") {
            None => {
                format!("key 'tools' entry {index} key 'write_when' is missing required key 'key'")
            }
            Some(other) => format!(
                "key 'tools' entry {index} key 'write_when' key 'key' must be a string, found {}",
                json_type(other)
            ),
        }));
    };
    let Some(any_of_value) = map.get("any_of") else {
        return Err(bad(format!(
            "key 'tools' entry {index} key 'write_when' is missing required key 'any_of'"
        )));
    };
    let any_of = nested_string_list(any_of_value, index, "write_when.any_of")?;
    if any_of.is_empty() {
        return Err(bad(format!(
            "key 'tools' entry {index} key 'write_when.any_of' must list at least one value; an \
             empty list means no call ever writes"
        )));
    }

    Ok(WriteWhen {
        key: key.clone(),
        any_of,
    })
}

/// A list of strings nested inside a `tools` rule, reported against the `tools`
/// key with the rule index and the nested key name.
fn nested_string_list(
    value: &Value,
    index: usize,
    label: &str,
) -> Result<Vec<String>, ConfigError> {
    let bad = |detail: String| ConfigError::new("tools", detail);

    let Value::Array(items) = value else {
        return Err(bad(format!(
            "key 'tools' entry {index} key '{label}' must be a list of strings, found {}",
            json_type(value)
        )));
    };
    let mut out = Vec::with_capacity(items.len());
    for (item_index, item) in items.iter().enumerate() {
        let Value::String(text) = item else {
            return Err(bad(format!(
                "key 'tools' entry {index} key '{label}' entry {item_index} must be a string, \
                 found {}",
                json_type(item)
            )));
        };
        out.push(text.clone());
    }
    Ok(out)
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

    fn tool(raw: &str) -> Result<PolicyConfig, ConfigError> {
        parse_config(PolicySide::Tool, Some(raw))
    }

    fn shell(raw: &str) -> Result<PolicyConfig, ConfigError> {
        parse_config(PolicySide::Shell, Some(raw))
    }

    #[test]
    fn default_patterns_all_compile() {
        for source in DEFAULT_PROTECT {
            assert!(Pattern::parse(source).is_ok(), "{source}");
        }
        let defaults = PolicyConfig::defaults(PolicySide::Tool);
        assert_eq!(defaults.protect.len(), DEFAULT_PROTECT.len());
        assert_eq!(defaults.tools.len(), 2);
        assert!(defaults.allow.is_empty());
        assert!(PolicyConfig::defaults(PolicySide::Shell).tools.is_empty());
    }

    #[test]
    fn an_absent_variable_is_the_defaults_not_an_empty_object() {
        let absent = parse_config(PolicySide::Tool, None).unwrap();
        assert_eq!(absent, PolicyConfig::defaults(PolicySide::Tool));
        assert_eq!(parse_config(PolicySide::Tool, Some("   ")).unwrap(), absent);
        assert_eq!(tool("{}").unwrap(), absent);
    }

    #[test]
    fn default_tool_rules_gate_the_editor_only_on_its_writing_operations() {
        let defaults = PolicyConfig::defaults(PolicySide::Tool);
        let editor = &defaults.tools[0];
        assert!(editor.tool_match.matches_name("murmur-tool-editor"));
        assert_eq!(editor.path_keys, vec!["path".to_string()]);
        let write_when = editor.write_when.as_ref().unwrap();
        assert_eq!(write_when.key, "operation");
        assert_eq!(write_when.any_of, vec!["write_file", "replace_in_file"]);

        let create = &defaults.tools[1];
        assert!(create.tool_match.matches_name("murmur-tool-create"));
        assert_eq!(
            create.path_keys,
            vec!["name".to_string(), "path".to_string()]
        );
        assert!(create.write_when.is_none());
    }

    #[test]
    fn protect_and_allow_are_read_and_compiled() {
        let config = tool(r#"{"protect":["a/","b*"],"allow":["a/keep.py"]}"#).unwrap();
        assert_eq!(config.protect.len(), 2);
        assert_eq!(config.protect[0].source(), "a/");
        assert_eq!(config.allow.len(), 1);
        // `tools` untouched means the defaults, not an empty list.
        assert_eq!(config.tools.len(), 2);
    }

    #[test]
    fn empty_protect_is_a_config_error_naming_the_key() {
        let err = tool(r#"{"protect": []}"#).unwrap_err();
        assert_eq!(err.key, "protect");
        assert!(err.to_string().contains("'protect'"), "{err}");
        assert!(err.to_string().contains("at least one pattern"), "{err}");
    }

    #[test]
    fn a_non_string_pattern_entry_names_the_key_and_the_index() {
        let err = tool(r#"{"protect": ["tests/", 7]}"#).unwrap_err();
        assert_eq!(err.key, "protect");
        assert!(err.to_string().contains("'protect'"), "{err}");
        assert!(err.to_string().contains("entry 1"), "{err}");
        assert!(err.to_string().contains("found a number"), "{err}");
    }

    #[test]
    fn a_malformed_pattern_names_the_key_the_index_and_the_pattern() {
        let err = tool(r#"{"protect": ["a**b"]}"#).unwrap_err();
        assert_eq!(err.key, "protect");
        assert!(err.to_string().contains("'protect'"), "{err}");
        assert!(err.to_string().contains("pattern 0"), "{err}");
        assert!(err.to_string().contains("a**b"), "{err}");

        let err = tool(r#"{"protect": ["tests/", ""]}"#).unwrap_err();
        assert!(err.to_string().contains("pattern 1"), "{err}");
        assert!(err.to_string().contains("must not be empty"), "{err}");

        let err = tool("{\"protect\": [\"a\\u0000b\"]}").unwrap_err();
        assert!(err.to_string().contains("NUL"), "{err}");
    }

    #[test]
    fn an_unknown_top_level_key_names_the_offending_key() {
        let err = tool(r#"{"protet": ["tests/"]}"#).unwrap_err();
        assert_eq!(err.key, "protet");
        assert!(err.to_string().contains("unknown key 'protet'"), "{err}");
        assert!(err.to_string().contains("allow, protect, tools"), "{err}");
    }

    #[test]
    fn each_side_rejects_the_other_sides_key() {
        let err = tool(r#"{"shell_write_binaries": ["python3"]}"#).unwrap_err();
        assert_eq!(err.key, "shell_write_binaries");
        let err = shell(r#"{"tools": []}"#).unwrap_err();
        assert_eq!(err.key, "tools");
        assert!(shell(r#"{"shell_write_binaries":["python3"]}"#).is_ok());
    }

    #[test]
    fn a_scalar_where_a_list_belongs_names_the_key_and_the_type() {
        let err = tool(r#"{"protect": "tests/"}"#).unwrap_err();
        assert_eq!(err.key, "protect");
        assert!(err.to_string().contains("'protect'"), "{err}");
        assert!(err.to_string().contains("found a string"), "{err}");
    }

    #[test]
    fn invalid_json_and_a_non_object_top_level_are_config_errors() {
        let err = tool("{not json").unwrap_err();
        assert_eq!(err.key, ARTIFACT_CONFIG_ENV);
        assert!(err.to_string().contains("not valid JSON"), "{err}");

        let err = tool("[1,2]").unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"), "{err}");
        assert!(err.to_string().contains("found a list"), "{err}");
    }

    #[test]
    fn tool_rules_are_parsed_and_their_faults_named() {
        let config = tool(
            r#"{"tools":[{"match":"my-editor","path_keys":["file"],
                 "write_when":{"key":"op","any_of":["put"]}}]}"#,
        )
        .unwrap();
        assert_eq!(config.tools.len(), 1);
        assert!(config.tools[0].tool_match.matches_name("my-editor"));

        let err = tool(r#"{"tools":["my-editor"]}"#).unwrap_err();
        assert!(
            err.to_string().contains("entry 0 must be an object"),
            "{err}"
        );

        let err = tool(r#"{"tools":[{"path_keys":["p"]}]}"#).unwrap_err();
        assert!(
            err.to_string().contains("missing required key 'match'"),
            "{err}"
        );

        let err = tool(r#"{"tools":[{"match":"x"}]}"#).unwrap_err();
        assert!(
            err.to_string().contains("missing required key 'path_keys'"),
            "{err}"
        );

        let err = tool(r#"{"tools":[{"match":"x","path_keys":[]}]}"#).unwrap_err();
        assert!(err.to_string().contains("at least one input key"), "{err}");

        let err = tool(r#"{"tools":[{"match":"x","path_keys":"p"}]}"#).unwrap_err();
        assert!(
            err.to_string().contains("must be a list of strings"),
            "{err}"
        );

        let err = tool(r#"{"tools":[{"match":"a/b","path_keys":["p"]}]}"#).unwrap_err();
        assert!(err.to_string().contains("must not contain '/'"), "{err}");

        let err = tool(r#"{"tools":[{"match":"x","path_keys":["p"],"pathkeys":[]}]}"#).unwrap_err();
        assert!(err.to_string().contains("unknown key 'pathkeys'"), "{err}");

        let err = tool(r#"{"tools":[{"match":"x","path_keys":["p"],"write_when":{"key":"o"}}]}"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("missing required key 'any_of'"),
            "{err}"
        );

        let err = tool(
            r#"{"tools":[{"match":"x","path_keys":["p"],"write_when":{"key":"o","any_of":[]}}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("at least one value"), "{err}");

        let err =
            tool(r#"{"tools":[{"match":"x","path_keys":["p"],"write_when":7}]}"#).unwrap_err();
        assert!(err.to_string().contains("must be an object"), "{err}");
    }

    #[test]
    fn shell_write_binaries_entries_must_be_non_empty_strings() {
        let config = shell(r#"{"shell_write_binaries":["python3","my-writer"]}"#).unwrap();
        assert_eq!(config.shell_write_binaries.len(), 2);

        let err = shell(r#"{"shell_write_binaries":[""]}"#).unwrap_err();
        assert!(
            err.to_string().contains("entry 0 must not be empty"),
            "{err}"
        );

        let err = shell(r#"{"shell_write_binaries":[3]}"#).unwrap_err();
        assert!(
            err.to_string().contains("entry 0 must be a string"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_tools_list_is_accepted_and_gates_nothing() {
        let config = tool(r#"{"tools":[]}"#).unwrap();
        assert!(config.tools.is_empty());
    }
}
