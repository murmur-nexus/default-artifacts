//! Host-target scenario tests for `fetch`: a fake GitHub serves its read API — GraphQL commit
//! history, REST trees and blobs — from a real source repository, in the shapes GitHub uses
//! (messages without their trailing newline, directories as `040000`, newline-wrapped base64,
//! a signed commit split into signature and payload), and the fetch must rebuild every object
//! in a second repository byte for byte.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use tool_github::git::date::to_iso8601;
use tool_github::git::object::{parse_commit, parse_tree, ObjectKind};
use tool_github::git::store::ObjectStore;
use tool_github::git::test_fixture::Fixture;
use tool_github::ops::{self, HttpReply, Response, Transport};

const CONFIG: &str = r#"{"config_version":1,"repo":"acme/widgets"}"#;

/// See `tests/pack_reading.rs`: C1 and C2 packed, C3 loose on top; C2 adds `sub/b.txt`.
const PACKED_CLONE_C3: &str = "f310270529aa5bd40cc65dfba01b276bc296e087";

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn data(response: &Response) -> Value {
    serde_json::from_str(&response.data).unwrap()
}

/// A fake GitHub answering from `source`'s objects.
struct Mirror {
    source: PathBuf,
    refs: HashMap<String, String>,
    /// Blob shas whose content is served altered, as a response that must not be written.
    tampered: Vec<String>,
    requests: RefCell<Vec<(String, String)>>,
}

impl Mirror {
    fn new(source: PathBuf, main: &str) -> Self {
        Self {
            source,
            refs: HashMap::from([("main".to_string(), main.to_string())]),
            tampered: Vec::new(),
            requests: RefCell::new(Vec::new()),
        }
    }

    fn read(&self, sha: &str) -> (ObjectKind, Vec<u8>) {
        let store = ObjectStore::open(&self.source).unwrap();
        let raw = store.read(sha).unwrap();
        (raw.kind, raw.content)
    }

    fn commit_node(&self, sha: &str) -> Value {
        let (_, content) = self.read(sha);
        let commit = parse_commit(&content).unwrap();
        let text = String::from_utf8(content).unwrap();
        let signature = text.find("\ngpgsig ").map(|start| {
            let (headers, message) = text.split_once("\n\n").unwrap();
            let armor: Vec<&str> = headers[start + 1..]
                .split('\n')
                .enumerate()
                .map(|(i, line)| {
                    if i == 0 {
                        line.strip_prefix("gpgsig ").unwrap()
                    } else {
                        line.strip_prefix(' ').unwrap()
                    }
                })
                .collect();
            json!({
                "signature": format!("{}\n", armor.join("\n")),
                "payload": format!("{}\n\n{message}", &headers[..start]),
            })
        });
        let identity = |id: &tool_github::git::date::Identity| json!({"name": id.name, "email": id.email, "date": to_iso8601(id)});
        json!({
            "oid": sha,
            "message": commit.message.strip_suffix('\n').unwrap_or(&commit.message),
            "tree": {"oid": commit.tree},
            "parents": {"nodes": commit.parents.iter().map(|p| json!({"oid": p})).collect::<Vec<_>>()},
            "author": identity(&commit.author),
            "committer": identity(&commit.committer),
            "signature": signature,
        })
    }

    fn history(&self, tip: &str) -> Vec<Value> {
        let mut nodes = Vec::new();
        let mut stack = vec![tip.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(sha) = stack.pop() {
            if nodes.len() == 100 || !seen.insert(sha.clone()) {
                continue;
            }
            let (_, content) = self.read(&sha);
            stack.extend(parse_commit(&content).unwrap().parents);
            nodes.push(self.commit_node(&sha));
        }
        nodes
    }
}

impl Transport for Mirror {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<HttpReply, String> {
        self.requests
            .borrow_mut()
            .push((method.to_string(), path.to_string()));
        let reply = |status: u16, body: Value| HttpReply {
            status,
            location: None,
            body: body.to_string().into_bytes(),
        };

        if path == "/repos/acme/widgets" {
            return Ok(reply(200, json!({"default_branch": "main"})));
        }
        if let Some(branch) = path.strip_prefix("/repos/acme/widgets/git/refs/heads/") {
            return Ok(match self.refs.get(branch) {
                Some(sha) => reply(200, json!({"object": {"sha": sha}})),
                None => reply(404, json!({"message": "Not Found"})),
            });
        }
        if path == "/graphql" {
            let oid = body.unwrap()["variables"]["oid"]
                .as_str()
                .unwrap()
                .to_string();
            let nodes = self.history(&oid);
            return Ok(reply(
                200,
                json!({"data": {"repository": {"object": {"history": {"nodes": nodes}}}}}),
            ));
        }
        if let Some(sha) = path.strip_prefix("/repos/acme/widgets/git/trees/") {
            let (_, content) = self.read(sha);
            let entries: Vec<Value> = parse_tree(&content)
                .unwrap()
                .into_iter()
                .map(|e| json!({"path": e.name, "mode": e.api_mode(), "type": e.api_type(), "sha": e.sha}))
                .collect();
            return Ok(reply(
                200,
                json!({"sha": sha, "tree": entries, "truncated": false}),
            ));
        }
        if let Some(sha) = path.strip_prefix("/repos/acme/widgets/git/blobs/") {
            let (_, mut content) = self.read(sha);
            if self.tampered.iter().any(|t| t == sha) {
                content.push(b'!');
            }
            return Ok(reply(
                200,
                json!({"sha": sha, "content": wrapped_base64(&content), "encoding": "base64"}),
            ));
        }
        Ok(reply(
            404,
            json!({"message": format!("unhandled {method} {path}")}),
        ))
    }
}

fn wrapped_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out.as_bytes()
        .chunks(60)
        .map(|line| format!("{}\n", std::str::from_utf8(line).unwrap()))
        .collect()
}

/// An empty repository: just the `objects` directory a git dir needs to be recognised.
fn empty_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("objects")).unwrap();
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// Every object reachable from `tip`, by sha, as `(kind, content)`.
fn reachable(git_dir: &Path, tip: &str) -> BTreeMap<String, (&'static str, Vec<u8>)> {
    let store = ObjectStore::open(git_dir).unwrap();
    let mut objects = BTreeMap::new();
    let mut stack = vec![tip.to_string()];
    while let Some(sha) = stack.pop() {
        if objects.contains_key(&sha) {
            continue;
        }
        let raw = store
            .read(&sha)
            .unwrap_or_else(|e| panic!("{sha} unreadable in {}: {e:?}", git_dir.display()));
        match raw.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&raw.content).unwrap();
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            ObjectKind::Tree => {
                stack.extend(parse_tree(&raw.content).unwrap().into_iter().map(|e| e.sha))
            }
            _ => {}
        }
        objects.insert(sha, (raw.kind.as_str(), raw.content));
    }
    objects
}

fn fetch(mirror: &mut Mirror, target: &Path, input: &str) -> Value {
    data(&ops::run(
        Some("http://gateway.invalid"),
        Some(CONFIG),
        input,
        mirror,
        target,
    ))
}

#[test]
fn fetching_into_an_empty_repository_rebuilds_the_whole_history_byte_for_byte() {
    let source = fixture_dir("packed_clone");
    let target = empty_repo();
    let mut mirror = Mirror::new(source.clone(), PACKED_CLONE_C3);

    let value = fetch(&mut mirror, target.path(), r#"{"op":"fetch"}"#);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(
        value["branch"],
        json!("main"),
        "the default branch, looked up from the repo"
    );
    assert_eq!(value["remote_ref"], json!("refs/remotes/origin/main"));
    assert_eq!(value["head_sha"], json!(PACKED_CLONE_C3));
    assert_eq!(value["previous_sha"], Value::Null);

    let expected = reachable(&source, PACKED_CLONE_C3);
    assert_eq!(
        reachable(target.path(), PACKED_CLONE_C3),
        expected,
        "every object, packed ones included"
    );
    assert_eq!(value["objects_written"], json!(expected.len()));
    assert_eq!(
        fs::read_to_string(target.path().join("refs/remotes/origin/main")).unwrap(),
        format!("{PACKED_CLONE_C3}\n")
    );
}

#[test]
fn fetching_requests_and_writes_only_what_the_repository_lacks() {
    let source = fixture_dir("packed_clone");
    let target = empty_repo();
    // C1 and C2 are in the pack; C3, its tree and its a.txt are the loose objects on top.
    copy_dir(
        &source.join("objects/pack"),
        &target.path().join("objects/pack"),
    );
    let mut mirror = Mirror::new(source.clone(), PACKED_CLONE_C3);

    let value = fetch(
        &mut mirror,
        target.path(),
        r#"{"op":"fetch","branch":"main"}"#,
    );
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["objects_written"], json!(3), "{value}");
    assert_eq!(
        reachable(target.path(), PACKED_CLONE_C3),
        reachable(&source, PACKED_CLONE_C3)
    );

    let requests = mirror.requests.borrow();
    assert_eq!(
        requests[0].1, "/repos/acme/widgets/git/refs/heads/main",
        "a named branch needs no repo lookup"
    );
    let trees = requests
        .iter()
        .filter(|(_, path)| path.contains("/git/trees/"))
        .count();
    assert_eq!(
        trees, 1,
        "the unchanged sub/ tree is already local: {requests:?}"
    );
}

#[test]
fn an_up_to_date_branch_writes_nothing() {
    let source = fixture_dir("packed_clone");
    let target = empty_repo();
    copy_dir(&source.join("objects"), &target.path().join("objects"));
    fs::create_dir_all(target.path().join("refs/remotes/origin")).unwrap();
    fs::write(
        target.path().join("refs/remotes/origin/main"),
        format!("{PACKED_CLONE_C3}\n"),
    )
    .unwrap();
    let mut mirror = Mirror::new(source, PACKED_CLONE_C3);

    let response = ops::run(
        Some("http://gateway.invalid"),
        Some(CONFIG),
        r#"{"op":"fetch","branch":"main"}"#,
        &mut mirror,
        target.path(),
    );
    let value = data(&response);
    assert_eq!(value["objects_written"], json!(0), "{value}");
    assert_eq!(value["previous_sha"], json!(PACKED_CLONE_C3));
    assert!(response.summary.contains("already"), "{}", response.summary);
    assert_eq!(mirror.requests.borrow().len(), 1, "only the ref lookup");
}

#[test]
fn a_signed_commit_is_rebuilt_with_its_signature() {
    let source = Fixture::new();
    source.write_file("a.txt", b"one");
    source.commit("first");
    source.write_file("a.txt", b"two");
    let tip = source.commit_signed("second, signed");
    let target = empty_repo();
    let mut mirror = Mirror::new(source.git_dir(), &tip);

    let value = fetch(
        &mut mirror,
        target.path(),
        r#"{"op":"fetch","remote":"upstream"}"#,
    );
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["remote_ref"], json!("refs/remotes/upstream/main"));
    assert_eq!(
        reachable(target.path(), &tip),
        reachable(&source.git_dir(), &tip)
    );
}

#[test]
fn an_object_that_does_not_hash_to_its_sha_is_refused_and_nothing_is_written() {
    let source = fixture_dir("packed_clone");
    let target = empty_repo();
    let mut mirror = Mirror::new(source.clone(), PACKED_CLONE_C3);
    let (_, c3) = mirror.read(PACKED_CLONE_C3);
    let (_, root) = mirror.read(&parse_commit(&c3).unwrap().tree);
    let a_txt = parse_tree(&root)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "a.txt")
        .unwrap();
    mirror.tampered.push(a_txt.sha.clone());

    let value = fetch(&mut mirror, target.path(), r#"{"op":"fetch"}"#);
    assert_eq!(value["error_kind"], json!("response_invalid"), "{value}");
    assert!(
        value["message"].as_str().unwrap().contains(&a_txt.sha),
        "{value}"
    );
    assert_eq!(
        fs::read_dir(target.path().join("objects")).unwrap().count(),
        0,
        "no object written"
    );
    assert!(!target.path().join("refs").exists(), "no ref written");
}

#[test]
fn a_branch_the_repo_does_not_have_is_not_found() {
    let target = empty_repo();
    let mut mirror = Mirror::new(fixture_dir("packed_clone"), PACKED_CLONE_C3);
    let value = fetch(
        &mut mirror,
        target.path(),
        r#"{"op":"fetch","branch":"nope"}"#,
    );
    assert_eq!(value["error_kind"], json!("not_found"), "{value}");
    assert!(
        value["message"].as_str().unwrap().contains("nope"),
        "{value}"
    );
}

#[test]
fn an_unsafe_remote_name_is_refused_before_any_request() {
    let target = empty_repo();
    let mut mirror = Mirror::new(fixture_dir("packed_clone"), PACKED_CLONE_C3);
    let value = fetch(
        &mut mirror,
        target.path(),
        r#"{"op":"fetch","remote":"../../hooks"}"#,
    );
    assert_eq!(value["error_kind"], json!("invalid_input"), "{value}");
    assert!(mirror.requests.borrow().is_empty());
}
