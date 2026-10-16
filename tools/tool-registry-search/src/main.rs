use std::collections::BTreeSet;
use std::io::Read;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

/// The index the latest default-artifacts release publishes beside its zips.
const DEFAULT_INDEX_URL: &str =
    "https://github.com/murmur-nexus/default-artifacts/releases/latest/download/artifacts-index.json";

/// The largest response body read as an index. A real index is a few hundred KiB at most.
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;

/// The `schema_version` this tool reads, as `mur index` writes it.
const INDEX_SCHEMA_VERSION: u64 = 2;

mod err {
    pub const FETCH_ERROR: &str = "fetch_error";
    pub const PARSE_ERROR: &str = "parse_error";
    pub const SCHEMA_ERROR: &str = "schema_error";
    pub const IO_ERROR: &str = "io_error";
}

// ── Schema 2 index types ──────────────────────────────────────────────────────
//
// Only the fields the results show are declared; serde ignores the rest
// (`interfaces`, each asset's `file` and `sha256`).

#[derive(Debug, Deserialize)]
struct Index {
    scope: String,
    artifacts: Vec<IndexedArtifact>,
}

#[derive(Debug, Deserialize)]
struct IndexedArtifact {
    name: String,
    runtime: String,
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    /// Newest first, as `mur index` writes them.
    versions: Vec<IndexedVersion>,
}

#[derive(Debug, Deserialize)]
struct IndexedVersion {
    version: String,
    release: String,
    built_at_level: Option<u64>,
    assets: Vec<IndexedAsset>,
}

#[derive(Debug, Deserialize)]
struct IndexedAsset {
    /// Present exactly when the payload is platform-specific.
    platform: Option<String>,
}

// ── Tool input ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    index: Option<String>,
    limit: Option<usize>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("fatal: failed to read stdin");
        std::process::exit(1);
    }
    let result = run(&raw);
    let output = serde_json::to_string(&result)
        .unwrap_or_else(|_| r#"{"ok":false,"message":"failed to serialize output"}"#.to_string());
    println!("{output}");
}

fn run(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return fail_msg("missing input on stdin");
    }

    let envelope: Value = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => return fail_msg(format!("invalid stdin JSON: {e}")),
    };

    let data_value = match envelope.get("data") {
        None | Some(Value::Null) => return fail_msg("missing data field"),
        Some(v) => v.clone(),
    };

    // data may be a JSON object directly or a JSON-encoded string (double-encoded)
    let params_value: Value = match &data_value {
        Value::String(s) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => return fail_msg(format!("invalid data JSON string: {e}")),
        },
        Value::Object(_) => data_value.clone(),
        _ => return fail_msg("data must be a JSON string or object"),
    };

    if params_value.get("registry").is_some() {
        return fail_msg(
            "the 'registry' input was removed; pass the index URL or file path as 'index' \
             (omit it for the latest default-artifacts release)",
        );
    }

    let params: SearchParams = match serde_json::from_value(params_value) {
        Ok(p) => p,
        Err(e) => return fail_msg(format!("invalid search parameters: {e}")),
    };

    if params.query.is_empty() {
        return fail_msg("query must not be empty");
    }

    if params.index.as_deref() == Some("local") {
        return fail_msg(format!(
            "'local' is the global artifact store on the host, which a capsule cannot see; \
             run 'mur search {} --index local' on the host instead",
            params.query
        ));
    }

    let source = params.index.as_deref().unwrap_or(DEFAULT_INDEX_URL);
    let limit = params.limit.unwrap_or(10);
    let query_lower = params.query.to_lowercase();

    let index = match load_index(source) {
        Ok(index) => index,
        Err(e) => return e,
    };

    let results: Vec<Value> = rank_and_limit(index.artifacts, &index.scope, &query_lower, limit)
        .into_iter()
        .map(|artifact| result_row(&index.scope, artifact))
        .collect();
    let count = results.len();
    let message = if count == 0 {
        format!("no results for '{}'", params.query)
    } else {
        format!("{count} result(s) for '{}'", params.query)
    };
    ok_with(
        &message,
        json!({ "index": source, "scope": index.scope, "count": count, "results": results }),
        format!("{count} result(s)"),
    )
}

// ── Loading the index ─────────────────────────────────────────────────────────

/// Reads the index at `source`: a URL when it starts with `http://` or `https://`, otherwise a
/// file path, resolved against the working directory when relative. This is how
/// `mur search --index` reads its value.
fn load_index(source: &str) -> Result<Index, Value> {
    let bytes = if source.starts_with("http://") || source.starts_with("https://") {
        fetch(source)?
    } else {
        std::fs::read(source)
            .map_err(|e| err_result(err::IO_ERROR, format!("failed to read {source}: {e}")))?
    };
    read_index(&bytes, source)
}

/// GETs `url`, following redirects, and returns its body of at most [`MAX_INDEX_BYTES`].
fn fetch(url: &str) -> Result<Vec<u8>, Value> {
    let response = match ureq::get(url).timeout(Duration::from_secs(30)).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            return Err(err_result(
                err::FETCH_ERROR,
                format!("HTTP {code} fetching {url}"),
            ))
        }
        Err(e) => {
            return Err(err_result(
                err::FETCH_ERROR,
                format!("failed to fetch {url}: {e}"),
            ))
        }
    };

    let mut body = Vec::new();
    if let Err(e) = response
        .into_reader()
        .take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut body)
    {
        return Err(err_result(
            err::FETCH_ERROR,
            format!("failed to fetch {url}: {e}"),
        ));
    }
    if body.len() as u64 > MAX_INDEX_BYTES {
        return Err(err_result(
            err::FETCH_ERROR,
            format!("{url} answered with more than 16 MiB; an artifact index is never that large"),
        ));
    }
    Ok(body)
}

/// Parses `bytes` read from `source` as a schema 2 artifact index. A schema 1 index is refused
/// with the command its publisher regenerates it with.
fn read_index(bytes: &[u8], source: &str) -> Result<Index, Value> {
    let parse_error = |detail: String| {
        err_result(
            err::PARSE_ERROR,
            format!("failed to parse the artifact index from {source}: {detail}"),
        )
    };

    let value: Value = serde_json::from_slice(bytes).map_err(|e| parse_error(e.to_string()))?;
    let Some(root) = value.as_object() else {
        return Err(parse_error("the document is not a JSON object".to_string()));
    };

    match root.get("schema_version") {
        Some(Value::Number(n)) if n.as_u64() == Some(INDEX_SCHEMA_VERSION) => {}
        Some(Value::Number(n)) if n.as_u64() == Some(1) => return Err(schema_1_error(source)),
        Some(Value::String(s)) if s == "1" => return Err(schema_1_error(source)),
        Some(other) => {
            return Err(err_result(
                err::SCHEMA_ERROR,
                format!(
                    "{source} has schema_version {other}; this tool reads only schema 2 \
                     artifact indexes"
                ),
            ))
        }
        None => {
            return Err(err_result(
                err::SCHEMA_ERROR,
                format!(
                    "{source} has no schema_version; this tool reads only schema 2 artifact \
                     indexes"
                ),
            ))
        }
    }

    serde_json::from_value(value).map_err(|e| parse_error(e.to_string()))
}

fn schema_1_error(source: &str) -> Value {
    err_result(
        err::SCHEMA_ERROR,
        format!(
            "{source} is a schema 1 artifact index, and this tool reads only schema 2; its \
             publisher regenerates it with `mur index <dir> --repo <owner>/<repo> --release <tag>`"
        ),
    )
}

// ── Filter and rank ───────────────────────────────────────────────────────────

/// The artifacts matching `query` (already lowercased), best match first, at most `limit`.
/// The sort is stable, so the index's name order holds within a rank.
fn rank_and_limit(
    artifacts: Vec<IndexedArtifact>,
    scope: &str,
    query: &str,
    limit: usize,
) -> Vec<IndexedArtifact> {
    let mut ranked: Vec<(usize, IndexedArtifact)> = artifacts
        .into_iter()
        .filter_map(|a| {
            let name_lower = a.name.to_lowercase();
            let full_lower = format!("{scope}/{name_lower}").to_lowercase();
            let rank = if full_lower == query || name_lower == query {
                0 // exact name match
            } else if full_lower.starts_with(query) || name_lower.starts_with(query) {
                1 // name prefix match
            } else {
                let in_name = full_lower.contains(query);
                let in_desc = a
                    .description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(query));
                let in_tags = a.tags.iter().any(|t| t.to_lowercase().contains(query));
                if in_name || in_desc || in_tags {
                    2 // substring match in name, description, or tags
                } else {
                    return None;
                }
            };
            Some((rank, a))
        })
        .collect();

    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().take(limit).map(|(_, a)| a).collect()
}

/// One result: the artifact under its full `<scope>/<name>`, every version in the index's
/// order. It states what is published; whether a version fits is `mur`'s call at install.
fn result_row(scope: &str, artifact: IndexedArtifact) -> Value {
    let versions: Vec<Value> = artifact
        .versions
        .into_iter()
        .map(|v| {
            json!({
                "version": v.version,
                "release": v.release,
                "built_at_level": v.built_at_level,
                "platforms": platforms(&v.assets),
            })
        })
        .collect();
    json!({
        "name": format!("{scope}/{}", artifact.name),
        "runtime": artifact.runtime,
        "description": artifact.description,
        "tags": artifact.tags,
        "versions": versions,
    })
}

/// The platforms a version's payloads cover, sorted and deduplicated; `any` stands for a
/// platform-independent payload.
fn platforms(assets: &[IndexedAsset]) -> BTreeSet<String> {
    assets
        .iter()
        .map(|a| a.platform.clone().unwrap_or_else(|| "any".to_string()))
        .collect()
}

// ── Output constructors ───────────────────────────────────────────────────────

fn ok_with(message: impl Into<String>, data: Value, summary: impl Into<String>) -> Value {
    let msg = message.into();
    let sum = summary.into();
    let data_clone = data.clone();
    let mut obj = json!({
        "ok": true,
        "message": &msg,
        "status": "passed",
        "summary": &sum,
        "data": data_clone,
        "data_path": null,
        "metadata": null,
    });
    if let Value::Object(map) = data {
        for (k, v) in map {
            obj[k] = v;
        }
    }
    obj
}

fn fail_msg(message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "message": &msg,
        "status": "error",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

fn err_result(error_kind: &str, message: impl Into<String>) -> Value {
    let msg = message.into();
    json!({
        "ok": false,
        "error_kind": error_kind,
        "message": &msg,
        "status": "error",
        "summary": &msg,
        "data": null,
        "data_path": null,
        "metadata": null,
    })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/artifacts-index.json"
    );

    fn artifact(name: &str, description: Option<&str>, tags: &[&str]) -> IndexedArtifact {
        IndexedArtifact {
            name: name.into(),
            runtime: "tool".into(),
            description: description.map(Into::into),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            versions: vec![],
        }
    }

    fn names(results: &[IndexedArtifact]) -> Vec<&str> {
        results.iter().map(|a| a.name.as_str()).collect()
    }

    fn index_json(schema_version: Value, artifacts: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": schema_version,
            "scope": "acme",
            "artifacts": artifacts,
        }))
        .unwrap()
    }

    fn message(out: &Value) -> &str {
        out["message"].as_str().unwrap()
    }

    // ── input handling ──

    #[test]
    fn empty_input_returns_error() {
        assert_eq!(message(&run("")), "missing input on stdin");
    }

    #[test]
    fn invalid_stdin_json_returns_error() {
        let out = run("{not json");
        assert_eq!(out["ok"], false);
        assert!(message(&out).starts_with("invalid stdin JSON: "));
    }

    #[test]
    fn missing_data_field_returns_error() {
        let out = run(r#"{"query":"git"}"#);
        assert_eq!(out["ok"], false);
        assert_eq!(message(&out), "missing data field");
        assert_eq!(message(&run(r#"{"data":null}"#)), "missing data field");
    }

    #[test]
    fn data_of_another_type_returns_error() {
        let out = run(r#"{"data":3}"#);
        assert_eq!(message(&out), "data must be a JSON string or object");
    }

    #[test]
    fn invalid_double_encoded_data_returns_error() {
        let out = run(r#"{"data":"{nope"}"#);
        assert!(message(&out).starts_with("invalid data JSON string: "));
    }

    #[test]
    fn missing_query_returns_error() {
        let out = run(r#"{"data":{}}"#);
        assert_eq!(out["ok"], false);
        assert!(message(&out).starts_with("invalid search parameters: "));
        assert!(message(&out).contains("query"));
    }

    #[test]
    fn empty_query_returns_error() {
        assert_eq!(
            message(&run(r#"{"data":{"query":""}}"#)),
            "query must not be empty"
        );
    }

    #[test]
    fn registry_input_is_refused_naming_index() {
        for registry in [r#""local""#, r#""https://example.com/i.json""#, "null"] {
            let out = run(&format!(
                r#"{{"data":{{"query":"git","registry":{registry}}}}}"#
            ));
            assert_eq!(out["ok"], false);
            assert!(out.get("error_kind").is_none());
            assert_eq!(
                message(&out),
                "the 'registry' input was removed; pass the index URL or file path as 'index' \
                 (omit it for the latest default-artifacts release)"
            );
        }
    }

    #[test]
    fn local_index_is_refused_naming_mur_search() {
        let out = run(r#"{"data":{"query":"Editor","index":"local"}}"#);
        assert_eq!(out["ok"], false);
        assert_eq!(
            message(&out),
            "'local' is the global artifact store on the host, which a capsule cannot see; \
             run 'mur search Editor --index local' on the host instead"
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let out = run(&format!(
            r#"{{"data":{{"query":"tool-editor","index":"{FIXTURE}","extra":1}}}}"#
        ));
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["count"], 1);
    }

    #[test]
    fn double_encoded_data_reads_the_fixture() {
        let inner = json!({ "query": "tool-editor", "index": FIXTURE }).to_string();
        let envelope = json!({ "data": inner }).to_string();
        let out = run(&envelope);
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["name"], "murmur/tool-editor");
    }

    #[test]
    fn success_output_mirrors_data_at_the_top_level() {
        let out = run(&json!({ "data": { "query": "zzz", "index": FIXTURE } }).to_string());
        assert_eq!(out["ok"], true);
        assert_eq!(out["status"], "passed");
        assert_eq!(message(&out), "no results for 'zzz'");
        assert_eq!(out["summary"], "0 result(s)");
        assert_eq!(out["data"]["count"], 0);
        assert_eq!(out["data"]["scope"], "murmur");
        assert_eq!(out["data"]["index"], FIXTURE);
        assert_eq!(out["count"], out["data"]["count"]);
    }

    // ── reading the index ──

    #[test]
    fn read_index_reads_schema_2() {
        let bytes = index_json(
            json!(2),
            json!([{
                "name": "tool-x", "runtime": "tool", "tags": ["x"],
                "versions": [{
                    "version": "0.1.0", "release": "v1", "built_at_level": 1,
                    "interfaces": ["murmur:tool/run@1.0.0"],
                    "assets": [{"file": "tool-x-0.1.0.mur.zip", "sha256": "00"}]
                }]
            }]),
        );
        let index = read_index(&bytes, "src").unwrap();
        assert_eq!(index.scope, "acme");
        assert_eq!(index.artifacts[0].versions[0].built_at_level, Some(1));
        assert_eq!(index.artifacts[0].description, None);
    }

    #[test]
    fn read_index_refuses_schema_1_as_string_and_integer() {
        for v in [json!("1"), json!(1)] {
            let out = read_index(&index_json(v, json!([])), "src").unwrap_err();
            assert_eq!(out["error_kind"], err::SCHEMA_ERROR);
            assert_eq!(
                message(&out),
                "src is a schema 1 artifact index, and this tool reads only schema 2; its \
                 publisher regenerates it with `mur index <dir> --repo <owner>/<repo> --release <tag>`"
            );
        }
    }

    #[test]
    fn read_index_refuses_other_schema_versions() {
        let out = read_index(&index_json(json!(3), json!([])), "src").unwrap_err();
        assert_eq!(out["error_kind"], err::SCHEMA_ERROR);
        assert_eq!(
            message(&out),
            "src has schema_version 3; this tool reads only schema 2 artifact indexes"
        );
        let out = read_index(&index_json(json!("2"), json!([])), "src").unwrap_err();
        assert_eq!(out["error_kind"], err::SCHEMA_ERROR);
        assert!(message(&out).contains(r#"schema_version "2""#));
    }

    #[test]
    fn read_index_refuses_a_missing_schema_version() {
        let bytes = br#"{"scope":"acme","artifacts":[]}"#;
        let out = read_index(bytes, "src").unwrap_err();
        assert_eq!(out["error_kind"], err::SCHEMA_ERROR);
        assert_eq!(
            message(&out),
            "src has no schema_version; this tool reads only schema 2 artifact indexes"
        );
    }

    #[test]
    fn read_index_refuses_malformed_documents() {
        for bytes in [&b"not json"[..], b"[1,2]", b"\xff\xfe"] {
            let out = read_index(bytes, "src").unwrap_err();
            assert_eq!(out["error_kind"], err::PARSE_ERROR);
            assert!(message(&out).starts_with("failed to parse the artifact index from src: "));
        }
    }

    #[test]
    fn read_index_names_a_missing_field() {
        let bytes = index_json(json!(2), json!([{"name": "tool-x", "runtime": "tool"}]));
        let out = read_index(&bytes, "src").unwrap_err();
        assert_eq!(out["error_kind"], err::PARSE_ERROR);
        assert!(message(&out).contains("src"));
        assert!(message(&out).contains("versions"), "{out}");
    }

    // ── results ──

    #[test]
    fn missing_built_at_level_is_null() {
        let bytes = index_json(
            json!(2),
            json!([{
                "name": "tool-x", "runtime": "tool", "tags": [],
                "versions": [{"version": "0.1.0", "release": "v1", "interfaces": [],
                              "assets": [{"file": "tool-x-0.1.0.mur.zip", "sha256": "00"}]}]
            }]),
        );
        let mut index = read_index(&bytes, "src").unwrap();
        let row = result_row(&index.scope, index.artifacts.remove(0));
        assert_eq!(row["name"], "acme/tool-x");
        assert_eq!(row["description"], Value::Null);
        assert_eq!(row["versions"][0]["built_at_level"], Value::Null);
        assert_eq!(row["versions"][0]["platforms"], json!(["any"]));
    }

    #[test]
    fn platforms_are_sorted_deduplicated_with_any_first() {
        let asset = |p: Option<&str>| IndexedAsset {
            platform: p.map(Into::into),
        };
        let assets = [
            asset(Some("linux-x86_64")),
            asset(Some("darwin-aarch64")),
            asset(Some("linux-x86_64")),
            asset(None),
        ];
        let got: Vec<String> = platforms(&assets).into_iter().collect();
        assert_eq!(got, ["any", "darwin-aarch64", "linux-x86_64"]);
    }

    #[test]
    fn rank_exact_full_or_bare_name_first() {
        let artifacts = || {
            vec![
                artifact("tool-git-extras", None, &[]),
                artifact("tool-git", None, &[]),
                artifact("git", None, &[]),
            ]
        };
        assert_eq!(
            names(&rank_and_limit(artifacts(), "acme", "git", 10))[0],
            "git"
        );
        assert_eq!(
            names(&rank_and_limit(artifacts(), "acme", "acme/tool-git", 10)),
            ["tool-git", "tool-git-extras"]
        );
    }

    #[test]
    fn rank_prefix_before_substring() {
        let artifacts = vec![
            artifact("tool-desc-editor", Some("git operations"), &[]),
            artifact("git-tool", None, &[]),
        ];
        let results = rank_and_limit(artifacts, "acme", "git", 10);
        assert_eq!(names(&results), ["git-tool", "tool-desc-editor"]);
    }

    #[test]
    fn rank_scope_prefix_matches_every_artifact() {
        let artifacts = vec![artifact("b", None, &[]), artifact("a", None, &[])];
        let results = rank_and_limit(artifacts, "acme", "acme/", 10);
        assert_eq!(names(&results), ["b", "a"]);
    }

    #[test]
    fn rank_matches_description_and_tags() {
        let artifacts = vec![
            artifact("tool-a", Some("Reads Patches"), &[]),
            artifact("tool-b", None, &["PATCHING"]),
            artifact("tool-c", None, &[]),
        ];
        let results = rank_and_limit(artifacts, "acme", "patch", 10);
        assert_eq!(names(&results), ["tool-a", "tool-b"]);
    }

    #[test]
    fn rank_respects_limit() {
        let artifacts: Vec<IndexedArtifact> = (0..20)
            .map(|i| artifact(&format!("tool-{i}"), None, &["murmur"]))
            .collect();
        assert_eq!(rank_and_limit(artifacts, "acme", "murmur", 3).len(), 3);
    }

    #[test]
    fn rank_no_match_returns_empty() {
        let artifacts = vec![artifact("tool-git", None, &[])];
        assert!(rank_and_limit(artifacts, "acme", "zzznomatch", 10).is_empty());
    }

    // ── output constructors ──

    #[test]
    fn fail_msg_returns_ok_false() {
        let out = fail_msg("something went wrong");
        assert_eq!(out["ok"], false);
        assert_eq!(out["message"], "something went wrong");
    }

    #[test]
    fn err_result_includes_error_kind() {
        let out = err_result(err::FETCH_ERROR, "failed to fetch url");
        assert_eq!(out["ok"], false);
        assert_eq!(out["error_kind"], err::FETCH_ERROR);
    }
}
