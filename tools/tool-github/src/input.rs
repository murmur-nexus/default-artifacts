//! What the agent may ask this tool to do: `{"op": "push"|"fetch"|"pr_create"|"pr_view"|
//! "pr_list"|"pr_merge"|"pr_edit", ...}`. Every op-specific field is validated here; everything about *which* repo
//! and *whether an op is even allowed* lives in `config:` (see [`crate::config`]) and is
//! checked by [`crate::ops`], not here.
//!
//! A field set to `""`, `0`, `false` or `null` counts as absent, so a model that fills every
//! schema property on every call (OpenAI's strict function calling does) sends the same request
//! as one that sends only the op's own fields. No field gives one of those values a meaning of
//! its own: every boolean defaults to `false`, and an absent string or number means unset.

use std::path::{Component, Path};

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushInput {
    /// Where the local git repository is, relative to the tool's workdir.
    pub repo_path: String,
    /// The local commit this push must land on the remote ref, as a 40-character hex SHA.
    pub head_sha: String,
    pub remote_branch: String,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchInput {
    /// Where the local git repository is, relative to the tool's workdir.
    pub repo_path: String,
    /// The branch on the pinned repo to fetch; `None` means its default branch.
    pub branch: Option<String>,
    /// The remote name the remote-tracking ref is written under: `refs/remotes/<remote>/<branch>`.
    pub remote: String,
}

/// Where a pull request body comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodySource {
    /// `body`: the text the caller passed.
    Inline(String),
    /// `body_file`: a relative path, already checked by [`check_body_file_path`], to a file in
    /// the tool's work folder whose bytes are the body.
    File(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCreateInput {
    pub title: String,
    pub body: Option<BodySource>,
    /// The branch carrying the change, `owner:branch` or a bare branch name (same-repo).
    pub head: String,
    pub base: String,
    /// Open the pull request as a draft. Defaults to `false`.
    pub draft: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrViewInput {
    pub number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrListInput {
    pub state: Option<String>,
    pub head: Option<String>,
    pub base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrMergeInput {
    pub number: u64,
    /// Absent means the first of `config.merge_methods`.
    pub method: Option<String>,
    /// The head the caller last saw; GitHub refuses the merge if the PR's head has moved.
    pub expected_head_sha: String,
    pub message: Option<String>,
}

/// `pr_edit`: at least one of `title` and `body` is `Some`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrEditInput {
    pub number: u64,
    pub title: Option<String>,
    pub body: Option<BodySource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Push(PushInput),
    Fetch(FetchInput),
    PrCreate(PrCreateInput),
    PrView(PrViewInput),
    PrList(PrListInput),
    PrMerge(PrMergeInput),
    PrEdit(PrEditInput),
}

/// Parse the tool input. The `Err` string is the agent-facing `invalid_input` message.
pub fn parse_input(data: &str) -> Result<Input, String> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err("no input; expected {\"op\": ...}".to_string());
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("input is not valid JSON: {e}"))?;
    let Value::Object(mut map) = value else {
        return Err("input must be a JSON object with an \"op\" field".to_string());
    };
    let op = match map.remove("op") {
        Some(Value::String(op)) => op,
        Some(other) => return Err(format!("\"op\" must be a string, got {other}")),
        None => return Err("\"op\" is required".to_string()),
    };
    map.retain(|_, value| !is_empty_value(value));
    match op.as_str() {
        "push" => parse_push(map).map(Input::Push),
        "fetch" => parse_fetch(map).map(Input::Fetch),
        "pr_create" => parse_pr_create(map).map(Input::PrCreate),
        "pr_view" => parse_pr_view(map).map(Input::PrView),
        "pr_list" => parse_pr_list(map).map(Input::PrList),
        "pr_merge" => parse_pr_merge(map).map(Input::PrMerge),
        "pr_edit" => parse_pr_edit(map).map(Input::PrEdit),
        other => Err(format!(
            "\"op\" must be one of push, fetch, pr_create, pr_view, pr_list, pr_merge, pr_edit, \
             got {other:?}"
        )),
    }
}

/// `""`, `0`, `false` or `null`: what a strict-mode caller sends for a field it is not using.
fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(_) | Value::Object(_) => false,
    }
}

fn take_string(map: &mut Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.remove(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(other) => Err(format!("\"{key}\" must be a string, got {other}")),
    }
}

fn require_string(map: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    take_string(map, key)?.ok_or_else(|| format!("\"{key}\" is required"))
}

fn require_sha(map: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    let value = require_string(map, key)?;
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value.to_lowercase())
    } else {
        Err(format!(
            "\"{key}\" must be a 40-character hex commit SHA, got {value:?}"
        ))
    }
}

fn take_bool(map: &mut Map<String, Value>, key: &str) -> Result<bool, String> {
    match map.remove(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(value),
        Some(other) => Err(format!("\"{key}\" must be a boolean, got {other}")),
    }
}

fn require_number(map: &mut Map<String, Value>, key: &str) -> Result<u64, String> {
    match map.remove(key) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("\"{key}\" must be a positive integer, got {value}")),
        None => Err(format!("\"{key}\" is required")),
    }
}

fn reject_unknown(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("'{key}' is not a recognised field for this op"));
    }
    Ok(())
}

fn parse_push(mut map: Map<String, Value>) -> Result<PushInput, String> {
    reject_unknown(&map, &["repo_path", "head_sha", "remote_branch", "force"])?;
    Ok(PushInput {
        repo_path: take_string(&mut map, "repo_path")?.unwrap_or_else(|| ".".to_string()),
        head_sha: require_sha(&mut map, "head_sha")?,
        remote_branch: require_string(&mut map, "remote_branch")?,
        force: take_bool(&mut map, "force")?,
    })
}

/// Whether `name` is safe as part of a ref path: git's `check-ref-format` rules, which also
/// keep it from climbing out of `refs/` (no `..`, no empty or dot-led component).
pub fn is_valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with('.')
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
        && !name
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b" ~^:?*[\\".contains(&b))
}

fn take_ref_name(map: &mut Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match take_string(map, key)? {
        Some(name) if !is_valid_ref_name(&name) => Err(format!(
            "\"{key}\" is not a valid git ref name, got {name:?}"
        )),
        other => Ok(other),
    }
}

fn parse_fetch(mut map: Map<String, Value>) -> Result<FetchInput, String> {
    reject_unknown(&map, &["repo_path", "branch", "remote"])?;
    Ok(FetchInput {
        repo_path: take_string(&mut map, "repo_path")?.unwrap_or_else(|| ".".to_string()),
        branch: take_ref_name(&mut map, "branch")?,
        remote: take_ref_name(&mut map, "remote")?.unwrap_or_else(|| "origin".to_string()),
    })
}

/// Whether `path` may name a body file: relative to the tool's work folder, with no `..`
/// component anywhere (even one that would stay inside). The `Err` is the `invalid_input`
/// message.
pub fn check_body_file_path(path: &str) -> Result<(), String> {
    let shape = Path::new(path);
    if shape.has_root() {
        return Err(format!(
            "\"body_file\" must be a path relative to the work folder, got {path:?}"
        ));
    }
    if shape.components().any(|part| part == Component::ParentDir) {
        return Err(format!(
            "\"body_file\" must not contain a \"..\" component, got {path:?}"
        ));
    }
    Ok(())
}

/// `body` or `body_file`, never both. A `body_file` is shape-checked here; it is read later,
/// by the op, before its first request.
fn take_body(map: &mut Map<String, Value>) -> Result<Option<BodySource>, String> {
    let inline = take_string(map, "body")?;
    let file = take_string(map, "body_file")?;
    match (inline, file) {
        (Some(_), Some(_)) => Err(
            "give either \"body\" or \"body_file\", not both: \"body_file\" sends a file's \
             bytes as the body"
                .to_string(),
        ),
        (Some(text), None) => Ok(Some(BodySource::Inline(text))),
        (None, Some(path)) => {
            check_body_file_path(&path)?;
            Ok(Some(BodySource::File(path)))
        }
        (None, None) => Ok(None),
    }
}

fn parse_pr_create(mut map: Map<String, Value>) -> Result<PrCreateInput, String> {
    reject_unknown(
        &map,
        &["title", "body", "body_file", "head", "base", "draft"],
    )?;
    Ok(PrCreateInput {
        title: require_string(&mut map, "title")?,
        body: take_body(&mut map)?,
        head: require_string(&mut map, "head")?,
        base: require_string(&mut map, "base")?,
        draft: take_bool(&mut map, "draft")?,
    })
}

fn parse_pr_view(mut map: Map<String, Value>) -> Result<PrViewInput, String> {
    reject_unknown(&map, &["number"])?;
    Ok(PrViewInput {
        number: require_number(&mut map, "number")?,
    })
}

fn parse_pr_list(mut map: Map<String, Value>) -> Result<PrListInput, String> {
    reject_unknown(&map, &["state", "head", "base"])?;
    Ok(PrListInput {
        state: take_string(&mut map, "state")?,
        head: take_string(&mut map, "head")?,
        base: take_string(&mut map, "base")?,
    })
}

fn parse_pr_merge(mut map: Map<String, Value>) -> Result<PrMergeInput, String> {
    reject_unknown(&map, &["number", "method", "expected_head_sha", "message"])?;
    Ok(PrMergeInput {
        number: require_number(&mut map, "number")?,
        method: take_string(&mut map, "method")?,
        expected_head_sha: require_sha(&mut map, "expected_head_sha")?,
        message: take_string(&mut map, "message")?,
    })
}

fn parse_pr_edit(mut map: Map<String, Value>) -> Result<PrEditInput, String> {
    reject_unknown(&map, &["number", "title", "body", "body_file"])?;
    let number = require_number(&mut map, "number")?;
    let title = take_string(&mut map, "title")?;
    let body = take_body(&mut map)?;
    if title.is_none() && body.is_none() {
        return Err("pr_edit needs at least one of title, body, body_file".to_string());
    }
    Ok(PrEditInput {
        number,
        title,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_push() {
        let sha = "a".repeat(40);
        let input = parse_input(&format!(
            r#"{{"op":"push","head_sha":"{sha}","remote_branch":"feature-x"}}"#
        ))
        .unwrap();
        assert_eq!(
            input,
            Input::Push(PushInput {
                repo_path: ".".to_string(),
                head_sha: sha,
                remote_branch: "feature-x".to_string(),
                force: false,
            })
        );
    }

    #[test]
    fn parses_a_fetch_with_its_defaults() {
        assert_eq!(
            parse_input(r#"{"op":"fetch"}"#).unwrap(),
            Input::Fetch(FetchInput {
                repo_path: ".".to_string(),
                branch: None,
                remote: "origin".to_string()
            })
        );
        assert_eq!(
            parse_input(r#"{"op":"fetch","branch":"feature/x","remote":"upstream"}"#).unwrap(),
            Input::Fetch(FetchInput {
                repo_path: ".".to_string(),
                branch: Some("feature/x".to_string()),
                remote: "upstream".to_string(),
            })
        );
    }

    #[test]
    fn rejects_a_fetch_ref_name_that_could_leave_refs() {
        for bad in [
            "../../HEAD",
            "a/../b",
            "/main",
            "main/",
            ".hidden",
            "main.lock",
            "has space",
            "a//b",
            " ",
        ] {
            let err = parse_input(&format!(r#"{{"op":"fetch","branch":{bad:?}}}"#)).unwrap_err();
            assert!(err.contains("branch"), "{bad}: {err}");
        }
        let err = parse_input(r#"{"op":"fetch","remote":"../x"}"#).unwrap_err();
        assert!(err.contains("remote"), "{err}");
        // An empty name is an absent one: the default branch and the `origin` remote.
        assert_eq!(
            parse_input(r#"{"op":"fetch","branch":"","remote":""}"#).unwrap(),
            parse_input(r#"{"op":"fetch"}"#).unwrap()
        );
    }

    #[test]
    fn rejects_a_short_sha() {
        let err = parse_input(r#"{"op":"push","head_sha":"abc","remote_branch":"x"}"#).unwrap_err();
        assert!(err.contains("head_sha"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_op() {
        let err = parse_input(r#"{"op":"delete_repo"}"#).unwrap_err();
        assert!(err.contains("delete_repo"), "{err}");
    }

    #[test]
    fn the_unknown_op_message_lists_every_op_fetch_included() {
        let err = parse_input(r#"{"op":"delete_repo"}"#).unwrap_err();
        assert_eq!(
            err,
            "\"op\" must be one of push, fetch, pr_create, pr_view, pr_list, pr_merge, \
             pr_edit, got \"delete_repo\""
        );
    }

    #[test]
    fn rejects_an_unknown_field() {
        let err = parse_input(r#"{"op":"pr_view","number":1,"extra":true}"#).unwrap_err();
        assert!(err.contains("extra"), "{err}");
    }

    /// Every `input_schema` property, as a strict-mode model sends it on every call.
    fn strict_form(fields: Value) -> String {
        let mut full = serde_json::json!({
            "base": "", "body": "", "body_file": "", "branch": "", "draft": false,
            "expected_head_sha": "", "force": false, "head": "", "head_sha": "", "message": "", "method": "",
            "number": 0, "op": "", "remote": "", "remote_branch": "", "repo_path": "",
            "state": "", "title": ""
        });
        assert_eq!(full.as_object().unwrap().len(), 18);
        for (key, value) in fields.as_object().unwrap() {
            full[key] = value.clone();
        }
        full.to_string()
    }

    #[test]
    fn empty_values_count_as_absent_for_every_op() {
        let sha = "a".repeat(40);
        for sparse in [
            serde_json::json!({"op": "push", "head_sha": sha, "remote_branch": "feature-x"}),
            serde_json::json!({"op": "fetch"}),
            serde_json::json!({"op": "pr_create", "title": "t", "head": "h", "base": "main"}),
            serde_json::json!({"op": "pr_view", "number": 3}),
            serde_json::json!({"op": "pr_list", "state": "open"}),
            serde_json::json!({"op": "pr_merge", "number": 3, "expected_head_sha": sha}),
            serde_json::json!({"op": "pr_create", "title": "t", "head": "h", "base": "main",
                               "body_file": "pr-body.md"}),
            serde_json::json!({"op": "pr_edit", "number": 3, "body_file": "pr-body.md"}),
            serde_json::json!({"op": "pr_edit", "number": 3, "title": "t", "body": "b"}),
        ] {
            assert_eq!(
                parse_input(&strict_form(sparse.clone())).unwrap(),
                parse_input(&sparse.to_string()).unwrap(),
                "{sparse}"
            );
        }
    }

    #[test]
    fn null_counts_as_absent() {
        assert_eq!(
            parse_input(r#"{"op":"fetch","branch":null,"remote":null,"title":null}"#).unwrap(),
            parse_input(r#"{"op":"fetch"}"#).unwrap()
        );
    }

    #[test]
    fn number_zero_is_the_missing_number_refusal() {
        for op in ["pr_view", "pr_merge", "pr_edit"] {
            let zero = parse_input(&format!(r#"{{"op":"{op}","number":0}}"#)).unwrap_err();
            let missing = parse_input(&format!(r#"{{"op":"{op}"}}"#)).unwrap_err();
            assert_eq!(zero, missing);
            assert_eq!(zero, "\"number\" is required");
        }
    }

    #[test]
    fn a_non_empty_foreign_field_is_still_refused() {
        for (field, value) in [
            ("title", serde_json::json!("x")),
            ("force", serde_json::json!(true)),
            ("number", serde_json::json!(4)),
            ("body_file", serde_json::json!("x")),
        ] {
            let mut input = serde_json::json!({"op": "fetch"});
            input[field] = value;
            let err = parse_input(&strict_form(input)).unwrap_err();
            assert_eq!(
                err,
                format!("'{field}' is not a recognised field for this op")
            );
        }
    }

    #[test]
    fn parses_pr_merge() {
        let sha = "A".repeat(40);
        let input = parse_input(&format!(
            r#"{{"op":"pr_merge","number":7,"method":"squash","expected_head_sha":"{sha}"}}"#
        ))
        .unwrap();
        assert_eq!(
            input,
            Input::PrMerge(PrMergeInput {
                number: 7,
                method: Some("squash".to_string()),
                expected_head_sha: "a".repeat(40),
                message: None,
            })
        );
    }

    #[test]
    fn pr_merge_requires_the_expected_head_sha() {
        let err = parse_input(r#"{"op":"pr_merge","number":7}"#).unwrap_err();
        assert!(err.contains("expected_head_sha"), "{err}");
    }

    #[test]
    fn parses_pr_edit_with_each_body_source() {
        assert_eq!(
            parse_input(r#"{"op":"pr_edit","number":12,"body_file":"docs/pr-body.md"}"#).unwrap(),
            Input::PrEdit(PrEditInput {
                number: 12,
                title: None,
                body: Some(BodySource::File("docs/pr-body.md".to_string())),
            })
        );
        assert_eq!(
            parse_input(r#"{"op":"pr_edit","number":12,"title":"New","body":"text"}"#).unwrap(),
            Input::PrEdit(PrEditInput {
                number: 12,
                title: Some("New".to_string()),
                body: Some(BodySource::Inline("text".to_string())),
            })
        );
    }

    #[test]
    fn pr_edit_needs_something_to_change() {
        for input in [
            r#"{"op":"pr_edit","number":12}"#,
            r#"{"op":"pr_edit","number":12,"title":"","body":null,"body_file":""}"#,
        ] {
            assert_eq!(
                parse_input(input).unwrap_err(),
                "pr_edit needs at least one of title, body, body_file",
                "{input}"
            );
        }
    }

    #[test]
    fn body_and_body_file_together_are_refused_naming_both() {
        for op in [
            r#""op":"pr_create","title":"t","head":"h","base":"main""#,
            r#""op":"pr_edit","number":1"#,
        ] {
            let err =
                parse_input(&format!(r#"{{{op},"body":"x","body_file":"y.md"}}"#)).unwrap_err();
            assert!(
                err.contains("\"body\"") && err.contains("\"body_file\""),
                "{err}"
            );
        }
    }

    #[test]
    fn a_body_file_must_stay_relative_with_no_parent_component() {
        for bad in [
            "/etc/passwd",
            "../outside.md",
            "a/../../b.md",
            "a/../b.md",
            "/sub/x.md",
        ] {
            let err = check_body_file_path(bad).unwrap_err();
            assert!(err.starts_with("\"body_file\""), "{bad}: {err}");
            assert!(err.contains(&format!("{bad:?}")), "{bad}: {err}");
        }
        for good in [
            "pr-body.md",
            "docs/pr-body.md",
            "./pr-body.md",
            "a/./b.md",
            "..md",
        ] {
            assert_eq!(check_body_file_path(good), Ok(()), "{good}");
        }
    }

    #[test]
    fn body_file_is_refused_on_every_other_op() {
        let sha = "a".repeat(40);
        for input in [
            serde_json::json!({"op": "push", "head_sha": sha, "remote_branch": "x"}),
            serde_json::json!({"op": "fetch"}),
            serde_json::json!({"op": "pr_view", "number": 1}),
            serde_json::json!({"op": "pr_list"}),
            serde_json::json!({"op": "pr_merge", "number": 1, "expected_head_sha": sha}),
        ] {
            let mut input = input;
            input["body_file"] = serde_json::json!("x");
            assert_eq!(
                parse_input(&input.to_string()).unwrap_err(),
                "'body_file' is not a recognised field for this op",
                "{input}"
            );
        }
    }
}
