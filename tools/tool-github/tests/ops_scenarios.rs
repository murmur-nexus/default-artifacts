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

/// A fake GitHub that replies with raw outcomes in order: a reply whose body is sent as given
/// (so it need not be JSON), or a transport error. It records each request's path.
struct RawFake {
    replies: Vec<RawReply>,
    paths: Vec<String>,
}

impl RawFake {
    fn new(mut replies: Vec<RawReply>) -> Self {
        replies.reverse();
        Self {
            replies,
            paths: Vec::new(),
        }
    }
}

impl Transport for RawFake {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Option<&Value>,
    ) -> Result<HttpReply, String> {
        self.paths.push(path.to_string());
        self.replies
            .pop()
            .unwrap_or_else(|| panic!("raw fake ran out of replies at {method} {path}"))
    }
}

/// One outcome a [`RawFake`] hands back: a reply, or a transport error.
type RawReply = Result<HttpReply, String>;

fn raw(status: u16, location: Option<&str>, body: &[u8]) -> RawReply {
    Ok(HttpReply {
        status,
        location: location.map(str::to_string),
        body: body.to_vec(),
    })
}

fn raw_json(status: u16, body: Value) -> RawReply {
    raw(status, None, body.to_string().as_bytes())
}

const PR_VIEW: &str = r#"{"op":"pr_view","number":1}"#;

/// The three paths `pr_view` of PR 1 sends, in order, for a head of `a` × 40.
fn pr_view_paths() -> Vec<String> {
    vec![
        "/repos/acme/widgets/pulls/1".to_string(),
        "/repos/acme/widgets/pulls/1/reviews".to_string(),
        format!("/repos/acme/widgets/commits/{}/check-runs", "a".repeat(40)),
    ]
}

/// `pr_json` plus the two fields only `GET /pulls/{n}` carries.
fn pr_view_json(draft: bool, mergeable_state: &str) -> Value {
    let mut pr = pr_json(1, "open", &"a".repeat(40));
    pr["draft"] = json!(draft);
    pr["mergeable_state"] = json!(mergeable_state);
    pr
}

/// GitHub's real answer to a fine-grained token listing check runs on a private repo.
fn checks_forbidden() -> Value {
    json!({
        "message": "Resource not accessible by personal access token",
        "documentation_url": "https://docs.github.com/rest/checks/runs#list-check-runs-for-a-git-reference",
        "status": "403"
    })
}

fn checks_forbidden_entry() -> Value {
    json!({
        "field": "checks",
        "error_kind": "upstream_forbidden",
        "http_status": 403,
        "message": "GitHub answered HTTP 403: Resource not accessible by personal access token",
        "needs": "Checks: read"
    })
}

fn paths(requests: &[Recorded]) -> Vec<String> {
    requests.iter().map(|r| r.path.clone()).collect()
}

#[test]
fn pr_view_with_every_scope_answers_every_field_and_misses_nothing() {
    let mut fake = Fake::new(vec![
        ok(pr_view_json(false, "clean")),
        ok(json!([{"user": {"login": "ada"}, "state": "APPROVED"}])),
        ok(json!({"check_runs": [{"name": "ci", "status": "completed", "conclusion": "success"}]})),
    ]);
    let response = run(Some(CONFIG), PR_VIEW, &mut fake, Path::new("."));
    assert_eq!(response.status, ops::OpStatus::Passed);
    assert_eq!(response.summary, "PR #1 is open");
    let value = data(&response);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["missing"], json!([]));
    assert_eq!(
        value["reviews"],
        json!([{"user": "ada", "state": "APPROVED"}])
    );
    assert_eq!(
        value["checks"],
        json!([{"name": "ci", "status": "completed", "conclusion": "success"}])
    );
    assert_eq!(value["number"], json!(1));
    assert_eq!(value["state"], json!("open"));
    assert_eq!(value["mergeable"], json!(true));
    assert_eq!(value["head_sha"], json!("a".repeat(40)));
    assert_eq!(value["title"], json!("Add feature"));
    assert_eq!(value["body"], json!("does the thing"));
    assert_eq!(
        value["url"],
        json!("https://github.com/acme/widgets/pull/1")
    );
    assert_eq!(value["head_ref"], json!("feature-x"));
    assert_eq!(value["base_ref"], json!("main"));
    assert_eq!(value["draft"], json!(false));
    assert_eq!(value["mergeable_state"], json!("clean"));
    assert_eq!(value.as_object().unwrap().len(), 15, "{value}");
    assert_eq!(paths(&fake.requests()), pr_view_paths());
    assert!(fake.requests().iter().all(|r| r.method == "GET"));
}

#[test]
fn pr_view_without_checks_read_passes_with_checks_null_and_one_missing_entry() {
    let mut fake = Fake::new(vec![
        ok(pr_view_json(false, "unstable")),
        ok(json!([{"user": {"login": "ada"}, "state": "APPROVED"}])),
        status(403, checks_forbidden()),
    ]);
    let response = run(Some(CONFIG), PR_VIEW, &mut fake, Path::new("."));
    assert_eq!(response.status, ops::OpStatus::Passed);
    assert_eq!(
        response.summary,
        "PR #1 is open; missing checks (HTTP 403, needs Checks: read)"
    );
    let value = data(&response);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["state"], json!("open"));
    assert_eq!(value["head_sha"], json!("a".repeat(40)));
    assert_eq!(value["mergeable_state"], json!("unstable"));
    assert_eq!(value["reviews"][0]["state"], json!("APPROVED"));
    assert_eq!(value["checks"], Value::Null);
    assert_eq!(value["missing"], json!([checks_forbidden_entry()]));
    assert_eq!(paths(&fake.requests()), pr_view_paths());
    assert_eq!(
        response.metadata,
        vec![
            ("state_effect".to_string(), "read".to_string()),
            (
                "resource_id".to_string(),
                "github:acme/widgets:pr:1".to_string()
            ),
        ]
    );
}

#[test]
fn pr_view_with_reviews_and_checks_both_refused_misses_both_in_request_order() {
    let refused = json!({"message": "Resource not accessible by integration"});
    let mut fake = Fake::new(vec![
        ok(pr_view_json(true, "draft")),
        status(403, refused.clone()),
        status(403, refused),
    ]);
    let response = run(Some(CONFIG), PR_VIEW, &mut fake, Path::new("."));
    assert_eq!(response.status, ops::OpStatus::Passed);
    assert_eq!(
        response.summary,
        "PR #1 is open; missing reviews (HTTP 403, needs Pull requests: read), \
         checks (HTTP 403, needs Checks: read)"
    );
    let value = data(&response);
    assert_eq!(value["reviews"], Value::Null);
    assert_eq!(value["checks"], Value::Null);
    assert_eq!(value["draft"], json!(true));
    let message = "GitHub answered HTTP 403: Resource not accessible by integration";
    assert_eq!(
        value["missing"],
        json!([
            {"field": "reviews", "error_kind": "upstream_forbidden", "http_status": 403,
             "message": message, "needs": "Pull requests: read"},
            {"field": "checks", "error_kind": "upstream_forbidden", "http_status": 403,
             "message": message, "needs": "Checks: read"},
        ])
    );
    assert_eq!(
        paths(&fake.requests()),
        pr_view_paths(),
        "check runs are still read after reviews fail"
    );
}

#[test]
fn pr_view_check_runs_failures_each_leave_checks_null_with_the_fatal_kind_and_message() {
    let pr = || raw_json(200, pr_view_json(false, "blocked"));
    let reviews = || raw_json(200, json!([]));
    let cases: Vec<(&str, RawReply, &str, Option<u16>)> = vec![
        (
            "404",
            raw_json(404, json!({"message": "Not Found"})),
            "not_found",
            Some(404),
        ),
        (
            "500",
            raw_json(500, json!({"message": "Server Error"})),
            "upstream_error",
            Some(500),
        ),
        (
            "secondary rate limit",
            raw_json(
                403,
                json!({"message": "You have exceeded a secondary rate limit."}),
            ),
            "rate_limited",
            Some(403),
        ),
        (
            "301",
            raw(301, Some("https://example.com/moved"), b""),
            "upstream_error",
            None,
        ),
        (
            "non-JSON 200",
            raw(200, None, b"<html>oops</html>"),
            "response_invalid",
            None,
        ),
        (
            "transport error",
            Err("connection reset".to_string()),
            "transport_error",
            None,
        ),
        (
            "422 with errors",
            raw_json(
                422,
                json!({"message": "Validation Failed", "errors": [{"field": "sha", "code": "invalid"}]}),
            ),
            "upstream_error",
            Some(422),
        ),
    ];
    for (name, reply, expected_kind, expected_status) in cases {
        let mut fake = RawFake::new(vec![pr(), reviews(), reply]);
        let response = ops::run(
            Some(GATEWAY),
            Some(CONFIG),
            PR_VIEW,
            &mut fake,
            Path::new("."),
        );
        assert_eq!(response.status, ops::OpStatus::Passed, "{name}");
        let value = data(&response);
        assert_eq!(value["ok"], json!(true), "{name}");
        assert_eq!(value["checks"], Value::Null, "{name}");
        assert_eq!(value["reviews"], json!([]), "{name}");
        assert_eq!(fake.paths, pr_view_paths(), "{name}");
        let missing = value["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 1, "{name}: {value}");
        let entry = missing[0].as_object().unwrap();
        assert_eq!(entry["field"], json!("checks"), "{name}");
        assert_eq!(entry["error_kind"], json!(expected_kind), "{name}");
        assert_eq!(entry["needs"], json!("Checks: read"), "{name}");
        assert_eq!(
            entry.get("http_status").cloned(),
            expected_status.map(|s| json!(s)),
            "{name}"
        );
        let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut want = vec!["error_kind", "field", "message", "needs"];
        if expected_status.is_some() {
            want.push("http_status");
        }
        want.sort_unstable();
        assert_eq!(keys, want, "{name}: no errors or errors_omitted key");
        let cause = match expected_status {
            Some(code) => format!("HTTP {code}"),
            None => expected_kind.to_string(),
        };
        assert_eq!(
            response.summary,
            format!("PR #1 is open; missing checks ({cause}, needs Checks: read)"),
            "{name}"
        );

        // The entry's message is the one the call returns when check runs are the only
        // request: the same failure the essential path builds.
        let message = entry["message"].as_str().unwrap();
        match name {
            "301" => assert!(message.contains("example.com/moved"), "{message}"),
            "non-JSON 200" => assert!(
                message.starts_with("GitHub's response is not valid JSON"),
                "{message}"
            ),
            "transport error" => assert_eq!(
                message,
                "the request to the credential gateway failed: connection reset"
            ),
            "422 with errors" => assert_eq!(
                message,
                "GitHub answered HTTP 422: Validation Failed (sha: invalid)"
            ),
            _ => assert!(
                message.starts_with(&format!(
                    "GitHub answered HTTP {}",
                    expected_status.unwrap()
                )),
                "{message}"
            ),
        }
    }
}

#[test]
fn pr_view_a_failed_reviews_request_leaves_reviews_null_and_still_reads_checks() {
    let mut fake = Fake::new(vec![
        ok(pr_view_json(false, "clean")),
        status(500, json!({"message": "Server Error"})),
        ok(json!({"check_runs": [{"name": "ci", "status": "completed", "conclusion": "success"}]})),
    ]);
    let response = run(Some(CONFIG), PR_VIEW, &mut fake, Path::new("."));
    assert_eq!(response.status, ops::OpStatus::Passed);
    assert_eq!(
        response.summary,
        "PR #1 is open; missing reviews (HTTP 500, needs Pull requests: read)"
    );
    let value = data(&response);
    assert_eq!(value["reviews"], Value::Null);
    assert_eq!(value["checks"][0]["conclusion"], json!("success"));
    assert_eq!(
        value["missing"],
        json!([{"field": "reviews", "error_kind": "upstream_error", "http_status": 500,
                "message": "GitHub answered HTTP 500: Server Error",
                "needs": "Pull requests: read"}])
    );
    assert_eq!(paths(&fake.requests()), pr_view_paths());
}

#[test]
fn pr_view_a_200_of_an_unexpected_shape_is_an_empty_list_not_a_missing_part() {
    let mut fake = Fake::new(vec![
        ok(pr_json(1, "open", &"a".repeat(40))),
        ok(json!({"not": "an array"})),
        ok(json!({"total_count": 0})),
    ]);
    let response = run(Some(CONFIG), PR_VIEW, &mut fake, Path::new("."));
    assert_eq!(response.summary, "PR #1 is open");
    let value = data(&response);
    assert_eq!(value["reviews"], json!([]));
    assert_eq!(value["checks"], json!([]));
    assert_eq!(value["missing"], json!([]));
    assert_eq!(value["draft"], json!(false), "absent draft is false");
    assert_eq!(
        value["mergeable_state"],
        Value::Null,
        "absent mergeable_state is null"
    );
}

#[test]
fn pr_view_a_null_body_and_odd_draft_and_mergeable_state_types_read_as_their_defaults() {
    let mut pr = pr_json(1, "open", &"a".repeat(40));
    pr["body"] = Value::Null;
    pr["draft"] = json!("yes");
    pr["mergeable_state"] = json!(7);
    let mut fake = Fake::new(vec![ok(pr), ok(json!([])), ok(json!({"check_runs": []}))]);
    let value = data(&run(Some(CONFIG), PR_VIEW, &mut fake, Path::new(".")));
    assert_eq!(value["body"], json!(""));
    assert_eq!(value["draft"], json!(false));
    assert_eq!(value["mergeable_state"], Value::Null);
}

#[test]
fn pr_view_a_failed_pull_request_request_fails_the_call_and_sends_nothing_more() {
    let cases = [
        (
            403,
            "Resource not accessible by personal access token",
            "upstream_forbidden",
        ),
        (404, "Not Found", "not_found"),
    ];
    for (code, message, expected) in cases {
        let mut fake = Fake::new(vec![status(code, json!({"message": message}))]);
        let response = run(Some(CONFIG), PR_VIEW, &mut fake, Path::new("."));
        assert_eq!(response.status, ops::OpStatus::Error, "{code}");
        let value = data(&response);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error_kind"], json!(expected));
        assert_eq!(value["http_status"], json!(code));
        assert_eq!(
            value["message"],
            json!(format!("GitHub answered HTTP {code}: {message}"))
        );
        assert!(value.get("missing").is_none());
        assert_eq!(fake.requests().len(), 1, "{code}");
    }

    for (name, reply, expected) in [
        ("non-JSON", raw(200, None, b"not json"), "response_invalid"),
        ("transport", Err("refused".to_string()), "transport_error"),
        (
            "no head",
            raw_json(200, json!({"number": 1, "base": {"ref": "main"}})),
            "response_invalid",
        ),
    ] {
        let mut fake = RawFake::new(vec![reply]);
        let response = ops::run(
            Some(GATEWAY),
            Some(CONFIG),
            PR_VIEW,
            &mut fake,
            Path::new("."),
        );
        assert_eq!(response.status, ops::OpStatus::Error, "{name}");
        assert_eq!(data(&response)["error_kind"], json!(expected), "{name}");
        assert_eq!(fake.paths.len(), 1, "{name}");
    }
}

#[test]
fn pr_list_and_pr_create_output_carries_no_pr_view_only_field() {
    let mut fake = Fake::new(vec![ok(json!([pr_view_json(true, "draft")]))]);
    let value = data(&run(
        Some(CONFIG),
        r#"{"op":"pr_list"}"#,
        &mut fake,
        Path::new("."),
    ));
    let pr = value["prs"][0].as_object().unwrap();
    assert!(
        !pr.contains_key("mergeable_state") && !pr.contains_key("missing"),
        "{value}"
    );

    let mut fake = Fake::new(vec![created(pr_view_json(true, "draft"))]);
    let input =
        json!({"op": "pr_create", "title": "Add feature", "head": "feature-x", "base": "main"});
    let value = data(&run(
        Some(CONFIG),
        &input.to_string(),
        &mut fake,
        Path::new("."),
    ));
    let pr = value.as_object().unwrap();
    assert!(
        !pr.contains_key("mergeable_state")
            && !pr.contains_key("draft")
            && !pr.contains_key("missing"),
        "{value}"
    );
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
fn pr_create_refused_with_an_error_list_names_the_refused_field() {
    let errors = json!([{"resource": "PullRequest", "field": "head", "code": "invalid"}]);
    let mut fake = Fake::new(vec![status(
        422,
        json!({"message": "Validation Failed", "errors": errors}),
    )]);
    let response = run(
        Some(CONFIG),
        r#"{"op":"pr_create","title":"t","head":"feature-x","base":"main"}"#,
        &mut fake,
        Path::new("."),
    );
    assert!(
        response.summary.contains("head: invalid"),
        "{:?}",
        response.summary
    );
    let value = data(&response);
    assert_eq!(value["errors"], errors);
    assert_eq!(value["error_kind"], json!("upstream_error"));
    assert_eq!(value["http_status"], json!(422));
    assert_eq!(fake.requests().len(), 1);
}

#[test]
fn pr_create_refusal_never_echoes_the_request() {
    let mut fake = Fake::new(vec![status(
        422,
        json!({"message": "Validation Failed", "errors": [
            {"resource": "PullRequest", "field": "title", "code": "invalid",
             "value": "TITLE-MARKER-7f3a"},
            {"resource": "PullRequest", "code": "custom", "message": "bad",
             "body": "BODY-MARKER-91c2"},
        ]}),
    )]);
    let response = run(
        Some(CONFIG),
        r#"{"op":"pr_create","title":"TITLE-MARKER-7f3a","body":"BODY-MARKER-91c2","head":"feature-x","base":"main"}"#,
        &mut fake,
        Path::new("."),
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let sent = requests[0].body.as_ref().unwrap().to_string();
    assert!(sent.contains("TITLE-MARKER-7f3a") && sent.contains("BODY-MARKER-91c2"));

    let mut needles = vec![
        "TITLE-MARKER-7f3a".to_string(),
        "BODY-MARKER-91c2".to_string(),
    ];
    needles.extend(
        requests[0]
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.clone()),
    );
    for needle in &needles {
        assert!(
            !response.summary.contains(needle.as_str()),
            "summary holds {needle:?}"
        );
        assert!(
            !response.data.contains(needle.as_str()),
            "data holds {needle:?}"
        );
    }
    assert!(
        response
            .summary
            .ends_with("(title: invalid; PullRequest: bad)"),
        "{:?}",
        response.summary
    );
    assert_eq!(
        data(&response)["errors"],
        json!([
            {"resource": "PullRequest", "field": "title", "code": "invalid"},
            {"resource": "PullRequest", "code": "custom", "message": "bad"},
        ])
    );
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

// ── body_file on pr_create and pr_edit ─────────────────────────────────────

/// A fake GitHub for one PR call: answers every request with `status` and a PR whose `body`
/// is `stored(<the body the request sent>)`, recording each request.
struct PrEcho {
    status: u16,
    stored: fn(Option<&str>) -> Value,
    requests: RefCell<Vec<Recorded>>,
}

impl PrEcho {
    fn new(status: u16, stored: fn(Option<&str>) -> Value) -> Self {
        Self {
            status,
            stored,
            requests: RefCell::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.borrow().clone()
    }
}

impl Transport for PrEcho {
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
        let number = path
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(9);
        let mut pr = pr_json(number, "open", &"a".repeat(40));
        pr["body"] = (self.stored)(body.and_then(|b| b.get("body")).and_then(Value::as_str));
        Ok(HttpReply {
            status: self.status,
            location: None,
            body: pr.to_string().into_bytes(),
        })
    }
}

/// GitHub storing exactly what it was sent.
fn echo_body(sent: Option<&str>) -> Value {
    json!(sent)
}

/// GitHub storing what it was sent minus its last line.
fn drop_last_line(sent: Option<&str>) -> Value {
    let text = sent.unwrap_or_default().trim_end_matches('\n');
    json!(&text[..text.rfind('\n').unwrap_or(0)])
}

/// GitHub storing no body at all.
fn null_body(_: Option<&str>) -> Value {
    Value::Null
}

/// A 30-line Markdown body: a GFM table, a fenced code block, a line with trailing spaces, a
/// non-ASCII char and a final newline. `tag` makes two bodies differ.
fn sample_body(tag: &str) -> String {
    let mut lines = vec![
        format!("## Summary ({tag})"),
        String::new(),
        "Adds the widget café counter — with an exact body.".to_string(),
        "This line ends in two trailing spaces.  ".to_string(),
        String::new(),
        "| Field | Before | After |".to_string(),
        "|-------|--------|-------|".to_string(),
        "| count | 0 | 1 |".to_string(),
        "| name | `old` | `new` |".to_string(),
        "| flag | false | true |".to_string(),
        String::new(),
        "```rust".to_string(),
        "fn main() {".to_string(),
        "    println!(\"{}\", 1 + 1);".to_string(),
        "}".to_string(),
        "```".to_string(),
        String::new(),
    ];
    let mut n = 1;
    while lines.len() < 30 {
        lines.push(format!("- point {n} for {tag}"));
        n += 1;
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// A tempdir holding `files` (path, bytes).
fn work_folder(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, bytes) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, bytes).unwrap();
    }
    dir
}

fn create_from(body_file: &str) -> String {
    json!({"op": "pr_create", "title": "Add feature", "head": "feature-x", "base": "main",
           "body_file": body_file})
    .to_string()
}

#[test]
fn pr_create_with_body_file_sends_the_file_byte_for_byte_and_verifies_it() {
    let text = sample_body("v1");
    assert_eq!(text.lines().count(), 30);
    let dir = work_folder(&[("pr-body.md", text.as_bytes())]);
    let mut fake = PrEcho::new(201, echo_body);
    let response = run(
        Some(CONFIG),
        &create_from("pr-body.md"),
        &mut fake,
        dir.path(),
    );
    assert_eq!(response.status, ops::OpStatus::Passed, "{}", response.data);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (requests[0].method.as_str(), requests[0].path.as_str()),
        ("POST", "/repos/acme/widgets/pulls")
    );
    assert_eq!(requests[0].body.as_ref().unwrap()["body"], json!(text));
    let value = data(&response);
    assert_eq!(value["number"], json!(9));
    assert_eq!(value["body_file"], json!("pr-body.md"));
    assert_eq!(value["body_bytes"], json!(text.len()));
    assert_eq!(value["body_verified"], json!(true));
    assert_eq!(
        response.summary,
        format!(
            "opened PR #9 on acme/widgets; body from pr-body.md ({} bytes, verified)",
            text.len()
        )
    );
}

#[test]
fn pr_create_with_body_file_keeps_a_bom_crlf_and_a_missing_final_newline() {
    let text = "\u{feff}line one  \r\nline two\r\n\r\nno final newline";
    let dir = work_folder(&[("pr-body.md", text.as_bytes())]);
    let mut fake = PrEcho::new(201, echo_body);
    let response = run(
        Some(CONFIG),
        &create_from("pr-body.md"),
        &mut fake,
        dir.path(),
    );
    assert_eq!(
        data(&response)["body_verified"],
        json!(true),
        "{}",
        response.data
    );
    assert_eq!(
        fake.requests()[0].body.as_ref().unwrap()["body"],
        json!(text)
    );
    assert_eq!(data(&response)["body_bytes"], json!(text.len()));
}

#[test]
fn pr_create_without_body_file_reports_exactly_the_old_keys() {
    let mut fake = PrEcho::new(201, echo_body);
    let input = r#"{"op":"pr_create","title":"Add feature","body":"inline","head":"feature-x","base":"main"}"#;
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    let value = data(&response);
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["head_sha", "number", "ok", "url"]);
    assert_eq!(response.summary, "opened PR #9 on acme/widgets");
}

#[test]
fn pr_create_with_body_file_and_a_footer_appends_the_footer_once_and_verifies() {
    let text = sample_body("v1");
    let with_footer = format!("{}\n\n{FOOTER_LINE}\n", text.trim_end_matches('\n'));
    let dir = work_folder(&[
        ("pr-body.md", text.as_bytes()),
        ("signed.md", with_footer.as_bytes()),
    ]);
    let config = footer_config();
    for (file, expected) in [
        (
            "pr-body.md",
            format!("{}\n\n{FOOTER_LINE}", text.trim_end_matches('\n')),
        ),
        ("signed.md", with_footer.clone()),
    ] {
        let mut fake = PrEcho::new(201, echo_body);
        let response = run(Some(&config), &create_from(file), &mut fake, dir.path());
        assert_eq!(
            fake.requests()[0].body.as_ref().unwrap()["body"],
            json!(expected),
            "{file}"
        );
        let value = data(&response);
        assert_eq!(value["body_verified"], json!(true), "{file}: {value}");
        assert_eq!(value["body_bytes"], json!(expected.len()), "{file}");
    }
}

#[test]
fn pr_edit_with_body_file_sends_one_patch_carrying_only_the_body() {
    let text = sample_body("v2");
    let dir = work_folder(&[("pr-body-v2.md", text.as_bytes())]);
    let mut fake = PrEcho::new(200, echo_body);
    let input = r#"{"op":"pr_edit","number":12,"body_file":"pr-body-v2.md"}"#;
    let response = run(Some(CONFIG), input, &mut fake, dir.path());
    assert_eq!(response.status, ops::OpStatus::Passed, "{}", response.data);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PATCH");
    assert_eq!(requests[0].path, "/repos/acme/widgets/pulls/12");
    let body_text = json!({"body": text}).to_string();
    assert_eq!(
        requests[0].headers,
        ops::standard_headers(Some(body_text.len()))
    );
    assert_eq!(requests[0].body, Some(json!({"body": text})));
    let value = data(&response);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["number"], json!(12));
    assert_eq!(
        value["url"],
        json!("https://github.com/acme/widgets/pull/12")
    );
    assert_eq!(value["head_sha"], json!("a".repeat(40)));
    assert_eq!(value["state"], json!("open"));
    assert_eq!(value["body_file"], json!("pr-body-v2.md"));
    assert_eq!(value["body_bytes"], json!(text.len()));
    assert_eq!(value["body_verified"], json!(true));
    assert_eq!(
        response.metadata,
        vec![
            ("state_effect".to_string(), "mutate".to_string()),
            (
                "resource_id".to_string(),
                "github:acme/widgets:pr:12".to_string()
            ),
        ]
    );
    assert_eq!(
        response.summary,
        format!(
            "edited PR #12 on acme/widgets: body from pr-body-v2.md ({} bytes, verified)",
            text.len()
        )
    );
}

#[test]
fn pr_edit_title_only_sends_only_the_title_and_verifies_nothing() {
    let mut fake = PrEcho::new(200, echo_body);
    let input = r#"{"op":"pr_edit","number":12,"title":"New title"}"#;
    let response = run(Some(&footer_config()), input, &mut fake, Path::new("."));
    assert_eq!(fake.requests()[0].body, Some(json!({"title": "New title"})));
    let value = data(&response);
    assert_eq!(value["ok"], json!(true));
    assert!(value.get("body_verified").is_none(), "{value}");
    assert!(value.get("body_file").is_none(), "{value}");
    assert_eq!(response.summary, "edited PR #12 on acme/widgets: title");
}

#[test]
fn pr_edit_title_and_inline_body_sends_both_with_the_footer_applied() {
    let input = r#"{"op":"pr_edit","number":12,"title":"New title","body":"new text"}"#;
    let mut fake = PrEcho::new(200, echo_body);
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    assert_eq!(
        fake.requests()[0].body,
        Some(json!({"title": "New title", "body": "new text"}))
    );
    assert!(data(&response).get("body_verified").is_none());
    assert_eq!(
        response.summary,
        "edited PR #12 on acme/widgets: title, body"
    );

    // An inline body is not verified: a different stored body is still a success.
    let mut fake = PrEcho::new(200, null_body);
    let response = run(Some(&footer_config()), input, &mut fake, Path::new("."));
    assert_eq!(
        fake.requests()[0].body,
        Some(json!({"title": "New title", "body": format!("new text\n\n{FOOTER_LINE}")}))
    );
    assert_eq!(data(&response)["ok"], json!(true));
}

#[test]
fn pr_edit_with_nothing_to_change_is_invalid_input_with_no_requests() {
    for input in [
        json!({"op": "pr_edit", "number": 12}).to_string(),
        strict_form(json!({"op": "pr_edit", "number": 12})),
    ] {
        let mut fake = Fake::new(vec![]);
        let response = run(Some(CONFIG), &input, &mut fake, Path::new("."));
        let value = data(&response);
        assert_eq!(value["error_kind"], json!("invalid_input"));
        assert_eq!(
            value["message"],
            json!("pr_edit needs at least one of title, body, body_file")
        );
        assert!(fake.requests().is_empty());
    }
    for input in [
        r#"{"op":"pr_edit","title":"t"}"#,
        r#"{"op":"pr_edit","number":0,"title":"t"}"#,
    ] {
        let mut fake = Fake::new(vec![]);
        let value = data(&run(Some(CONFIG), input, &mut fake, Path::new(".")));
        assert_eq!(value["message"], json!("\"number\" is required"), "{input}");
        assert!(fake.requests().is_empty());
    }
}

#[test]
fn body_and_body_file_together_are_refused_before_the_file_is_opened() {
    for input in [
        json!({"op": "pr_create", "title": "t", "head": "feature-x", "base": "main",
               "body": "inline", "body_file": "does-not-exist.md"}),
        json!({"op": "pr_edit", "number": 12, "body": "inline", "body_file": "does-not-exist.md"}),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Fake::new(vec![]);
        let response = run(Some(CONFIG), &input.to_string(), &mut fake, dir.path());
        let value = data(&response);
        assert_eq!(response.status, ops::OpStatus::Failed);
        assert_eq!(value["error_kind"], json!("invalid_input"), "{input}");
        let message = value["message"].as_str().unwrap();
        assert!(
            message.contains("\"body\"") && message.contains("\"body_file\""),
            "{message}"
        );
        assert!(
            !message.contains("does not exist"),
            "the file was looked at: {message}"
        );
        assert!(fake.requests().is_empty());
    }
}

#[test]
fn a_body_file_that_could_leave_the_work_folder_is_refused_with_no_requests() {
    let dir = work_folder(&[("a/b.md", b"inside")]);
    for bad in ["/etc/passwd", "../outside.md", "a/../../b.md", "a/../b.md"] {
        for input in [
            create_from(bad),
            json!({"op": "pr_edit", "number": 12, "body_file": bad}).to_string(),
        ] {
            let mut fake = Fake::new(vec![]);
            let value = data(&run(Some(CONFIG), &input, &mut fake, dir.path()));
            assert_eq!(value["error_kind"], json!("invalid_input"), "{input}");
            let message = value["message"].as_str().unwrap();
            assert!(message.starts_with("\"body_file\""), "{message}");
            assert!(message.contains(&format!("{bad:?}")), "{message}");
            assert!(fake.requests().is_empty(), "{input}");
        }
    }
}

#[test]
fn a_nested_body_file_path_is_accepted() {
    let text = sample_body("nested");
    let dir = work_folder(&[("docs/pr-body.md", text.as_bytes())]);
    let mut fake = PrEcho::new(201, echo_body);
    let response = run(
        Some(CONFIG),
        &create_from("docs/pr-body.md"),
        &mut fake,
        dir.path(),
    );
    let value = data(&response);
    assert_eq!(value["body_verified"], json!(true), "{value}");
    assert_eq!(value["body_file"], json!("docs/pr-body.md"));
}

#[test]
fn an_unusable_body_file_is_invalid_input_naming_it_with_no_requests() {
    let big = vec![b'x'; 1_048_577];
    let at_limit = vec![b'x'; 1_048_576];
    let dir = work_folder(&[
        ("empty.md", b""),
        ("big.md", &big),
        ("at-limit.md", &at_limit),
        ("bad.md", b"ok\xff\xfe"),
        ("docs/keep.md", b"x"),
    ]);
    let cases: [(&str, &[&str]); 5] = [
        ("missing.md", &["does not exist"]),
        ("docs", &["not a regular file"]),
        ("empty.md", &["is empty"]),
        ("big.md", &["1048577 bytes", "1 MiB", "1048576"]),
        ("bad.md", &["not valid UTF-8", "byte offset 2"]),
    ];
    for (file, expected) in cases {
        for input in [
            create_from(file),
            json!({"op": "pr_edit", "number": 12, "body_file": file}).to_string(),
        ] {
            let mut fake = Fake::new(vec![]);
            let response = run(Some(CONFIG), &input, &mut fake, dir.path());
            let value = data(&response);
            assert_eq!(response.status, ops::OpStatus::Failed, "{input}");
            assert_eq!(value["error_kind"], json!("invalid_input"), "{input}");
            let message = value["message"].as_str().unwrap();
            assert!(
                message.contains(&format!("body_file {file:?}")),
                "{message}"
            );
            for part in expected {
                assert!(message.contains(part), "{file}: {message}");
            }
            assert!(fake.requests().is_empty(), "{input}");
        }
    }

    // Exactly 1 MiB is within the limit.
    let mut fake = PrEcho::new(201, echo_body);
    let response = run(
        Some(CONFIG),
        &create_from("at-limit.md"),
        &mut fake,
        dir.path(),
    );
    assert_eq!(
        data(&response)["body_bytes"],
        json!(1_048_576),
        "{}",
        response.summary
    );
}

#[test]
fn a_fork_head_is_refused_before_the_body_file_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![]);
    let input = json!({"op": "pr_create", "title": "t", "head": "someone-else:feature-x",
                       "base": "main", "body_file": "missing.md"});
    let value = data(&run(
        Some(CONFIG),
        &input.to_string(),
        &mut fake,
        dir.path(),
    ));
    assert_eq!(value["error_kind"], json!("repo_not_allowed"));
    assert!(fake.requests().is_empty());
}

#[test]
fn a_stored_body_that_differs_from_the_file_is_body_mismatch() {
    let text = sample_body("v1");
    let dir = work_folder(&[("pr-body.md", text.as_bytes())]);
    let stored = drop_last_line(Some(&text));
    let stored_len = stored.as_str().unwrap().len();
    for (input, code, number) in [
        (create_from("pr-body.md"), 201, 9),
        (
            json!({"op": "pr_edit", "number": 12, "body_file": "pr-body.md"}).to_string(),
            200,
            12,
        ),
    ] {
        let mut fake = PrEcho::new(code, drop_last_line);
        let response = run(Some(CONFIG), &input, &mut fake, dir.path());
        assert_eq!(response.status, ops::OpStatus::Error, "{input}");
        let value = data(&response);
        let url = format!("https://github.com/acme/widgets/pull/{number}");
        assert_eq!(
            value,
            json!({
                "ok": false,
                "error_kind": "body_mismatch",
                "message": value["message"],
                "number": number,
                "url": url,
                "body_file": "pr-body.md",
                "sent_bytes": text.len(),
                "stored_bytes": stored_len,
                "first_difference": stored_len,
            })
        );
        let message = value["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("PR #{number} exists at {url}")),
            "{message}"
        );
        assert!(message.contains("pr_edit"), "{message}");
        assert_eq!(fake.requests().len(), 1);
    }

    let mut fake = PrEcho::new(201, null_body);
    let value = data(&run(
        Some(CONFIG),
        &create_from("pr-body.md"),
        &mut fake,
        dir.path(),
    ));
    assert_eq!(value["error_kind"], json!("body_mismatch"));
    assert_eq!(value["stored_bytes"], json!(0));
    assert_eq!(value["first_difference"], json!(0));
    assert_eq!(value["sent_bytes"], json!(text.len()));
}

#[test]
fn pr_edit_refusals_come_back_through_upstream_error() {
    let mut fake = Fake::new(vec![status(404, json!({"message": "Not Found"}))]);
    let input = r#"{"op":"pr_edit","number":99999,"title":"t"}"#;
    let response = run(Some(CONFIG), input, &mut fake, Path::new("."));
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("not_found"));
    assert_eq!(value["http_status"], json!(404));
    assert_eq!(fake.requests()[0].path, "/repos/acme/widgets/pulls/99999");

    let errors = json!([{"resource": "PullRequest", "field": "body", "code": "custom",
                         "message": "body is too long (maximum is 65536 characters)"}]);
    let mut fake = Fake::new(vec![status(
        422,
        json!({"message": "Validation Failed", "errors": errors}),
    )]);
    let dir = work_folder(&[("pr-body.md", b"text")]);
    let input = r#"{"op":"pr_edit","number":12,"body_file":"pr-body.md"}"#;
    let response = run(Some(CONFIG), input, &mut fake, dir.path());
    let value = data(&response);
    assert_eq!(value["error_kind"], json!("upstream_error"));
    assert_eq!(value["http_status"], json!(422));
    assert_eq!(value["errors"], errors);
    assert_eq!(
        response.summary,
        "GitHub answered HTTP 422: Validation Failed \
         (body: body is too long (maximum is 65536 characters))"
    );
}

// ── strict-mode input: every schema property on every call ──────────────────

/// The full 18-property input a strict-mode model (OpenAI's Responses API, with no `strict`
/// field on the tool) sends on every call, captured from a `gpt-5.6-luna` session: the op's own
/// fields set, every other property `""`, `0` or `false`.
fn strict_form(fields: Value) -> String {
    let mut full = json!({
        "base": "", "body": "", "body_file": "", "branch": "", "draft": false,
        "expected_head_sha": "", "force": false, "head": "", "head_sha": "", "message": "",
        "method": "", "number": 0, "op": "", "remote": "", "remote_branch": "", "repo_path": "",
        "state": "", "title": ""
    });
    assert_eq!(full.as_object().unwrap().len(), 18);
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
fn strict_form_pr_view_without_checks_read_sends_what_the_sparse_pr_view_sends() {
    let script = vec![
        ok(pr_view_json(false, "unstable")),
        ok(json!([])),
        status(403, checks_forbidden()),
    ];
    let response =
        assert_strict_form_matches(CONFIG, json!({"op": "pr_view", "number": 1}), script);
    assert_eq!(response.status, ops::OpStatus::Passed);
    assert_eq!(
        data(&response)["missing"],
        json!([checks_forbidden_entry()])
    );
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
fn strict_form_pr_edit_sends_what_the_sparse_pr_edit_sends() {
    let script = vec![ok(pr_json(12, "open", &"a".repeat(40)))];
    let sparse = json!({"op": "pr_edit", "number": 12, "title": "New title"});
    let response = assert_strict_form_matches(CONFIG, sparse, script.clone());
    assert_eq!(data(&response)["number"], json!(12));

    let mut fake = Fake::new(script);
    let strict = strict_form(json!({"op": "pr_edit", "number": 12, "title": "New title"}));
    run(Some(CONFIG), &strict, &mut fake, Path::new("."));
    assert_eq!(
        fake.requests()[0].body,
        Some(json!({"title": "New title"})),
        "an empty body and body_file send no body key"
    );
}

#[test]
fn strict_form_body_file_calls_send_what_the_sparse_calls_send() {
    let dir = tempfile::tempdir().unwrap();
    let text = sample_body("strict");
    std::fs::write(dir.path().join("pr-body.md"), &text).unwrap();
    for sparse in [
        json!({"op": "pr_create", "title": "Add feature", "head": "feature-x", "base": "main",
               "body_file": "pr-body.md"}),
        json!({"op": "pr_edit", "number": 12, "body_file": "pr-body.md"}),
    ] {
        let code = if sparse["op"] == "pr_create" {
            201
        } else {
            200
        };
        let mut fake = PrEcho::new(code, echo_body);
        let expected = run(Some(CONFIG), &sparse.to_string(), &mut fake, dir.path());
        let mut strict_fake = PrEcho::new(code, echo_body);
        let actual = run(
            Some(CONFIG),
            &strict_form(sparse.clone()),
            &mut strict_fake,
            dir.path(),
        );
        assert_eq!(
            data(&actual)["body_verified"],
            json!(true),
            "{}",
            actual.data
        );
        assert_eq!(actual, expected, "{sparse}");
        assert_eq!(sent(strict_fake.requests()), sent(fake.requests()));
    }
}

#[test]
fn strict_form_with_body_file_on_fetch_is_refused_as_unrecognised() {
    let mut fake = Fake::new(vec![]);
    let input = strict_form(json!({"op": "fetch", "body_file": "x"}));
    let response = run(Some(CONFIG), &input, &mut fake, Path::new("."));
    assert_eq!(
        data(&response)["message"],
        json!("'body_file' is not a recognised field for this op")
    );
    assert!(fake.requests().is_empty());
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

// ── pr_merge with commit_identity: the tool-built commit ──────────────────────

/// Base tip, PR head, test-merge tree, built merge commit, built squash commit, and GitHub's
/// own test-merge commit (`merge_commit_sha`).
const B: &str = "1111111111111111111111111111111111111111";
const H: &str = "2222222222222222222222222222222222222222";
const T: &str = "3333333333333333333333333333333333333333";
const M: &str = "4444444444444444444444444444444444444444";
const S: &str = "5555555555555555555555555555555555555555";
const G: &str = "6666666666666666666666666666666666666666";

const PROTECTED_HINT: &str = "; a protected base branch refuses a commit this tool builds itself — unset commit_identity to let GitHub merge as the token owner";

fn identity() -> Value {
    json!({"name": "Release Bot", "email": "bot@example.com"})
}

fn identity_config_with(methods: Value, commit_identity: Value) -> String {
    json!({
        "config_version": 1, "repo": "acme/widgets", "allow_merge": true,
        "merge_methods": methods, "merge_trailer": TRAILER,
        "commit_identity": commit_identity,
    })
    .to_string()
}

fn identity_config() -> String {
    identity_config_with(json!(["merge", "squash", "rebase"]), identity())
}

/// A PR as GitHub reports it while open and mergeable, with its test merge computed.
fn identity_pr() -> Value {
    let mut pr = pr_json(1, "open", H);
    pr["merge_commit_sha"] = json!(G);
    pr["head"]["repo"] = json!({"full_name": "acme/widgets"});
    pr["base"]["repo"] = json!({"full_name": "acme/widgets", "default_branch": "main"});
    pr
}

fn test_merge_commit() -> Value {
    json!({
        "sha": G,
        "tree": {"sha": T},
        "parents": [{"sha": B}, {"sha": H}],
    })
}

fn merge_call(method: &str) -> String {
    json!({"op": "pr_merge", "number": 1, "expected_head_sha": H, "method": method}).to_string()
}

fn sent_requests(fake: &Fake) -> Vec<(String, String, Option<Value>)> {
    sent(fake.requests())
}

fn get(path: &str) -> (String, String, Option<Value>) {
    ("GET".to_string(), path.to_string(), None)
}

fn with_body(method: &str, path: &str, body: Value) -> (String, String, Option<Value>) {
    (method.to_string(), path.to_string(), Some(body))
}

fn built_commit(message: &str, parents: Value) -> Value {
    json!({
        "message": message,
        "tree": T,
        "parents": parents,
        "author": identity(),
        "committer": identity(),
    })
}

const BUILT_MERGE_MESSAGE: &str = "Merge pull request #1 from acme/feature-x\n\nAdd feature\n\nCo-authored-by: Release Bot <bot@example.com>";
const BUILT_SQUASH_MESSAGE: &str =
    "Add feature (#1)\n\ndoes the thing\n\nCo-authored-by: Release Bot <bot@example.com>";

/// The replies an identity squash gets up to (and including) its head-branch update.
fn squash_until_head_moved() -> Vec<Scripted> {
    vec![
        ok(identity_pr()),
        ok(test_merge_commit()),
        created(json!({"sha": S})),
        ok(json!({"object": {"sha": H}})),
        ok(json!({"object": {"sha": S}})),
    ]
}

#[test]
fn identity_merge_builds_the_commit_and_fast_forwards_the_base() {
    let mut fake = Fake::new(vec![
        ok(identity_pr()),
        ok(test_merge_commit()),
        created(json!({"sha": M})),
        ok(json!({"object": {"sha": M}})),
    ]);
    let response = run(
        Some(&identity_config()),
        &merge_call("merge"),
        &mut fake,
        Path::new("."),
    );
    assert_eq!(
        sent_requests(&fake),
        vec![
            get("/repos/acme/widgets/pulls/1"),
            get(&format!("/repos/acme/widgets/git/commits/{G}")),
            with_body(
                "POST",
                "/repos/acme/widgets/git/commits",
                built_commit(BUILT_MERGE_MESSAGE, json!([B, H])),
            ),
            with_body(
                "PATCH",
                "/repos/acme/widgets/git/refs/heads/main",
                json!({"sha": M, "force": false}),
            ),
        ]
    );
    let post = fake.requests()[2].body.clone().unwrap();
    assert!(post["author"].get("date").is_none() && post["committer"].get("date").is_none());
    assert_eq!(
        data(&response),
        json!({
            "ok": true, "merged": true, "merge_sha": M, "trailer": TRAILER,
            "identity": identity(),
        })
    );
    assert_eq!(
        response.summary,
        "merged PR #1 on acme/widgets via merge as Release Bot <bot@example.com>"
    );
    assert_eq!(
        response.metadata,
        vec![
            ("state_effect".to_string(), "mutate".to_string()),
            (
                "resource_id".to_string(),
                "github:acme/widgets:pr:1".to_string()
            ),
        ]
    );
}

#[test]
fn identity_squash_moves_the_head_then_fast_forwards_the_base() {
    let mut script = squash_until_head_moved();
    script.push(ok(json!({"object": {"sha": S}})));
    let mut fake = Fake::new(script);
    let response = run(
        Some(&identity_config()),
        &merge_call("squash"),
        &mut fake,
        Path::new("."),
    );
    assert_eq!(
        sent_requests(&fake),
        vec![
            get("/repos/acme/widgets/pulls/1"),
            get(&format!("/repos/acme/widgets/git/commits/{G}")),
            with_body(
                "POST",
                "/repos/acme/widgets/git/commits",
                built_commit(BUILT_SQUASH_MESSAGE, json!([B])),
            ),
            get("/repos/acme/widgets/git/refs/heads/feature-x"),
            with_body(
                "PATCH",
                "/repos/acme/widgets/git/refs/heads/feature-x",
                json!({"sha": S, "force": true}),
            ),
            with_body(
                "PATCH",
                "/repos/acme/widgets/git/refs/heads/main",
                json!({"sha": S, "force": false}),
            ),
        ]
    );
    assert_eq!(
        data(&response),
        json!({
            "ok": true, "merged": true, "merge_sha": S, "trailer": TRAILER,
            "identity": identity(),
        })
    );
    assert_eq!(
        response.summary,
        "merged PR #1 on acme/widgets via squash as Release Bot <bot@example.com>"
    );
}

#[test]
fn identity_merge_with_no_trailer_reports_a_null_trailer_and_sends_none() {
    let config = json!({
        "config_version": 1, "repo": "acme/widgets", "allow_merge": true,
        "merge_methods": ["merge"], "commit_identity": identity(),
    })
    .to_string();
    let mut fake = Fake::new(vec![
        ok(identity_pr()),
        ok(test_merge_commit()),
        created(json!({"sha": M})),
        ok(json!({"object": {"sha": M}})),
    ]);
    let value = data(&run(
        Some(&config),
        &merge_call("merge"),
        &mut fake,
        Path::new("."),
    ));
    assert_eq!(value["trailer"], Value::Null);
    assert_eq!(value["identity"], identity());
    assert_eq!(
        fake.requests()[2].body.as_ref().unwrap()["message"],
        json!("Merge pull request #1 from acme/feature-x\n\nAdd feature")
    );
}

#[test]
fn identity_merge_uses_the_callers_message_as_the_base_text() {
    let mut fake = Fake::new(vec![
        ok(identity_pr()),
        ok(test_merge_commit()),
        created(json!({"sha": S})),
        ok(json!({"object": {"sha": H}})),
        ok(json!({"object": {"sha": S}})),
        ok(json!({"object": {"sha": S}})),
    ]);
    let input = json!({
        "op": "pr_merge", "number": 1, "expected_head_sha": H, "method": "squash",
        "message": "Ship it",
    });
    run(
        Some(&identity_config()),
        &input.to_string(),
        &mut fake,
        Path::new("."),
    );
    assert_eq!(
        fake.requests()[2].body.as_ref().unwrap()["message"],
        json!("Ship it\n\nCo-authored-by: Release Bot <bot@example.com>")
    );
}

#[test]
fn commit_identity_unset_or_false_sends_the_merge_endpoint_requests_for_every_method() {
    let expected_messages = [
        (
            "merge",
            Some("Add feature\n\nCo-authored-by: Release Bot <bot@example.com>"),
        ),
        (
            "squash",
            Some("does the thing\n\nCo-authored-by: Release Bot <bot@example.com>"),
        ),
        ("rebase", None),
    ];
    let unset = trailer_config(r#"["merge","squash","rebase"]"#);
    let off = identity_config_with(json!(["merge", "squash", "rebase"]), json!(false));
    for config in [unset, off] {
        for (method, message) in expected_messages {
            let mut fake = Fake::new(vec![
                ok(identity_pr()),
                ok(json!({"merged": true, "sha": M})),
            ]);
            let value = data(&run(
                Some(&config),
                &merge_call(method),
                &mut fake,
                Path::new("."),
            ));
            let mut body = json!({"sha": H, "merge_method": method});
            if let Some(message) = message {
                body["commit_message"] = json!(message);
            }
            assert_eq!(
                sent_requests(&fake),
                vec![
                    get("/repos/acme/widgets/pulls/1"),
                    with_body("PUT", "/repos/acme/widgets/pulls/1/merge", body),
                ],
                "{method}"
            );
            let trailer = if method == "rebase" {
                Value::Null
            } else {
                json!(TRAILER)
            };
            assert_eq!(
                value,
                json!({"ok": true, "merged": true, "merge_sha": M, "trailer": trailer}),
                "{method}: no identity key"
            );
        }
    }
}

#[test]
fn rebase_with_commit_identity_takes_the_merge_endpoint_and_reports_a_null_identity() {
    let mut fake = Fake::new(vec![
        ok(identity_pr()),
        ok(json!({"merged": true, "sha": M})),
    ]);
    let response = run(
        Some(&identity_config()),
        &merge_call("rebase"),
        &mut fake,
        Path::new("."),
    );
    assert_eq!(
        sent_requests(&fake),
        vec![
            get("/repos/acme/widgets/pulls/1"),
            with_body(
                "PUT",
                "/repos/acme/widgets/pulls/1/merge",
                json!({"sha": H, "merge_method": "rebase"}),
            ),
        ]
    );
    assert_eq!(
        data(&response),
        json!({"ok": true, "merged": true, "merge_sha": M, "trailer": null, "identity": null})
    );
    assert_eq!(response.summary, "merged PR #1 on acme/widgets via rebase");
}

#[test]
fn identity_gates_run_first_with_nothing_sent() {
    let no_merge = json!({
        "config_version": 1, "repo": "acme/widgets", "merge_methods": ["merge"],
        "commit_identity": identity(),
    })
    .to_string();
    let squash_only = identity_config_with(json!(["squash"]), identity());
    let no_methods = identity_config_with(json!([]), identity());
    for (config, input, expected) in [
        (no_merge, merge_call("merge"), "operation_not_allowed"),
        (squash_only, merge_call("merge"), "operation_not_allowed"),
        (identity_config(), merge_call("octopus"), "invalid_input"),
        (
            no_methods,
            json!({"op": "pr_merge", "number": 1, "expected_head_sha": H}).to_string(),
            "operation_not_allowed",
        ),
    ] {
        let mut fake = Fake::new(vec![]);
        let value = data(&run(Some(&config), &input, &mut fake, Path::new(".")));
        assert_eq!(value["error_kind"], json!(expected), "{input}: {value}");
        assert!(fake.requests().is_empty(), "{input}");
    }
}

#[test]
fn a_bad_commit_identity_is_config_invalid_with_nothing_sent() {
    for bad in [
        json!(true),
        json!("Release Bot <bot@example.com>"),
        json!({"name": "Release Bot"}),
        json!({"name": "Release Bot", "email": "bot@example.com", "extra": "x"}),
        json!({"name": "Release Bot", "email": 1}),
        json!({"name": "  ", "email": "bot@example.com"}),
        json!({"name": "Release Bot", "email": "not-an-address"}),
    ] {
        let mut fake = Fake::new(vec![]);
        let value = data(&run(
            Some(&identity_config_with(json!(["merge"]), bad.clone())),
            &merge_call("merge"),
            &mut fake,
            Path::new("."),
        ));
        assert_eq!(value["error_kind"], json!("config_invalid"), "{bad}");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("commit_identity"),
            "{bad}: {value}"
        );
        assert!(fake.requests().is_empty(), "{bad}");
    }
}

#[test]
fn the_model_cannot_set_an_identity_through_the_call() {
    for field in ["commit_identity", "author", "committer"] {
        let mut input =
            json!({"op": "pr_merge", "number": 1, "expected_head_sha": H, "method": "merge"});
        input[field] = json!({"name": "Mallory", "email": "m@example.com"});
        let mut fake = Fake::new(vec![]);
        let response = run(
            Some(&identity_config()),
            &input.to_string(),
            &mut fake,
            Path::new("."),
        );
        let value = data(&response);
        assert_eq!(value["error_kind"], json!("invalid_input"), "{field}");
        assert_eq!(
            value["message"],
            json!(format!("'{field}' is not a recognised field for this op"))
        );
        assert!(fake.requests().is_empty(), "{field}");
    }
}

/// Run an identity `method` against `script`, returning the result's data and how many
/// requests were sent.
fn identity_refusal(method: &str, script: Vec<Scripted>) -> (Value, Response, usize) {
    let mut fake = Fake::new(script);
    let response = run(
        Some(&identity_config()),
        &merge_call(method),
        &mut fake,
        Path::new("."),
    );
    (data(&response), response, fake.requests().len())
}

#[test]
fn the_pr_checks_refuse_after_one_request_in_order() {
    let case = |edit: &dyn Fn(&mut Value)| {
        let mut pr = identity_pr();
        edit(&mut pr);
        pr
    };
    let cases: Vec<(&str, Value, &str, &str)> = vec![
        (
            "merge",
            case(&|pr| pr["state"] = json!("closed")),
            "not_mergeable",
            "closed",
        ),
        (
            "merge",
            case(&|pr| pr["head"]["repo"] = json!({"full_name": "mallory/widgets"})),
            "repo_not_allowed",
            "a commit this tool builds can only merge a head branch in the pinned repo",
        ),
        (
            "squash",
            case(&|pr| pr["head"]["repo"] = Value::Null),
            "repo_not_allowed",
            "a commit this tool builds can only merge a head branch in the pinned repo",
        ),
        (
            "merge",
            case(&|pr| pr["head"]["sha"] = json!("e".repeat(40))),
            "head_moved",
            "moved",
        ),
        (
            "merge",
            case(&|pr| pr["mergeable"] = Value::Null),
            "not_mergeable",
            "run pr_merge again in a few seconds",
        ),
        (
            "squash",
            case(&|pr| pr["mergeable"] = json!(false)),
            "not_mergeable",
            "not mergeable",
        ),
        (
            "merge",
            case(&|pr| {
                pr.as_object_mut().unwrap().remove("merge_commit_sha");
            }),
            "response_invalid",
            "merge_commit_sha",
        ),
        (
            "squash",
            case(&|pr| pr["base"]["repo"]["default_branch"] = json!("feature-x")),
            "operation_not_allowed",
            "rewrites the head branch",
        ),
        (
            "squash",
            case(&|pr| {
                pr["base"].as_object_mut().unwrap().remove("repo");
            }),
            "response_invalid",
            "default_branch",
        ),
    ];
    for (method, pr, expected, words) in cases {
        let (value, _, count) = identity_refusal(method, vec![ok(pr.clone())]);
        assert_eq!(
            value["error_kind"],
            json!(expected),
            "{method} {pr}: {value}"
        );
        assert!(
            value["message"].as_str().unwrap().contains(words),
            "{method}: {value}"
        );
        assert_eq!(count, 1, "{method} {expected}: only the PR read is sent");
    }
}

#[test]
fn a_closed_pr_is_refused_before_the_head_repo_check() {
    let mut pr = identity_pr();
    pr["state"] = json!("closed");
    pr["head"]["repo"] = Value::Null;
    pr["mergeable"] = Value::Null;
    let (value, _, _) = identity_refusal("merge", vec![ok(pr)]);
    assert_eq!(value["error_kind"], json!("not_mergeable"));
}

#[test]
fn a_merge_on_the_default_branch_head_is_not_refused() {
    let mut pr = identity_pr();
    pr["base"]["repo"]["default_branch"] = json!("feature-x");
    let (value, _, count) = identity_refusal(
        "merge",
        vec![
            ok(pr),
            ok(test_merge_commit()),
            created(json!({"sha": M})),
            ok(json!({"object": {"sha": M}})),
        ],
    );
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(count, 4);
}

#[test]
fn the_test_merge_read_refuses_after_two_requests() {
    let stale = "GitHub's test merge for PR #1 is out of date";
    let cases = [
        (
            status(404, json!({"message": "Not Found"})),
            "not_found",
            "HTTP 404",
        ),
        (
            ok(json!({"sha": G, "parents": []})),
            "response_invalid",
            "tree.sha",
        ),
        (
            ok(json!({"tree": {"sha": T}, "parents": [{"sha": B}]})),
            "conflict",
            stale,
        ),
        (
            ok(json!({"tree": {"sha": T}, "parents": [{"sha": B}, {"sha": "e".repeat(40)}]})),
            "conflict",
            stale,
        ),
        (
            ok(json!({"tree": {"sha": T}, "parents": [{"sha": B}, {"sha": H}, {"sha": G}]})),
            "conflict",
            stale,
        ),
    ];
    for (reply, expected, words) in cases {
        let (value, _, count) = identity_refusal("squash", vec![ok(identity_pr()), reply]);
        assert_eq!(value["error_kind"], json!(expected), "{value}");
        assert!(
            value["message"].as_str().unwrap().contains(words),
            "{value}"
        );
        assert_eq!(count, 2, "{expected}: nothing after the commit read");
    }
}

#[test]
fn a_refused_commit_creation_writes_no_ref() {
    let (value, _, count) = identity_refusal(
        "merge",
        vec![
            ok(identity_pr()),
            ok(test_merge_commit()),
            status(
                422,
                json!({"message": "Tree SHA does not exist", "errors": ["tree"]}),
            ),
        ],
    );
    assert_eq!(value["error_kind"], json!("upstream_error"));
    assert_eq!(value["http_status"], json!(422));
    assert_eq!(count, 3, "no ref is written");
}

#[test]
fn a_squash_whose_head_branch_moved_writes_no_ref() {
    let mut script = squash_until_head_moved();
    script.truncate(3);
    script.push(ok(json!({"object": {"sha": "e".repeat(40)}})));
    let (value, _, count) = identity_refusal("squash", script);
    assert_eq!(value["error_kind"], json!("head_moved"), "{value}");
    assert_eq!(count, 4, "no ref is written");
    assert!(value.get("head_sha").is_none());
}

#[test]
fn a_refused_head_branch_update_leaves_the_base_alone() {
    let mut script = squash_until_head_moved();
    script.truncate(4);
    script.push(status(
        422,
        json!({"message": "Reference cannot be updated"}),
    ));
    let (value, _, count) = identity_refusal("squash", script);
    assert_eq!(value["error_kind"], json!("upstream_error"), "{value}");
    assert_eq!(value["http_status"], json!(422));
    assert_eq!(count, 5, "the base is not touched");
    assert!(
        value.get("head_sha").is_none(),
        "the head was not rewritten"
    );
}

#[test]
fn a_base_that_moved_is_conflict_for_merge() {
    let (value, _, count) = identity_refusal(
        "merge",
        vec![
            ok(identity_pr()),
            ok(test_merge_commit()),
            created(json!({"sha": M})),
            status(422, json!({"message": "Update is not a fast forward"})),
        ],
    );
    assert_eq!(value["error_kind"], json!("conflict"), "{value}");
    let message = value["message"].as_str().unwrap();
    assert!(
        message.contains("moved since GitHub computed the merge"),
        "{message}"
    );
    assert!(message.contains("nothing was merged"), "{message}");
    assert!(message.ends_with("run pr_merge again"), "{message}");
    assert!(value.get("head_sha").is_none());
    assert_eq!(count, 4);
}

#[test]
fn a_base_that_moved_is_conflict_for_squash_and_names_the_squashed_head() {
    let mut script = squash_until_head_moved();
    script.push(status(
        422,
        json!({"message": "Update is not a FAST FORWARD"}),
    ));
    let (value, _, count) = identity_refusal("squash", script);
    assert_eq!(value["error_kind"], json!("conflict"), "{value}");
    let message = value["message"].as_str().unwrap();
    assert!(message.contains("nothing was merged"), "{message}");
    assert!(
        message.contains(&format!("now holds the squashed commit {S}")),
        "{message}"
    );
    assert!(
        message.contains(&format!("expected_head_sha {S}")),
        "{message}"
    );
    assert_eq!(value["head_sha"], json!(S));
    assert_eq!(count, 6);
}

#[test]
fn a_protected_base_refusal_keeps_its_kind_and_ends_with_the_hint() {
    let refusal = json!({
        "message": "Protected branch update failed for refs/heads/main.",
        "errors": [{"resource": "Ref", "code": "custom", "message": "Required status check is expected."}],
    });
    for (code, expected) in [(422, "upstream_error"), (403, "upstream_forbidden")] {
        for method in ["merge", "squash"] {
            let script = if method == "merge" {
                vec![
                    ok(identity_pr()),
                    ok(test_merge_commit()),
                    created(json!({"sha": M})),
                    status(code, refusal.clone()),
                ]
            } else {
                let mut script = squash_until_head_moved();
                script.push(status(code, refusal.clone()));
                script
            };
            let (value, response, _) = identity_refusal(method, script);
            assert_eq!(value["error_kind"], json!(expected), "{method} {code}");
            assert_eq!(value["http_status"], json!(code));
            assert_eq!(
                value["errors"],
                json!([{"resource": "Ref", "code": "custom", "message": "Required status check is expected."}])
            );
            let message = value["message"].as_str().unwrap();
            assert!(
                message.starts_with(&format!(
                    "GitHub answered HTTP {code}: Protected branch update failed"
                )),
                "{message}"
            );
            assert!(message.ends_with(PROTECTED_HINT), "{message}");
            assert!(response.summary.ends_with(PROTECTED_HINT));
            if method == "squash" {
                assert_eq!(value["head_sha"], json!(S));
            } else {
                assert!(value.get("head_sha").is_none());
            }
        }
    }
}

#[test]
fn a_squash_transport_failure_after_the_head_rewrite_carries_head_sha() {
    // A redirect on the base update is a failure after the head moved.
    let mut script = squash_until_head_moved();
    script.push(Scripted {
        status: 302,
        location: Some("https://example.com/elsewhere".to_string()),
        body: json!({}),
    });
    let (value, _, count) = identity_refusal("squash", script);
    assert_eq!(value["error_kind"], json!("upstream_error"), "{value}");
    assert_eq!(value["head_sha"], json!(S));
    assert_eq!(count, 6);
}
