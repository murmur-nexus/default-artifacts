//! Where `repo_path` opens, how the tool names it back, and how `impact_analysis` reduces its
//! `file` input to an indexed path.
//!
//! `repo_path` goes through `murmur_host_roots::locate`: a relative value opens in the work
//! folder as written, an absolute one only through the host root covering it (as
//! `murmur-host/…`). Nothing is canonicalised, and an absolute value never reaches `std::fs` or
//! SQLite except as that open path. What the tool reports — `data.repo_path`, `data_path`,
//! metadata resource ids and messages — names the repository as the call wrote it.

use std::path::{Path, PathBuf};

use murmur_host_roots::{describe_roots, locate, HostRoot, Located, Refusal, RootAccess};
use serde_json::Value;

use crate::db;
use crate::out::failed;

/// The tool's name, as refusals name it.
const TOOL_NAME: &str = "tool-code-graph";

/// A located, existing repository directory.
pub struct Repo {
    /// `repo_path` exactly as the call wrote it (`.` when omitted). Messages quote it.
    pub written: String,
    /// The name the tool reports: `written` with a trailing `/` trimmed (`/` stays `/`).
    pub shown: String,
    /// Where it opens: the value as written (relative to the work folder) or `murmur-host/…`.
    pub open: PathBuf,
    /// The package name used when no manifest names one: [`fallback_name`] of `written`.
    pub fallback: Option<String>,
    /// The host folder it opened in, when absolute.
    pub root: Option<HostRoot>,
}

impl Repo {
    /// Whether the folder holding the repository was opened read-write. A relative value opens
    /// in the work folder, which the tool writes as the native tool wrote its cwd.
    pub fn writable(&self) -> bool {
        self.root
            .as_ref()
            .is_none_or(|root| root.access == RootAccess::ReadWrite)
    }

    /// `text` (an error SQLite or the filesystem wrote) with the `murmur-host/…` path a host
    /// value opened at replaced by the value as written, so no message names the preopen.
    pub fn as_written(&self, text: &str) -> String {
        match &self.root {
            Some(_) => text.replace(&*self.open.to_string_lossy(), &self.shown),
            None => text.to_string(),
        }
    }

    /// `rest` under the repository, as the call would name it: `<shown>/<rest>`, or `rest`
    /// alone for `.`.
    pub fn shown_join(&self, rest: &str) -> String {
        match self.shown.as_str() {
            "." => rest.to_string(),
            "/" => format!("/{rest}"),
            shown => format!("{shown}/{rest}"),
        }
    }
}

/// Locate `repo_path` (default `.`) and check it is an existing directory. Returns the `failed`
/// envelope naming the value as written when it is refused, missing or not a directory.
pub fn resolve_repo(op: &Value, operation: &str, roots: &[HostRoot]) -> Result<Repo, Value> {
    let written = op
        .get("repo_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let (open, root) = match locate(written, roots) {
        Ok(Located::Relative(value)) => (PathBuf::from(value), None),
        Ok(Located::Host { open, root }) => (PathBuf::from(open), Some(root.clone())),
        Err(refusal) => return Err(failed(refusal_message(operation, written, refusal, roots))),
    };
    if !open.exists() {
        return Err(failed(format!(
            "{operation}: repo_path does not exist: {written}"
        )));
    }
    if !open.is_dir() {
        return Err(failed(format!(
            "{operation}: repo_path is not a directory: {written}"
        )));
    }
    let shown = match written.trim_end_matches('/') {
        "" => "/".to_string(),
        trimmed => trimmed.to_string(),
    };
    Ok(Repo {
        written: written.to_string(),
        shown,
        open,
        fallback: fallback_name(written),
        root,
    })
}

/// The refusal of an absolute `repo_path` no host root covers, or of a relative one starting
/// with the reserved `murmur-host` segment, before anything is opened.
fn refusal_message(operation: &str, value: &str, refusal: Refusal, roots: &[HostRoot]) -> String {
    match refusal {
        Refusal::NoRoots => format!(
            "{operation}: could not open repo_path '{value}': this session opens no host folder \
             to {TOOL_NAME}; a relative path opens in the work folder ('.' is the work folder \
             itself)"
        ),
        Refusal::Uncovered => format!(
            "{operation}: could not open repo_path '{value}': it is outside every host folder \
             {TOOL_NAME} may open: {}. A relative path opens in the work folder ('.' is the work \
             folder itself)",
            describe_roots(roots)
        ),
        Refusal::ReservedName { suggestion } => format!(
            "{operation}: 'repo_path' starts with murmur-host, where this tool's host folders are \
             opened; name the host path absolutely: '{suggestion}'"
        ),
    }
}

/// `index_repository`'s refusal of a repository in a host folder opened read-only, before
/// SQLite is touched; `None` when the folder is writable.
pub fn read_only_refusal(repo: &Repo) -> Option<Value> {
    let root = repo.root.as_ref().filter(|_| !repo.writable())?;
    Some(failed(format!(
        "index_repository: repo_path '{}' is inside a host folder opened read-only ({}); \
         index_repository writes {}, so it needs that folder read-write",
        repo.written,
        root.path,
        repo.shown_join(&format!("{}/{}", db::STORE_DIR, db::DB_FILE)),
    )))
}

/// The segments of `value` after lexical normalisation: `.` and empty segments dropped, `..`
/// popping the previous normal segment. A relative value keeps a `..` it cannot pop; an
/// absolute one drops it at `/`.
fn lexical_segments(value: &str) -> Vec<&str> {
    let absolute = value.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(&last) if last != ".." => {
                    segments.pop();
                }
                _ if absolute => {}
                _ => segments.push(".."),
            },
            normal => segments.push(normal),
        }
    }
    segments
}

/// The package name for files under no Rust package or Python project name: the last normal
/// segment of `repo_path` as written, lexically normalised. `None` when there is none (`.`,
/// `./`, `..`, `/`, an omitted value): the parsers then use `crate` for Rust and `package` for
/// Python. Computed from the value as written, never from the `murmur-host…` open path.
pub fn fallback_name(written: &str) -> Option<String> {
    match lexical_segments(written).last() {
        Some(&last) if last != ".." => Some(last.to_string()),
        _ => None,
    }
}

/// Reduce `impact_analysis`'s `file` to the stored repo-relative form:
///
/// 1. `\` becomes `/` and any leading `./` is stripped;
/// 2. an absolute `file` under an absolute `repo_path` loses that prefix, compared whole
///    component by whole component after lexical normalisation;
/// 3. an absolute `file` under a relative `repo_path` becomes the longest indexed path
///    (`indexed()`, the `files.path` rows) that is a whole-component suffix of it;
/// 4. anything else is kept, its components joined with `/` as the native tool joined them,
///    and an unindexed result fails as "file not indexed".
pub fn reduce_impact_file<F>(file: &str, repo_path: &str, indexed: F) -> String
where
    F: FnOnce() -> Vec<String>,
{
    let mut f = file.replace('\\', "/");
    while let Some(rest) = f.strip_prefix("./") {
        f = rest.to_string();
    }
    if f.starts_with('/') {
        let file_segments = lexical_segments(&f);
        if repo_path.starts_with('/') {
            let repo_segments = lexical_segments(repo_path);
            if file_segments.len() >= repo_segments.len()
                && file_segments[..repo_segments.len()] == repo_segments[..]
            {
                return file_segments[repo_segments.len()..].join("/");
            }
        } else {
            let best = indexed()
                .into_iter()
                .filter(|path| {
                    let path_segments = lexical_segments(path);
                    !path_segments.is_empty()
                        && file_segments.len() >= path_segments.len()
                        && file_segments[file_segments.len() - path_segments.len()..]
                            == path_segments[..]
                })
                .max_by_key(|path| lexical_segments(path).len());
            if let Some(best) = best {
                return best;
            }
        }
    }
    Path::new(&f)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_name_is_the_last_normal_segment_as_written() {
        for (written, expected) in [
            (".", None),
            ("./", None),
            ("..", None),
            ("/", None),
            ("a/b/", Some("b")),
            ("/x/y", Some("y")),
            ("a/./b/..", Some("a")),
            ("proj/", Some("proj")),
            ("../x", Some("x")),
            ("a/..", None),
        ] {
            assert_eq!(
                fallback_name(written).as_deref(),
                expected,
                "fallback_name({written:?})"
            );
        }
    }

    fn none() -> Vec<String> {
        panic!("only an absolute file under a relative repo_path reads the indexed paths")
    }

    #[test]
    fn an_absolute_file_under_an_absolute_repo_loses_the_repo_prefix() {
        assert_eq!(
            reduce_impact_file("/w/repo/src/lib.rs", "/w/repo", none),
            "src/lib.rs"
        );
        assert_eq!(
            reduce_impact_file("/w/repo/./src/../src/lib.rs", "/w/repo/", none),
            "src/lib.rs"
        );
        // Whole components only: /w/repository is not under /w/repo.
        assert_eq!(
            reduce_impact_file("/w/repository/src/lib.rs", "/w/repo", none),
            "//w/repository/src/lib.rs"
        );
    }

    #[test]
    fn an_absolute_file_under_a_relative_repo_is_its_longest_indexed_suffix() {
        let indexed = || vec!["src/lib.rs".to_string(), "crates/a/src/lib.rs".to_string()];
        assert_eq!(
            reduce_impact_file("/w/crates/a/src/lib.rs", "repo", indexed),
            "crates/a/src/lib.rs"
        );
        assert_eq!(
            reduce_impact_file("/w/src/lib.rs", ".", indexed),
            "src/lib.rs"
        );
    }

    #[test]
    fn the_suffix_matches_whole_components_only() {
        let indexed = || vec!["src/lib.rs".to_string()];
        assert_eq!(
            reduce_impact_file("/w/xsrc/lib.rs", "repo", indexed),
            "//w/xsrc/lib.rs"
        );
    }

    #[test]
    fn an_unmatched_absolute_file_is_kept() {
        let indexed = || vec!["src/main.rs".to_string()];
        assert_eq!(
            reduce_impact_file("/w/src/lib.rs", "repo", indexed),
            "//w/src/lib.rs"
        );
    }

    #[test]
    fn a_relative_file_is_normalised_as_before() {
        assert_eq!(
            reduce_impact_file(".\\src\\lib.rs", "repo", none),
            "src/lib.rs"
        );
        assert_eq!(
            reduce_impact_file("././src//lib.rs", "/w/repo", none),
            "src/lib.rs"
        );
        assert_eq!(reduce_impact_file("src/lib.rs", ".", none), "src/lib.rs");
    }
}
