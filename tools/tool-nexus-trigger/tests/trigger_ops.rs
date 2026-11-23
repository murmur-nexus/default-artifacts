//! Host tests of one call end to end: `ops::run` over a real [`Inbox`] on a temp folder, with a
//! fake Nexus window answering it (tests/common), or over a recording [`Exchange`] where the
//! request's bytes are the point.

mod common;

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tool_nexus_trigger::answer::{self, AWAITS_APPROVAL, PREFIX};
use tool_nexus_trigger::inbox::{
    rid, Clock, Inbox, Names, SystemClock, ANSWER_MAX_BYTES, ANSWER_WAIT, POLL_INTERVAL, RID_BYTES,
};
use tool_nexus_trigger::ops::{run, Exchange, Exchanged, Grant, OpStatus, Response};
use tool_nexus_trigger::request::{LABEL_MAX_CHARS, REQUEST_MAX_BYTES};

use common::{listing, FakeNexus, Mode};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Distinct request ids across every thread of the test binary.
fn next_random() -> [u8; RID_BYTES] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = Instant::now().elapsed().as_nanos() as u64;
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    (n.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ nanos ^ u64::from(std::process::id()) << 32)
        .to_be_bytes()
}

/// One call through a real inbox at `dir`, waiting at most `wait`.
fn call_with_wait(dir: &Path, input: Value, wait: Duration) -> Response {
    let mut random = next_random;
    let mut clock = SystemClock::new();
    let mut inbox = Inbox::new(dir, &mut random, &mut clock).with_wait(wait);
    run(Grant::Inbox, Some(&input.to_string()), &mut inbox)
}

fn call(dir: &Path, input: Value) -> Response {
    call_with_wait(dir, input, ANSWER_WAIT)
}

fn data(response: &Response) -> Value {
    serde_json::from_str(&response.data).expect("data is JSON")
}

/// An exchange that records what it was sent and answers with a fixed outcome.
struct Recording {
    sent: Vec<Vec<u8>>,
    reply: Exchanged,
}

impl Recording {
    fn answering(body: Value) -> Self {
        Self {
            sent: Vec::new(),
            reply: Exchanged::Answer(format!("{body}\n").into_bytes()),
        }
    }
    fn ok() -> Self {
        Self::answering(json!({"v": 1, "exit": 0, "out": ["ok"], "err": []}))
    }
    fn sent_text(&self) -> Vec<String> {
        self.sent
            .iter()
            .map(|bytes| String::from_utf8(bytes.clone()).unwrap())
            .collect()
    }
}

impl Exchange for Recording {
    fn exchange(&mut self, request: &[u8]) -> Exchanged {
        self.sent.push(request.to_vec());
        self.reply.clone()
    }
}

/// A clock that only moves when slept on.
struct FakeClock {
    now: Cell<Duration>,
    sleeps: u32,
}

impl Clock for FakeClock {
    fn elapsed(&self) -> Duration {
        self.now.get()
    }
    fn sleep(&mut self, duration: Duration) {
        self.sleeps += 1;
        self.now.set(self.now.get() + duration);
    }
}

// ── the happy path ───────────────────────────────────────────────────────────

#[test]
fn add_list_show_cancel_against_a_window() {
    let dir = tempfile::tempdir().unwrap();
    let nexus = FakeNexus::start(dir.path(), Mode::Answer);

    let added = call(
        dir.path(),
        json!({"op": "add", "label": "ready file", "every": "30s", "for": "1h", "until": "test -e ready.txt"}),
    );
    assert_eq!(added.status, OpStatus::Passed, "{added:?}");
    assert!(
        added.summary.starts_with(AWAITS_APPROVAL),
        "{}",
        added.summary
    );
    assert!(added
        .summary
        .ends_with(&format!("trigger t1 awaits approval: a person types /trigger approve t1 in a nexus window open on {}", dir.path().display())));
    let added_data = data(&added);
    assert_eq!(added_data["ok"], true);
    assert_eq!(added_data["op"], "add");
    assert_eq!(added_data["exit"], 0);
    assert_eq!(added_data["out"][1], "label ready file");
    assert_eq!(added_data["err"], json!([]));
    assert_eq!(
        added_data["summary"],
        added.summary.as_str(),
        "the model reads data"
    );
    assert_eq!(
        added_data.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["err", "exit", "ok", "op", "out", "summary"]
    );
    assert!(listing(dir.path()).is_empty());

    let listed = call(dir.path(), json!({"op": "list"}));
    assert_eq!(listed.status, OpStatus::Passed);
    assert!(listed
        .summary
        .starts_with("t1  awaiting-approval  ready file"));
    assert!(!listed.summary.starts_with(AWAITS_APPROVAL));
    assert!(listing(dir.path()).is_empty());

    let shown = call(dir.path(), json!({"op": "show", "id": "t1"}));
    assert_eq!(shown.status, OpStatus::Passed);
    assert_eq!(shown.summary, "t1  awaiting-approval  ready file");

    let unconfirmed = call(dir.path(), json!({"op": "cancel", "id": "t1"}));
    assert_eq!(unconfirmed.status, OpStatus::Failed);
    assert!(unconfirmed.summary.contains("nothing was cancelled"));
    assert_eq!(data(&unconfirmed)["exit"], 1);
    assert_eq!(data(&unconfirmed)["ok"], false);

    let cancelled = call(dir.path(), json!({"op": "cancel", "id": "t1", "yes": true}));
    assert_eq!(cancelled.status, OpStatus::Passed);
    assert_eq!(cancelled.summary, "cancelled trigger t1 · ready file");

    let missing = call(dir.path(), json!({"op": "show", "id": "t1"}));
    assert_eq!(missing.status, OpStatus::Failed);
    assert_eq!(
        missing.summary,
        format!("no trigger t1 in {}", dir.path().display())
    );

    assert_eq!(nexus.requests().len(), 6, "one request per call");
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn a_string_holding_the_object_is_unwrapped_once() {
    let mut io = Recording::ok();
    let input = json!(json!({"op": "list"}).to_string()).to_string();
    let response = run(Grant::Inbox, Some(&input), &mut io);
    assert_eq!(response.status, OpStatus::Passed);
    assert_eq!(io.sent_text(), [r#"{"v":1,"op":"list"}"#]);
}

// ── refusals, each before anything is written ────────────────────────────────

/// The input refused with `status` and `line`, nothing written to a real inbox, and no request
/// reaching a window.
fn assert_refused(grant: Grant, input: Option<&str>, status: OpStatus, line: &str) {
    let dir = tempfile::tempdir().unwrap();
    let nexus = FakeNexus::start(dir.path(), Mode::Answer);
    let mut random = next_random;
    let mut clock = SystemClock::new();
    let mut inbox = Inbox::new(dir.path(), &mut random, &mut clock);
    let response = run(grant, input, &mut inbox);
    assert_eq!(response.status, status, "{response:?}");
    assert_eq!(response.summary, line);
    assert!(response.summary.starts_with(PREFIX));
    assert!(response.summary.ends_with("Nothing was written."));
    let data = data(&response);
    assert_eq!(data["ok"], false);
    assert_eq!(data["error"], line);
    thread_settle();
    assert!(listing(dir.path()).is_empty(), "the inbox stays empty");
    assert!(nexus.requests().is_empty());
}

fn thread_settle() {
    std::thread::sleep(Duration::from_millis(30));
}

#[test]
fn whole_workdir_is_refused_first() {
    for input in [
        r#"{"op":"list"}"#,
        r#"{"op":"approve","id":"t1"}"#,
        "not json",
    ] {
        assert_refused(
            Grant::WholeWorkdir,
            Some(input),
            OpStatus::Error,
            answer::WHOLE_WORKDIR,
        );
    }
    assert_eq!(
        answer::WHOLE_WORKDIR,
        "tool-nexus-trigger: this entry reaches the whole work folder; give it capabilities.filesystem.scope: .nexus/triggers/inbox so it reaches the trigger inbox and nothing else. Nothing was written."
    );
}

#[test]
fn whole_workdir_creates_no_folder() {
    let parent = tempfile::tempdir().unwrap();
    let inbox_dir = parent.path().join(".nexus/triggers/inbox");
    let mut random = next_random;
    let mut clock = SystemClock::new();
    let mut inbox = Inbox::new(&inbox_dir, &mut random, &mut clock);
    let response = run(Grant::WholeWorkdir, Some(r#"{"op":"list"}"#), &mut inbox);
    assert_eq!(response.status, OpStatus::Error);
    assert!(listing(parent.path()).is_empty());
}

#[test]
fn input_that_is_not_an_object_is_refused() {
    for input in [
        None,
        Some(""),
        Some("  "),
        Some("not json"),
        Some("[1]"),
        Some("\"list\""),
    ] {
        assert_refused(
            Grant::Inbox,
            input,
            OpStatus::Failed,
            answer::INPUT_NOT_OBJECT,
        );
    }
}

#[test]
fn a_missing_op_is_refused() {
    for input in [r#"{}"#, r#"{"op": 1}"#, r#"{"op": null, "id": "t1"}"#] {
        assert_refused(Grant::Inbox, Some(input), OpStatus::Failed, answer::NO_OP);
    }
}

#[test]
fn approve_refuse_and_renew_are_a_persons() {
    for op in ["approve", "refuse", "renew"] {
        assert_refused(
            Grant::Inbox,
            Some(&json!({"op": op, "id": "t2"}).to_string()),
            OpStatus::Failed,
            &format!("tool-nexus-trigger: {op} happens only in a Nexus window, by a person: /trigger {op} t2. Nothing was written."),
        );
    }
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"approve"}"#),
        OpStatus::Failed,
        "tool-nexus-trigger: approve happens only in a Nexus window, by a person: /trigger approve <id>. Nothing was written.",
    );
}

#[test]
fn an_unknown_op_is_refused() {
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"delete","id":"t1"}"#),
        OpStatus::Failed,
        r#"tool-nexus-trigger: op "delete" is not one of add, list, show, cancel. Nothing was written."#,
    );
}

#[test]
fn show_and_cancel_need_an_id() {
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"show"}"#),
        OpStatus::Failed,
        "tool-nexus-trigger: show needs id, the trigger's id such as t3 (list shows them); show takes id. Nothing was written.",
    );
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"cancel","yes":true}"#),
        OpStatus::Failed,
        "tool-nexus-trigger: cancel needs id, the trigger's id such as t3 (list shows them); cancel takes id, yes. Nothing was written.",
    );
}

#[test]
fn a_field_the_op_does_not_take_is_refused() {
    // `project` is left out of the protocol: a window would open another board.
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"add","until":"true","project":"/elsewhere"}"#),
        OpStatus::Failed,
        r#"tool-nexus-trigger: add does not take "project"; add takes until, every, for, label, at, cron, wake, scope, reaches, onEvent, card, transitEvent, eventCard. Nothing was written."#,
    );
    // The shell's spelling is not the protocol's.
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"add","on_event":"pr-merged"}"#),
        OpStatus::Failed,
        r#"tool-nexus-trigger: add does not take "on_event"; add takes until, every, for, label, at, cron, wake, scope, reaches, onEvent, card, transitEvent, eventCard. Nothing was written."#,
    );
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"list","id":"t1"}"#),
        OpStatus::Failed,
        r#"tool-nexus-trigger: list does not take "id"; list takes no field but op. Nothing was written."#,
    );
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"show","id":"t1","yes":true}"#),
        OpStatus::Failed,
        r#"tool-nexus-trigger: show does not take "yes"; show takes id. Nothing was written."#,
    );
}

#[test]
fn a_non_string_field_is_refused() {
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"add","until":"true","every":30}"#),
        OpStatus::Failed,
        "tool-nexus-trigger: every must be text; add takes until, every, for, label, at, cron, wake, scope, reaches, onEvent, card, transitEvent, eventCard. Nothing was written.",
    );
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"show","id":3}"#),
        OpStatus::Failed,
        "tool-nexus-trigger: id must be text; show takes id. Nothing was written.",
    );
    assert_refused(
        Grant::Inbox,
        Some(r#"{"op":"cancel","id":"t1","yes":"true"}"#),
        OpStatus::Failed,
        answer::YES_NOT_BOOL,
    );
}

#[test]
fn null_fields_count_as_not_sent() {
    let mut io = Recording::ok();
    let input = json!({"op": "add", "until": "true", "label": null, "wake": null, "id": null});
    let response = run(Grant::Inbox, Some(&input.to_string()), &mut io);
    assert_eq!(response.status, OpStatus::Passed);
    assert_eq!(io.sent_text(), [r#"{"v":1,"op":"add","until":"true"}"#]);
}

// ── the two pre-write checks, at their boundaries ────────────────────────────

#[test]
fn a_label_of_80_characters_passes_and_81_is_refused() {
    // Multi-byte characters: the limit counts characters, not bytes.
    let label80: String = "é".repeat(LABEL_MAX_CHARS);
    let mut io = Recording::ok();
    let response = run(
        Grant::Inbox,
        Some(&json!({"op": "add", "until": "true", "label": label80}).to_string()),
        &mut io,
    );
    assert_eq!(response.status, OpStatus::Passed);
    assert_eq!(io.sent.len(), 1);

    let label81: String = "é".repeat(LABEL_MAX_CHARS + 1);
    assert_refused(
        Grant::Inbox,
        Some(&json!({"op": "add", "until": "true", "label": label81}).to_string()),
        OpStatus::Failed,
        "tool-nexus-trigger: label is 81 characters, over Nexus's limit of 80. Nothing was written.",
    );
}

#[test]
fn a_request_of_the_maximum_size_passes_and_one_byte_more_is_refused() {
    let base = r#"{"v":1,"op":"add","until":"true","wake":""}"#.len();
    let wake = "w".repeat(REQUEST_MAX_BYTES - base);
    let mut io = Recording::ok();
    let response = run(
        Grant::Inbox,
        Some(&json!({"op": "add", "until": "true", "wake": wake}).to_string()),
        &mut io,
    );
    assert_eq!(response.status, OpStatus::Passed);
    assert_eq!(io.sent[0].len(), REQUEST_MAX_BYTES);

    let wake = "w".repeat(REQUEST_MAX_BYTES - base + 1);
    assert_refused(
        Grant::Inbox,
        Some(&json!({"op": "add", "until": "true", "wake": wake}).to_string()),
        OpStatus::Failed,
        "tool-nexus-trigger: the request is 16385 bytes, over the trigger inbox's limit of 16384 bytes; shorten until, wake or label. Nothing was written.",
    );
}

// ── the exact request bytes ──────────────────────────────────────────────────

fn sent_for(input: Value) -> String {
    let mut io = Recording::ok();
    run(Grant::Inbox, Some(&input.to_string()), &mut io);
    assert_eq!(io.sent.len(), 1, "one request per call");
    io.sent_text().remove(0)
}

#[test]
fn request_bytes_per_source_kind() {
    // A command.
    assert_eq!(
        sent_for(
            json!({"op": "add", "label": "ready", "for": "1h", "every": "30s", "until": "test -e /w/ready.txt"})
        ),
        r#"{"v":1,"op":"add","until":"test -e /w/ready.txt","every":"30s","for":"1h","label":"ready"}"#
    );
    // --every with no command: a clock.
    assert_eq!(
        sent_for(
            json!({"op": "add", "every": "5m", "wake": "check the build", "scope": "session"})
        ),
        r#"{"v":1,"op":"add","every":"5m","wake":"check the build","scope":"session"}"#
    );
    // --cron.
    assert_eq!(
        sent_for(
            json!({"op": "add", "cron": "0 9 * * 1-5", "label": "standup", "wake": "post the standup"})
        ),
        r#"{"v":1,"op":"add","label":"standup","cron":"0 9 * * 1-5","wake":"post the standup"}"#
    );
    // --at.
    assert_eq!(
        sent_for(json!({"op": "add", "at": "+20m", "wake": "look again"})),
        r#"{"v":1,"op":"add","at":"+20m","wake":"look again"}"#
    );
    // A board source, with an event action.
    assert_eq!(
        sent_for(
            json!({"op": "add", "card": "4", "reaches": "Review", "every": "1m", "transitEvent": "review-ready", "eventCard": "5"})
        ),
        r#"{"v":1,"op":"add","every":"1m","reaches":"Review","card":"4","transitEvent":"review-ready","eventCard":"5"}"#
    );
    assert_eq!(
        sent_for(json!({"op": "add", "onEvent": "pr-merged", "card": "3"})),
        r#"{"v":1,"op":"add","onEvent":"pr-merged","card":"3"}"#
    );
    assert_eq!(sent_for(json!({"op": "list"})), r#"{"v":1,"op":"list"}"#);
    assert_eq!(
        sent_for(json!({"op": "show", "id": "t3"})),
        r#"{"v":1,"op":"show","id":"t3"}"#
    );
    assert_eq!(
        sent_for(json!({"yes": true, "id": "t3", "op": "cancel"})),
        r#"{"v":1,"op":"cancel","id":"t3","yes":true}"#
    );
    assert_eq!(
        sent_for(json!({"op": "cancel", "id": "t3", "yes": false})),
        r#"{"v":1,"op":"cancel","id":"t3","yes":false}"#
    );
    // Text is JSON-escaped, and non-ASCII kept as UTF-8.
    assert_eq!(
        sent_for(json!({"op": "add", "until": "test -f \"/tmp/my file\"", "label": "café\n"})),
        "{\"v\":1,\"op\":\"add\",\"until\":\"test -f \\\"/tmp/my file\\\"\",\"label\":\"café\\n\"}"
    );
}

#[test]
fn the_tool_adds_no_default() {
    assert_eq!(sent_for(json!({"op": "add"})), r#"{"v":1,"op":"add"}"#);
}

// ── no answer ────────────────────────────────────────────────────────────────

#[test]
fn no_answer_waits_exactly_the_answer_wait_and_removes_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut random = next_random;
    let mut clock = FakeClock {
        now: Cell::new(Duration::from_secs(1000)),
        sleeps: 0,
    };
    let mut inbox = Inbox::new(dir.path(), &mut random, &mut clock);
    let response = run(
        Grant::Inbox,
        Some(r#"{"op":"add","until":"true"}"#),
        &mut inbox,
    );
    assert_eq!(response.status, OpStatus::Error);
    assert_eq!(response.summary, answer::NO_ANSWER);
    assert_eq!(
        answer::NO_ANSWER,
        "tool-nexus-trigger: no Nexus window answered within 5 s, so the request was removed and no trigger was created. Only a Nexus window open on this work folder answers the trigger inbox (the transit daemon does not read it), and this entry's capabilities.filesystem.scope must be .nexus/triggers/inbox."
    );
    assert_eq!(data(&response)["op"], "add");
    assert_eq!(clock.now.get(), Duration::from_secs(1000) + ANSWER_WAIT);
    assert_eq!(
        clock.sleeps,
        (ANSWER_WAIT.as_millis() / POLL_INTERVAL.as_millis()) as u32
    );
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn no_answer_on_the_real_clock_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let wait = Duration::from_millis(300);
    let started = Instant::now();
    let response = call_with_wait(dir.path(), json!({"op": "list"}), wait);
    let took = started.elapsed();
    assert_eq!(response.summary, answer::NO_ANSWER);
    assert!(took >= wait, "{took:?}");
    assert!(
        took < wait + POLL_INTERVAL + Duration::from_millis(200),
        "{took:?}"
    );
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn a_taken_request_with_no_answer_says_a_trigger_may_exist() {
    let dir = tempfile::tempdir().unwrap();
    let nexus = FakeNexus::start(dir.path(), Mode::TakeNoAnswer);
    let response = call_with_wait(
        dir.path(),
        json!({"op": "add", "until": "true"}),
        Duration::from_millis(400),
    );
    assert_eq!(response.status, OpStatus::Error);
    assert_eq!(response.summary, answer::TAKEN_NO_ANSWER);
    assert_eq!(
        answer::TAKEN_NO_ANSWER,
        "tool-nexus-trigger: a Nexus window took the request but sent no answer within 5 s, so a trigger may have been created; call list to see."
    );
    assert!(!response.summary.contains("no trigger was created"));
    assert_eq!(nexus.requests().len(), 1);
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn a_folder_that_does_not_exist_cannot_be_written_and_is_not_made() {
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("inbox");
    let response = call(&missing, json!({"op": "list"}));
    assert_eq!(response.status, OpStatus::Error);
    assert!(response
        .summary
        .starts_with("tool-nexus-trigger: cannot write the trigger inbox: "));
    assert!(response.summary.ends_with(". Nothing was written."));
    assert!(!missing.exists(), "the tool never creates a folder");
}

#[test]
fn a_tmp_already_there_is_not_removed() {
    let dir = tempfile::tempdir().unwrap();
    let fixed = [0xab; RID_BYTES];
    let names = Names::new(dir.path(), &rid(fixed));
    fs::write(&names.request_tmp, b"someone else's").unwrap();
    let mut random = || fixed;
    let mut clock = SystemClock::new();
    let mut inbox = Inbox::new(dir.path(), &mut random, &mut clock);
    let response = run(Grant::Inbox, Some(r#"{"op":"list"}"#), &mut inbox);
    assert_eq!(response.status, OpStatus::Error);
    assert!(response
        .summary
        .starts_with("tool-nexus-trigger: cannot write the trigger inbox: "));
    assert_eq!(fs::read(&names.request_tmp).unwrap(), b"someone else's");
    assert_eq!(listing(dir.path()), ["abababababababab.tmp"]);
}

#[test]
fn rid_is_16_lowercase_hex_digits() {
    assert_eq!(
        rid([0, 1, 0x0a, 0xff, 0x10, 0x20, 0xab, 0xcd]),
        "00010aff1020abcd"
    );
    let names = Names::new(Path::new("."), "00010aff1020abcd");
    assert_eq!(names.request_tmp, Path::new("./00010aff1020abcd.tmp"));
    assert_eq!(names.request, Path::new("./00010aff1020abcd.json"));
    assert_eq!(names.answer, Path::new("./00010aff1020abcd.answer.json"));
}

// ── answers this tool does not read ──────────────────────────────────────────

fn assert_malformed(mode: Mode, reason: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let _nexus = FakeNexus::start(dir.path(), mode);
    let response = call(dir.path(), json!({"op": "list"}));
    assert_eq!(response.status, OpStatus::Error, "{response:?}");
    assert_eq!(
        response.summary,
        format!("tool-nexus-trigger: the answer from Nexus cannot be read ({reason}); the request may have taken effect, so call list to see.")
    );
    dir
}

#[test]
fn malformed_answers_are_errors_and_removed() {
    for (raw, reason) in [
        (&b"not json"[..], "not a JSON object"),
        (b"[1]", "not a JSON object"),
        (
            br#"{"v":1,"exit":0,"out":[]}"#,
            "its keys are not exactly v, exit, out and err",
        ),
        (
            br#"{"v":1,"exit":0,"out":[],"err":[],"confirm":{"id":"t1","label":"x"}}"#,
            "its keys are not exactly v, exit, out and err",
        ),
        (br#"{"v":2,"exit":0,"out":[],"err":[]}"#, "v is not 1"),
        (
            br#"{"v":1,"exit":"0","out":[],"err":[]}"#,
            "exit is not an integer",
        ),
        (
            br#"{"v":1,"exit":0,"out":[1],"err":[]}"#,
            "out is not a list of lines",
        ),
        (
            br#"{"v":1,"exit":0,"out":[],"err":"x"}"#,
            "err is not a list of lines",
        ),
    ] {
        let dir = assert_malformed(Mode::Raw(raw.to_vec()), reason);
        assert!(listing(dir.path()).is_empty());
    }
}

#[test]
fn an_oversized_answer_is_not_read() {
    let mut raw = br#"{"v":1,"exit":0,"out":[""#.to_vec();
    raw.resize(ANSWER_MAX_BYTES as usize + 1 - 7, b'x');
    raw.extend_from_slice(br#""],"err":[]}"#);
    let dir = assert_malformed(Mode::Raw(raw), "larger than 256 KiB");
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn an_answer_at_the_cap_is_read() {
    let mut raw = br#"{"v":1,"exit":0,"out":[""#.to_vec();
    let tail = br#""],"err":[]}"#;
    raw.resize(ANSWER_MAX_BYTES as usize - tail.len(), b'x');
    raw.extend_from_slice(tail);
    assert_eq!(raw.len() as u64, ANSWER_MAX_BYTES);
    let dir = tempfile::tempdir().unwrap();
    let _nexus = FakeNexus::start(dir.path(), Mode::Raw(raw));
    let response = call(dir.path(), json!({"op": "list"}));
    assert_eq!(response.status, OpStatus::Passed);
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn a_link_at_the_answer_name_is_not_followed() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.json");
    let body = br#"{"v":1,"exit":0,"out":["from outside"],"err":[]}"#;
    fs::write(&target, body).unwrap();
    let dir = assert_malformed(Mode::Symlink(target.clone()), "a link, not a file");
    assert!(listing(dir.path()).is_empty(), "the link is removed");
    assert_eq!(fs::read(&target).unwrap(), body, "the target is untouched");
}

#[test]
fn a_folder_at_the_answer_name_is_left_alone() {
    let dir = assert_malformed(Mode::Folder, "not a regular file");
    let names = listing(dir.path());
    assert_eq!(names.len(), 1);
    assert!(names[0].ends_with(".answer.json"));
    assert!(dir.path().join(&names[0]).is_dir());
}

// ── concurrency ──────────────────────────────────────────────────────────────

#[test]
fn parallel_calls_each_get_their_own_answer() {
    let dir = tempfile::tempdir().unwrap();
    let nexus = FakeNexus::start(dir.path(), Mode::Answer);
    let root = dir.path().to_path_buf();
    let handles: Vec<_> = (0..4)
        .map(|thread| {
            let root = root.clone();
            std::thread::spawn(move || {
                (0..3)
                    .map(|n| {
                        let label = format!("thread {thread} call {n}");
                        let response =
                            call(&root, json!({"op": "add", "until": "true", "label": label}));
                        (label, response)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut ids = Vec::new();
    for handle in handles {
        for (label, response) in handle.join().unwrap() {
            assert_eq!(response.status, OpStatus::Passed, "{response:?}");
            let data = data(&response);
            assert_eq!(data["out"][1], format!("label {label}"));
            ids.push(
                data["out"][0]
                    .as_str()
                    .unwrap()
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .to_string(),
            );
        }
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 12, "twelve distinct triggers");
    assert_eq!(nexus.requests().len(), 12);
    assert!(listing(dir.path()).is_empty());
}

// ── never reported active ────────────────────────────────────────────────────

#[test]
fn no_line_the_tool_writes_calls_a_trigger_active() {
    use tool_nexus_trigger::request::Op;
    let mut lines: Vec<String> = vec![
        AWAITS_APPROVAL.into(),
        answer::WHOLE_WORKDIR.into(),
        answer::NO_ANSWER.into(),
        answer::TAKEN_NO_ANSWER.into(),
        answer::INPUT_NOT_OBJECT.into(),
        answer::NO_OP.into(),
        answer::YES_NOT_BOOL.into(),
        answer::person_only("approve", Some("t1")),
        answer::unknown_op("x"),
        answer::label_too_long(81),
        answer::request_too_large(16385),
        answer::cannot_write("e"),
        answer::not_removed("e"),
        answer::malformed_answer("r"),
        answer::no_lines(0),
    ];
    for op in Op::ALL {
        lines.push(answer::unknown_field(op, "f"));
        lines.push(answer::not_text(op, "f"));
        lines.push(answer::missing_id(op));
    }
    // A passed add's whole result, with a Nexus line that says nothing of state.
    let added = answer::from_nexus(Op::Add, br#"{"v":1,"exit":0,"out":[],"err":[]}"#);
    lines.push(added.summary.clone());
    lines.push(added.data.clone());
    for line in &lines {
        let lower = line.to_lowercase();
        for word in ["active", "running now", "is running", "started"] {
            assert!(!lower.contains(word), "{line:?} contains {word:?}");
        }
        assert!(!line.contains('\n'), "{line:?} is one line");
    }
    assert!(AWAITS_APPROVAL.contains("approval"));
    assert_eq!(added.summary, AWAITS_APPROVAL);
    assert_eq!(
        AWAITS_APPROVAL,
        "Requested: it waits for a person's approval in a Nexus window and does not run until then."
    );
}

#[test]
fn a_failed_add_never_says_requested() {
    use tool_nexus_trigger::request::Op;
    let refused = answer::from_nexus(
        Op::Add,
        br#"{"v":1,"exit":1,"out":[],"err":["4 triggers already await approval in /w; approve or refuse one first"]}"#,
    );
    assert_eq!(refused.status, OpStatus::Failed);
    assert_eq!(
        refused.summary,
        "4 triggers already await approval in /w; approve or refuse one first"
    );
    let usage = answer::from_nexus(Op::Add, br#"{"v":1,"exit":2,"out":[],"err":[]}"#);
    assert_eq!(
        usage.summary,
        "tool-nexus-trigger: Nexus answered exit 2 with no lines."
    );
}
