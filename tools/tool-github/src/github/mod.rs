//! GitHub REST/Git-Data-API request shapes and response parsing, addressed only as a path
//! relative to the credential gateway — never the real GitHub host directly — and carrying no
//! credential header of this tool's own making.
//!
//! Every function here is a pure translation between this crate's types and GitHub's JSON; the
//! actual HTTP call is [`crate::ops::Transport::request`], driven by [`crate::ops::run`].

use serde_json::{json, Value};

use crate::git::date::{to_iso8601, Identity};
use crate::git::object::{Commit, TreeEntry};
use crate::git::rebuild::{RemoteCommit, RemoteIdentity, RemoteSignature};

/// `/repos/{owner}/{repo}`.
pub fn repo_base(repo: &str) -> String {
    format!("/repos/{repo}")
}

// ── reads used to plan a push ────────────────────────────────────────────────

pub fn get_repo_path(repo: &str) -> String {
    repo_base(repo)
}

/// `value["default_branch"]` from a `GET /repos/{owner}/{repo}` response.
pub fn default_branch(body: &Value) -> Result<String, String> {
    body.get("default_branch")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "GitHub's repo response has no default_branch".to_string())
}

pub fn get_ref_path(repo: &str, branch: &str) -> String {
    format!("{}/git/refs/heads/{branch}", repo_base(repo))
}

/// `value["object"]["sha"]` from a `GET .../git/refs/heads/{branch}` response.
pub fn ref_sha(body: &Value) -> Result<String, String> {
    body.get("object")
        .and_then(|object| object.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "GitHub's ref response has no object.sha".to_string())
}

// ── writes that reproduce a commit range ─────────────────────────────────────

pub fn create_blob_path(repo: &str) -> String {
    format!("{}/git/blobs", repo_base(repo))
}

pub fn create_blob_body(content: &[u8]) -> Value {
    json!({ "content": base64_encode(content), "encoding": "base64" })
}

pub fn create_tree_path(repo: &str) -> String {
    format!("{}/git/trees", repo_base(repo))
}

pub fn create_tree_body(entries: &[TreeEntry]) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "path": entry.name,
                "mode": entry.api_mode(),
                "type": entry.api_type(),
                "sha": entry.sha,
            })
        })
        .collect();
    json!({ "tree": entries })
}

pub fn create_commit_path(repo: &str) -> String {
    format!("{}/git/commits", repo_base(repo))
}

/// `GET` answers 200 when the repository has the commit, 404 when it does not.
pub fn get_commit_path(repo: &str, sha: &str) -> String {
    format!("{}/git/commits/{sha}", repo_base(repo))
}

pub fn create_commit_body(commit: &Commit) -> Value {
    json!({
        "message": commit.message,
        "tree": commit.tree,
        "parents": commit.parents,
        "author": identity_json(&commit.author),
        "committer": identity_json(&commit.committer),
    })
}

fn identity_json(identity: &Identity) -> Value {
    json!({
        "name": identity.name,
        "email": identity.email,
        "date": to_iso8601(identity),
    })
}

/// `value["sha"]`, common to blob/tree/commit creation responses.
pub fn created_sha(body: &Value) -> Result<String, String> {
    body.get("sha")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "GitHub's response has no sha".to_string())
}

pub fn create_ref_path(repo: &str) -> String {
    format!("{}/git/refs", repo_base(repo))
}

pub fn create_ref_body(branch: &str, sha: &str) -> Value {
    json!({ "ref": format!("refs/heads/{branch}"), "sha": sha })
}

pub fn update_ref_path(repo: &str, branch: &str) -> String {
    format!("{}/git/refs/heads/{branch}", repo_base(repo))
}

pub fn update_ref_body(sha: &str, force: bool) -> Value {
    json!({ "sha": sha, "force": force })
}

// ── reads that reproduce a remote branch's history locally ───────────────────

/// GitHub's GraphQL endpoint, relative to the gateway like every REST path here.
pub const GRAPHQL_PATH: &str = "/graphql";

/// Up to 100 commits of history from `$oid`, newest first, each with what
/// [`crate::git::rebuild::rebuild_commit`] needs. GraphQL rather than REST because only its
/// `GitTimestamp` keeps the offset a date was written with.
const COMMIT_HISTORY_QUERY: &str = "query($owner: String!, $name: String!, $oid: GitObjectID!) { \
    repository(owner: $owner, name: $name) { object(oid: $oid) { ... on Commit { \
    history(first: 100) { nodes { oid message tree { oid } parents(first: 100) { nodes { oid } } \
    author { name email date } committer { name email date } \
    signature { signature payload } } } } } } }";

pub fn commit_history_body(repo: &str, sha: &str) -> Value {
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    json!({
        "query": COMMIT_HISTORY_QUERY,
        "variables": { "owner": owner, "name": name, "oid": sha },
    })
}

/// The messages of a GraphQL response's `errors`, which GitHub sends with HTTP 200.
pub fn graphql_errors(body: &Value) -> Option<String> {
    let errors = body.get("errors")?.as_array()?;
    let messages: Vec<&str> = errors
        .iter()
        .map(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("<no message>")
        })
        .collect();
    (!messages.is_empty()).then(|| messages.join("; "))
}

/// The commits of a [`commit_history_body`] response, newest first.
pub fn parse_commit_history(body: &Value) -> Result<Vec<RemoteCommit>, String> {
    let repository = body
        .pointer("/data/repository")
        .filter(|value| !value.is_null())
        .ok_or("GitHub's GraphQL response has no repository; the token may not see this repo")?;
    let nodes = repository
        .pointer("/object/history/nodes")
        .and_then(Value::as_array)
        .ok_or("GitHub's GraphQL response has no commit history for the requested sha")?;
    nodes.iter().map(parse_remote_commit).collect()
}

fn parse_remote_commit(node: &Value) -> Result<RemoteCommit, String> {
    let text = |pointer: &str| {
        node.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("a commit in GitHub's GraphQL response has no {pointer}"))
    };
    let identity = |field: &str| -> Result<RemoteIdentity, String> {
        Ok(RemoteIdentity {
            name: text(&format!("/{field}/name"))?,
            email: text(&format!("/{field}/email"))?,
            date: text(&format!("/{field}/date"))?,
        })
    };
    let parents = node
        .pointer("/parents/nodes")
        .and_then(Value::as_array)
        .ok_or("a commit in GitHub's GraphQL response has no parents list")?
        .iter()
        .map(|parent| {
            parent
                .get("oid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "a parent in GitHub's GraphQL response has no oid".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let signature = match node.get("signature") {
        Some(signature) if !signature.is_null() => Some(RemoteSignature {
            signature: text("/signature/signature")?,
            payload: text("/signature/payload")?,
        }),
        _ => None,
    };
    Ok(RemoteCommit {
        sha: text("/oid")?,
        tree: text("/tree/oid")?,
        parents,
        author: identity("author")?,
        committer: identity("committer")?,
        message: text("/message")?,
        signature,
    })
}

pub fn get_tree_path(repo: &str, sha: &str) -> String {
    format!("{}/git/trees/{sha}", repo_base(repo))
}

/// A `GET .../git/trees/{sha}` response's entries, in tree order, with each mode in git's
/// on-disk form: the API reports a directory as `040000`, a tree object stores `40000`.
pub fn parse_tree_response(body: &Value) -> Result<Vec<TreeEntry>, String> {
    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
        return Err(
            "GitHub truncated its tree listing; this tree is too large to fetch".to_string(),
        );
    }
    body.get("tree")
        .and_then(Value::as_array)
        .ok_or("GitHub's tree response has no tree list")?
        .iter()
        .map(|entry| {
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("a tree entry in GitHub's response has no {key}"))
            };
            let mode = text("mode")?;
            Ok(TreeEntry {
                mode: if mode == "040000" {
                    "40000".to_string()
                } else {
                    mode
                },
                name: text("path")?,
                sha: text("sha")?,
            })
        })
        .collect()
}

pub fn get_blob_path(repo: &str, sha: &str) -> String {
    format!("{}/git/blobs/{sha}", repo_base(repo))
}

/// A `GET .../git/blobs/{sha}` response's content, decoded.
pub fn parse_blob_response(body: &Value) -> Result<Vec<u8>, String> {
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .ok_or("GitHub's blob response has no content")?;
    match body.get("encoding").and_then(Value::as_str) {
        Some("base64") => base64_decode(content),
        Some("utf-8") => Ok(content.as_bytes().to_vec()),
        other => Err(format!(
            "GitHub's blob response has an unknown encoding {other:?}"
        )),
    }
}

// ── pull requests ────────────────────────────────────────────────────────────

pub fn create_pr_path(repo: &str) -> String {
    format!("{}/pulls", repo_base(repo))
}

/// `draft: true` sends `"draft": true`; `false` sends no `draft` field at all, matching how
/// `body` is only sent when `Some`.
pub fn create_pr_body(
    title: &str,
    body: Option<&str>,
    head: &str,
    base: &str,
    draft: bool,
) -> Value {
    let mut value = json!({ "title": title, "head": head, "base": base });
    if let Some(body) = body {
        value["body"] = json!(body);
    }
    if draft {
        value["draft"] = json!(true);
    }
    value
}

pub fn get_pr_path(repo: &str, number: u64) -> String {
    format!("{}/pulls/{number}", repo_base(repo))
}

pub fn list_prs_path(
    repo: &str,
    state: Option<&str>,
    head: Option<&str>,
    base: Option<&str>,
) -> String {
    let mut query = Vec::new();
    if let Some(state) = state {
        query.push(format!("state={}", url_encode(state)));
    }
    if let Some(head) = head {
        query.push(format!("head={}", url_encode(head)));
    }
    if let Some(base) = base {
        query.push(format!("base={}", url_encode(base)));
    }
    let path = format!("{}/pulls", repo_base(repo));
    if query.is_empty() {
        path
    } else {
        format!("{path}?{}", query.join("&"))
    }
}

pub fn reviews_path(repo: &str, number: u64) -> String {
    format!("{}/pulls/{number}/reviews", repo_base(repo))
}

pub fn check_runs_path(repo: &str, head_sha: &str) -> String {
    format!("{}/commits/{head_sha}/check-runs", repo_base(repo))
}

pub fn merge_pr_path(repo: &str, number: u64) -> String {
    format!("{}/pulls/{number}/merge", repo_base(repo))
}

pub fn merge_pr_body(
    method: &str,
    expected_head_sha: &str,
    commit_title: Option<&str>,
    commit_message: Option<&str>,
) -> Value {
    let mut value = json!({ "sha": expected_head_sha, "merge_method": method });
    if let Some(title) = commit_title {
        value["commit_title"] = json!(title);
    }
    if let Some(message) = commit_message {
        value["commit_message"] = json!(message);
    }
    value
}

/// A pull request as this tool reports it, taken from a `GET .../pulls/{number}` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    /// The PR's page on GitHub (`html_url`).
    pub url: String,
    pub state: String,
    pub mergeable: Option<bool>,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub title: String,
    pub body: String,
}

pub fn parse_pull_request(body: &Value) -> Result<PullRequest, String> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let number = body
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "pull request response has no number".to_string())?;
    let head = body
        .get("head")
        .ok_or_else(|| "pull request response has no head".to_string())?;
    let base = body
        .get("base")
        .ok_or_else(|| "pull request response has no base".to_string())?;
    Ok(PullRequest {
        number,
        url: text("html_url"),
        state: text("state"),
        mergeable: body.get("mergeable").and_then(Value::as_bool),
        head_sha: head
            .get("sha")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        head_ref: head
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        base_ref: base
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: text("title"),
        body: text("body"),
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Standard base64, as GitHub sends a blob: padded, wrapped with newlines.
fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
    {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("GitHub's blob content is not valid base64".to_string()),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_without_padding_edge_cases() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn builds_a_query_string_for_list_prs() {
        let path = list_prs_path("acme/widgets", Some("open"), Some("acme:feature"), None);
        assert_eq!(
            path,
            "/repos/acme/widgets/pulls?state=open&head=acme%3Afeature"
        );
    }

    #[test]
    fn parses_a_pull_request_response() {
        let body = json!({
            "number": 7,
            "state": "open",
            "mergeable": true,
            "title": "Add feature",
            "body": "Does the thing",
            "user": {"login": "release-bot"},
            "head": {"sha": "a".repeat(40), "ref": "feature-x"},
            "base": {"ref": "main"},
        });
        let pr = parse_pull_request(&body).unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.mergeable, Some(true));
        assert_eq!(pr.head_ref, "feature-x");
    }

    #[test]
    fn base64_round_trips_through_githubs_wrapped_form() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foobar", &[0, 255, 128, 7]] {
            let wrapped: String = base64_encode(bytes)
                .chars()
                .flat_map(|c| [c, '\n'])
                .collect();
            assert_eq!(base64_decode(&wrapped).unwrap(), bytes);
        }
    }

    #[test]
    fn reads_a_tree_response_with_the_on_disk_directory_mode() {
        let body = json!({"sha": "x", "truncated": false, "tree": [
            {"path": "a.txt", "mode": "100644", "type": "blob", "sha": "a".repeat(40)},
            {"path": "sub", "mode": "040000", "type": "tree", "sha": "b".repeat(40)},
        ]});
        let entries = parse_tree_response(&body).unwrap();
        assert_eq!(entries[1].mode, "40000");
        assert!(entries[1].is_tree());
        assert_eq!(
            create_tree_body(&entries)["tree"][1]["mode"],
            json!("040000")
        );
    }

    #[test]
    fn reports_graphql_errors_sent_with_a_200() {
        let body =
            json!({"data": null, "errors": [{"message": "Could not resolve to a Repository"}]});
        assert_eq!(
            graphql_errors(&body).as_deref(),
            Some("Could not resolve to a Repository")
        );
        assert_eq!(graphql_errors(&json!({"data": {}})), None);
    }

    #[test]
    fn sends_a_directory_with_the_six_digit_mode_github_requires() {
        let entry = |mode: &str| TreeEntry {
            mode: mode.to_string(),
            name: "x".to_string(),
            sha: "a".repeat(40),
        };
        let body = create_tree_body(&[
            entry("40000"),
            entry("100644"),
            entry("100755"),
            entry("120000"),
            entry("160000"),
        ]);
        let modes: Vec<&str> = body["tree"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["mode"].as_str().unwrap())
            .collect();
        assert_eq!(modes, ["040000", "100644", "100755", "120000", "160000"]);
    }
}
