//! The shape a capsule is allowed to emit, declared by the operator and not by the agent.
//!
//! It lives in the `config:` block on this artifact's entry in the operator's capsule
//! manifest, where the agent cannot reach it and where a change to it is a change to a
//! file under the operator's own review. The runtime delivers the block as compact JSON;
//! what arrives here is that JSON.
//!
//! The block is optional, and that is deliberate: with no block at all, any non-empty
//! `kind` and any non-empty `stage` is accepted, so a capsule can install this tool and
//! start reporting before anyone has agreed on a vocabulary. With a block, whichever
//! vocabularies it declares are closed and a value outside them is refused.
//!
//! `require_report` is parsed and type-checked here and acted on nowhere. It is the
//! operator's declaration that a report is expected, readable by a consumer that has the
//! capsule's manifest; enforcing it — making a capsule that exits without a report failed
//! whatever its exit code — is that consumer's job, not this artifact's.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::json_type_name;

/// The only `config_version` this build understands.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

/// A loaded, validated configuration.
///
/// `None` on either vocabulary means "undeclared, therefore open": the distinction between
/// an absent list and an empty one is the whole of the permissive/closed split, so neither
/// is collapsed into a `BTreeSet::new()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportConfig {
    pub config_version: u64,
    /// Declared by the operator, read by the consumer, branched on by nothing here.
    pub require_report: bool,
    pub deliverable_kinds: Option<BTreeSet<String>>,
    pub note_stages: Option<BTreeSet<String>>,
}

impl ReportConfig {
    /// What an artifact entry with no `config:` block gets: no `require_report`, and both
    /// vocabularies open.
    pub fn permissive() -> Self {
        Self {
            config_version: SUPPORTED_CONFIG_VERSION,
            require_report: false,
            deliverable_kinds: None,
            note_stages: None,
        }
    }

    /// Whether the operator declared `kind` — vacuously true when no vocabulary was
    /// declared at all.
    pub fn accepts_kind(&self, kind: &str) -> bool {
        self.deliverable_kinds.as_ref().is_none_or(|kinds| kinds.contains(kind))
    }

    /// Whether the operator declared `stage`, on the same terms as [`Self::accepts_kind`].
    pub fn accepts_stage(&self, stage: &str) -> bool {
        self.note_stages.as_ref().is_none_or(|stages| stages.contains(stage))
    }

    /// The declared kinds as a message fragment, for the refusal that names them.
    pub fn declared_kinds(&self) -> String {
        render_set(self.deliverable_kinds.as_ref())
    }

    /// The declared stages as a message fragment.
    pub fn declared_stages(&self) -> String {
        render_set(self.note_stages.as_ref())
    }
}

fn render_set(set: Option<&BTreeSet<String>>) -> String {
    set.map(|values| values.iter().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// Parse and validate the operator config. The `Err` string is the operator-facing
/// message; the caller turns it into a `config_invalid` result.
///
/// Validation is hand-rolled rather than derived so every message names the offending key
/// and the file it lives in. A serde type error names neither, and an operator reading
/// "invalid type: string, expected a boolean" in a tool result has to guess which of four
/// keys they mistyped.
///
/// Unknown top-level keys are accepted, so an operator can annotate the block.
pub fn parse_config(text: &str) -> Result<ReportConfig, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| operator_error("the `config:` block", &format!("is not valid JSON: {e}")))?;
    let Value::Object(map) = value else {
        return Err(operator_error(
            "the `config:` block",
            &format!("must be a mapping, got {}", json_type_name(&value)),
        ));
    };

    match map.get("config_version") {
        Some(v) if v.as_u64() == Some(SUPPORTED_CONFIG_VERSION) => {}
        Some(other) => {
            return Err(operator_error(
                "config_version",
                &format!("must be {SUPPORTED_CONFIG_VERSION}, got {other}"),
            ))
        }
        None => {
            return Err(operator_error(
                "config_version",
                &format!("is required and must be {SUPPORTED_CONFIG_VERSION}"),
            ))
        }
    }

    let require_report = match map.get("require_report") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(other) => {
            return Err(operator_error(
                "require_report",
                &format!("must be a boolean, got {}", json_type_name(other)),
            ))
        }
    };

    let deliverable_kinds = optional_vocabulary(&map, "deliverables", "kinds")?;
    let note_stages = optional_vocabulary(&map, "notes_for", "stages")?;

    Ok(ReportConfig {
        config_version: SUPPORTED_CONFIG_VERSION,
        require_report,
        deliverable_kinds,
        note_stages,
    })
}

/// One optional `<block>.<key>` list of non-empty strings.
///
/// An absent block, or a present block with the key absent, leaves the vocabulary open. An
/// empty list is refused rather than read either way: as a closed set it accepts nothing,
/// which no operator means, and as an open one it would silently ignore what they wrote.
fn optional_vocabulary(
    map: &serde_json::Map<String, Value>,
    block: &str,
    key: &str,
) -> Result<Option<BTreeSet<String>>, String> {
    let path = format!("{block}.{key}");
    let inner = match map.get(block) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(inner)) => inner,
        Some(other) => {
            return Err(operator_error(
                block,
                &format!("must be a mapping, got {}", json_type_name(other)),
            ))
        }
    };

    let values = match inner.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(values)) => values,
        Some(other) => {
            return Err(operator_error(
                &path,
                &format!("must be a list of strings, got {}", json_type_name(other)),
            ))
        }
    };

    if values.is_empty() {
        return Err(operator_error(
            &path,
            "is an empty list; remove the key to accept any value, or list the values this \
             capsule may emit",
        ));
    }

    let mut set = BTreeSet::new();
    for value in values {
        match value {
            Value::String(s) if !s.trim().is_empty() => {
                set.insert(s.clone());
            }
            Value::String(_) => {
                return Err(operator_error(&path, "contains an empty string"))
            }
            other => {
                return Err(operator_error(
                    &path,
                    &format!("must contain only strings, got {}", json_type_name(other)),
                ))
            }
        }
    }
    Ok(Some(set))
}

/// Every message this module produces names the key and the file the operator has to edit.
fn operator_error(key: &str, detail: &str) -> String {
    format!(
        "{key} {detail} — fix the `config:` block on this artifact's entry in the capsule's \
         murmur.yaml"
    )
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Result<ReportConfig, String> {
        parse_config(&value.to_string())
    }

    #[test]
    fn a_full_block_declares_both_vocabularies() {
        let cfg = parse(json!({
            "config_version": 1,
            "require_report": true,
            "deliverables": { "kinds": ["diff", "dataset"] },
            "notes_for": { "stages": ["review"] }
        }))
        .expect("a valid block must load");

        assert!(cfg.require_report);
        assert!(cfg.accepts_kind("diff"));
        assert!(!cfg.accepts_kind("binary"));
        assert!(cfg.accepts_stage("review"));
        assert!(!cfg.accepts_stage("deploy"));
        assert_eq!(cfg.declared_kinds(), "dataset, diff");
    }

    #[test]
    fn each_vocabulary_is_independently_optional() {
        let cfg = parse(json!({
            "config_version": 1,
            "deliverables": { "kinds": ["diff"] }
        }))
        .expect("a block declaring one vocabulary must load");
        assert!(!cfg.accepts_kind("dataset"));
        assert!(cfg.accepts_stage("anything at all"));

        let cfg = parse(json!({
            "config_version": 1,
            "notes_for": { "stages": ["review"] }
        }))
        .expect("a block declaring the other vocabulary must load");
        assert!(cfg.accepts_kind("anything at all"));
        assert!(!cfg.accepts_stage("deploy"));
    }

    #[test]
    fn an_absent_block_accepts_anything_non_empty() {
        let cfg = ReportConfig::permissive();
        assert!(cfg.accepts_kind("whatever"));
        assert!(cfg.accepts_stage("whatever"));
        assert!(!cfg.require_report);
    }

    #[test]
    fn require_report_defaults_to_false_and_is_type_checked() {
        let cfg = parse(json!({ "config_version": 1 })).expect("a minimal block must load");
        assert!(!cfg.require_report);

        let err = parse(json!({ "config_version": 1, "require_report": "yes" }))
            .expect_err("a non-boolean require_report must be rejected");
        assert!(err.contains("require_report"), "{err}");
        assert!(err.contains("murmur.yaml"), "{err}");
    }

    #[test]
    fn a_wrong_or_missing_config_version_is_rejected() {
        for value in [json!({ "config_version": 99 }), json!({ "require_report": true })] {
            let err = parse(value).expect_err("config_version must be checked");
            assert!(err.contains("config_version"), "{err}");
            assert!(err.contains("murmur.yaml"), "{err}");
        }
    }

    #[test]
    fn a_malformed_vocabulary_is_rejected_naming_its_key() {
        let cases = [
            json!({ "config_version": 1, "deliverables": { "kinds": [] } }),
            json!({ "config_version": 1, "deliverables": { "kinds": "diff" } }),
            json!({ "config_version": 1, "deliverables": { "kinds": ["diff", 7] } }),
            json!({ "config_version": 1, "deliverables": { "kinds": ["diff", ""] } }),
        ];
        for value in cases {
            let err = parse(value).expect_err("a malformed vocabulary must be rejected");
            assert!(err.contains("deliverables.kinds"), "{err}");
            assert!(err.contains("murmur.yaml"), "{err}");
        }

        let err = parse(json!({ "config_version": 1, "notes_for": ["review"] }))
            .expect_err("a non-mapping block must be rejected");
        assert!(err.contains("notes_for"), "{err}");
    }

    #[test]
    fn unknown_top_level_keys_are_accepted_so_the_block_can_be_annotated() {
        let cfg = parse(json!({
            "config_version": 1,
            "note": "reviewed 2026-09-01 by the platform team",
            "deliverables": { "kinds": ["diff"] }
        }))
        .expect("an annotated block must load");
        assert!(cfg.accepts_kind("diff"));
    }

    #[test]
    fn malformed_json_is_rejected() {
        let err = parse_config("{ not json").expect_err("malformed JSON must be rejected");
        assert!(err.contains("`config:` block"), "{err}");
    }
}
