//! Host-target scenario tests for `ops::run`: every operation, every `error_kind`, driven
//! through a recording fake `Transport` and (for `push`) a real loose-object repository built
//! by `git::test_fixture::Fixture`.
//!
//! These are the scenarios named in the card's spec that do not require the compiled wasm
//! component: config gates, the merge trailer and PR footer, push's fast-forward/force/
//! default-branch rules, and the packed/signed/gitlink refusals.

use std::cell::RefCell;
use std::path::Path;

use serde_json::{json, Value};
use tool_github::git::test_fixture::Fixture;
use tool_github::ops::{self, HttpReply, Response, Transport};

/// A scripted response: status, optional `Location`, and a JSON body.
#[derive(Clone)]
struct Scripted {
    status: u16,
    location: Option<String>,
    body: Value,
}

fn ok(body: Value) -> Scripted {
    Scripted {
        status: 200,
        location: None,
        body,
    }
}
fn created(body: Value) -> Scripted {
    Scripted {
        status: 201,
        location: None,
        body,
    }
}
fn status(code: u16, body: Value) -> Scripted {
    Scripted {
        status: code,
        location: None,
        body,
    }
}

/// A recorded request: method, path and the JSON body sent, if any.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

/// A fake GitHub: replies to requests in the scripted order, recording every one.
struct Fake {
    scripted: RefCell<Vec<Scripted>>,
    requests: RefCell<Vec<Recorded>>,
}

impl Fake {
    fn new(scripted: Vec<Scripted>) -> Self {
        Self {
            scripted: RefCell::new(scripted.into_iter().rev().collect()),
            requests: RefCell::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.borrow().clone()
    }
}

impl Transport for Fake {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<HttpReply, String> {
        self.requests.borrow_mut().push(Recorded {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers.to_vec(),
            body: body.cloned(),
        });
        let next = self
            .scripted
            .borrow_mut()
            .pop()
            .unwrap_or_else(|| panic!("fake ran out of scripted responses at {method} {path}"));
        Ok(HttpReply {
            status: next.status,
            location: next.location,
            body: next.body.to_string().into_bytes(),
        })
    }
}

const CONFIG: &str = r#"{"config_version":1,"repo":"acme/widgets"}"#;
const GATEWAY: &str = "http://127.0.0.1:1";

fn run(config: Option<&str>, data: &str, fake: &mut dyn Transport, root: &Path) -> Response {
    ops::run(Some(GATEWAY), config, data, fake, root)
}

fn data(response: &Response) -> Value {
    serde_json::from_str(&response.data).unwrap()
}

// ── gateway and config gates ────────────────────────────────────────────────

#[test]
fn no_config_block_is_config_invalid_with_no_requests() {
    let mut fake = Fake::new(vec![]);
    let response = ops::run(
        Some(GATEWAY),
        None,
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(data(&response)["error_kind"], json!("config_invalid"));
    assert!(fake.requests().is_empty());
}

#[test]
fn no_gateway_is_gateway_missing_with_no_requests() {
    let mut fake = Fake::new(vec![]);
    let response = ops::run(
        None,
        Some(CONFIG),
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(data(&response)["error_kind"], json!("gateway_missing"));
    assert!(fake.requests().is_empty());
}

#[test]
fn gateway_missing_names_the_tool_github_entry_and_the_credential() {
    let mut fake = Fake::new(vec![]);
    let response = ops::run(
        None,
        Some(CONFIG),
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    let message = data(&response)["message"].as_str().unwrap().to_string();
    assert!(
        message.contains("to the tool-github entry in the capsule manifest"),
        "{message}"
    );
    assert!(
        message.contains("mur config set -g credentials.GITHUB_TOKEN <token>"),
        "{message}"
    );
}

#[test]
fn the_user_agent_is_tool_github_with_no_version() {
    let user_agent = ops::standard_headers(None)
        .into_iter()
        .find(|(name, _)| name == "user-agent")
        .map(|(_, value)| value);
    assert_eq!(user_agent.as_deref(), Some("tool-github"));
}

#[test]
fn a_credential_shaped_config_key_is_refused() {
    let config = r#"{"config_version":1,"repo":"acme/widgets","token":"ghp_xxx"}"#;
    let mut fake = Fake::new(vec![]);
    let response = run(
        Some(config),
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(data(&response)["error_kind"], json!("config_invalid"));
    assert!(fake.requests().is_empty());
}

#[test]
fn every_request_carries_the_standard_headers_and_a_relative_path() {
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", "a".repeat(40).as_str())),
        ok(json!([])),
        ok(json!({"check_runs": []})),
    ]);
    let response = run(
        Some(CONFIG),
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(data(&response)["ok"], json!(true));
    let requests = fake.requests();
    assert_eq!(requests[0].path, "/repos/acme/widgets/pulls/1");
    let names: Vec<&str> = requests[0]
        .headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(names.contains(&"user-agent"));
    assert!(names.contains(&"accept"));
    assert!(names.contains(&"x-github-api-version"));
    assert!(!requests[0].path.starts_with("http"));
}

#[test]
fn a_3xx_response_is_upstream_error_and_not_followed() {
    let mut fake = Fake::new(vec![Scripted {
        status: 301,
        location: Some("https://example.com/moved".to_string()),
        body: Value::Null,
    }]);
    let response = run(
        Some(CONFIG),
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("upstream_error"));
    assert!(value["message"]
        .as_str()
        .unwrap()
        .contains("example.com/moved"));
    assert_eq!(fake.requests().len(), 1, "no redirect is ever followed");
}

// ── pr_create ────────────────────────────────────────────────────────────────

fn pr_json(number: u64, state: &str, sha: &str) -> Value {
    json!({
        "number": number,
        "html_url": format!("https://github.com/acme/widgets/pull/{number}"),
        "state": state,
        "mergeable": true,
        "title": "Add feature",
        "body": "does the thing",
        "user": {"login": "someone"},
        "head": {"sha": sha, "ref": "feature-x"},
        "base": {"ref": "main"},
    })
}

const FOOTER_LINE: &str = "Opened by our release bot";

fn footer_config() -> String {
    format!(r#"{{"config_version":1,"repo":"acme/widgets","pr_footer":"{FOOTER_LINE}"}}"#)
}

#[test]
fn pr_create_with_no_footer_sends_the_body_byte_for_byte() {
    let body = "does the thing\n\n  - with odd spacing  \n\n";
    let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
    let input = json!({"op": "pr_create", "title": "Add feature", "body": body, "head": "feature-x", "base": "main"});
    let response = run(Some(CONFIG), &input.to_string(), &mut fake, Path::new("."));
    assert_eq!(data(&response)["ok"], json!(true));
    let sent = fake.requests()[0].body.clone().unwrap();
    assert_eq!(sent["body"], json!(body));

    let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
    let input = r#"{"op":"pr_create","title":"Add feature","head":"feature-x","base":"main"}"#;
    run(Some(CONFIG), input, &mut fake, Path::new("."));
    let sent = fake.requests()[0].body.clone().unwrap();
    assert!(
        sent.get("body").is_none(),
        "no body given, no body field sent: {sent}"
    );
}

#[test]
fn pr_create_with_footer_off_sends_body_untouched() {
    let config = r#"{"config_version":1,"repo":"acme/widgets","pr_footer":false}"#;
    let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
    let input = r#"{"op":"pr_create","title":"Add feature","body":"does the thing","head":"feature-x","base":"main"}"#;
    run(Some(config), input, &mut fake, Path::new("."));
    let sent = fake.requests()[0].body.clone().unwrap();
    assert_eq!(sent["body"], json!("does the thing"));
}

#[test]
fn pr_create_with_a_footer_line_appends_it_once() {
    let config = footer_config();
    for (input, expected) in [
        (
            r#"{"op":"pr_create","title":"Add feature","body":"does the thing","head":"feature-x","base":"main"}"#,
            "does the thing\n\nOpened by our release bot",
        ),
        (
            r#"{"op":"pr_create","title":"Add feature","head":"feature-x","base":"main"}"#,
            FOOTER_LINE,
        ),
        (
            r#"{"op":"pr_create","title":"Add feature","body":"","head":"feature-x","base":"main"}"#,
            FOOTER_LINE,
        ),
        (
            r#"{"op":"pr_create","title":"Add feature","body":"does the thing\n\nOpened by our release bot","head":"feature-x","base":"main"}"#,
            "does the thing\n\nOpened by our release bot",
        ),
    ] {
        let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
        let response = run(Some(&config), input, &mut fake, Path::new("."));
        assert_eq!(data(&response)["ok"], json!(true), "{input}");
        let sent = fake.requests()[0].body.clone().unwrap();
        assert_eq!(sent["body"], json!(expected), "{input}");
    }
}

#[test]
fn pr_create_a_footer_that_is_not_one_line_is_config_invalid() {
    for footer in [
        json!(true),
        json!(""),
        json!("   "),
        json!("one\ntwo"),
        json!(7),
    ] {
        let config = json!({"config_version": 1, "repo": "acme/widgets", "pr_footer": footer});
        let mut fake = Fake::new(vec![]);
        let input = r#"{"op":"pr_create","title":"t","head":"feature-x","base":"main"}"#;
        let response = run(Some(&config.to_string()), input, &mut fake, Path::new("."));
        let value = data(&response);
        assert_eq!(value["error_kind"], json!("config_invalid"), "{footer}");
        assert!(
            value["message"].as_str().unwrap().contains("pr_footer"),
            "{footer}: {value}"
        );
        assert!(
            fake.requests().is_empty(),
            "{footer}: nothing may reach the upstream"
        );
    }
}

#[test]
fn pr_create_head_naming_a_fork_is_repo_not_allowed() {
    let mut fake = Fake::new(vec![]);
    let input = r#"{"op":"pr_create","title":"t","head":"someone-else:feature-x","base":"main"}"#;
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    assert_eq!(data(&response)["error_kind"], json!("repo_not_allowed"));
    assert!(fake.requests().is_empty());
}

#[test]
fn pr_create_draft_true_sends_draft_true() {
    let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
    let input =
        r#"{"op":"pr_create","title":"Add feature","head":"feature-x","base":"main","draft":true}"#;
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    assert_eq!(data(&response)["ok"], json!(true));
    let sent = fake.requests()[0].body.clone().unwrap();
    assert_eq!(sent["draft"], json!(true));
    // draft is never echoed back in the output.
    assert!(data(&response).get("draft").is_none());
}

#[test]
fn pr_create_draft_false_or_absent_sends_no_draft_field() {
    for input in [
        r#"{"op":"pr_create","title":"Add feature","head":"feature-x","base":"main","draft":false}"#,
        r#"{"op":"pr_create","title":"Add feature","head":"feature-x","base":"main"}"#,
    ] {
        let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
        run(Some(CONFIG), input, &mut fake, Path::new("."));
        let sent = fake.requests()[0].body.clone().unwrap();
        assert!(sent.get("draft").is_none(), "{input}: {sent}");
    }
}

#[test]
fn pr_create_a_non_boolean_draft_is_invalid_input_with_nothing_sent() {
    let mut fake = Fake::new(vec![]);
    let input = r#"{"op":"pr_create","title":"t","head":"feature-x","base":"main","draft":"yes"}"#;
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    assert_eq!(data(&response)["error_kind"], json!("invalid_input"));
    assert!(fake.requests().is_empty(), "nothing may reach the upstream");
}

#[test]
fn pr_create_draft_still_gets_the_footer_applied() {
    let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
    let input = r#"{"op":"pr_create","title":"Add feature","body":"does the thing","head":"feature-x","base":"main","draft":true}"#;
    run(Some(&footer_config()), input, &mut fake, Path::new("."));
    let sent = fake.requests()[0].body.clone().unwrap();
    assert_eq!(
        sent["body"],
        json!("does the thing\n\nOpened by our release bot")
    );
    assert_eq!(sent["draft"], json!(true));
}

// ── pr_view / pr_list ────────────────────────────────────────────────────────

#[test]
fn pr_view_reports_checks_and_reviews() {
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", "a".repeat(40).as_str())),
        ok(json!([{"user": {"login": "ada"}, "state": "APPROVED"}])),
        ok(json!({"check_runs": [{"name": "ci", "status": "completed", "conclusion": "success"}]})),
    ]);
    let response = run(
        Some(CONFIG),
        r#"{"op":"pr_view","number":1}"#,
        &mut fake,
        Path::new("."),
    );
    let value = data(&response);
    assert_eq!(value["number"], json!(1));
    assert_eq!(value["reviews"][0]["state"], json!("APPROVED"));
    assert_eq!(value["checks"][0]["conclusion"], json!("success"));
}

#[test]
fn pr_list_sends_query_parameters() {
    let mut fake = Fake::new(vec![ok(json!([pr_json(
        1,
        "open",
        "a".repeat(40).as_str()
    )]))]);
    let input = r#"{"op":"pr_list","state":"open","base":"main"}"#;
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    assert_eq!(data(&response)["prs"].as_array().unwrap().len(), 1);
    assert_eq!(
        fake.requests()[0].path,
        "/repos/acme/widgets/pulls?state=open&base=main"
    );
}

// ── pr_merge ─────────────────────────────────────────────────────────────────

const MERGE_CONFIG: &str = r#"{"config_version":1,"repo":"acme/widgets","allow_merge":true,"merge_methods":["merge","squash","rebase"]}"#;

#[test]
fn merge_without_allow_merge_is_operation_not_allowed() {
    let mut fake = Fake::new(vec![]);
    let response = run(
        Some(CONFIG),
        r#"{"op":"pr_merge","number":1,"expected_head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","method":"squash"}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(
        data(&response)["error_kind"],
        json!("operation_not_allowed")
    );
    assert!(fake.requests().is_empty());
}

#[test]
fn merge_with_a_disallowed_method_is_operation_not_allowed() {
    let config = r#"{"config_version":1,"repo":"acme/widgets","allow_merge":true,"merge_methods":["squash"]}"#;
    let mut fake = Fake::new(vec![]);
    let response = run(
        Some(config),
        r#"{"op":"pr_merge","number":1,"expected_head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","method":"merge"}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(
        data(&response)["error_kind"],
        json!("operation_not_allowed")
    );
    assert!(fake.requests().is_empty());
}

#[test]
fn every_merge_sends_the_expected_head_sha() {
    let sha = "b".repeat(40);
    // The PR's head as GitHub reports it now differs from what the caller saw: the caller's
    // SHA is the one sent, so GitHub, not this tool, decides whether the head moved.
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", &"e".repeat(40))),
        ok(json!({"merged": true, "sha": "c".repeat(40)})),
    ]);
    run(
        Some(MERGE_CONFIG),
        r#"{"op":"pr_merge","number":1,"expected_head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","method":"squash"}"#,
        &mut fake,
        Path::new("."),
    );
    let merge_request = &fake.requests()[1];
    assert_eq!(merge_request.body.as_ref().unwrap()["sha"], json!(sha));
}

#[test]
fn a_409_on_merge_is_head_moved() {
    let sha = "b".repeat(40);
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", &sha)),
        status(409, json!({"message": "moved"})),
    ]);
    let response = run(
        Some(MERGE_CONFIG),
        r#"{"op":"pr_merge","number":1,"expected_head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","method":"squash"}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(data(&response)["error_kind"], json!("head_moved"));
}

const TRAILER: &str = "Co-authored-by: Release Bot <bot@example.com>";

fn trailer_config(methods: &str) -> String {
    format!(
        r#"{{"config_version":1,"repo":"acme/widgets","allow_merge":true,"merge_methods":{methods},"merge_trailer":"{TRAILER}"}}"#
    )
}

#[test]
fn no_merge_trailer_sends_none_for_any_method() {
    for method in ["merge", "squash", "rebase"] {
        let mut fake = Fake::new(vec![
            ok(pr_json(1, "open", "c".repeat(40).as_str())),
            ok(json!({"merged": true, "sha": "d".repeat(40)})),
        ]);
        let input = format!(
            r#"{{"op":"pr_merge","number":1,"expected_head_sha":"{}","method":"{method}"}}"#,
            "b".repeat(40)
        );
        let response = run(Some(MERGE_CONFIG), &input, &mut fake, Path::new("."));
        assert_eq!(data(&response)["trailer"], Value::Null, "{method}");
        let merge_request = &fake.requests()[1];
        assert!(
            merge_request
                .body
                .as_ref()
                .unwrap()
                .get("commit_message")
                .is_none(),
            "{method}"
        );
    }
}

#[test]
fn a_configured_merge_trailer_is_appended_for_merge_and_squash() {
    for (method, expected) in [
        (
            "merge",
            "Add feature\n\nCo-authored-by: Release Bot <bot@example.com>",
        ),
        (
            "squash",
            "does the thing\n\nCo-authored-by: Release Bot <bot@example.com>",
        ),
    ] {
        let mut pr = pr_json(1, "open", "c".repeat(40).as_str());
        // Who opened the PR no longer matters: the configured trailer is always sent.
        pr["user"]["login"] = json!("release-bot");
        let mut fake = Fake::new(vec![
            ok(pr),
            ok(json!({"merged": true, "sha": "d".repeat(40)})),
        ]);
        let input = format!(
            r#"{{"op":"pr_merge","number":1,"expected_head_sha":"{}","method":"{method}"}}"#,
            "b".repeat(40)
        );
        let response = run(
            Some(&trailer_config(r#"["merge","squash"]"#)),
            &input,
            &mut fake,
            Path::new("."),
        );
        assert_eq!(data(&response)["trailer"], json!(TRAILER), "{method}");
        let merge_request = &fake.requests()[1];
        assert_eq!(
            merge_request.body.as_ref().unwrap()["commit_message"],
            json!(expected),
            "{method}"
        );
    }
}

#[test]
fn a_merge_trailer_that_is_not_a_trailer_line_is_config_invalid() {
    for trailer in [
        json!(true),
        json!("not a trailer"),
        json!(""),
        json!("A: b\nC: d"),
        json!(3),
    ] {
        let config = json!({
            "config_version": 1, "repo": "acme/widgets", "allow_merge": true,
            "merge_methods": ["squash"], "merge_trailer": trailer,
        });
        let mut fake = Fake::new(vec![]);
        let input = format!(
            r#"{{"op":"pr_merge","number":1,"expected_head_sha":"{}","method":"squash"}}"#,
            "b".repeat(40)
        );
        let response = run(Some(&config.to_string()), &input, &mut fake, Path::new("."));
        let value = data(&response);
        assert_eq!(value["error_kind"], json!("config_invalid"), "{trailer}");
        assert!(
            value["message"].as_str().unwrap().contains("merge_trailer"),
            "{trailer}: {value}"
        );
        assert!(
            fake.requests().is_empty(),
            "{trailer}: nothing may reach the upstream"
        );
    }
}

#[test]
fn rebase_never_sends_a_trailer_even_with_config_on() {
    let config = trailer_config(r#"["rebase"]"#);
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", "c".repeat(40).as_str())),
        ok(json!({"merged": true, "sha": "d".repeat(40)})),
    ]);
    let response = run(
        Some(&config),
        r#"{"op":"pr_merge","number":1,"expected_head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","method":"rebase"}"#,
        &mut fake,
        Path::new("."),
    );
    assert_eq!(data(&response)["trailer"], Value::Null);
    let merge_request = &fake.requests()[1];
    assert!(merge_request
        .body
        .as_ref()
        .unwrap()
        .get("commit_message")
        .is_none());
}

// ── push ─────────────────────────────────────────────────────────────────────

fn repo_json(default_branch: &str) -> Value {
    json!({ "default_branch": default_branch })
}

/// A fake GitHub that computes real git object hashes for blob/tree/commit creation, the same
/// way `git::test_fixture::Fixture` does — so a push test proves this tool's request bodies
/// really do byte-reproduce the local objects, not just that some sha was echoed back.
struct GithubEcho {
    default_branch: String,
    refs: RefCell<std::collections::HashMap<String, String>>,
    /// Commits the fake repository holds: those created through it, plus any seeded.
    commits: RefCell<std::collections::HashSet<String>>,
    requests: RefCell<Vec<Recorded>>,
}

impl GithubEcho {
    fn new(default_branch: &str) -> Self {
        Self {
            default_branch: default_branch.to_string(),
            refs: RefCell::new(std::collections::HashMap::new()),
            commits: RefCell::new(std::collections::HashSet::new()),
            requests: RefCell::new(Vec::new()),
        }
    }

    #[allow(dead_code)]
    fn requests(&self) -> Vec<Recorded> {
        self.requests.borrow().clone()
    }
}

fn hex_sha1(store: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(store);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn object_sha(kind: &str, content: &[u8]) -> String {
    let header = format!("{kind} {}\0", content.len());
    let mut store = header.into_bytes();
    store.extend_from_slice(content);
    hex_sha1(&store)
}

fn base64_decode(text: &str) -> Vec<u8> {
    const TABLE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let clean: Vec<u8> = text
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let values: Vec<u32> = chunk
            .iter()
            .map(|&b| TABLE.find(b as char).unwrap() as u32)
            .collect();
        let n = values
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, v)| acc | (v << (18 - 6 * i)));
        out.push((n >> 16) as u8);
        if values.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if values.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn hex_decode(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// The inverse of `civil_from_days` in `src/git/date.rs`, duplicated here: integration tests
/// compile against the crate as an external consumer and cannot reach `#[cfg(test)]` items.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Parse a `date` field GitHub's Git Data API takes (`YYYY-MM-DDTHH:MM:SS±HH:MM`) back into a
/// git identity line's `<seconds> <offset>` tail, the inverse of `to_iso8601`.
fn identity_tail(name: &str, email: &str, date: &str) -> String {
    let year: i64 = date[0..4].parse().unwrap();
    let month: i64 = date[5..7].parse().unwrap();
    let day: i64 = date[8..10].parse().unwrap();
    let hour: i64 = date[11..13].parse().unwrap();
    let minute: i64 = date[14..16].parse().unwrap();
    let second: i64 = date[17..19].parse().unwrap();
    let sign = &date[19..20];
    let oh: i64 = date[20..22].parse().unwrap();
    let om: i64 = date[23..25].parse().unwrap();
    let offset_minutes = if sign == "-" {
        -(oh * 60 + om)
    } else {
        oh * 60 + om
    };
    let days = days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    let seconds = local_seconds - offset_minutes * 60;
    format!("{name} <{email}> {seconds} {sign}{oh:02}{om:02}")
}

impl Transport for GithubEcho {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<HttpReply, String> {
        self.requests.borrow_mut().push(Recorded {
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            body: body.cloned(),
        });
        let reply = |status: u16, body: Value| HttpReply {
            status,
            location: None,
            body: body.to_string().into_bytes(),
        };

        if path.ends_with("/git/blobs") {
            let content = base64_decode(body.unwrap()["content"].as_str().unwrap());
            return Ok(reply(201, json!({"sha": object_sha("blob", &content)})));
        }
        if path.ends_with("/git/trees") {
            let mut content = Vec::new();
            for entry in body.unwrap()["tree"].as_array().unwrap() {
                // Like GitHub: only six-digit modes are accepted, and a directory is stored as `40000`.
                let mode = match entry["mode"].as_str().unwrap() {
                    "040000" => "40000",
                    mode @ ("100644" | "100755" | "120000" | "160000") => mode,
                    _ => {
                        return Ok(reply(
                            422,
                            json!({"message": "Must supply a valid tree.mode"}),
                        ))
                    }
                };
                let name = entry["path"].as_str().unwrap();
                let sha = entry["sha"].as_str().unwrap();
                content.extend_from_slice(format!("{mode} {name}\0").as_bytes());
                content.extend_from_slice(&hex_decode(sha));
            }
            return Ok(reply(201, json!({"sha": object_sha("tree", &content)})));
        }
        if path.ends_with("/git/commits") {
            let body = body.unwrap();
            let mut header = format!("tree {}\n", body["tree"].as_str().unwrap());
            for parent in body["parents"].as_array().unwrap() {
                header.push_str(&format!("parent {}\n", parent.as_str().unwrap()));
            }
            let author = &body["author"];
            let committer = &body["committer"];
            header.push_str(&format!(
                "author {}\n",
                identity_tail(
                    author["name"].as_str().unwrap(),
                    author["email"].as_str().unwrap(),
                    author["date"].as_str().unwrap()
                )
            ));
            header.push_str(&format!(
                "committer {}\n",
                identity_tail(
                    committer["name"].as_str().unwrap(),
                    committer["email"].as_str().unwrap(),
                    committer["date"].as_str().unwrap()
                )
            ));
            let content = format!("{header}\n{}", body["message"].as_str().unwrap());
            let sha = object_sha("commit", content.as_bytes());
            self.commits.borrow_mut().insert(sha.clone());
            return Ok(reply(201, json!({"sha": sha})));
        }
        if let Some(sha) = path.strip_prefix("/repos/acme/widgets/git/commits/") {
            return Ok(match self.commits.borrow().contains(sha) {
                true => reply(200, json!({"sha": sha})),
                false => reply(404, json!({"message": "Not Found"})),
            });
        }
        if path.contains("/git/refs/heads/") && method == "GET" {
            let branch = path.rsplit('/').next().unwrap();
            return Ok(match self.refs.borrow().get(branch) {
                Some(sha) => reply(200, json!({"object": {"sha": sha}})),
                None => reply(404, json!({"message": "not found"})),
            });
        }
        if path.ends_with("/git/refs") && method == "POST" {
            let body = body.unwrap();
            let ref_name = body["ref"].as_str().unwrap();
            let branch = ref_name.strip_prefix("refs/heads/").unwrap();
            let sha = body["sha"].as_str().unwrap().to_string();
            self.refs
                .borrow_mut()
                .insert(branch.to_string(), sha.clone());
            return Ok(reply(201, json!({"ref": ref_name, "object": {"sha": sha}})));
        }
        if path.contains("/git/refs/heads/") && method == "PATCH" {
            let branch = path.rsplit('/').next().unwrap();
            let body = body.unwrap();
            let sha = body["sha"].as_str().unwrap().to_string();
            self.refs
                .borrow_mut()
                .insert(branch.to_string(), sha.clone());
            return Ok(reply(200, json!({"object": {"sha": sha}})));
        }
        if path == "/repos/acme/widgets" {
            return Ok(reply(200, json!({"default_branch": self.default_branch})));
        }
        Ok(reply(
            404,
            json!({"message": format!("unhandled path {path}")}),
        ))
    }
}

#[test]
fn a_fresh_branch_push_uploads_every_object_and_creates_the_ref() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let _c1 = fx.commit("first");
    fx.write_file("a.txt", b"two");
    let c2 = fx.commit("second");

    let mut fake = GithubEcho::new("main");
    let input = format!(r#"{{"op":"push","head_sha":"{c2}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["created"], json!(true));
    assert_eq!(
        value["objects_uploaded"],
        json!(6),
        "2 blobs, 2 trees, 2 commits"
    );
    assert_eq!(value["head_sha"], json!(c2));
    assert_eq!(
        fake.refs.borrow().get("feature-x"),
        Some(&c2),
        "the fake independently recomputed the same sha as the local HEAD"
    );
}

#[test]
fn a_fast_forward_push_excludes_the_boundary_and_stays_a_fast_forward() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    fx.write_file("a.txt", b"two");
    let c2 = fx.commit("second");

    let mut fake = GithubEcho::new("main");
    fake.refs
        .borrow_mut()
        .insert("feature-x".to_string(), c1.clone());
    let input = format!(r#"{{"op":"push","head_sha":"{c2}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["created"], json!(false));
    assert_eq!(
        value["objects_uploaded"],
        json!(3),
        "only the new commit, tree and blob"
    );
    assert_eq!(fake.refs.borrow().get("feature-x"), Some(&c2));
}

#[test]
fn force_push_moves_the_ref_and_reports_created_false() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let _c1 = fx.commit("first");
    fx.write_file("a.txt", b"unrelated");
    let unrelated = fx.commit_root("unrelated");

    let config = r#"{"config_version":1,"repo":"acme/widgets","allow_force_push":true}"#;
    let mut fake = GithubEcho::new("main");
    let existing_remote = "f".repeat(40);
    fake.refs
        .borrow_mut()
        .insert("feature-x".to_string(), existing_remote);
    let input = format!(
        r#"{{"op":"push","head_sha":"{unrelated}","remote_branch":"feature-x","force":true}}"#
    );
    let response = run(Some(config), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["created"], json!(false));
    assert_eq!(fake.refs.borrow().get("feature-x"), Some(&unrelated));
}

#[test]
fn a_branch_off_a_signed_github_merge_pushes_only_its_own_commit() {
    // The default branch's tip is a squash merge GitHub made and signed; the new branch builds
    // on it. GitHub has the merge, so the push must neither re-upload nor refuse it.
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    fx.write_file("b.txt", b"unchanged");
    let c1 = fx.commit("first");
    let merge = fx.commit_signed("Squash merge (#1)");
    fx.write_file("a.txt", b"two");
    let work = fx.commit("new work");

    let mut fake = GithubEcho::new("main");
    fake.refs
        .borrow_mut()
        .insert("main".to_string(), merge.clone());
    fake.commits.borrow_mut().extend([c1, merge.clone()]);
    let input = format!(r#"{{"op":"push","head_sha":"{work}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["created"], json!(true));
    assert_eq!(
        value["objects_uploaded"],
        json!(3),
        "the new commit, its tree and the changed blob"
    );
    assert_eq!(fake.refs.borrow().get("feature-x"), Some(&work));
    let asked: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| r.path.contains("/git/commits/"))
        .map(|r| r.path)
        .collect();
    assert_eq!(
        asked,
        [
            format!("/repos/acme/widgets/git/commits/{work}"),
            format!("/repos/acme/widgets/git/commits/{merge}")
        ],
        "the walk stops at the first commit GitHub has"
    );
}

#[test]
fn pushing_a_commit_github_already_has_uploads_nothing() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let merge = fx.commit_signed("Squash merge (#1)");

    let mut fake = GithubEcho::new("main");
    fake.commits.borrow_mut().insert(merge.clone());
    let input = format!(r#"{{"op":"push","head_sha":"{merge}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["objects_uploaded"], json!(0));
    assert_eq!(fake.refs.borrow().get("feature-x"), Some(&merge));
}

#[test]
fn a_signed_commit_github_does_not_have_is_still_refused() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    let signed = fx.commit_signed("signed locally");

    let mut fake = GithubEcho::new("main");
    fake.commits.borrow_mut().insert(c1);
    let input = format!(r#"{{"op":"push","head_sha":"{signed}","remote_branch":"feature-x"}}"#);
    let value = data(&run(Some(CONFIG), &input, &mut fake, &fx.git_dir()));
    assert_eq!(value["error_kind"], json!("invalid_input"));
    assert!(value["message"].as_str().unwrap().contains(&signed));
    assert!(
        fake.requests().iter().all(|r| r.method == "GET"),
        "nothing is uploaded"
    );
}

#[test]
fn a_failed_commit_lookup_is_reported_as_the_upstream_error() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    let mut fake = Fake::new(vec![
        ok(repo_json("main")),
        status(404, json!({})),
        status(
            403,
            json!({"message": "API rate limit exceeded for user ID 1."}),
        ),
    ]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x"}}"#);
    let value = data(&run(Some(CONFIG), &input, &mut fake, &fx.git_dir()));
    assert_eq!(value["error_kind"], json!("rate_limited"));
    assert_eq!(fake.requests().len(), 3);
}

#[test]
fn pushing_the_default_branch_with_config_allowed_succeeds() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    let config = r#"{"config_version":1,"repo":"acme/widgets","allow_push_default_branch":true}"#;
    let mut fake = GithubEcho::new("main");
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"main"}}"#);
    let response = run(Some(config), &input, &mut fake, &fx.git_dir());
    assert_eq!(data(&response)["ok"], json!(true));
}

#[test]
fn pushing_the_default_branch_is_refused_without_config() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    let mut fake = Fake::new(vec![ok(repo_json("main"))]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"main"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    assert_eq!(
        data(&response)["error_kind"],
        json!("operation_not_allowed")
    );
    assert_eq!(fake.requests().len(), 1, "only the repo-info read happens");
}

#[test]
fn an_object_missing_from_every_store_refuses_before_any_upload() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let _c1 = fx.commit("first");
    let nonexistent = "f".repeat(40);
    let mut fake = Fake::new(vec![
        ok(repo_json("main")),
        status(404, json!({})),
        status(404, json!({})),
    ]);
    let input =
        format!(r#"{{"op":"push","head_sha":"{nonexistent}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("repo_unreadable"));
    assert!(value["message"].as_str().unwrap().contains(&nonexistent));
    assert!(
        fake.requests().iter().all(|r| r.method == "GET"),
        "only reads happen; nothing is uploaded"
    );
}

#[test]
fn objects_info_alternates_refuses_before_any_request() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    fx.write_alternates("/some/other/repo/.git/objects");
    let mut fake = Fake::new(vec![ok(repo_json("main"))]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("repo_unreadable"));
    assert!(value["message"].as_str().unwrap().contains("alternates"));
    assert!(fake.requests().is_empty(), "nothing may reach the upstream");
}

#[test]
fn an_unmatched_pack_file_refuses_before_any_request() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    fx.write_unmatched_pack_file();
    let mut fake = Fake::new(vec![ok(repo_json("main"))]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("repo_unreadable"));
    assert!(value["message"].as_str().unwrap().contains("no matching"));
    assert!(fake.requests().is_empty(), "nothing may reach the upstream");
}

#[test]
fn an_oversized_pack_set_refuses_before_any_request_or_read() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    // Larger than the pack-size ceiling, but sparse (`set_len`): if this test finishes at all,
    // the ceiling check never read the file's bytes.
    fx.write_oversized_pack_pair(300 * 1024 * 1024);
    let mut fake = Fake::new(vec![ok(repo_json("main"))]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("repo_unreadable"));
    assert!(value["message"].as_str().unwrap().contains("byte"));
    assert!(fake.requests().is_empty(), "nothing may reach the upstream");
}

#[test]
fn a_signed_commit_refuses_before_any_upload() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit_signed("signed");
    let mut fake = Fake::new(vec![
        ok(repo_json("main")),
        status(404, json!({})),
        status(404, json!({})),
    ]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("invalid_input"));
    assert!(value["message"].as_str().unwrap().contains("signed"));
}

#[test]
fn a_gitlink_refuses_before_any_upload() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    fx.write_gitlink("sub", &"a".repeat(40));
    let c1 = fx.commit("with a submodule");
    let mut fake = Fake::new(vec![
        ok(repo_json("main")),
        status(404, json!({})),
        status(404, json!({})),
    ]);
    let input = format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("invalid_input"));
    assert!(value["message"].as_str().unwrap().contains("gitlink"));
}

#[test]
fn non_fast_forward_without_force_is_conflict() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let _c1 = fx.commit("first");
    fx.write_file("a.txt", b"unrelated");
    let unrelated = fx.commit_root("unrelated");
    let remote_sha = "e".repeat(40);
    let mut fake = Fake::new(vec![
        ok(repo_json("main")),
        ok(json!({"object": {"sha": remote_sha}})),
    ]);
    let input = format!(r#"{{"op":"push","head_sha":"{unrelated}","remote_branch":"feature-x"}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    assert_eq!(data(&response)["error_kind"], json!("conflict"));
}

#[test]
fn force_without_allow_force_push_is_operation_not_allowed() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    let c1 = fx.commit("first");
    let mut fake = Fake::new(vec![ok(repo_json("main"))]);
    let input =
        format!(r#"{{"op":"push","head_sha":"{c1}","remote_branch":"feature-x","force":true}}"#);
    let response = run(Some(CONFIG), &input, &mut fake, &fx.git_dir());
    assert_eq!(
        data(&response)["error_kind"],
        json!("operation_not_allowed")
    );
    assert_eq!(fake.requests().len(), 1);
}

// ── contract fields and error kinds ──────────────────────────────────────────

#[test]
fn pr_create_returns_number_url_and_head_sha() {
    let mut fake = Fake::new(vec![created(pr_json(9, "open", "a".repeat(40).as_str()))]);
    let input = r#"{"op":"pr_create","title":"Add feature","head":"feature-x","base":"main"}"#;
    let value = data(&run(Some(CONFIG), input, &mut fake, Path::new(".")));
    assert_eq!(value["number"], json!(9));
    assert_eq!(
        value["url"],
        json!("https://github.com/acme/widgets/pull/9")
    );
    assert_eq!(value["head_sha"], json!("a".repeat(40)));
}

#[test]
fn pr_merge_returns_merged_merge_sha_and_trailer() {
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", &"b".repeat(40))),
        ok(json!({"merged": true, "sha": "c".repeat(40)})),
    ]);
    let input = format!(
        r#"{{"op":"pr_merge","number":1,"expected_head_sha":"{}","method":"squash"}}"#,
        "b".repeat(40)
    );
    let value = data(&run(
        Some(&trailer_config(r#"["squash"]"#)),
        &input,
        &mut fake,
        Path::new("."),
    ));
    assert_eq!(value["merged"], json!(true));
    assert_eq!(value["merge_sha"], json!("c".repeat(40)));
    assert!(value.get("sha").is_none());
    assert_eq!(value["trailer"], json!(TRAILER));
}

#[test]
fn pr_merge_without_a_method_uses_the_first_configured_one() {
    let config = r#"{"config_version":1,"repo":"acme/widgets","allow_merge":true,"merge_methods":["squash","merge"]}"#;
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", &"b".repeat(40))),
        ok(json!({"merged": true, "sha": "c".repeat(40)})),
    ]);
    let input = format!(
        r#"{{"op":"pr_merge","number":1,"expected_head_sha":"{}"}}"#,
        "b".repeat(40)
    );
    run(Some(config), &input, &mut fake, Path::new("."));
    assert_eq!(
        fake.requests()[1].body.as_ref().unwrap()["merge_method"],
        json!("squash")
    );
}

#[test]
fn a_405_on_merge_is_not_mergeable() {
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", &"b".repeat(40))),
        status(405, json!({"message": "Pull Request is not mergeable"})),
    ]);
    let input = format!(
        r#"{{"op":"pr_merge","number":1,"expected_head_sha":"{}","method":"squash"}}"#,
        "b".repeat(40)
    );
    let value = data(&run(Some(MERGE_CONFIG), &input, &mut fake, Path::new(".")));
    assert_eq!(value["error_kind"], json!("not_mergeable"));
}

#[test]
fn upstream_statuses_map_to_their_own_error_kinds() {
    let cases = [
        (401, "Bad credentials", "upstream_unauthorized"),
        (
            403,
            "Resource not accessible by personal access token",
            "upstream_forbidden",
        ),
        (
            403,
            "API rate limit exceeded for user ID 1.",
            "rate_limited",
        ),
        (
            403,
            "You have exceeded a secondary rate limit.",
            "rate_limited",
        ),
        (429, "Too Many Requests", "rate_limited"),
        (404, "Not Found", "not_found"),
        (500, "Server Error", "upstream_error"),
    ];
    for (code, message, expected) in cases {
        let mut fake = Fake::new(vec![status(code, json!({"message": message}))]);
        let value = data(&run(
            Some(CONFIG),
            r#"{"op":"pr_view","number":1}"#,
            &mut fake,
            Path::new("."),
        ));
        assert_eq!(
            value["error_kind"],
            json!(expected),
            "HTTP {code} {message:?}"
        );
        assert_eq!(value["http_status"], json!(code));
    }
}

#[test]
fn push_from_a_directory_that_is_not_a_repository_is_repo_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![]);
    let input = format!(
        r#"{{"op":"push","head_sha":"{}","remote_branch":"feature-x"}}"#,
        "a".repeat(40)
    );
    let value = data(&run(Some(CONFIG), &input, &mut fake, dir.path()));
    assert_eq!(value["error_kind"], json!("repo_unreadable"));
    assert!(
        fake.requests().is_empty(),
        "nothing is sent for an unreadable repository"
    );
}

// ── strict-mode input: every schema property on every call ──────────────────

/// The full 17-property input a strict-mode model (OpenAI's Responses API, with no `strict`
/// field on the tool) sends on every call, captured from a `gpt-5.6-luna` session: the op's own
/// fields set, every other property `""`, `0` or `false`.
fn strict_form(fields: Value) -> String {
    let mut full = json!({
        "base": "", "body": "", "branch": "", "draft": false, "expected_head_sha": "",
        "force": false, "head": "", "head_sha": "", "message": "", "method": "", "number": 0,
        "op": "", "remote": "", "remote_branch": "", "repo_path": "", "state": "", "title": ""
    });
    assert_eq!(full.as_object().unwrap().len(), 17);
    for (key, value) in fields.as_object().unwrap() {
        full[key] = value.clone();
    }
    full.to_string()
}

type Sent = Vec<(String, String, Option<Value>)>;

fn sent(requests: Vec<Recorded>) -> Sent {
    requests
        .into_iter()
        .map(|r| (r.method, r.path, r.body))
        .collect()
}

/// Run `sparse` and its strict form against two copies of the same scripted fake, and assert
/// both send the same requests and return the same result.
fn assert_strict_form_matches(config: &str, sparse: Value, script: Vec<Scripted>) -> Response {
    let mut fake = Fake::new(script.clone());
    let expected = run(Some(config), &sparse.to_string(), &mut fake, Path::new("."));
    let mut strict_fake = Fake::new(script);
    let actual = run(
        Some(config),
        &strict_form(sparse.clone()),
        &mut strict_fake,
        Path::new("."),
    );
    assert_eq!(actual, expected, "{sparse}");
    assert_eq!(sent(strict_fake.requests()), sent(fake.requests()));
    actual
}

#[test]
fn strict_form_push_sends_what_the_sparse_push_sends() {
    let fx = Fixture::new();
    fx.write_file("a.txt", b"one");
    fx.write_file("dir/b.txt", b"sub");
    let head = fx.commit("first");
    let sparse = json!({"op": "push", "head_sha": head, "remote_branch": "feature-x"});

    let mut fake = GithubEcho::new("main");
    let expected = run(Some(CONFIG), &sparse.to_string(), &mut fake, &fx.git_dir());
    let mut strict_fake = GithubEcho::new("main");
    let actual = run(
        Some(CONFIG),
        &strict_form(sparse),
        &mut strict_fake,
        &fx.git_dir(),
    );
    assert_eq!(data(&actual)["ok"], json!(true), "{}", actual.data);
    assert_eq!(actual, expected);
    assert_eq!(sent(strict_fake.requests()), sent(fake.requests()));
    assert_eq!(strict_fake.refs.borrow().get("feature-x"), Some(&head));
}

#[test]
fn strict_form_fetch_sends_what_the_sparse_fetch_sends() {
    let run_fetch = |input: &str| {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let head = fx.commit("first");
        let mut fake = Fake::new(vec![
            ok(repo_json("main")),
            ok(json!({"object": {"sha": head}})),
        ]);
        let response = run(Some(CONFIG), input, &mut fake, &fx.git_dir());
        let written = std::fs::read_to_string(fx.git_dir().join("refs/remotes/origin/main"));
        (response, sent(fake.requests()), written.ok())
    };
    let sparse = json!({"op": "fetch"});
    let expected = run_fetch(&sparse.to_string());
    let actual = run_fetch(&strict_form(sparse));
    assert_eq!(data(&actual.0)["ok"], json!(true), "{}", actual.0.data);
    assert_eq!(
        data(&actual.0)["branch"],
        json!("main"),
        "an empty branch means the default branch"
    );
    assert_eq!(actual, expected);
}

#[test]
fn strict_form_pr_create_sends_what_the_sparse_pr_create_sends() {
    let script = vec![created(pr_json(9, "open", &"a".repeat(40)))];
    let sparse = json!({"op": "pr_create", "title": "Add feature", "head": "feature-x", "base": "main", "draft": true});
    let response = assert_strict_form_matches(CONFIG, sparse, script.clone());
    assert_eq!(data(&response)["number"], json!(9));

    let mut fake = Fake::new(script);
    let strict = strict_form(
        json!({"op": "pr_create", "title": "Add feature", "head": "feature-x", "base": "main"}),
    );
    run(Some(CONFIG), &strict, &mut fake, Path::new("."));
    let body = fake.requests()[0].body.clone().unwrap();
    assert!(
        body.get("body").is_none() && body.get("draft").is_none(),
        "empty body and draft false send neither field: {body}"
    );
}

#[test]
fn strict_form_pr_view_sends_what_the_sparse_pr_view_sends() {
    let script = vec![
        ok(pr_json(1, "open", &"a".repeat(40))),
        ok(json!([{"user": {"login": "ada"}, "state": "APPROVED"}])),
        ok(json!({"check_runs": [{"name": "ci", "status": "completed", "conclusion": "success"}]})),
    ];
    let response =
        assert_strict_form_matches(CONFIG, json!({"op": "pr_view", "number": 1}), script);
    assert_eq!(data(&response)["reviews"][0]["state"], json!("APPROVED"));
}

#[test]
fn strict_form_pr_list_sends_what_the_sparse_pr_list_sends() {
    let script = vec![ok(json!([pr_json(1, "open", &"a".repeat(40))]))];
    let sparse = json!({"op": "pr_list", "state": "open", "base": "main"});
    assert_strict_form_matches(CONFIG, sparse, script.clone());

    let mut fake = Fake::new(script);
    let strict = strict_form(json!({"op": "pr_list", "state": "open", "base": "main"}));
    run(Some(CONFIG), &strict, &mut fake, Path::new("."));
    assert_eq!(
        fake.requests()[0].path,
        "/repos/acme/widgets/pulls?state=open&base=main",
        "an empty head sends no head parameter"
    );
}

#[test]
fn strict_form_pr_merge_sends_what_the_sparse_pr_merge_sends() {
    let script = vec![
        ok(pr_json(1, "open", &"b".repeat(40))),
        ok(json!({"merged": true, "sha": "c".repeat(40)})),
    ];
    let sparse = json!({"op": "pr_merge", "number": 1, "expected_head_sha": "b".repeat(40)});
    let response = assert_strict_form_matches(MERGE_CONFIG, sparse, script);
    let value = data(&response);
    assert_eq!(value["merged"], json!(true));
    assert_eq!(value["trailer"], json!(null));
}

#[test]
fn strict_form_with_a_non_empty_foreign_field_is_still_refused() {
    let mut fake = Fake::new(vec![]);
    let input = strict_form(json!({"op": "pr_view", "number": 1, "title": "stray"}));
    let response = run(Some(CONFIG), &input, &mut fake, Path::new("."));
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("invalid_input"), "{value}");
    assert_eq!(
        value["message"],
        json!("'title' is not a recognised field for this op")
    );
    assert!(fake.requests().is_empty());
}
