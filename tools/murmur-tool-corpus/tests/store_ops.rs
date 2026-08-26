//! Host tests for the four operations and the append-only file access underneath them.
//!
//! These live outside `src/` on purpose: a test fixture has to create the state directory
//! the tool itself must never create, and keeping that call out of `src/` keeps the
//! crate's source free of every directory-creating and file-rewriting call by inspection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use murmur_tool_corpus::ops::{self, OpStatus};
use murmur_tool_corpus::store::{CONFIG_FILE, CORPUS_FILE};

static DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A throwaway parent directory holding a `state/` the fixture creates itself, standing in
/// for the durable-state preopen the runtime will one day grant.
fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "murmur_corpus_{tag}_{}_{}",
        std::process::id(),
        DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the test root");
    root
}

fn config_json() -> Value {
    json!({
        "config_version": 1,
        "read_recent": { "default": 2, "max": 3 },
        "search": { "default_k": 2, "max_k": 3 },
        "types": {
            "note": {
                "schema_version": 1,
                "schema": {
                    "type": "object",
                    "required": ["text"],
                    "properties": { "text": { "type": "string" } },
                    "additionalProperties": false
                }
            },
            "memo": {
                "schema_version": 4,
                "schema": {
                    "type": "object",
                    "required": ["text"],
                    "properties": { "text": { "type": "string" } },
                    "additionalProperties": false
                }
            },
            "withdrawal": {
                "schema_version": 1,
                "schema": {
                    "type": "object",
                    "required": ["reason"],
                    "properties": { "reason": { "type": "string" } },
                    "additionalProperties": false
                }
            }
        }
    })
}

/// A state directory with the standard operator config already in place.
fn state_with_config(tag: &str) -> PathBuf {
    let state = temp_root(tag).join("state");
    std::fs::create_dir_all(&state).expect("create the state dir");
    std::fs::write(state.join(CONFIG_FILE), config_json().to_string()).expect("write config");
    state
}

struct Call {
    status: OpStatus,
    envelope: Value,
    metadata: Vec<(String, String)>,
}

impl Call {
    fn kind(&self) -> String {
        self.envelope["error_kind"].as_str().unwrap_or_default().to_string()
    }
    fn message(&self) -> String {
        self.envelope["message"].as_str().unwrap_or_default().to_string()
    }
    fn meta(&self, key: &str) -> Option<&str> {
        self.metadata.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn call(state: &Path, payload: Value) -> Call {
    let response = ops::run(state, &payload.to_string());
    let envelope: Value =
        serde_json::from_str(&response.data).expect("the envelope is always valid JSON");
    assert!(!response.summary.is_empty(), "every response carries a summary");
    Call { status: response.status, envelope, metadata: response.metadata }
}

fn append(state: &Path, type_tag: &str, body: Value) -> Call {
    call(state, json!({ "operation": "append", "type": type_tag, "body": body }))
}

fn append_note(state: &Path, text: &str) -> String {
    let c = append(state, "note", json!({ "text": text }));
    assert_eq!(c.status, OpStatus::Passed, "append failed: {}", c.envelope);
    c.envelope["id"].as_str().expect("append returns an id").to_string()
}

fn corpus_bytes(state: &Path) -> Vec<u8> {
    std::fs::read(state.join(CORPUS_FILE)).unwrap_or_default()
}

// ── append ────────────────────────────────────────────────────────────────────

#[test]
fn append_happy_path() {
    let state = state_with_config("append_ok");
    let c = append(&state, "note", json!({ "text": "first entry" }));

    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    assert_eq!(c.envelope["ok"], true);
    assert_eq!(c.envelope["operation"], "append");
    assert_eq!(c.envelope["deduped"], false);
    let id = c.envelope["id"].as_str().expect("an id");
    assert!(id.starts_with("not_"), "id must carry the derived prefix: {id}");
    assert_eq!(c.meta("state_effect"), Some("mutate"));
    assert_eq!(c.meta("resource_id"), Some(format!("corpus:{id}").as_str()));

    let text = String::from_utf8(corpus_bytes(&state)).expect("utf-8 corpus");
    assert_eq!(text.lines().count(), 1, "one append is one line: {text}");
    let line: Value = serde_json::from_str(text.lines().next().unwrap()).expect("a JSON line");
    assert_eq!(line["id"], id);
    assert_eq!(line["type"], "note");
    assert_eq!(line["schema_version"], 1);
    assert!(line["created_at"].as_str().unwrap().ends_with('Z'));
    assert!(line.get("external_id").is_none(), "absent optional is an absent key");
    assert!(line.get("withdraws").is_none(), "absent optional is an absent key");
}

#[test]
fn the_store_stamps_the_types_schema_version_not_the_callers() {
    let state = state_with_config("stamp");
    // `memo` declares schema_version 4; the caller's attempt to say otherwise is ignored,
    // because `schema_version` is not an input field at all.
    let c = call(
        &state,
        json!({ "operation": "append", "type": "memo", "body": { "text": "x" },
                "schema_version": 99, "id": "not_deadbeef", "created_at": "1999-01-01T00:00:00.000Z" }),
    );
    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    let id = c.envelope["id"].as_str().unwrap();
    assert!(id.starts_with("mem_"), "{id}");

    let text = String::from_utf8(corpus_bytes(&state)).unwrap();
    let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(line["schema_version"], 4);
    assert_ne!(line["id"], "not_deadbeef");
    assert_ne!(line["created_at"], "1999-01-01T00:00:00.000Z");
}

#[test]
fn earlier_bytes_stay_a_byte_exact_prefix_across_three_appends() {
    let state = state_with_config("prefix");

    let empty = corpus_bytes(&state);
    assert!(empty.is_empty(), "no corpus exists before the first append");

    let first_id = append_note(&state, "one");
    let after_first = corpus_bytes(&state);

    append_note(&state, "two");
    let after_second = corpus_bytes(&state);
    assert!(
        after_second.starts_with(&after_first),
        "the second append rewrote earlier bytes"
    );

    let w = call(
        &state,
        json!({ "operation": "append", "type": "withdrawal",
                "body": { "reason": "superseded" }, "withdraws": first_id }),
    );
    assert_eq!(w.status, OpStatus::Passed, "{}", w.envelope);
    let after_third = corpus_bytes(&state);
    assert!(
        after_third.starts_with(&after_second),
        "the withdrawal rewrote earlier bytes"
    );
    assert!(after_third.len() > after_second.len(), "the withdrawal appended nothing");
}

#[test]
fn a_repeated_external_id_returns_the_first_id_and_changes_nothing() {
    let state = state_with_config("dedupe");
    let first = call(
        &state,
        json!({ "operation": "append", "type": "note", "body": { "text": "once" },
                "external_id": "retry-1" }),
    );
    assert_eq!(first.status, OpStatus::Passed, "{}", first.envelope);
    assert_eq!(first.envelope["deduped"], false);
    let id = first.envelope["id"].as_str().unwrap().to_string();
    let before = corpus_bytes(&state);

    let second = call(
        &state,
        json!({ "operation": "append", "type": "note", "body": { "text": "different body" },
                "external_id": "retry-1" }),
    );
    assert_eq!(second.status, OpStatus::Passed, "{}", second.envelope);
    assert_eq!(second.envelope["deduped"], true);
    assert_eq!(second.envelope["id"], id);
    assert_eq!(corpus_bytes(&state), before, "a deduped append must not write");
    assert_eq!(second.meta("state_effect"), Some("read"), "a deduped append changed nothing");
    assert_eq!(second.meta("resource_id"), Some(format!("corpus:{id}").as_str()));
}

#[test]
fn the_same_external_id_under_a_different_type_is_not_a_duplicate() {
    let state = state_with_config("dedupe_type");
    let a = call(
        &state,
        json!({ "operation": "append", "type": "note", "body": { "text": "x" },
                "external_id": "shared" }),
    );
    let b = call(
        &state,
        json!({ "operation": "append", "type": "memo", "body": { "text": "x" },
                "external_id": "shared" }),
    );
    assert_eq!(b.status, OpStatus::Passed, "{}", b.envelope);
    assert_eq!(b.envelope["deduped"], false);
    assert_ne!(a.envelope["id"], b.envelope["id"]);
    assert_eq!(corpus_bytes(&state).iter().filter(|b| **b == b'\n').count(), 2);
}

#[test]
fn a_schema_violation_writes_nothing() {
    let state = state_with_config("schema_violation");
    append_note(&state, "seed");
    let before = corpus_bytes(&state);

    let c = append(&state, "note", json!({ "text": 42 }));
    assert_eq!(c.status, OpStatus::Failed, "{}", c.envelope);
    assert_eq!(c.kind(), "schema_violation");
    assert!(c.message().contains("body.text"), "must name the field: {}", c.message());
    assert_eq!(corpus_bytes(&state), before);

    let missing = append(&state, "note", json!({}));
    assert_eq!(missing.kind(), "schema_violation");
    assert!(missing.message().contains("required"), "{}", missing.message());

    let extra = append(&state, "note", json!({ "text": "x", "sneak": 1 }));
    assert_eq!(extra.kind(), "schema_violation");
    assert!(extra.message().contains("sneak"), "{}", extra.message());
    assert_eq!(corpus_bytes(&state), before);
}

#[test]
fn an_undeclared_type_writes_nothing() {
    let state = state_with_config("unknown_type");
    append_note(&state, "seed");
    let before = corpus_bytes(&state);

    let c = append(&state, "invoice", json!({ "text": "x" }));
    assert_eq!(c.status, OpStatus::Failed, "{}", c.envelope);
    assert_eq!(c.kind(), "unknown_type");
    assert!(c.message().contains("invoice"), "{}", c.message());
    assert_eq!(corpus_bytes(&state), before);
}

#[test]
fn append_requires_a_type_and_a_body_even_for_a_withdrawal() {
    let state = state_with_config("append_required");
    let id = append_note(&state, "target");

    let no_type = call(&state, json!({ "operation": "append", "body": { "text": "x" } }));
    assert_eq!(no_type.kind(), "invalid_input");

    let no_body = call(
        &state,
        json!({ "operation": "append", "type": "withdrawal", "withdraws": id }),
    );
    assert_eq!(no_body.status, OpStatus::Failed, "{}", no_body.envelope);
    assert_eq!(no_body.kind(), "invalid_input");
    assert!(no_body.message().contains("body"), "{}", no_body.message());
}

// ── withdrawal ────────────────────────────────────────────────────────────────

fn withdraw(state: &Path, target: &str) -> Call {
    call(
        state,
        json!({ "operation": "append", "type": "withdrawal",
                "body": { "reason": "no longer accurate" }, "withdraws": target }),
    )
}

#[test]
fn withdrawing_a_missing_target_fails() {
    let state = state_with_config("withdraw_missing");
    let c = withdraw(&state, "not_00000000000000000000000000000000");
    assert_eq!(c.status, OpStatus::Failed, "{}", c.envelope);
    assert_eq!(c.kind(), "withdraw_target_not_found");
    assert!(corpus_bytes(&state).is_empty(), "a rejected withdrawal writes nothing");
}

#[test]
fn withdrawing_twice_fails_the_second_time() {
    let state = state_with_config("withdraw_twice");
    let id = append_note(&state, "target");
    assert_eq!(withdraw(&state, &id).status, OpStatus::Passed);
    let before = corpus_bytes(&state);

    let second = withdraw(&state, &id);
    assert_eq!(second.status, OpStatus::Failed, "{}", second.envelope);
    assert_eq!(second.kind(), "already_withdrawn");
    assert_eq!(corpus_bytes(&state), before);
}

#[test]
fn withdrawing_a_withdrawal_never_restores_its_target() {
    let state = state_with_config("withdraw_terminal");
    let target = append_note(&state, "original");
    let w1 = withdraw(&state, &target);
    let w1_id = w1.envelope["id"].as_str().unwrap().to_string();

    let w2 = withdraw(&state, &w1_id);
    assert_eq!(w2.status, OpStatus::Passed, "{}", w2.envelope);

    // The original is still withdrawn, and still points at the first withdrawal.
    let got = call(&state, json!({ "operation": "get", "id": target }));
    assert_eq!(got.status, OpStatus::Passed, "{}", got.envelope);
    assert_eq!(got.envelope["record"]["body"], Value::Null);
    assert_eq!(got.envelope["record"]["withdrawn_by"], w1_id);

    let recent = call(&state, json!({ "operation": "read_recent", "type": "note", "n": 3 }));
    assert_eq!(recent.envelope["returned"], 0, "{}", recent.envelope);
}

// ── get ───────────────────────────────────────────────────────────────────────

#[test]
fn get_resolves_a_live_record_in_full() {
    let state = state_with_config("get_live");
    let id = append_note(&state, "hello");
    let c = call(&state, json!({ "operation": "get", "id": id }));

    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    assert_eq!(c.envelope["ok"], true);
    assert_eq!(c.envelope["operation"], "get");
    let record = &c.envelope["record"];
    assert_eq!(record["id"], id);
    assert_eq!(record["type"], "note");
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["body"], json!({ "text": "hello" }));
    assert!(record.get("withdrawn_by").is_none());
    assert_eq!(c.meta("state_effect"), Some("read"));
    assert_eq!(c.meta("resource_id"), Some(format!("corpus:{id}").as_str()));
}

#[test]
fn get_on_an_unknown_id_is_not_found() {
    let state = state_with_config("get_missing");
    let c = call(&state, json!({ "operation": "get", "id": "not_ffffffffffffffffffffffffffffffff" }));
    assert_eq!(c.status, OpStatus::Failed, "{}", c.envelope);
    assert_eq!(c.kind(), "not_found");
}

#[test]
fn get_on_a_withdrawn_record_resolves_with_a_null_body_and_the_tombstone() {
    let state = state_with_config("get_withdrawn");
    let id = append_note(&state, "retracted");
    let w = withdraw(&state, &id);
    let w_id = w.envelope["id"].as_str().unwrap().to_string();

    let c = call(&state, json!({ "operation": "get", "id": id }));
    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    let record = &c.envelope["record"];
    assert_eq!(record["id"], id);
    assert_eq!(record["type"], "note");
    assert_eq!(record["schema_version"], 1);
    assert!(record["created_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(record["body"], Value::Null);
    assert_eq!(record["withdrawn_by"], w_id);
    assert!(record["withdrawn_at"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn get_requires_an_id() {
    let state = state_with_config("get_required");
    let c = call(&state, json!({ "operation": "get" }));
    assert_eq!(c.kind(), "invalid_input");
    assert!(c.message().contains("id"), "{}", c.message());
}

// ── read_recent ───────────────────────────────────────────────────────────────

#[test]
fn read_recent_is_newest_first_type_filtered_and_withdrawn_excluding() {
    let state = state_with_config("recent_order");
    let a = append_note(&state, "alpha");
    let b = append_note(&state, "bravo");
    append(&state, "memo", json!({ "text": "other type" }));
    let c_id = append_note(&state, "charlie");
    withdraw(&state, &b);

    let c = call(&state, json!({ "operation": "read_recent", "type": "note", "n": 3 }));
    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    let ids: Vec<&str> = c.envelope["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![c_id.as_str(), a.as_str()], "{}", c.envelope);
    assert_eq!(c.envelope["returned"], 2);
    assert_eq!(c.envelope["requested"], 3);
    assert_eq!(c.meta("state_effect"), Some("read"));
    assert_eq!(c.meta("resource_id"), Some("corpus:type:note"));
}

#[test]
fn read_recent_clamps_an_oversized_n_and_reports_both_counts() {
    let state = state_with_config("recent_clamp");
    for i in 0..6 {
        append_note(&state, &format!("entry {i}"));
    }
    let c = call(&state, json!({ "operation": "read_recent", "type": "note", "n": 100 }));
    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    assert_eq!(c.envelope["requested"], 100, "requested is what was asked for");
    assert_eq!(c.envelope["returned"], 3, "returned is clamped to read_recent.max");
    assert_eq!(c.envelope["records"].as_array().unwrap().len(), 3);
}

#[test]
fn read_recent_defaults_n_when_it_is_omitted() {
    let state = state_with_config("recent_default");
    for i in 0..5 {
        append_note(&state, &format!("entry {i}"));
    }
    let c = call(&state, json!({ "operation": "read_recent", "type": "note" }));
    assert_eq!(c.envelope["requested"], 2, "read_recent.default is 2 in this config");
    assert_eq!(c.envelope["returned"], 2);
}

#[test]
fn read_recent_rejects_a_non_positive_n() {
    let state = state_with_config("recent_bad_n");
    for payload in [
        json!({ "operation": "read_recent", "type": "note", "n": 0 }),
        json!({ "operation": "read_recent", "type": "note", "n": -1 }),
        json!({ "operation": "read_recent", "type": "note", "n": "many" }),
    ] {
        let c = call(&state, payload);
        assert_eq!(c.kind(), "invalid_input", "{}", c.envelope);
    }
}

#[test]
fn read_recent_requires_a_type() {
    let state = state_with_config("recent_required");
    let c = call(&state, json!({ "operation": "read_recent" }));
    assert_eq!(c.kind(), "invalid_input");
}

// ── search ────────────────────────────────────────────────────────────────────

#[test]
fn search_ranks_by_the_fraction_of_distinct_query_terms_matched() {
    let state = state_with_config("search_rank");
    let partial = append_note(&state, "rollback only, nothing else");
    let full = append_note(&state, "the rollback plan for the release");

    let c = call(&state, json!({ "operation": "search", "query": "rollback plan", "k": 3 }));
    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    let hits = c.envelope["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{}", c.envelope);
    assert_eq!(hits[0]["id"], full);
    assert_eq!(hits[0]["score"], 1.0);
    assert_eq!(hits[0]["type"], "note");
    assert_eq!(hits[0]["body"], json!({ "text": "the rollback plan for the release" }));
    assert!(hits[0]["created_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(hits[1]["id"], partial);
    assert_eq!(hits[1]["score"], 0.5);
    assert_eq!(c.meta("state_effect"), Some("read"));
    assert_eq!(c.meta("resource_id"), Some("corpus:search:rollback plan"));
}

#[test]
fn search_breaks_score_ties_by_recency_and_repeats_byte_identically() {
    let state = state_with_config("search_ties");
    let first = append_note(&state, "rollback plan alpha");
    let second = append_note(&state, "rollback plan bravo");
    let third = append_note(&state, "rollback plan charlie");

    let payload = json!({ "operation": "search", "query": "rollback plan", "k": 3 });
    let a = ops::run(&state, &payload.to_string());
    let b = ops::run(&state, &payload.to_string());
    assert_eq!(a.data, b.data, "repeated searches must be byte-identical");

    let envelope: Value = serde_json::from_str(&a.data).unwrap();
    let ids: Vec<&str> = envelope["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![third.as_str(), second.as_str(), first.as_str()]);
}

#[test]
fn a_same_millisecond_score_tie_orders_newest_first_in_both_retrieval_paths() {
    // The tie-break only runs when two records share a `created_at`, and `created_at` is
    // millisecond-resolution — appending through `ops::run` lands in the same millisecond
    // only sometimes, which makes a timing-dependent assertion out of a fixed rule. Seed
    // the corpus directly so the tie is always taken.
    let state = state_with_config("search_same_ms");
    let at = "2026-08-25T12:00:00.000Z";
    // Ids share the embedded millisecond and differ only in the mint sequence, exactly as
    // two ids minted back to back inside one millisecond do. `older` sorts first.
    let older = "not_01a03b59b6d0701db0ef1ec3f4c49092";
    let newer = "not_01a03b59b6d070208eabe63702b51cfd";
    let lines = [
        json!({ "id": older, "type": "note", "schema_version": 1, "created_at": at,
                "body": { "text": "rollback plan alpha" } }),
        json!({ "id": newer, "type": "note", "schema_version": 1, "created_at": at,
                "body": { "text": "rollback plan bravo" } }),
    ];
    let file: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(state.join(CORPUS_FILE), file).expect("seed the corpus");

    let hits = call(&state, json!({ "operation": "search", "query": "rollback plan", "k": 3 }));
    let hit_ids: Vec<&str> = hits.envelope["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(hit_ids, vec![newer, older], "search must be newest-first on a tie");

    // Both scores are 1.0, so this is purely the tie-break — and it must agree with the
    // other retrieval path, which orders the same two records by the same rule.
    let recent = call(&state, json!({ "operation": "read_recent", "type": "note", "n": 3 }));
    let recent_ids: Vec<&str> = recent.envelope["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(recent_ids, hit_ids, "search and read_recent must agree on a tie");
}

#[test]
fn search_honours_the_type_filter_and_excludes_withdrawn_records() {
    let state = state_with_config("search_filter");
    let note = append_note(&state, "rollback plan in a note");
    append(&state, "memo", json!({ "text": "rollback plan in a memo" }));
    let doomed = append_note(&state, "rollback plan that gets withdrawn");
    withdraw(&state, &doomed);

    let c = call(
        &state,
        json!({ "operation": "search", "query": "rollback plan", "k": 3, "type": "note" }),
    );
    let hits = c.envelope["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{}", c.envelope);
    assert_eq!(hits[0]["id"], note);

    let unfiltered = call(&state, json!({ "operation": "search", "query": "rollback plan", "k": 3 }));
    assert_eq!(unfiltered.envelope["hits"].as_array().unwrap().len(), 2);
}

#[test]
fn search_clamps_k_and_defaults_it() {
    let state = state_with_config("search_k");
    for i in 0..6 {
        append_note(&state, &format!("rollback plan {i}"));
    }
    let clamped = call(&state, json!({ "operation": "search", "query": "rollback plan", "k": 99 }));
    assert_eq!(clamped.envelope["hits"].as_array().unwrap().len(), 3, "search.max_k is 3");

    let defaulted = call(&state, json!({ "operation": "search", "query": "rollback plan" }));
    assert_eq!(
        defaulted.envelope["hits"].as_array().unwrap().len(),
        2,
        "search.default_k is 2"
    );
}

#[test]
fn search_matching_nothing_succeeds_with_an_empty_hit_list() {
    let state = state_with_config("search_empty");
    append_note(&state, "rollback plan");
    let c = call(&state, json!({ "operation": "search", "query": "unrelated vocabulary" }));
    assert_eq!(c.status, OpStatus::Passed, "{}", c.envelope);
    assert_eq!(c.envelope["ok"], true);
    assert_eq!(c.envelope["hits"], json!([]));
}

#[test]
fn search_matches_the_type_tag_and_the_external_id() {
    let state = state_with_config("search_seams");
    let by_tag = call(&state, json!({ "operation": "search", "query": "note" }));
    assert_eq!(by_tag.envelope["hits"], json!([]), "nothing appended yet");

    let id = call(
        &state,
        json!({ "operation": "append", "type": "note", "body": { "text": "irrelevant words" },
                "external_id": "ticket-4711" }),
    );
    let id = id.envelope["id"].as_str().unwrap().to_string();

    let tag_hit = call(&state, json!({ "operation": "search", "query": "note" }));
    assert_eq!(tag_hit.envelope["hits"][0]["id"], id, "the type tag is searchable");

    let ext_hit = call(&state, json!({ "operation": "search", "query": "ticket 4711" }));
    assert_eq!(ext_hit.envelope["hits"][0]["id"], id, "external_id is searchable");
}

#[test]
fn search_normalises_the_query_in_its_resource_id() {
    let state = state_with_config("search_resource");
    let c = call(&state, json!({ "operation": "search", "query": "  Rollback\t  PLAN \n" }));
    assert_eq!(c.meta("resource_id"), Some("corpus:search:rollback plan"));
}

#[test]
fn search_requires_a_query() {
    let state = state_with_config("search_required");
    let c = call(&state, json!({ "operation": "search" }));
    assert_eq!(c.kind(), "invalid_input");
}

// ── environment and operator faults ───────────────────────────────────────────

#[test]
fn a_corrupt_line_fails_the_whole_operation_and_names_its_line_number() {
    let state = state_with_config("corrupt");
    append_note(&state, "good one");
    append_note(&state, "good two");
    let corpus = state.join(CORPUS_FILE);
    let mut text = std::fs::read_to_string(&corpus).unwrap();
    text.push_str("this line is not a record\n");
    std::fs::write(&corpus, text).unwrap();

    for payload in [
        json!({ "operation": "read_recent", "type": "note", "n": 3 }),
        json!({ "operation": "search", "query": "good" }),
        json!({ "operation": "get", "id": "not_00000000000000000000000000000000" }),
        json!({ "operation": "append", "type": "note", "body": { "text": "x" } }),
    ] {
        let c = call(&state, payload.clone());
        assert_eq!(c.status, OpStatus::Error, "{}: {}", payload, c.envelope);
        assert_eq!(c.kind(), "corpus_corrupt", "{}", c.envelope);
        assert!(c.message().contains("line 3"), "must name the line: {}", c.message());
        assert!(c.envelope.get("records").is_none(), "no partial result set");
        assert!(c.envelope.get("hits").is_none(), "no partial result set");
    }
}

#[test]
fn a_state_dir_without_a_config_is_config_missing() {
    let root = temp_root("config_missing");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let c = call(&state, json!({ "operation": "read_recent", "type": "note" }));
    assert_eq!(c.status, OpStatus::Error, "{}", c.envelope);
    assert_eq!(c.kind(), "config_missing");
    assert!(c.message().contains(CONFIG_FILE), "{}", c.message());
}

#[test]
fn an_invalid_config_is_config_invalid() {
    let root = temp_root("config_invalid");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join(CONFIG_FILE), "{ not json").unwrap();

    let c = call(&state, json!({ "operation": "read_recent", "type": "note" }));
    assert_eq!(c.status, OpStatus::Error, "{}", c.envelope);
    assert_eq!(c.kind(), "config_invalid");
}

#[test]
fn a_missing_state_dir_fails_closed_and_creates_nothing() {
    let root = temp_root("no_state");
    let state = root.join("state");

    for payload in [
        json!({ "operation": "append", "type": "note", "body": { "text": "x" } }),
        json!({ "operation": "get", "id": "not_00000000000000000000000000000000" }),
        json!({ "operation": "read_recent", "type": "note" }),
        json!({ "operation": "search", "query": "anything" }),
    ] {
        let c = call(&state, payload.clone());
        assert_eq!(c.status, OpStatus::Error, "{}: {}", payload, c.envelope);
        assert_eq!(c.kind(), "state_unavailable", "{}", c.envelope);
        assert!(
            c.message().contains("capabilities.state"),
            "message must name the grant: {}",
            c.message()
        );
    }

    assert!(!state.exists(), "the tool must never create the state directory");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "the tool must not create anything outside the state directory either"
    );
}

// ── dispatch ──────────────────────────────────────────────────────────────────

#[test]
fn an_unrecognised_operation_is_rejected_before_state_is_touched() {
    let root = temp_root("unknown_op");
    let c = call(&root.join("state"), json!({ "operation": "delete", "id": "x" }));
    assert_eq!(c.status, OpStatus::Failed, "{}", c.envelope);
    assert_eq!(c.kind(), "unknown_operation");
    assert_eq!(c.envelope["operation"], "delete");
    assert!(c.message().contains("append"), "{}", c.message());
}

#[test]
fn malformed_input_is_invalid_input() {
    let state = state_with_config("bad_input");
    for raw in ["", "   ", "not json", "[1,2,3]", "\"just a string\"", "{}"] {
        let response = ops::run(&state, raw);
        let envelope: Value = serde_json::from_str(&response.data).unwrap();
        assert_eq!(response.status, OpStatus::Failed, "{raw:?} -> {envelope}");
        assert_eq!(envelope["error_kind"], "invalid_input", "{raw:?} -> {envelope}");
        assert_eq!(envelope["ok"], false);
    }
}

#[test]
fn a_double_encoded_payload_is_re_parsed_once() {
    let state = state_with_config("double_encoded");
    let inner = json!({ "operation": "append", "type": "note", "body": { "text": "wrapped" } })
        .to_string();
    let outer = Value::String(inner).to_string();

    let response = ops::run(&state, &outer);
    let envelope: Value = serde_json::from_str(&response.data).unwrap();
    assert_eq!(response.status, OpStatus::Passed, "{envelope}");
    assert_eq!(envelope["operation"], "append");
    assert_eq!(envelope["deduped"], false);
}

#[test]
fn every_failure_envelope_carries_the_same_four_keys_and_no_metadata() {
    let state = state_with_config("failure_shape");
    let c = append(&state, "invoice", json!({ "text": "x" }));
    let object = c.envelope.as_object().unwrap();
    assert_eq!(object.len(), 4, "{}", c.envelope);
    for key in ["ok", "operation", "error_kind", "message"] {
        assert!(object.contains_key(key), "missing {key} in {}", c.envelope);
    }
    assert!(c.metadata.is_empty(), "a failed call addressed nothing");
}
