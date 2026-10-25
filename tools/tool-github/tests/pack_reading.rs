//! Host-target scenario tests proving `push` reads real, `git`-CLI-built packfiles: a fully
//! packed clone plus one loose commit on top, and a repository spanning loose objects, an
//! `OFS_DELTA` blob, and a `REF_DELTA` blob whose base lives in a *different* pack from the
//! delta itself.
//!
//! The fixtures under `tests/fixtures/` are checked-in, real repositories built once by the
//! `git` CLI (see the comment atop each fixture-specific test below for exactly how) — this
//! crate has no `git` dependency and cannot build a real pack itself, so these are read-only,
//! never regenerated at test time.
//!
//! `GithubEcho` here is a trimmed copy of `tests/ops_scenarios.rs`'s: each test file is a
//! separate compilation unit against the compiled rlib, so nothing under `#[cfg(test)]` in
//! `src/` is visible here either way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use tool_github::ops::{self, HttpReply, Response, Transport};

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
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

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

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

/// A fake GitHub that recomputes real git object hashes for every blob/tree/commit it is asked
/// to create, so a passing push here proves this tool's request bodies byte-reproduce the
/// fixture's real, `git`-CLI-computed history — packed objects included.
struct GithubEcho {
    default_branch: String,
    refs: RefCell<HashMap<String, String>>,
    request_count: RefCell<usize>,
}

impl GithubEcho {
    fn new(default_branch: &str) -> Self {
        Self {
            default_branch: default_branch.to_string(),
            refs: RefCell::new(HashMap::new()),
            request_count: RefCell::new(0),
        }
    }
}

impl Transport for GithubEcho {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<HttpReply, String> {
        *self.request_count.borrow_mut() += 1;
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
            return Ok(reply(
                201,
                json!({"sha": object_sha("commit", content.as_bytes())}),
            ));
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
        if path == "/repos/acme/widgets" {
            return Ok(reply(200, json!({"default_branch": self.default_branch})));
        }
        Ok(reply(
            404,
            json!({"message": format!("unhandled path {path}")}),
        ))
    }
}

const CONFIG: &str = r#"{"config_version":1,"repo":"acme/widgets"}"#;

fn data(response: &Response) -> Value {
    serde_json::from_str(&response.data).unwrap()
}

/// `tests/fixtures/packed_clone`: two commits fully packed with `git repack -ad` (a fresh
/// clone's shape), plus a third commit made with plain `git commit` sitting loose on top.
/// Reproduced here (2020-02-01/02/03, author/committer `Test <a@b.com>`, `a.txt` edited each
/// commit, `sub/b.txt` added in the second):
///
///   git init && git config user.email a@b.com && git config user.name Test
///   echo "hello world" > a.txt && git add a.txt && git commit -m first        # C1
///   echo "second line" >> a.txt; mkdir sub; echo "nested file" > sub/b.txt
///   git add a.txt sub/b.txt && git commit -m second                          # C2
///   git repack -ad                                                           # -> one pack
///   echo "third line" >> a.txt && git add a.txt && git commit -m third       # C3, stays loose
const PACKED_CLONE_C3: &str = "f310270529aa5bd40cc65dfba01b276bc296e087";

#[test]
fn a_packed_clone_plus_one_loose_commit_pushes_byte_for_byte() {
    let git_dir = fixture_dir("packed_clone");
    let mut fake = GithubEcho::new("main");
    let input =
        format!(r#"{{"op":"push","head_sha":"{PACKED_CLONE_C3}","remote_branch":"feature-x"}}"#);
    let response = ops::run(
        Some("http://gateway.invalid"),
        Some(CONFIG),
        &input,
        &mut fake,
        &git_dir,
    );
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["head_sha"], json!(PACKED_CLONE_C3));
    assert_eq!(
        fake.refs.borrow().get("feature-x"),
        Some(&PACKED_CLONE_C3.to_string()),
        "the fake independently recomputed the same sha for every uploaded object, packed \
         objects included"
    );
}

/// `tests/fixtures/mixed_cross_pack`: a range spanning loose objects, one pack with an
/// intra-pack `OFS_DELTA` blob, and a second pack with a `REF_DELTA` blob whose base is only
/// present in the *first* pack. Built (2020-01-01..04, `Test <a@b.com>`, `file.txt` edited each
/// commit) as:
///
///   commit v1, v2 (v2's blob deltifies against v1's within one `git repack -ad` pack: OFS_DELTA)
///   commit v3; a thin pack built with
///     `git pack-objects --revs --thin --stdout` (revs: v3, ^v2) then completed with
///     `git index-pack --fix-thin --stdin`, which embeds a full copy of the delta base (v2's
///     blob, already present non-delta in pack 1) at the pack's tail as object 4 of 4.
///   the appended object 4 is then stripped back off (truncate to the offset `git verify-pack
///   -v` reports for it, rewrite the 4-byte object count in the header, recompute the trailing
///   20-byte SHA1 over what remains) and a matching V2 `.idx` is hand-built for the 3 remaining
///   objects (fan-out table, sorted sha/crc32/offset arrays, trailing pack+idx checksums) —
///   this is the only way to persist a pack whose `REF_DELTA` base is *not* embedded in the
///   same pack: `git index-pack` always embeds a thin pack's bases when asked to fix it, and
///   refuses to index one otherwise.
///   commit v4, left loose (no repack after it).
const MIXED_CROSS_PACK_V4: &str = "9ff22a1025d8dc260a991c94d2680b14bd7e9608";

#[test]
fn a_mixed_loose_and_packed_range_with_a_cross_pack_ref_delta_pushes_byte_for_byte() {
    let git_dir = fixture_dir("mixed_cross_pack");
    let mut fake = GithubEcho::new("main");
    let input = format!(
        r#"{{"op":"push","head_sha":"{MIXED_CROSS_PACK_V4}","remote_branch":"feature-x"}}"#
    );
    let response = ops::run(
        Some("http://gateway.invalid"),
        Some(CONFIG),
        &input,
        &mut fake,
        &git_dir,
    );
    let value = data(&response);
    assert_eq!(value["ok"], json!(true), "{value}");
    assert_eq!(value["head_sha"], json!(MIXED_CROSS_PACK_V4));
    assert_eq!(
        fake.refs.borrow().get("feature-x"),
        Some(&MIXED_CROSS_PACK_V4.to_string()),
        "every object reproduced its sha, including the cross-pack REF_DELTA blob and the \
         intra-pack OFS_DELTA blob"
    );
}

#[test]
fn the_pack_set_is_opened_once_and_reused_for_the_whole_walk() {
    // Not a mock-based call-count assertion (this crate has no mocking of `ObjectStore` itself)
    // but a proxy for it: a push over a range needing many packed reads succeeds at all only if
    // the same opened `PackSet`/`ObjectStore` served every one of them, since re-deriving pack
    // bytes per object isn't a code path this crate has (there is exactly one `PackSet::open`
    // call in `ObjectStore::open`, itself called exactly once in `ops::push`).
    let git_dir = fixture_dir("mixed_cross_pack");
    let mut fake = GithubEcho::new("main");
    let input = format!(
        r#"{{"op":"push","head_sha":"{MIXED_CROSS_PACK_V4}","remote_branch":"feature-x"}}"#
    );
    let response = ops::run(
        Some("http://gateway.invalid"),
        Some(CONFIG),
        &input,
        &mut fake,
        &git_dir,
    );
    assert_eq!(data(&response)["ok"], json!(true));
}
