//! The hook's `config:` block: parsing and validation.
//!
//! The runtime delivers the block as compact JSON in `MURMUR_ARTIFACT_CONFIG`. A config the
//! guard cannot fully read is a [`ConfigError`] naming the exact field path; the caller turns
//! it into a refusal of every command.

use std::collections::BTreeSet;
use std::fmt;

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};

use crate::words::{config_path, PathKey};

/// The environment variable the runtime puts the `config:` block in.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// The compiled-size limit for every configured regular expression.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// What to do with a command the guard cannot resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnUnclear {
    #[default]
    Deny,
    Allow,
}

/// A config field the guard cannot read: where, and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The field path, such as `rules[2].all_flags[1][0]`; `config` for the block itself.
    pub path: String,
    pub problem: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.problem)
    }
}

impl std::error::Error for ConfigError {}

/// A program name: an exact basename, or a prefix when written with a trailing `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramName {
    /// The entry as written in the config.
    pub written: String,
    /// True when the entry ends in `*`.
    pub prefix: bool,
}

impl ProgramName {
    pub fn matches(&self, program: &str) -> bool {
        if self.prefix {
            let stem = self.written.strip_suffix('*').unwrap_or(&self.written);
            program.starts_with(stem)
        } else {
            program == self.written
        }
    }
}

/// A flag spec: present when any of its spellings is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagSpec {
    pub spellings: Vec<String>,
}

impl FlagSpec {
    pub fn present(&self, flags: &BTreeSet<String>) -> bool {
        self.spellings.iter().any(|s| flags.contains(s))
    }
}

/// A rule: a program plus argument conditions. A condition list that is empty was not stated.
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: Option<String>,
    pub program: Vec<ProgramName>,
    pub reason: String,
    pub subcommand: Vec<String>,
    pub all_flags: Vec<FlagSpec>,
    pub any_flag: Vec<FlagSpec>,
    pub path_is: Vec<PathKey>,
    pub path_under: Vec<PathKey>,
    pub any_arg: Vec<Regex>,
    pub unless: Vec<FlagSpec>,
}

/// A regular expression matched against whole script texts.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub regex: Regex,
    pub reason: String,
}

/// The parsed config. [`Config::default`] is the empty config, which allows everything.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub on_unclear: OnUnclear,
    pub deny_programs: Vec<ProgramName>,
    pub rules: Vec<Rule>,
    pub patterns: Vec<Pattern>,
}

impl Config {
    /// True when the config states no `deny_programs`, `rules` or `patterns`: nothing can
    /// apply, so nothing is unclear either.
    pub fn is_empty(&self) -> bool {
        self.deny_programs.is_empty() && self.rules.is_empty() && self.patterns.is_empty()
    }
}

fn err(path: impl Into<String>, problem: impl Into<String>) -> ConfigError {
    ConfigError {
        path: path.into(),
        problem: problem.into(),
    }
}

const TOP_KEYS: &[&str] = &["on_unclear", "deny_programs", "rules", "patterns"];
const RULE_KEYS: &[&str] = &[
    "program",
    "reason",
    "name",
    "subcommand",
    "all_flags",
    "any_flag",
    "path_is",
    "path_under",
    "any_arg",
    "unless",
];
const PATTERN_KEYS: &[&str] = &["pattern", "reason"];

/// Parses the `config:` block. `None` (the variable is unset) is the empty config.
pub fn parse_config(raw: Option<&str>) -> Result<Config, ConfigError> {
    let Some(raw) = raw else {
        return Ok(Config::default());
    };
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| err("config", format!("not JSON ({})", one_line(&e.to_string()))))?;
    let Value::Object(map) = value else {
        return Err(err("config", "must be an object"));
    };
    check_keys(&map, TOP_KEYS, "")?;
    let mut config = Config::default();
    if let Some(value) = map.get("on_unclear") {
        config.on_unclear = match value.as_str() {
            Some("deny") => OnUnclear::Deny,
            Some("allow") => OnUnclear::Allow,
            _ => return Err(err("on_unclear", "must be \"deny\" or \"allow\"")),
        };
    }
    if let Some(value) = map.get("deny_programs") {
        config.deny_programs = program_list(value, "deny_programs", false)?;
    }
    if let Some(value) = map.get("rules") {
        for (i, rule) in list(value, "rules")?.iter().enumerate() {
            config.rules.push(parse_rule(rule, &format!("rules[{i}]"))?);
        }
    }
    if let Some(value) = map.get("patterns") {
        for (i, pattern) in list(value, "patterns")?.iter().enumerate() {
            config
                .patterns
                .push(parse_pattern(pattern, &format!("patterns[{i}]"))?);
        }
    }
    Ok(config)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Refuses the first key of `map` that is not in `known`. `prefix` is the path of `map`.
fn check_keys(map: &Map<String, Value>, known: &[&str], prefix: &str) -> Result<(), ConfigError> {
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            return Err(err(path, "unknown key"));
        }
    }
    Ok(())
}

/// A non-empty list at `path`.
fn list<'v>(value: &'v Value, path: &str) -> Result<&'v Vec<Value>, ConfigError> {
    let Value::Array(items) = value else {
        return Err(err(path, "must be a list"));
    };
    if items.is_empty() {
        return Err(err(path, "must not be empty"));
    }
    Ok(items)
}

fn non_empty_string(value: &Value, path: &str) -> Result<String, ConfigError> {
    match value {
        Value::String(s) if s.is_empty() => Err(err(path, "must not be empty")),
        Value::String(s) => Ok(s.clone()),
        _ => Err(err(path, "must be a string")),
    }
}

/// A list of program names, or (when `single_allowed`) one name.
fn program_list(
    value: &Value,
    path: &str,
    single_allowed: bool,
) -> Result<Vec<ProgramName>, ConfigError> {
    if single_allowed {
        if let Value::String(_) = value {
            return Ok(vec![program_name(value, path)?]);
        }
        if !value.is_array() {
            return Err(err(path, "must be a program name or a list of them"));
        }
    }
    list(value, path)?
        .iter()
        .enumerate()
        .map(|(i, v)| program_name(v, &format!("{path}[{i}]")))
        .collect()
}

fn program_name(value: &Value, path: &str) -> Result<ProgramName, ConfigError> {
    let text = non_empty_string(value, path)?;
    if text.contains('/') {
        return Err(err(
            path,
            "a program name is a basename and cannot contain `/`",
        ));
    }
    let stem = text.strip_suffix('*').unwrap_or(&text);
    if stem.contains('*') {
        return Err(err(
            path,
            "`*` is allowed only at the end of a program name",
        ));
    }
    if stem.is_empty() {
        return Err(err(
            path,
            "a prefix needs at least one character before `*`",
        ));
    }
    Ok(ProgramName {
        prefix: text.ends_with('*'),
        written: text,
    })
}

/// A non-empty list of non-empty strings, or one string.
fn string_or_list(value: &Value, path: &str) -> Result<Vec<String>, ConfigError> {
    match value {
        Value::String(_) => Ok(vec![non_empty_string(value, path)?]),
        Value::Array(_) => list(value, path)?
            .iter()
            .enumerate()
            .map(|(i, v)| non_empty_string(v, &format!("{path}[{i}]")))
            .collect(),
        _ => Err(err(path, "must be a string or a list of strings")),
    }
}

fn flag_specs(value: &Value, path: &str) -> Result<Vec<FlagSpec>, ConfigError> {
    list(value, path)?
        .iter()
        .enumerate()
        .map(|(i, v)| flag_spec(v, &format!("{path}[{i}]")))
        .collect()
}

fn flag_spec(value: &Value, path: &str) -> Result<FlagSpec, ConfigError> {
    let spellings = match value {
        Value::String(_) => vec![(path.to_string(), value)],
        Value::Array(_) => list(value, path)?
            .iter()
            .enumerate()
            .map(|(i, v)| (format!("{path}[{i}]"), v))
            .collect(),
        _ => return Err(err(path, "must be a flag or a list of flag spellings")),
    };
    let mut out = Vec::new();
    for (spelling_path, v) in spellings {
        let text = non_empty_string(v, &spelling_path)?;
        if !text.starts_with('-') || text.chars().count() < 2 {
            return Err(err(
                spelling_path,
                "a flag spelling starts with `-` and has at least two characters",
            ));
        }
        out.push(text);
    }
    Ok(FlagSpec { spellings: out })
}

fn paths(value: &Value, path: &str) -> Result<Vec<PathKey>, ConfigError> {
    list(value, path)?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let item_path = format!("{path}[{i}]");
            let text = non_empty_string(v, &item_path)?;
            config_path(&text).map_err(|problem| err(item_path, problem))
        })
        .collect()
}

fn regex(value: &Value, path: &str) -> Result<Regex, ConfigError> {
    let text = non_empty_string(value, path)?;
    RegexBuilder::new(&text)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| {
            err(
                path,
                format!(
                    "not a valid regular expression ({})",
                    one_line(&e.to_string())
                ),
            )
        })
}

fn regexes(value: &Value, path: &str) -> Result<Vec<Regex>, ConfigError> {
    list(value, path)?
        .iter()
        .enumerate()
        .map(|(i, v)| regex(v, &format!("{path}[{i}]")))
        .collect()
}

fn parse_rule(value: &Value, path: &str) -> Result<Rule, ConfigError> {
    let Value::Object(map) = value else {
        return Err(err(path, "must be an object"));
    };
    check_keys(map, RULE_KEYS, path)?;
    let field = |key: &str| format!("{path}.{key}");
    let program = match map.get("program") {
        Some(v) => program_list(v, &field("program"), true)?,
        None => return Err(err(field("program"), "missing; every rule names a program")),
    };
    let reason = match map.get("reason") {
        Some(v) => non_empty_string(v, &field("reason"))?,
        None => return Err(err(field("reason"), "missing; every rule gives a reason")),
    };
    let name = map
        .get("name")
        .map(|v| non_empty_string(v, &field("name")))
        .transpose()?;
    let get = |key: &str| map.get(key);
    Ok(Rule {
        name,
        program,
        reason,
        subcommand: match get("subcommand") {
            Some(v) => string_or_list(v, &field("subcommand"))?,
            None => Vec::new(),
        },
        all_flags: match get("all_flags") {
            Some(v) => flag_specs(v, &field("all_flags"))?,
            None => Vec::new(),
        },
        any_flag: match get("any_flag") {
            Some(v) => flag_specs(v, &field("any_flag"))?,
            None => Vec::new(),
        },
        path_is: match get("path_is") {
            Some(v) => paths(v, &field("path_is"))?,
            None => Vec::new(),
        },
        path_under: match get("path_under") {
            Some(v) => paths(v, &field("path_under"))?,
            None => Vec::new(),
        },
        any_arg: match get("any_arg") {
            Some(v) => regexes(v, &field("any_arg"))?,
            None => Vec::new(),
        },
        unless: match get("unless") {
            Some(v) => flag_specs(v, &field("unless"))?,
            None => Vec::new(),
        },
    })
}

fn parse_pattern(value: &Value, path: &str) -> Result<Pattern, ConfigError> {
    let Value::Object(map) = value else {
        return Err(err(path, "must be an object"));
    };
    check_keys(map, PATTERN_KEYS, path)?;
    let regex = match map.get("pattern") {
        Some(v) => regex(v, &format!("{path}.pattern"))?,
        None => return Err(err(format!("{path}.pattern"), "missing")),
    };
    let reason = match map.get("reason") {
        Some(v) => non_empty_string(v, &format!("{path}.reason"))?,
        None => return Err(err(format!("{path}.reason"), "missing")),
    };
    Ok(Pattern { regex, reason })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn path_of(raw: &str) -> String {
        parse_config(Some(raw)).unwrap_err().path
    }

    #[test]
    fn empty_configs() {
        for raw in [None, Some("{}"), Some("{\"on_unclear\":\"deny\"}")] {
            let config = parse_config(raw).unwrap();
            assert!(config.is_empty());
            assert_eq!(config.on_unclear, OnUnclear::Deny);
        }
        let config = parse_config(Some("{\"on_unclear\":\"allow\"}")).unwrap();
        assert_eq!(config.on_unclear, OnUnclear::Allow);
    }

    #[test]
    fn malformed_configs_name_the_field() {
        let cases = [
            ("not json", "config"),
            ("[]", "config"),
            ("{\"rulez\":[]}", "rulez"),
            ("{\"on_unclear\":\"maybe\"}", "on_unclear"),
            ("{\"deny_programs\":[\"/bin/rm\"]}", "deny_programs[0]"),
            ("{\"deny_programs\":[\"mk*fs\"]}", "deny_programs[0]"),
            ("{\"deny_programs\":[\"*\"]}", "deny_programs[0]"),
            ("{\"deny_programs\":[\"\"]}", "deny_programs[0]"),
            ("{\"deny_programs\":[]}", "deny_programs"),
            ("{\"deny_programs\":\"rm\"}", "deny_programs"),
            ("{\"rules\":[{\"program\":\"rm\"}]}", "rules[0].reason"),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"\"}]}",
                "rules[0].reason",
            ),
            ("{\"rules\":[{\"reason\":\"x\"}]}", "rules[0].program"),
            (
                "{\"rules\":[{\"program\":[],\"reason\":\"x\"}]}",
                "rules[0].program",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"all_flags\":[[\"r\"]]}]}",
                "rules[0].all_flags[0][0]",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"all_flags\":[\"-\"]}]}",
                "rules[0].all_flags[0]",
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
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"name\":\"\"}]}",
                "rules[0].name",
            ),
            (
                "{\"rules\":[{\"program\":\"git\",\"reason\":\"x\",\"subcommand\":[\"\"]}]}",
                "rules[0].subcommand[0]",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"path_is\":[\"~bob\"]}]}",
                "rules[0].path_is[0]",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"any_arg\":[\"[\"]}]}",
                "rules[0].any_arg[0]",
            ),
            (
                "{\"rules\":[{\"program\":\"rm\",\"reason\":\"x\",\"unless\":[[]]}]}",
                "rules[0].unless[0]",
            ),
            ("{\"rules\":[1]}", "rules[0]"),
            (
                "{\"patterns\":[{\"pattern\":\"(\",\"reason\":\"x\"}]}",
                "patterns[0].pattern",
            ),
            ("{\"patterns\":[{\"reason\":\"x\"}]}", "patterns[0].pattern"),
            ("{\"patterns\":[{\"pattern\":\"x\"}]}", "patterns[0].reason"),
        ];
        for (raw, path) in cases {
            assert_eq!(path_of(raw), path, "{raw}");
        }
    }

    #[test]
    fn problems_are_specific() {
        let e = parse_config(Some("not json")).unwrap_err();
        assert!(e.problem.starts_with("not JSON"), "{e}");
        let e = parse_config(Some("[]")).unwrap_err();
        assert_eq!(e.problem, "must be an object");
        let e = parse_config(Some("{\"rulez\":[]}")).unwrap_err();
        assert_eq!(e.to_string(), "rulez: unknown key");
    }

    #[test]
    fn oversized_regex_is_refused() {
        let raw = r#"{"patterns":[{"pattern":"\\w{1000}\\w{1000}\\w{1000}","reason":"x"}]}"#;
        assert_eq!(path_of(raw), "patterns[0].pattern");
    }

    #[test]
    fn rule_shapes() {
        let raw = r#"{"rules":[{"name":"n","program":["cargo","mkfs.*"],"reason":"r",
            "subcommand":"publish","all_flags":["-f",["-r","--recursive"]],
            "path_is":["/","~"],"path_under":["/etc"],"any_arg":["^x$"],"unless":["-n"]}]}"#;
        let config = parse_config(Some(raw)).unwrap();
        let rule = &config.rules[0];
        assert_eq!(rule.name.as_deref(), Some("n"));
        assert!(rule.program[0].matches("cargo"));
        assert!(rule.program[1].matches("mkfs.ext4"));
        assert!(!rule.program[1].matches("mkfs"));
        assert_eq!(rule.subcommand, vec!["publish"]);
        assert_eq!(rule.all_flags[1].spellings, vec!["-r", "--recursive"]);
        assert_eq!(
            rule.path_is,
            vec![PathKey::Root(vec![]), PathKey::Home(vec![])]
        );
        assert!(rule.any_flag.is_empty());
    }
}
