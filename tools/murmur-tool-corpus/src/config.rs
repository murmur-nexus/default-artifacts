//! The operator-written configuration that decides what this corpus will accept.
//!
//! The file lives beside the corpus itself, under the durable-state grant, where the
//! agent cannot reach it: an agent that could edit the schemas could append anything.
//!
//! Loading is fail-closed. A type that is not declared here cannot be appended, an
//! unsupported schema keyword is a hard error rather than an ignored constraint, and a
//! type whose id prefix would collide with a runtime prefix is refused with a message
//! naming `prefix_map`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::id::{derive_prefix, is_reserved, is_valid_explicit_prefix, RESERVED_PREFIXES};
use crate::schema::check_schema;

/// The only `config_version` this build understands.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

/// `read_recent.default` when the block is absent.
pub const DEFAULT_READ_RECENT_DEFAULT: usize = 10;
/// `read_recent.max` when the block is absent.
pub const DEFAULT_READ_RECENT_MAX: usize = 50;
/// `search.default_k` when the block is absent.
pub const DEFAULT_SEARCH_DEFAULT_K: usize = 5;
/// `search.max_k` when the block is absent.
pub const DEFAULT_SEARCH_MAX_K: usize = 25;
/// `types.<name>.schema_version` when the field is absent.
pub const DEFAULT_SCHEMA_VERSION: i64 = 1;

/// Bounds on `read_recent`. `default` applies when the caller omits `n`; `max` is the
/// ceiling `n` is clamped to, which is what keeps an unbounded read inexpressible.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReadRecentCaps {
    pub default: usize,
    pub max: usize,
}

impl Default for ReadRecentCaps {
    fn default() -> Self {
        Self { default: DEFAULT_READ_RECENT_DEFAULT, max: DEFAULT_READ_RECENT_MAX }
    }
}

/// Bounds on `search`, with the same reasoning as [`ReadRecentCaps`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SearchCaps {
    pub default_k: usize,
    pub max_k: usize,
}

impl Default for SearchCaps {
    fn default() -> Self {
        Self { default_k: DEFAULT_SEARCH_DEFAULT_K, max_k: DEFAULT_SEARCH_MAX_K }
    }
}

/// One declared record type: the schema its bodies are validated against, the schema
/// version stamped onto every record of the type, and the resolved id prefix.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeConfig {
    pub schema_version: i64,
    pub schema: Value,
    pub prefix: String,
}

/// A loaded, validated configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub config_version: u64,
    pub read_recent: ReadRecentCaps,
    pub search: SearchCaps,
    pub prefix_map: BTreeMap<String, String>,
    pub types: BTreeMap<String, TypeConfig>,
}

impl Config {
    /// The declared type named `name`, or `None` if the operator never declared it.
    pub fn type_config(&self, name: &str) -> Option<&TypeConfig> {
        self.types.get(name)
    }
}

/// The wire shape of `corpus.config.json`, before validation.
///
/// `config_version` is an `Option` so that omitting it produces this module's own message
/// rather than serde's. Unknown top-level keys are accepted so an operator can annotate
/// the file; every key this build acts on is listed here.
#[derive(Debug, Deserialize)]
struct RawConfig {
    config_version: Option<u64>,
    #[serde(default)]
    read_recent: ReadRecentCaps,
    #[serde(default)]
    search: SearchCaps,
    #[serde(default)]
    prefix_map: BTreeMap<String, String>,
    #[serde(default)]
    types: BTreeMap<String, RawType>,
}

#[derive(Debug, Deserialize)]
struct RawType {
    #[serde(default = "default_schema_version")]
    schema_version: i64,
    schema: Value,
}

fn default_schema_version() -> i64 {
    DEFAULT_SCHEMA_VERSION
}

/// Parse and validate the operator config. The `Err` string is the operator-facing
/// message; the caller turns it into a `config_invalid` result.
pub fn parse_config(text: &str) -> Result<Config, String> {
    let raw: RawConfig = serde_json::from_str(text)
        .map_err(|e| format!("corpus.config.json is not valid configuration JSON: {e}"))?;

    match raw.config_version {
        Some(SUPPORTED_CONFIG_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "config_version must be {SUPPORTED_CONFIG_VERSION}, got {other}"
            ))
        }
        None => return Err(format!("config_version is required and must be {SUPPORTED_CONFIG_VERSION}")),
    }

    for (type_name, prefix) in &raw.prefix_map {
        if !is_valid_explicit_prefix(prefix) {
            return Err(format!(
                "prefix_map.\"{type_name}\" = \"{prefix}\" must match ^[a-z][a-z0-9]{{0,7}}$"
            ));
        }
        if is_reserved(prefix) {
            return Err(format!(
                "prefix_map.\"{type_name}\" = \"{prefix}\" is a reserved runtime id prefix \
                 (reserved: {}); choose another",
                RESERVED_PREFIXES.join(", ")
            ));
        }
    }

    if raw.types.is_empty() {
        return Err(
            "types is absent or empty; declare at least one record type — the store refuses \
             an append of a type it was never configured for"
                .to_string(),
        );
    }

    let mut types = BTreeMap::new();
    for (type_name, raw_type) in raw.types {
        check_schema(&raw_type.schema)
            .map_err(|e| format!("types.\"{type_name}\".{e}"))?;

        let prefix = match raw.prefix_map.get(&type_name) {
            Some(explicit) => explicit.clone(),
            None => {
                let derived = derive_prefix(&type_name)
                    .map_err(|e| format!("types.\"{type_name}\": {e}"))?;
                if is_reserved(&derived) {
                    return Err(format!(
                        "types.\"{type_name}\" derives the reserved runtime id prefix \
                         \"{derived}\"; set prefix_map.\"{type_name}\" to an unreserved prefix"
                    ));
                }
                derived
            }
        };

        types.insert(
            type_name,
            TypeConfig { schema_version: raw_type.schema_version, schema: raw_type.schema, prefix },
        );
    }

    Ok(Config {
        config_version: SUPPORTED_CONFIG_VERSION,
        read_recent: raw.read_recent,
        search: raw.search,
        prefix_map: raw.prefix_map,
        types,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_text(value: serde_json::Value) -> String {
        value.to_string()
    }

    fn minimal() -> serde_json::Value {
        json!({
            "config_version": 1,
            "types": {
                "note": {
                    "schema_version": 1,
                    "schema": {
                        "type": "object",
                        "required": ["text"],
                        "properties": { "text": { "type": "string" } },
                        "additionalProperties": false
                    }
                }
            }
        })
    }

    #[test]
    fn a_valid_config_loads() {
        let cfg = parse_config(&config_text(json!({
            "config_version": 1,
            "read_recent": { "default": 3, "max": 7 },
            "search": { "default_k": 2, "max_k": 4 },
            "prefix_map": { "request": "rqt" },
            "types": {
                "note": { "schema_version": 2, "schema": { "type": "object" } },
                "request": { "schema": { "type": "object" } }
            }
        })))
        .expect("valid config must load");

        assert_eq!(cfg.config_version, 1);
        assert_eq!(cfg.read_recent, ReadRecentCaps { default: 3, max: 7 });
        assert_eq!(cfg.search, SearchCaps { default_k: 2, max_k: 4 });
        assert_eq!(cfg.type_config("note").unwrap().schema_version, 2);
        assert_eq!(cfg.type_config("note").unwrap().prefix, "not");
        assert_eq!(cfg.type_config("request").unwrap().prefix, "rqt");
        assert_eq!(
            cfg.type_config("request").unwrap().schema_version,
            DEFAULT_SCHEMA_VERSION
        );
    }

    #[test]
    fn caps_default_when_their_blocks_are_absent() {
        let cfg = parse_config(&config_text(minimal())).expect("valid config must load");
        assert_eq!(cfg.read_recent.default, DEFAULT_READ_RECENT_DEFAULT);
        assert_eq!(cfg.read_recent.max, DEFAULT_READ_RECENT_MAX);
        assert_eq!(cfg.search.default_k, DEFAULT_SEARCH_DEFAULT_K);
        assert_eq!(cfg.search.max_k, DEFAULT_SEARCH_MAX_K);
    }

    #[test]
    fn a_partial_caps_block_defaults_its_missing_field() {
        let cfg = parse_config(&config_text(json!({
            "config_version": 1,
            "read_recent": { "max": 7 },
            "types": { "note": { "schema": { "type": "object" } } }
        })))
        .expect("valid config must load");
        assert_eq!(cfg.read_recent.default, DEFAULT_READ_RECENT_DEFAULT);
        assert_eq!(cfg.read_recent.max, 7);
    }

    #[test]
    fn a_wrong_config_version_is_rejected() {
        let err = parse_config(&config_text(json!({
            "config_version": 2,
            "types": { "note": { "schema": { "type": "object" } } }
        })))
        .expect_err("config_version 2 must be rejected");
        assert!(err.contains("config_version"), "{err}");
    }

    #[test]
    fn a_missing_config_version_is_rejected() {
        let err = parse_config(&config_text(json!({
            "types": { "note": { "schema": { "type": "object" } } }
        })))
        .expect_err("a missing config_version must be rejected");
        assert!(err.contains("config_version"), "{err}");
    }

    #[test]
    fn absent_or_empty_types_is_rejected() {
        for value in [json!({ "config_version": 1 }), json!({ "config_version": 1, "types": {} })] {
            let err = parse_config(&config_text(value)).expect_err("types must be non-empty");
            assert!(err.contains("types"), "{err}");
        }
    }

    #[test]
    fn a_type_deriving_a_reserved_prefix_is_rejected_and_points_at_prefix_map() {
        let err = parse_config(&config_text(json!({
            "config_version": 1,
            "types": { "session": { "schema": { "type": "object" } } }
        })))
        .expect_err("a reserved derived prefix must be rejected");
        assert!(err.contains("session"), "message must name the type: {err}");
        assert!(err.contains("prefix_map"), "message must point at prefix_map: {err}");
        assert!(err.contains("ses"), "message must name the prefix: {err}");
    }

    #[test]
    fn a_prefix_map_override_rescues_a_reserved_collision() {
        let cfg = parse_config(&config_text(json!({
            "config_version": 1,
            "prefix_map": { "session": "sess" },
            "types": { "session": { "schema": { "type": "object" } } }
        })))
        .expect("an explicit prefix must rescue the collision");
        assert_eq!(cfg.type_config("session").unwrap().prefix, "sess");
    }

    #[test]
    fn a_reserved_prefix_map_value_is_rejected() {
        let err = parse_config(&config_text(json!({
            "config_version": 1,
            "prefix_map": { "note": "ctx" },
            "types": { "note": { "schema": { "type": "object" } } }
        })))
        .expect_err("a reserved override must be rejected");
        assert!(err.contains("prefix_map"), "{err}");
        assert!(err.contains("ctx"), "{err}");
    }

    #[test]
    fn a_malformed_prefix_map_value_is_rejected() {
        for bad in ["Ab", "1ab", "abcdefghi", "", "a_b"] {
            let err = parse_config(&config_text(json!({
                "config_version": 1,
                "prefix_map": { "note": bad },
                "types": { "note": { "schema": { "type": "object" } } }
            })))
            .expect_err("a malformed override must be rejected");
            assert!(err.contains("prefix_map"), "{err}");
        }
    }

    #[test]
    fn malformed_json_is_rejected() {
        let err = parse_config("{ not json").expect_err("malformed JSON must be rejected");
        assert!(err.contains("corpus.config.json"), "{err}");
    }

    #[test]
    fn an_unsupported_schema_keyword_is_rejected_naming_the_type_and_keyword() {
        let err = parse_config(&config_text(json!({
            "config_version": 1,
            "types": {
                "note": { "schema": { "type": "object", "properties": { "t": { "pattern": "^a" } } } }
            }
        })))
        .expect_err("an unsupported keyword must be rejected");
        assert!(err.contains("note"), "message must name the type: {err}");
        assert!(err.contains("pattern"), "message must name the keyword: {err}");
    }

    #[test]
    fn two_types_may_derive_the_same_prefix() {
        let cfg = parse_config(&config_text(json!({
            "config_version": 1,
            "types": {
                "note": { "schema": { "type": "object" } },
                "notice": { "schema": { "type": "object" } }
            }
        })))
        .expect("a shared derived prefix is allowed");
        assert_eq!(cfg.type_config("note").unwrap().prefix, "not");
        assert_eq!(cfg.type_config("notice").unwrap().prefix, "not");
    }
}
