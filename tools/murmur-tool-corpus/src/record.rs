//! The record written to the corpus, and the text a retrieval layer is allowed to see.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One line of `corpus.jsonl`.
///
/// Field order here is the on-disk key order — `serde_json` emits struct fields in
/// declaration order — so a human reading the file sees identity, then provenance, then
/// content. `id`, `schema_version` and `created_at` are assigned by the store, never by
/// the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    /// The operator-declared type tag. Opaque to this crate: nothing here branches on its
    /// value. `type` is a Rust keyword, hence the rename.
    #[serde(rename = "type")]
    pub type_tag: String,
    /// The version of the type's schema the body was validated against, copied from
    /// `types.<name>.schema_version` at append time. Not the record-format version and
    /// not the config-file version.
    pub schema_version: i64,
    /// RFC 3339 UTC, millisecond precision.
    pub created_at: String,
    /// Caller-supplied idempotency key, unique per `(type, external_id)`. Absent means an
    /// absent key on disk, not a `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// The id of the record this one withdraws. A withdrawal is itself an appended
    /// record, which is what keeps "append is the only write" literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdraws: Option<String>,
    pub body: Value,
}

/// The text a retrieval layer may consume: the type tag, the `external_id` when present,
/// and every string leaf in `body`, recursively.
///
/// Object keys are visited in sorted order and array elements in index order, so the
/// output is deterministic for a given record. Object **keys** are never returned — a key
/// is schema, not content — nor are numbers, booleans, nulls, or anything derived from
/// `id`, `created_at` or `schema_version`.
///
/// This is the seam between retrieval versions. v1 term-matches over this output; a
/// future embedding-based v2 consumes the identical output. Its signature and ordering
/// are the contract, so both versions index exactly the same text.
pub fn searchable_text(record: &Record) -> Vec<String> {
    let mut out = vec![record.type_tag.clone()];
    if let Some(external_id) = &record.external_id {
        out.push(external_id.clone());
    }
    collect_strings(&record.body, &mut out);
    out
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        Value::Object(map) => {
            // serde_json's Map is sorted by key, so iteration order is already the sorted
            // order this function promises.
            for (_key, sub) in map {
                collect_strings(sub, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The current wall-clock instant as RFC 3339 UTC with millisecond precision.
///
/// `SystemTime::now()` resolves through `wasi:clocks/wall-clock` in a `wasm32-wasip2`
/// guest, so this works identically on the host and in the component. Formatted by hand;
/// a date-time crate would be a dependency bought for one `format!`.
pub fn now_rfc3339_millis() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339_millis(since_epoch.as_millis() as i64)
}

/// RFC 3339 UTC rendering of a Unix millisecond timestamp.
pub fn format_rfc3339_millis(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let seconds_of_day = ms_of_day / 1000;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
        ms_of_day % 1000
    )
}

/// Days since the Unix epoch to a proleptic Gregorian `(year, month, day)`, via Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(external_id: Option<&str>, withdraws: Option<&str>, body: Value) -> Record {
        Record {
            id: "not_0192a1b2c3d47000800000000000000a".to_string(),
            type_tag: "note".to_string(),
            schema_version: 1,
            created_at: "2026-08-25T12:00:00.000Z".to_string(),
            external_id: external_id.map(str::to_string),
            withdraws: withdraws.map(str::to_string),
            body,
        }
    }

    #[test]
    fn a_line_has_the_documented_key_order_with_optionals_omitted() {
        let line = serde_json::to_string(&sample(None, None, json!({ "text": "hi" })))
            .expect("record serialises");
        assert_eq!(
            line,
            r#"{"id":"not_0192a1b2c3d47000800000000000000a","type":"note","schema_version":1,"created_at":"2026-08-25T12:00:00.000Z","body":{"text":"hi"}}"#
        );
        assert!(!line.contains("external_id"), "an absent optional is an absent key");
        assert!(!line.contains("withdraws"), "an absent optional is an absent key");
    }

    #[test]
    fn a_line_carries_all_seven_keys_in_order_when_the_optionals_are_present() {
        let line = serde_json::to_string(&sample(Some("ext-1"), Some("not_aaa"), json!({ "text": "hi" })))
            .expect("record serialises");
        let mut cursor = 0;
        for key in [
            "\"id\":",
            "\"type\":",
            "\"schema_version\":",
            "\"created_at\":",
            "\"external_id\":",
            "\"withdraws\":",
            "\"body\":",
        ] {
            let at = line.find(key).unwrap_or_else(|| panic!("{key} missing from {line}"));
            assert!(at >= cursor, "{key} is out of order in {line}");
            cursor = at;
        }
        let map: serde_json::Map<String, Value> =
            serde_json::from_str(&line).expect("a line is a JSON object");
        assert_eq!(map.len(), 7, "exactly seven keys: {line}");
    }

    #[test]
    fn a_line_round_trips() {
        let record = sample(Some("ext-1"), Some("not_aaa"), json!({ "text": "hi", "n": 3 }));
        let line = serde_json::to_string(&record).expect("record serialises");
        let back: Record = serde_json::from_str(&line).expect("record deserialises");
        assert_eq!(back, record);
    }

    #[test]
    fn searchable_text_returns_the_tag_then_string_leaves_in_deterministic_order() {
        let record = sample(
            None,
            None,
            json!({ "zeta": "last", "alpha": "first", "middle": ["m1", "m2"] }),
        );
        assert_eq!(
            searchable_text(&record),
            vec!["note", "first", "m1", "m2", "last"]
        );
    }

    #[test]
    fn searchable_text_includes_external_id_when_present() {
        let record = sample(Some("ext-42"), None, json!({ "text": "hi" }));
        assert_eq!(searchable_text(&record), vec!["note", "ext-42", "hi"]);
    }

    #[test]
    fn searchable_text_excludes_keys_scalars_and_store_assigned_fields() {
        let record = sample(
            None,
            Some("not_aaa"),
            json!({ "keyname": 42, "flag": true, "nothing": null, "deep": { "inner": "kept" } }),
        );
        let text = searchable_text(&record);
        assert_eq!(text, vec!["note", "kept"]);
        for absent in ["keyname", "flag", "nothing", "deep", "inner", "42", "true", "null"] {
            assert!(!text.iter().any(|s| s == absent), "{absent} must not appear in {text:?}");
        }
        assert!(!text.iter().any(|s| s.contains("0192a1b2")), "the id must not appear");
        assert!(!text.iter().any(|s| s.contains("2026-08-25")), "created_at must not appear");
        assert!(!text.iter().any(|s| s == "not_aaa"), "withdraws must not appear");
    }

    #[test]
    fn searchable_text_is_stable_across_calls() {
        let record = sample(Some("ext"), None, json!({ "b": ["x", { "c": "y" }], "a": "z" }));
        assert_eq!(searchable_text(&record), searchable_text(&record));
        assert_eq!(searchable_text(&record), vec!["note", "ext", "z", "x", "y"]);
    }

    #[test]
    fn searchable_text_handles_a_non_object_body() {
        let record = sample(None, None, json!("bare string body"));
        assert_eq!(searchable_text(&record), vec!["note", "bare string body"]);
    }

    #[test]
    fn timestamps_render_as_rfc_3339_utc_with_milliseconds() {
        assert_eq!(format_rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_millis(1_000), "1970-01-01T00:00:01.000Z");
        assert_eq!(format_rfc3339_millis(1_774_000_000_123), "2026-03-20T09:46:40.123Z");
        // A leap day, to prove the civil-date conversion is not an approximation.
        assert_eq!(format_rfc3339_millis(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn now_is_formatted_and_plausible() {
        let now = now_rfc3339_millis();
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert!(now.starts_with("20"), "{now}");
    }
}
