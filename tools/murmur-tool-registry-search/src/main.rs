use std::io::Read;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/murmur-nexus/default-artifacts/main/artifacts-index.json";

mod err {
    pub const FETCH_ERROR: &str = "fetch_error";
    pub const PARSE_ERROR: &str = "parse_error";
    pub const SCHEMA_ERROR: &str = "schema_error";
    pub const IO_ERROR: &str = "io_error";
}

// ── Remote index types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ArtifactIndex {
    schema_version: String,
    updated_at: String,
    artifacts: Vec<ArtifactIndexEntry>,
}

#[derive(Debug, Deserialize)]
struct ArtifactIndexEntry {
    name: String,
    version: String,
    runtime: String,
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    platforms: Vec<String>,
}

// ── Local .meta.json types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MetaFile {
    meta: StoredMeta,
}

#[derive(Debug, Deserialize)]
struct StoredMeta {
    name: String,
    version: String,
    artifact_runtime: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

// ── Tool input ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    registry: Option<String>,
    limit: Option<usize>,
}

// ── Internal candidate for ranking ───────────────────────────────────────────

struct Candidate {
    name: String,
    version: String,
    runtime: String,
    description: Option<String>,
    tags: Vec<String>,
}

// ── Output row ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct SearchRow {
    name: String,
    version: String,
    runtime: String,
    description: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("fatal: failed to read stdin");
        std::process::exit(1);
    }
    let result = run(&raw);
    let output = serde_json::to_string(&result).unwrap_or_else(|_| {
        r#"{"ok":false,"message":"failed to serialize output"}"#.to_string()
    });
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

    let params: SearchParams = match serde_json::from_value(params_value) {
        Ok(p) => p,
        Err(e) => return fail_msg(format!("invalid search parameters: {e}")),
    };

    if params.query.is_empty() {
        return fail_msg("query must not be empty");
    }

    let limit = params.limit.unwrap_or(10);
    let query_lower = params.query.to_lowercase();

    match do_search(&params.registry, &query_lower, limit) {
        Ok((results, published_date)) => {
            let count = results.len();
            let message = if count == 0 {
                format!("no results for '{}'", params.query)
            } else {
                format!("{count} result(s) for '{}'", params.query)
            };
            let items: Vec<Value> = results
                .iter()
                .map(|r| {
                    json!({
                        "name": r.name,
                        "version": r.version,
                        "runtime": r.runtime,
                        "description": r.description,
                        "published_date": published_date.as_str(),
                    })
                })
                .collect();
            ok_with(
                &message,
                json!({ "results": items, "count": count }),
                format!("{count} result(s)"),
            )
        }
        Err(e) => e,
    }
}

// ── Search dispatch ───────────────────────────────────────────────────────────

fn do_search(
    registry: &Option<String>,
    query_lower: &str,
    limit: usize,
) -> Result<(Vec<SearchRow>, String), Value> {
    match registry.as_deref() {
        None => fetch_and_search(DEFAULT_INDEX_URL, query_lower, limit),
        Some("local") => local_search(query_lower, limit),
        Some(r) if r.starts_with('/') => file_search(r, query_lower, limit),
        Some(url) => fetch_and_search(url, query_lower, limit),
    }
}

// ── Remote / file-path fetch ──────────────────────────────────────────────────

/// Returns true if the URL hostname is a GitHub endpoint that accepts bearer token auth.
fn is_github_url(url: &str) -> bool {
    // Extract hostname from "https://hostname/..." without pulling in the url crate.
    let after_scheme = url
        .find("://")
        .map(|i| &url[i + 3..])
        .unwrap_or(url);
    let host = after_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    host == "raw.githubusercontent.com" || host == "api.github.com"
}

fn fetch_and_search(
    url: &str,
    query_lower: &str,
    limit: usize,
) -> Result<(Vec<SearchRow>, String), Value> {
    let mut request = ureq::get(url).timeout(Duration::from_secs(30));

    // Inject GitHub bearer token when available and the URL is a GitHub endpoint.
    if is_github_url(url) {
        if let Ok(token) = std::env::var("MURMUR_REGISTRY_TOKEN") {
            if !token.is_empty() {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
        }
    }

    let response = match request.call() {
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

    let index: ArtifactIndex = match response.into_json() {
        Ok(i) => i,
        Err(e) => {
            return Err(err_result(
                err::PARSE_ERROR,
                format!("failed to parse index from {url}: {e}"),
            ))
        }
    };

    apply_index(index, url, query_lower, limit)
}

fn file_search(
    path: &str,
    query_lower: &str,
    limit: usize,
) -> Result<(Vec<SearchRow>, String), Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return Err(err_result(
                err::IO_ERROR,
                format!("failed to read {path}: {e}"),
            ))
        }
    };

    let index: ArtifactIndex = match serde_json::from_str(&content) {
        Ok(i) => i,
        Err(e) => {
            return Err(err_result(
                err::PARSE_ERROR,
                format!("failed to parse index from {path}: {e}"),
            ))
        }
    };

    apply_index(index, path, query_lower, limit)
}

fn apply_index(
    index: ArtifactIndex,
    source: &str,
    query_lower: &str,
    limit: usize,
) -> Result<(Vec<SearchRow>, String), Value> {
    if index.schema_version != "1" {
        return Err(err_result(
            err::SCHEMA_ERROR,
            format!(
                "unsupported schema_version '{}' from {}",
                index.schema_version, source
            ),
        ));
    }

    let published_date = index.updated_at.clone();
    let candidates: Vec<Candidate> = index
        .artifacts
        .into_iter()
        .map(|e| Candidate {
            name: e.name,
            version: e.version,
            runtime: e.runtime,
            description: e.description,
            tags: e.tags,
        })
        .collect();

    let rows = rank_and_limit(candidates, query_lower, limit)
        .into_iter()
        .map(|c| SearchRow {
            name: c.name,
            version: c.version,
            runtime: c.runtime,
            description: c.description.unwrap_or_else(|| "\u{2014}".to_string()),
        })
        .collect();

    Ok((rows, published_date))
}

// ── Local store scan ──────────────────────────────────────────────────────────

fn local_search(query_lower: &str, limit: usize) -> Result<(Vec<SearchRow>, String), Value> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            return Err(err_result(
                err::IO_ERROR,
                "HOME environment variable not set",
            ))
        }
    };

    let store_root = std::path::Path::new(&home)
        .join(".murmur")
        .join("artifacts");

    let mut candidates: Vec<Candidate> = Vec::new();

    if store_root.exists() {
        let name_dirs = match std::fs::read_dir(&store_root) {
            Ok(d) => d,
            Err(e) => {
                return Err(err_result(
                    err::IO_ERROR,
                    format!("failed to read artifact store: {e}"),
                ))
            }
        };

        for name_entry in name_dirs.flatten() {
            if !name_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let version_dirs = match std::fs::read_dir(name_entry.path()) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for version_entry in version_dirs.flatten() {
                if !version_entry
                    .file_type()
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let files = match std::fs::read_dir(version_entry.path()) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    let is_meta_json = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.ends_with(".meta.json"))
                        .unwrap_or(false);
                    if !is_meta_json {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(mf) = serde_json::from_str::<MetaFile>(&content) {
                            candidates.push(Candidate {
                                name: mf.meta.name,
                                version: mf.meta.version,
                                runtime: mf.meta.artifact_runtime,
                                description: mf.meta.description,
                                tags: mf.meta.tags,
                            });
                        }
                    }
                }
            }
        }
    }

    let rows = rank_and_limit(candidates, query_lower, limit)
        .into_iter()
        .map(|c| SearchRow {
            name: c.name,
            version: c.version,
            runtime: c.runtime,
            description: c.description.unwrap_or_else(|| "\u{2014}".to_string()),
        })
        .collect();

    Ok((rows, String::new()))
}

// ── Filter and rank ───────────────────────────────────────────────────────────

fn rank_and_limit(candidates: Vec<Candidate>, query: &str, limit: usize) -> Vec<Candidate> {
    let mut ranked: Vec<(usize, Candidate)> = candidates
        .into_iter()
        .filter_map(|c| {
            let name_lower = c.name.to_lowercase();
            let rank = if name_lower == query {
                0 // exact name match
            } else if name_lower.starts_with(query) {
                1 // name prefix match
            } else {
                let in_name = name_lower.contains(query);
                let in_desc = c
                    .description
                    .as_deref()
                    .map(|d| d.to_lowercase().contains(query))
                    .unwrap_or(false);
                let in_tags = c.tags.iter().any(|t| t.to_lowercase().contains(query));
                if in_name || in_desc || in_tags {
                    2 // substring match in name, description, or tags
                } else {
                    return None;
                }
            };
            Some((rank, c))
        })
        .collect();

    ranked.sort_by_key(|(rank, _)| *rank); // stable sort preserves insertion order within tier
    ranked.into_iter().take(limit).map(|(_, c)| c).collect()
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

    #[test]
    fn is_github_url_recognises_raw_githubusercontent() {
        assert!(is_github_url("https://raw.githubusercontent.com/org/repo/main/file.json"));
    }

    #[test]
    fn is_github_url_recognises_api_github() {
        assert!(is_github_url("https://api.github.com/repos/org/repo/contents/file"));
    }

    #[test]
    fn is_github_url_rejects_other_hosts() {
        assert!(!is_github_url("https://example.com/artifacts-index.json"));
        assert!(!is_github_url("https://my-registry.internal/index.json"));
    }

    #[test]
    fn is_github_url_rejects_github_lookalike() {
        // Subdomain of github.com is not auto-injected — only the exact hostnames.
        assert!(!is_github_url("https://github.com/org/repo/file.json"));
    }

    #[test]
    fn empty_input_returns_error() {
        let out = run("");
        assert_eq!(out["ok"], false);
        assert!(out["message"].as_str().unwrap().contains("missing input"));
    }

    #[test]
    fn missing_data_field_returns_error() {
        let out = run(r#"{"query":"git"}"#);
        assert_eq!(out["ok"], false);
        assert!(out["message"].as_str().unwrap().contains("missing data"));
    }

    #[test]
    fn missing_query_returns_error() {
        let out = run(r#"{"data":{"registry":null}}"#);
        assert_eq!(out["ok"], false);
    }

    #[test]
    fn double_encoded_data_local_registry() {
        // Use "local" registry to avoid network call; tests double-encoding path
        let inner = r#"{"query":"zzznomatch999","registry":"local"}"#;
        let escaped = inner.replace('\\', "\\\\").replace('"', "\\\"");
        let envelope = format!(r#"{{"data":"{}"}}"#, escaped);
        let out = run(&envelope);
        assert_eq!(out["ok"], true);
        assert_eq!(out["count"], 0);
    }

    #[test]
    fn rank_and_limit_exact_match_first() {
        let candidates = vec![
            Candidate {
                name: "murmur-tool-git-extras".into(),
                version: "1.0.0".into(),
                runtime: "tool".into(),
                description: None,
                tags: vec![],
            },
            Candidate {
                name: "git".into(),
                version: "1.0.0".into(),
                runtime: "tool".into(),
                description: None,
                tags: vec![],
            },
            Candidate {
                name: "murmur-tool-git".into(),
                version: "1.0.0".into(),
                runtime: "tool".into(),
                description: None,
                tags: vec![],
            },
        ];
        let results = rank_and_limit(candidates, "git", 10);
        assert_eq!(results[0].name, "git");
    }

    #[test]
    fn rank_and_limit_prefix_before_substring() {
        let candidates = vec![
            Candidate {
                name: "tool-desc-editor".into(),
                version: "1.0".into(),
                runtime: "tool".into(),
                description: Some("git operations".into()),
                tags: vec![],
            },
            Candidate {
                name: "git-tool".into(),
                version: "1.0".into(),
                runtime: "tool".into(),
                description: None,
                tags: vec![],
            },
        ];
        let results = rank_and_limit(candidates, "git", 10);
        assert_eq!(results[0].name, "git-tool");
    }

    #[test]
    fn rank_and_limit_respects_limit() {
        let candidates: Vec<Candidate> = (0..20)
            .map(|i| Candidate {
                name: format!("murmur-tool-{i}"),
                version: "1.0.0".into(),
                runtime: "tool".into(),
                description: None,
                tags: vec!["murmur".into()],
            })
            .collect();
        let results = rank_and_limit(candidates, "murmur", 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn rank_and_limit_no_match_returns_empty() {
        let candidates = vec![Candidate {
            name: "murmur-tool-git".into(),
            version: "1.0.0".into(),
            runtime: "tool".into(),
            description: None,
            tags: vec![],
        }];
        let results = rank_and_limit(candidates, "zzznomatch", 10);
        assert!(results.is_empty());
    }

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

    #[test]
    fn apply_index_rejects_unknown_schema_version() {
        let index = ArtifactIndex {
            schema_version: "99".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            artifacts: vec![],
        };
        let result = apply_index(index, "test-source", "git", 10);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err["ok"], false);
        assert_eq!(err["error_kind"], err::SCHEMA_ERROR);
        assert!(err["message"].as_str().unwrap().contains("99"));
        assert!(err["message"].as_str().unwrap().contains("test-source"));
    }

    #[test]
    fn apply_index_accepts_schema_version_1() {
        let index = ArtifactIndex {
            schema_version: "1".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            artifacts: vec![ArtifactIndexEntry {
                name: "murmur-tool-git".into(),
                version: "1.0.0".into(),
                runtime: "tool".into(),
                description: Some("git tool".into()),
                tags: vec!["git".into()],
                platforms: vec![],
            }],
        };
        let result = apply_index(index, "test-source", "git", 10);
        assert!(result.is_ok());
        let (rows, date) = result.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "murmur-tool-git");
        assert_eq!(date, "2026-01-01T00:00:00Z");
    }
}
