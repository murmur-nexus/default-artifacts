//! Resolve a stable `symbol_id` to its current file and line range by reading
//! tool-code-graph's `.murmur/code-graph.db` under the repository.
//!
//! The database is read and never written: it is opened read-only as the URI
//! `file:<path>?immutable=1`. SQLite's WASI build locks with a `<db>.lock` dotfile (its
//! `unix-dotfile` VFS): creating it fails in a read-only host folder and would otherwise appear in
//! the project, and `immutable=1` reads without any lock or journal. SQLite is built for WASI with
//! `SQLITE_OMIT_WAL`, so a database in WAL mode cannot be read; tool-code-graph never sets a
//! journal mode, so its databases are not in WAL mode.

use murmur_host_roots::{locate, sqlite_immutable_uri, Located, Refusal};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::env::Git;

/// Where the database sits below a repository root.
const GRAPH_DB: &str = ".murmur/code-graph.db";

/// A symbol's current location as code-graph indexed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    /// Repo-relative path of the file containing the symbol.
    pub file: String,
    /// 1-based inclusive start line.
    pub start_line: i64,
    /// 1-based inclusive end line.
    pub end_line: i64,
}

/// Why a `symbol_id` could not be resolved to a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No `.murmur/code-graph.db` under the repo root — the repo was never indexed.
    NotIndexed,
    /// The database exists but holds no row for this `symbol_id`.
    NotFound,
    /// The database lies neither in the work folder nor under a host folder the session opens
    /// to the tool.
    OutsideRoots,
    /// A relative repo whose first segment is `murmur-host` while a host folder is held;
    /// `suggestion` is the absolute repository path the caller most likely meant.
    ReservedName { suggestion: String },
    /// Opening or querying the database failed (corrupt file, WAL mode, schema mismatch, …).
    Internal(String),
}

/// `<dir>/.murmur/code-graph.db`.
fn db_under(dir: &str) -> String {
    format!("{}/{GRAPH_DB}", dir.trim_end_matches('/'))
}

/// Whether the relative path `value` climbs above the folder it is relative to.
fn climbs(value: &str) -> bool {
    let mut depth = 0usize;
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return true,
            },
            _ => depth += 1,
        }
    }
    false
}

/// The path to open the database of the repository `repo` at — relative to the work folder, or a
/// `murmur-host…` path — tried in this order:
///
/// 1. `repo` relative and not climbing out of the work folder: `<repo>/.murmur/code-graph.db` in
///    the work folder.
/// 2. Otherwise git places it: `git -C <repo> rev-parse --show-toplevel --absolute-git-dir
///    --show-prefix` gives the worktree `T`, its git folder `G` and `repo`'s place `P` below `T`,
///    and `git --git-dir=<G> --work-tree=<T> rev-parse --path-format=relative --show-toplevel`,
///    run in the work folder, gives `T` relative to it. A result that does not climb (no `..`
///    segment leaves the work folder; `""` and `./` mean the work folder itself) opens
///    `<T rel>/<P>.murmur/code-graph.db` in the work folder.
/// 3. Otherwise `<T>/<P>.murmur/code-graph.db` (or `<repo>/…` when git did not answer) through
///    the held host folder covering it, the longest winning.
/// 4. Otherwise [`ResolveError::OutsideRoots`].
pub fn graph_db_path(git: &Git, repo: &str) -> Result<String, ResolveError> {
    let roots = git.env().roots();
    let relative = |dir: &str| match locate(&db_under(dir), roots) {
        Ok(Located::Relative(open)) => Ok(open),
        Ok(Located::Host { open, .. }) => Ok(open),
        Err(Refusal::ReservedName { suggestion }) => Err(ResolveError::ReservedName {
            suggestion: suggestion
                .strip_suffix(&format!("/{GRAPH_DB}"))
                .unwrap_or(&suggestion)
                .to_string(),
        }),
        Err(_) => Err(ResolveError::OutsideRoots),
    };

    if !repo.starts_with('/') && !climbs(repo) {
        return relative(repo);
    }

    let place = git.git(&[
        "-C",
        repo,
        "rev-parse",
        "--show-toplevel",
        "--absolute-git-dir",
        "--show-prefix",
    ]);
    let mut root = repo.to_string();
    if place.success {
        let mut lines = place.stdout.lines();
        let (top, git_dir) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
        let prefix = lines.next().unwrap_or("");
        let git_dir_arg = format!("--git-dir={git_dir}");
        let work_tree_arg = format!("--work-tree={top}");
        let rel = git.git(&[
            &git_dir_arg,
            &work_tree_arg,
            "rev-parse",
            "--path-format=relative",
            "--show-toplevel",
        ]);
        if rel.success {
            let rel = rel.stdout.trim_end_matches('/');
            if !climbs(rel) {
                let dir = if rel.is_empty() || rel == "." {
                    "."
                } else {
                    rel
                };
                return relative(&format!("{dir}/{prefix}"));
            }
        }
        root = format!("{top}/{prefix}");
    }

    match locate(&db_under(&root), roots) {
        Ok(Located::Host { open, .. }) => Ok(open),
        _ => Err(ResolveError::OutsideRoots),
    }
}

/// Look `symbol_id` up in the database at `open` (as [`graph_db_path`] returned it), read-only.
///
/// Query: `SELECT f.path, s.start_line, s.end_line FROM symbols s JOIN files f ON f.id =
/// s.file_id WHERE s.symbol_id = ?1` — the `symbols.symbol_id` column is `UNIQUE`, so at most
/// one row matches.
pub fn resolve_symbol_location(
    git: &Git,
    open: &str,
    symbol_id: &str,
) -> Result<SymbolLocation, ResolveError> {
    let path = git.env().fs_path(open);
    if !path.exists() {
        return Err(ResolveError::NotIndexed);
    }
    let uri = sqlite_immutable_uri(&path.to_string_lossy());
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| ResolveError::Internal(e.to_string()))?;

    conn.query_row(
        "SELECT f.path, s.start_line, s.end_line \
         FROM symbols s JOIN files f ON f.id = s.file_id \
         WHERE s.symbol_id = ?1",
        rusqlite::params![symbol_id],
        |row| {
            Ok(SymbolLocation {
                file: row.get(0)?,
                start_line: row.get(1)?,
                end_line: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| ResolveError::Internal(e.to_string()))?
    .ok_or(ResolveError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn climbing_is_lexical() {
        for v in ["..", "../x", "a/../..", "./../x", "a/b/../../../c"] {
            assert!(climbs(v), "{v}");
        }
        for v in [".", "", "a", "a/..", "a/b/../c", "./a/./b", "..a", "..a/b"] {
            assert!(!climbs(v), "{v}");
        }
    }

    #[test]
    fn the_database_sits_under_the_dir() {
        assert_eq!(db_under("repo"), "repo/.murmur/code-graph.db");
        assert_eq!(db_under("repo/"), "repo/.murmur/code-graph.db");
        assert_eq!(db_under("./"), "./.murmur/code-graph.db");
        assert_eq!(db_under("/srv/r"), "/srv/r/.murmur/code-graph.db");
    }
}
