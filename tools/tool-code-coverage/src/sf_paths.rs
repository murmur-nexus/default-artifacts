//! Mapping LCOV `SF:` paths onto the repo-relative paths `tool-code-graph` indexed, without
//! opening or canonicalizing them.
//!
//! `cargo llvm-cov` writes absolute host paths (`SF:/home/me/repo/src/lib.rs`). A WASM tool cannot
//! learn the host path of its work folder, and an absolute path opened without a `murmur-host`
//! prefix resolves inside the work folder, so the repository's host prefix cannot be stripped
//! the way the native tool did. Instead the prefix is inferred from the paths themselves: the
//! one directory that, removed from the most `SF:` paths, leaves a path the code graph indexed.
//! A relative `SF:` path is relative to `repo_path` and is used as written.

use std::collections::{HashMap, HashSet};

/// An `SF:` path after lexical normalisation, as path segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SfPath {
    Relative(Vec<String>),
    Absolute(Vec<String>),
}

/// Normalise a raw `SF:` value: `\` becomes `/`, leading `./` is stripped repeatedly, and the
/// path is absolute iff it then starts with `/`. Empty and `.` segments are dropped and `..` pops
/// the previous one. `None` when a `..` has nothing to pop: such a path names nothing in the
/// repository and matches no symbol.
pub fn normalize_sf(raw: &str) -> Option<SfPath> {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    let absolute = path.starts_with('/');
    let mut segments: Vec<String> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other.to_string()),
        }
    }
    Some(if absolute {
        SfPath::Absolute(segments)
    } else {
        SfPath::Relative(segments)
    })
}

/// The host directory of the repository, inferred from `paths` — the distinct absolute normalised
/// `SF:` paths of every retained section of every parsed `.info` file of the call — and
/// `indexed`, the code graph's `files.path` values.
///
/// For each path and each split point `k` where `segments[k..]` joined with `/` is a non-empty
/// indexed path, `segments[..k]` is a candidate. A candidate's score is the number of distinct
/// paths proposing it. The highest score wins; on a tie the candidate with the fewest segments,
/// then the lexicographically smallest `/`-joined string. `None` when no path ends in an indexed
/// file.
pub fn infer_root(paths: &[Vec<String>], indexed: &HashSet<String>) -> Option<Vec<String>> {
    let mut scores: HashMap<Vec<String>, usize> = HashMap::new();
    let mut seen: HashSet<&Vec<String>> = HashSet::new();
    for path in paths {
        if !seen.insert(path) {
            continue;
        }
        let mut proposed: HashSet<&[String]> = HashSet::new();
        for k in 0..path.len() {
            if indexed.contains(&path[k..].join("/")) {
                proposed.insert(&path[..k]);
            }
        }
        for root in proposed {
            *scores.entry(root.to_vec()).or_insert(0) += 1;
        }
    }
    scores
        .into_iter()
        .min_by(|(a, a_score), (b, b_score)| {
            b_score
                .cmp(a_score)
                .then(a.len().cmp(&b.len()))
                .then_with(|| a.join("/").cmp(&b.join("/")))
        })
        .map(|(root, _)| root)
}

/// The repo-relative form of a raw `SF:` value: a relative path's segments joined with `/`, an
/// absolute path's segments after `root` (a whole-component prefix) joined with `/`. `None` for
/// an absolute path outside `root`, with no `root`, or one that does not normalise.
pub fn map_sf(raw: &str, root: Option<&[String]>) -> Option<String> {
    match normalize_sf(raw)? {
        SfPath::Relative(segments) => Some(segments.join("/")),
        SfPath::Absolute(segments) => {
            let root = root?;
            segments.strip_prefix(root).map(|rest| rest.join("/"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(path: &str) -> Vec<String> {
        match normalize_sf(path).unwrap() {
            SfPath::Absolute(s) | SfPath::Relative(s) => s,
        }
    }

    fn indexed(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn a_repo_root_explaining_two_paths_beats_a_dependency_root_explaining_one() {
        // A vendored dependency also holds a `src/lib.rs`; only the repository explains both.
        let paths = vec![
            segs("/home/me/repo/src/lib.rs"),
            segs("/home/me/repo/src/main.rs"),
            segs("/home/me/.cargo/registry/dep/src/lib.rs"),
        ];
        let root = infer_root(&paths, &indexed(&["src/lib.rs", "src/main.rs"])).unwrap();
        assert_eq!(root, segs("/home/me/repo"));
        assert_eq!(
            map_sf("/home/me/.cargo/registry/dep/src/lib.rs", Some(&root)),
            None
        );
        assert_eq!(
            map_sf("/home/me/repo/src/main.rs", Some(&root)).as_deref(),
            Some("src/main.rs")
        );
    }

    #[test]
    fn a_nested_workspace_member_maps_through_the_workspace_root() {
        let idx = indexed(&["crates/a/src/lib.rs", "src/lib.rs"]);
        let paths = vec![segs("/w/crates/a/src/lib.rs"), segs("/w/src/lib.rs")];
        let root = infer_root(&paths, &idx).unwrap();
        assert_eq!(root, segs("/w"));
        assert_eq!(
            map_sf("/w/crates/a/src/lib.rs", Some(&root)).as_deref(),
            Some("crates/a/src/lib.rs")
        );
    }

    #[test]
    fn dots_dot_dots_and_backslashes_normalise_lexically() {
        assert_eq!(
            normalize_sf("././src/./lib.rs"),
            Some(SfPath::Relative(segs("src/lib.rs")))
        );
        assert_eq!(
            normalize_sf("/r//src/x/../lib.rs"),
            Some(SfPath::Absolute(vec![
                "r".into(),
                "src".into(),
                "lib.rs".into()
            ]))
        );
        assert_eq!(
            normalize_sf("C:\\repo\\src\\lib.rs"),
            Some(SfPath::Relative(vec![
                "C:".into(),
                "repo".into(),
                "src".into(),
                "lib.rs".into()
            ]))
        );
        assert_eq!(
            normalize_sf("\\r\\src\\lib.rs"),
            Some(SfPath::Absolute(segs("r/src/lib.rs")))
        );
        assert_eq!(normalize_sf("../outside.rs"), None);
        assert_eq!(normalize_sf("/.."), None);
        assert_eq!(map_sf("src/../../x.rs", None), None);
    }

    #[test]
    fn a_relative_sf_path_is_used_as_written() {
        assert_eq!(map_sf("src/lib.rs", None).as_deref(), Some("src/lib.rs"));
        assert_eq!(map_sf("./src/lib.rs", None).as_deref(), Some("src/lib.rs"));
        let root = segs("/elsewhere");
        assert_eq!(
            map_sf("src/lib.rs", Some(&root)).as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn with_no_match_there_is_no_root_and_every_absolute_path_is_unmatched() {
        let paths = vec![segs("/a/b/other.rs"), segs("/c/d.rs")];
        let root = infer_root(&paths, &indexed(&["src/lib.rs"]));
        assert_eq!(root, None);
        for raw in ["/a/b/other.rs", "/c/d.rs", "/r/src/lib.rs"] {
            assert_eq!(map_sf(raw, root.as_deref()), None, "{raw}");
        }
        assert_eq!(infer_root(&[], &indexed(&["src/lib.rs"])), None);
    }

    #[test]
    fn a_root_matches_whole_components_only() {
        let root = segs("/a/b");
        assert_eq!(map_sf("/a/bc/src/lib.rs", Some(&root)), None);
        assert_eq!(
            map_sf("/a/b/src/lib.rs", Some(&root)).as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn a_tie_goes_to_the_fewest_segments_then_the_smallest_string() {
        // Two checkouts, each explaining one path: equal scores, equal length → `/a/repo` sorts
        // before `/b/repo`.
        let idx = indexed(&["src/lib.rs"]);
        let paths = vec![segs("/b/repo/src/lib.rs"), segs("/a/repo/src/lib.rs")];
        assert_eq!(infer_root(&paths, &idx), Some(segs("/a/repo")));
        // Equal scores, different lengths → the shorter root wins.
        let paths = vec![segs("/x/y/repo/src/lib.rs"), segs("/z/src/lib.rs")];
        assert_eq!(infer_root(&paths, &idx), Some(segs("/z")));
        // A path proposing two roots (indexed `lib.rs` and `src/lib.rs`) counts once per root;
        // the shorter-root rule then picks the repository over its `src/`.
        let idx = indexed(&["src/lib.rs", "lib.rs"]);
        let paths = vec![segs("/r/src/lib.rs")];
        assert_eq!(infer_root(&paths, &idx), Some(segs("/r")));
    }

    #[test]
    fn the_filesystem_root_can_be_the_repository() {
        let idx = indexed(&["src/lib.rs"]);
        let root = infer_root(&[segs("/src/lib.rs")], &idx).unwrap();
        assert!(root.is_empty());
        assert_eq!(
            map_sf("/src/lib.rs", Some(&root)).as_deref(),
            Some("src/lib.rs")
        );
    }
}
