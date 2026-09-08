//! Host-target tests for the two operations, run against a real directory.
//!
//! Everything the tool decides lives outside `#[cfg(target_arch = "wasm32")]`, so these
//! exercise the same code the component runs: `ops::run` takes the state directory, the
//! operator configuration and the run identity as parameters, and nothing below `lib.rs`
//! reads an environment. What only a component can prove — that the state grant is a
//! separate preopen and that nothing lands in the workdir — is in `wasm_component.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use murmur_tool_report::ops::{self, OpStatus, MAX_REFERENCE_CHARS};
use murmur_tool_report::report::{RunIdentity, MAX_PROGRESS_NOTES};
use murmur_tool_report::store::REPORT_FILE;
use serde_json::{json, Value};

static DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A scratch state directory that no other test shares.
struct Fixture {
    root: PathBuf,
    state: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `with_state` off stands for the capsule that never granted `capabilities.state`: the
/// directory is absent, exactly as the guest would see it without the preopen.
fn fixture(tag: &str, with_state: bool) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "murmur_report_{tag}_{}_{}",
        std::process::id(),
        DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create fixture root");
    let state = root.join("state");
    if with_state {
        std::fs::create_dir_all(&state).expect("create state dir");
    }
    Fixture { root, state }
}

fn identity() -> RunIdentity {
    RunIdentity::new(Some("sess-abc".into()), Some("demo-capsule".into()))
}

/// One call, with no operator `config:` block. Returns the status and the decoded envelope.
fn call(state: &Path, payload: Value) -> (OpStatus, Value) {
    call_with(state, None, &identity(), payload)
}

fn call_with(
    state: &Path,
    config: Option<&str>,
    identity: &RunIdentity,
    payload: Value,
) -> (OpStatus, Value) {
    let response = ops::run(state, config, identity, &payload.to_string());
    let envelope: Value =
        serde_json::from_str(&response.data).expect("the envelope is always JSON");
    assert!(
        response
            .metadata
            .iter()
            .any(|(k, v)| k == "state_effect" && v == "mutate"),
        "every response declares state_effect=mutate: {:?}",
        response.metadata
    );
    assert!(
        response.metadata.iter().any(|(k, v)| {
            k == "resource_id" && v == &format!("report:{}", state.join(REPORT_FILE).display())
        }),
        "every response declares the report file as its resource: {:?}",
        response.metadata
    );
    (response.status, envelope)
}

/// The report file as JSON. Fails the test when it is absent — absence is asserted
/// directly by the tests that mean it.
fn report(state: &Path) -> Value {
    let text = std::fs::read_to_string(state.join(REPORT_FILE))
        .unwrap_or_else(|e| panic!("reading the report: {e}"));
    serde_json::from_str(&text).expect("the report file is one JSON object")
}

/// Every entry in the state directory, sorted. The report is the only file this tool may
/// leave behind — a staged temp file that survived a save is a failure.
fn state_entries(state: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(state)
        .expect("read state dir")
        .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn a_report() -> Value {
    json!({
        "operation": "report",
        "outcome": "success",
        "summary": "Ported the parser and all tests pass.",
        "deliverables": [{ "name": "patch", "kind": "diff", "uri": "out/parser.patch" }],
        "notes_for": [{ "stage": "review", "body": "The lexer change is the risky part." }]
    })
}

// ── the happy path ────────────────────────────────────────────────────────────

#[test]
fn a_report_writes_one_file_carrying_the_capsules_verdict() {
    let f = fixture("happy", true);
    let (status, envelope) = call(&f.state, a_report());

    assert_eq!(status, OpStatus::Passed, "{envelope}");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["operation"], "report");
    assert_eq!(envelope["concluded"], true);
    assert_eq!(envelope["outcome"], "success");
    assert_eq!(envelope["revision"], 1);

    assert_eq!(state_entries(&f.state), vec![REPORT_FILE.to_string()]);

    let doc = report(&f.state);
    assert_eq!(doc["report_version"], 1);
    assert_eq!(doc["concluded"], true);
    assert_eq!(doc["outcome"], "success");
    assert_eq!(doc["summary"], "Ported the parser and all tests pass.");
    assert_eq!(
        doc["deliverables"],
        json!([{ "name": "patch", "kind": "diff", "uri": "out/parser.patch" }])
    );
    assert_eq!(
        doc["notes_for"],
        json!([{ "stage": "review", "body": "The lexer change is the risky part." }])
    );
    assert_eq!(doc["revision"], 1);
    assert_eq!(doc["superseded"], json!([]));
    assert_eq!(doc["superseded_count"], 0);
    assert_eq!(doc["progress"], json!([]));
    assert_eq!(doc["progress_count"], 0);

    let reported_at = doc["reported_at"].as_str().expect("an RFC 3339 timestamp");
    assert!(
        reported_at.len() == 24 && reported_at.ends_with('Z') && reported_at.contains('T'),
        "reported_at must be RFC 3339 UTC: {reported_at}"
    );
}

#[test]
fn a_report_with_no_deliverables_or_notes_is_a_complete_conclusion() {
    let f = fixture("bare", true);
    let (status, _envelope) = call(
        &f.state,
        json!({ "operation": "report", "outcome": "rejected", "summary": "Out of scope." }),
    );
    assert_eq!(status, OpStatus::Passed);

    let doc = report(&f.state);
    assert_eq!(doc["concluded"], true);
    assert_eq!(doc["outcome"], "rejected");
    assert_eq!(doc["deliverables"], json!([]));
    assert_eq!(doc["notes_for"], json!([]));
}

// ── progress is not a conclusion ──────────────────────────────────────────────

#[test]
fn a_progress_note_does_not_conclude_the_task() {
    let f = fixture("progress", true);
    let (status, envelope) = call(
        &f.state,
        json!({ "operation": "progress", "note": "Indexed 4 of 9 crates." }),
    );

    assert_eq!(status, OpStatus::Passed, "{envelope}");
    assert_eq!(envelope["concluded"], false);
    assert_eq!(envelope["progress_count"], 1);

    let doc = report(&f.state);
    assert_eq!(doc["concluded"], false);
    assert_eq!(doc["outcome"], Value::Null);
    assert_eq!(doc["summary"], Value::Null);
    assert_eq!(doc["reported_at"], Value::Null);
    assert_eq!(doc["revision"], 0);
    assert_eq!(doc["progress"].as_array().expect("a progress list").len(), 1);
    assert_eq!(doc["progress"][0]["note"], "Indexed 4 of 9 crates.");
    assert!(doc["progress"][0]["at"].is_string());
    assert_eq!(doc["progress_count"], 1);
}

#[test]
fn progress_notes_survive_a_later_terminal_report() {
    let f = fixture("progress-then-report", true);
    for note in ["one", "two", "three"] {
        let (status, _e) = call(&f.state, json!({ "operation": "progress", "note": note }));
        assert_eq!(status, OpStatus::Passed);
    }
    let (status, _e) = call(
        &f.state,
        json!({ "operation": "report", "outcome": "blocked", "summary": "Needs a credential." }),
    );
    assert_eq!(status, OpStatus::Passed);

    let doc = report(&f.state);
    assert_eq!(doc["concluded"], true);
    assert_eq!(doc["outcome"], "blocked");
    assert_eq!(doc["revision"], 1);
    let notes: Vec<&str> = doc["progress"]
        .as_array()
        .expect("a progress list")
        .iter()
        .map(|n| n["note"].as_str().expect("a note"))
        .collect();
    assert_eq!(notes, vec!["one", "two", "three"], "notes stay in call order");
    assert!(doc["progress"][0]["at"].is_string(), "timestamps stay intact");
}

#[test]
fn progress_is_capped_in_the_file_while_the_count_stays_true() {
    let f = fixture("progress-cap", true);
    for n in 0..MAX_PROGRESS_NOTES + 2 {
        let (status, _e) =
            call(&f.state, json!({ "operation": "progress", "note": format!("note {n}") }));
        assert_eq!(status, OpStatus::Passed);
    }
    let doc = report(&f.state);
    assert_eq!(
        doc["progress"].as_array().expect("a progress list").len(),
        MAX_PROGRESS_NOTES
    );
    assert_eq!(doc["progress_count"], MAX_PROGRESS_NOTES as u64 + 2);
}

// ── last write wins, and the previous verdict is kept ─────────────────────────

#[test]
fn a_second_report_wins_and_the_first_is_kept_in_superseded() {
    let f = fixture("supersede", true);
    call(&f.state, a_report());
    let (status, envelope) = call(
        &f.state,
        json!({
            "operation": "report",
            "outcome": "failed",
            "summary": "The port regressed on nested groups.",
        }),
    );
    assert_eq!(status, OpStatus::Passed, "{envelope}");
    assert_eq!(envelope["revision"], 2);

    let doc = report(&f.state);
    assert_eq!(doc["outcome"], "failed");
    assert_eq!(doc["summary"], "The port regressed on nested groups.");
    assert_eq!(doc["revision"], 2);
    assert_eq!(doc["superseded_count"], 1);
    let superseded = doc["superseded"].as_array().expect("a superseded list");
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0]["outcome"], "success");
    assert_eq!(superseded[0]["summary"], "Ported the parser and all tests pass.");
    assert_eq!(
        superseded[0]["deliverables"],
        json!([{ "name": "patch", "kind": "diff", "uri": "out/parser.patch" }])
    );
    assert_eq!(
        superseded[0]["notes_for"],
        json!([{ "stage": "review", "body": "The lexer change is the risky part." }])
    );
    assert!(superseded[0]["reported_at"].is_string());
}

#[test]
fn a_third_report_leaves_two_superseded_entries_oldest_first() {
    let f = fixture("supersede-twice", true);
    for outcome in ["success", "blocked", "failed"] {
        let (status, _e) = call(
            &f.state,
            json!({ "operation": "report", "outcome": outcome, "summary": format!("verdict {outcome}") }),
        );
        assert_eq!(status, OpStatus::Passed);
    }

    let doc = report(&f.state);
    assert_eq!(doc["outcome"], "failed");
    assert_eq!(doc["revision"], 3);
    assert_eq!(doc["superseded_count"], 2);
    let superseded = doc["superseded"].as_array().expect("a superseded list");
    assert_eq!(superseded.len(), 2);
    assert_eq!(superseded[0]["outcome"], "success");
    assert_eq!(superseded[1]["outcome"], "blocked");
}

// ── never reported is its own state ───────────────────────────────────────────

#[test]
fn a_state_directory_the_tool_was_never_called_against_holds_nothing() {
    let f = fixture("never", true);
    assert_eq!(state_entries(&f.state), Vec::<String>::new());
    assert!(!f.state.join(REPORT_FILE).exists());
}

// ── deliverables are references only ──────────────────────────────────────────

#[test]
fn a_deliverable_carrying_an_inline_payload_is_refused_and_nothing_is_written() {
    let f = fixture("inline-payload", true);
    let (status, envelope) = call(
        &f.state,
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "Generated the dataset.",
            "deliverables": [{
                "name": "rows",
                "kind": "dataset",
                "uri": "out/rows.csv",
                "content": "id,name\n1,alice\n"
            }]
        }),
    );

    assert_eq!(status, OpStatus::Failed, "{envelope}");
    assert_eq!(envelope["error_kind"], "inline_payload_refused");
    let message = envelope["message"].as_str().expect("a message");
    assert!(message.contains("rows"), "the message names the deliverable: {message}");
    assert!(message.contains("content"), "the message names the key: {message}");
    assert_eq!(
        state_entries(&f.state),
        Vec::<String>::new(),
        "a refused report writes nothing at all"
    );
}

#[test]
fn a_refused_report_leaves_the_previous_report_exactly_as_it_was() {
    let f = fixture("refusal-keeps-previous", true);
    call(&f.state, a_report());
    let before = std::fs::read_to_string(f.state.join(REPORT_FILE)).expect("the first report");

    let (status, envelope) = call(
        &f.state,
        json!({
            "operation": "report",
            "outcome": "failed",
            "summary": "Second thoughts.",
            "deliverables": [{ "name": "blob", "kind": "diff", "uri": "x", "body": "payload" }]
        }),
    );
    assert_eq!(status, OpStatus::Failed, "{envelope}");

    let after = std::fs::read_to_string(f.state.join(REPORT_FILE)).expect("the first report");
    assert_eq!(before, after, "a refused report must not touch the previous one");
    assert_eq!(state_entries(&f.state), vec![REPORT_FILE.to_string()]);
}

#[test]
fn every_smuggled_payload_shape_is_refused_by_the_same_rule() {
    let long_uri = "a".repeat(MAX_REFERENCE_CHARS + 1);
    let cases: Vec<(&str, Value)> = vec![
        ("an unknown key", json!({ "name": "d", "kind": "diff", "uri": "a", "payload": "x" })),
        ("a data: URI", json!({ "name": "d", "kind": "diff", "uri": "data:text/plain,hello" })),
        ("an upper-case data: URI", json!({ "name": "d", "kind": "diff", "uri": "DATA:text/plain,hi" })),
        ("a newline", json!({ "name": "d", "kind": "diff", "uri": "out/a\nout/b" })),
        ("an over-long reference", json!({ "name": "d", "kind": "diff", "uri": long_uri })),
    ];

    for (what, deliverable) in cases {
        let f = fixture("payload-shapes", true);
        let (status, envelope) = call(
            &f.state,
            json!({
                "operation": "report",
                "outcome": "success",
                "summary": "done",
                "deliverables": [deliverable]
            }),
        );
        assert_eq!(status, OpStatus::Failed, "{what}: {envelope}");
        assert_eq!(envelope["error_kind"], "inline_payload_refused", "{what}");
        assert!(
            envelope["message"].as_str().expect("a message").contains('d'),
            "{what}: the message names the deliverable"
        );
        assert_eq!(state_entries(&f.state), Vec::<String>::new(), "{what}: nothing written");
    }
}

#[test]
fn a_deliverable_missing_a_field_or_repeating_a_name_is_invalid_input() {
    let cases: Vec<Value> = vec![
        json!([{ "name": "d", "kind": "diff" }]),
        json!([{ "name": "", "kind": "diff", "uri": "a" }]),
        json!([{ "name": "d", "kind": "", "uri": "a" }]),
        json!([{ "name": "d", "kind": "diff", "uri": "" }]),
        json!(["not an object"]),
        json!([
            { "name": "d", "kind": "diff", "uri": "a" },
            { "name": "d", "kind": "diff", "uri": "b" }
        ]),
    ];
    for deliverables in cases {
        let f = fixture("deliverable-invalid", true);
        let (status, envelope) = call(
            &f.state,
            json!({
                "operation": "report",
                "outcome": "success",
                "summary": "done",
                "deliverables": deliverables
            }),
        );
        assert_eq!(status, OpStatus::Failed, "{envelope}");
        assert_eq!(envelope["error_kind"], "invalid_input", "{envelope}");
        assert_eq!(state_entries(&f.state), Vec::<String>::new());
    }
}

// ── the outcome vocabulary is closed ──────────────────────────────────────────

#[test]
fn an_unknown_outcome_is_refused_and_the_message_lists_the_four() {
    let f = fixture("unknown-outcome", true);
    let (status, envelope) = call(
        &f.state,
        json!({ "operation": "report", "outcome": "partial", "summary": "some of it" }),
    );
    assert_eq!(status, OpStatus::Failed, "{envelope}");
    assert_eq!(envelope["error_kind"], "unknown_outcome");
    let message = envelope["message"].as_str().expect("a message");
    for outcome in ["success", "rejected", "blocked", "failed"] {
        assert!(message.contains(outcome), "the message lists {outcome}: {message}");
    }
    assert_eq!(state_entries(&f.state), Vec::<String>::new());
}

#[test]
fn every_declared_outcome_is_accepted_and_none_is_branched_on() {
    for outcome in ["success", "rejected", "blocked", "failed"] {
        let f = fixture("outcomes", true);
        let (status, envelope) = call(
            &f.state,
            json!({ "operation": "report", "outcome": outcome, "summary": "s" }),
        );
        assert_eq!(status, OpStatus::Passed, "{outcome}: {envelope}");
        let doc = report(&f.state);
        assert_eq!(doc["outcome"], outcome);
        // The whole document is identical but for the outcome string: no arm of the
        // vocabulary changes what is written.
        assert_eq!(doc["concluded"], true);
        assert_eq!(doc["revision"], 1);
    }
}

#[test]
fn a_missing_outcome_or_summary_is_invalid_input_and_writes_nothing() {
    let cases: Vec<Value> = vec![
        json!({ "operation": "report", "summary": "no outcome" }),
        json!({ "operation": "report", "outcome": "success" }),
        json!({ "operation": "report", "outcome": "success", "summary": "" }),
        json!({ "operation": "report", "outcome": "success", "summary": "   " }),
        json!({ "operation": "report", "outcome": 7, "summary": "s" }),
        json!({ "operation": "progress" }),
        json!({ "operation": "progress", "note": "" }),
    ];
    for payload in cases {
        let f = fixture("required-fields", true);
        let (status, envelope) = call(&f.state, payload.clone());
        assert_eq!(status, OpStatus::Failed, "{payload}: {envelope}");
        assert_eq!(envelope["error_kind"], "invalid_input", "{payload}");
        assert_eq!(state_entries(&f.state), Vec::<String>::new(), "{payload}");
    }
}

// ── fail closed without the state grant ───────────────────────────────────────

#[test]
fn without_the_state_grant_a_valid_report_fails_closed_and_creates_nothing() {
    let f = fixture("no-state", false);
    let (status, envelope) = call(&f.state, a_report());

    assert_eq!(status, OpStatus::Error, "{envelope}");
    assert_eq!(envelope["error_kind"], "state_unavailable");
    assert!(
        envelope["message"]
            .as_str()
            .expect("a message")
            .contains("capabilities.state"),
        "the message names the grant: {envelope}"
    );
    assert!(!f.state.exists(), "the tool must never create the state directory");
    assert_eq!(
        std::fs::read_dir(&f.root).expect("read root").count(),
        0,
        "nothing is written anywhere"
    );
}

#[test]
fn without_the_state_grant_progress_fails_closed_too() {
    let f = fixture("no-state-progress", false);
    let (status, envelope) = call(&f.state, json!({ "operation": "progress", "note": "n" }));
    assert_eq!(status, OpStatus::Error, "{envelope}");
    assert_eq!(envelope["error_kind"], "state_unavailable");
    assert!(!f.state.exists());
}

// ── the operator's declared vocabulary ────────────────────────────────────────

const DECLARED: &str = r#"{"config_version":1,"require_report":true,
    "deliverables":{"kinds":["diff","dataset"]},"notes_for":{"stages":["review"]}}"#;

#[test]
fn a_declared_vocabulary_is_closed() {
    let f = fixture("declared-kind", true);
    let (status, envelope) = call_with(
        &f.state,
        Some(DECLARED),
        &identity(),
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "done",
            "deliverables": [{ "name": "d", "kind": "screenshot", "uri": "out/a.png" }]
        }),
    );
    assert_eq!(status, OpStatus::Failed, "{envelope}");
    assert_eq!(envelope["error_kind"], "undeclared_kind");
    assert_eq!(state_entries(&f.state), Vec::<String>::new());

    let f = fixture("declared-stage", true);
    let (status, envelope) = call_with(
        &f.state,
        Some(DECLARED),
        &identity(),
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "done",
            "notes_for": [{ "stage": "deploy", "body": "watch the migration" }]
        }),
    );
    assert_eq!(status, OpStatus::Failed, "{envelope}");
    assert_eq!(envelope["error_kind"], "undeclared_stage");
    assert_eq!(state_entries(&f.state), Vec::<String>::new());
}

#[test]
fn the_same_calls_pass_with_no_declaration_at_all() {
    let f = fixture("permissive", true);
    let (status, envelope) = call(
        &f.state,
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "done",
            "deliverables": [{ "name": "d", "kind": "screenshot", "uri": "out/a.png" }],
            "notes_for": [{ "stage": "deploy", "body": "watch the migration" }]
        }),
    );
    assert_eq!(status, OpStatus::Passed, "{envelope}");
    let doc = report(&f.state);
    assert_eq!(doc["deliverables"][0]["kind"], "screenshot");
    assert_eq!(doc["notes_for"][0]["stage"], "deploy");
}

#[test]
fn a_declared_value_inside_the_vocabulary_passes() {
    let f = fixture("declared-ok", true);
    let (status, envelope) = call_with(
        &f.state,
        Some(DECLARED),
        &identity(),
        json!({
            "operation": "report",
            "outcome": "success",
            "summary": "done",
            "deliverables": [{ "name": "d", "kind": "dataset", "uri": "out/rows.csv" }],
            "notes_for": [{ "stage": "review", "body": "check the schema" }]
        }),
    );
    assert_eq!(status, OpStatus::Passed, "{envelope}");
}

#[test]
fn require_report_is_validated_and_acted_on_nowhere() {
    let with_flag = fixture("require-on", true);
    call_with(
        &with_flag.state,
        Some(r#"{"config_version":1,"require_report":true}"#),
        &identity(),
        json!({ "operation": "report", "outcome": "success", "summary": "done" }),
    );
    let without_flag = fixture("require-off", true);
    call_with(
        &without_flag.state,
        Some(r#"{"config_version":1}"#),
        &identity(),
        json!({ "operation": "report", "outcome": "success", "summary": "done" }),
    );

    let normalise = |state: &Path| {
        let mut doc = report(state);
        // The one field that legitimately differs between two runs.
        doc["reported_at"] = Value::Null;
        doc
    };
    assert_eq!(
        normalise(&with_flag.state),
        normalise(&without_flag.state),
        "require_report must change nothing about the file that is written"
    );
}

#[test]
fn a_malformed_declaration_is_an_operator_error() {
    for config in [
        r#"{"config_version":99}"#,
        r#"{"config_version":1,"require_report":"yes"}"#,
    ] {
        let f = fixture("config-invalid", true);
        let (status, envelope) = call_with(
            &f.state,
            Some(config),
            &identity(),
            json!({ "operation": "report", "outcome": "success", "summary": "done" }),
        );
        assert_eq!(status, OpStatus::Error, "{config}: {envelope}");
        assert_eq!(envelope["error_kind"], "config_invalid", "{config}");
        let message = envelope["message"].as_str().expect("a message");
        assert!(message.contains("murmur.yaml"), "{message}");
        assert_eq!(state_entries(&f.state), Vec::<String>::new());
    }
}

// ── provenance ────────────────────────────────────────────────────────────────

#[test]
fn the_report_says_which_run_produced_it() {
    let f = fixture("stamped", true);
    call_with(
        &f.state,
        None,
        &RunIdentity::new(Some("sess-1".into()), Some("porter".into())),
        json!({ "operation": "report", "outcome": "success", "summary": "done" }),
    );
    let doc = report(&f.state);
    assert_eq!(doc["session_id"], "sess-1");
    assert_eq!(doc["capsule"], "porter");

    // A later session finds the previous run's report still sitting there, and its own
    // call restamps the file with the run that wrote it.
    call_with(
        &f.state,
        None,
        &RunIdentity::new(Some("sess-2".into()), Some("porter".into())),
        json!({ "operation": "progress", "note": "second run" }),
    );
    let doc = report(&f.state);
    assert_eq!(doc["session_id"], "sess-2");
    assert_eq!(doc["outcome"], "success", "the previous verdict is still there");
}

#[test]
fn an_absent_session_or_capsule_is_recorded_as_empty_and_never_fails_a_call() {
    let f = fixture("unstamped", true);
    let (status, _e) = call_with(
        &f.state,
        None,
        &RunIdentity::new(None, None),
        json!({ "operation": "report", "outcome": "success", "summary": "done" }),
    );
    assert_eq!(status, OpStatus::Passed);
    let doc = report(&f.state);
    assert_eq!(doc["session_id"], "");
    assert_eq!(doc["capsule"], "");
}

// ── the call itself ───────────────────────────────────────────────────────────

#[test]
fn an_unknown_operation_names_the_two_that_exist() {
    let f = fixture("unknown-op", true);
    for payload in [
        json!({ "operation": "read" }),
        json!({ "operation": "status" }),
    ] {
        let (status, envelope) = call(&f.state, payload);
        assert_eq!(status, OpStatus::Failed, "{envelope}");
        assert_eq!(envelope["error_kind"], "unknown_operation");
        let message = envelope["message"].as_str().expect("a message");
        assert!(message.contains("report") && message.contains("progress"), "{message}");
    }
    assert_eq!(state_entries(&f.state), Vec::<String>::new());
}

#[test]
fn unparseable_or_non_object_input_is_invalid_input() {
    let f = fixture("bad-input", true);
    for data in ["", "   ", "{ not json", "[1,2,3]", "\"just a string\""] {
        let response = ops::run(&f.state, None, &identity(), data);
        assert_eq!(response.status, OpStatus::Failed, "{data:?}");
        let envelope: Value = serde_json::from_str(&response.data).expect("JSON envelope");
        assert_eq!(envelope["error_kind"], "invalid_input", "{data:?}");
    }
    assert_eq!(state_entries(&f.state), Vec::<String>::new());
}

#[test]
fn a_double_encoded_payload_is_re_parsed_once() {
    let f = fixture("double-encoded", true);
    let inner = a_report().to_string();
    let response = ops::run(
        &f.state,
        None,
        &identity(),
        &Value::String(inner).to_string(),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert_eq!(report(&f.state)["outcome"], "success");
}
