//! Best-effort `stable_id` resolution against an existing code-graph database.
//!
//! This is the join key `tool-code-coverage` will use to connect a
//! dynamic `cargo_test` failure back to the static symbol graph. Resolution is
//! soft: a missing `repo_path`, missing db file, a corrupt/unreadable db, or a
//! zero/multiple-row match all leave `stable_id` at `None`. This tool NEVER
//! writes to the db (read-only `OpenFlags`, `immutable=1`), and NEVER recomputes
//! the identity itself — it reads code-graph's `symbols.symbol_id` verbatim.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::parse::Failure;

/// Resolve `stable_id` for each `cargo_test` failure in place from the code-graph
/// database at `db_path`, the path to open (relative to the work folder, or a
/// `murmur-host/…` path). Silent on every failure mode (best-effort by design).
///
/// The database is opened read-only as the URI `file:<db_path>?immutable=1`.
/// SQLite's WASI build locks with a `<db>.lock` dotfile (its `unix-dotfile` VFS):
/// creating it fails in a read-only host folder, which would turn every lookup
/// there into `database is locked`, and elsewhere it would appear in the project.
/// `immutable=1` reads without any lock or journal. SQLite is built for WASI with
/// `SQLITE_OMIT_WAL`, so a database in WAL mode cannot be read at all and degrades
/// to `stable_id: null` like any other unreadable database.
pub fn resolve_stable_ids(db_path: &str, failures: &mut [Failure]) {
    if !Path::new(db_path).exists() {
        return;
    }
    let conn = match Connection::open_with_flags(
        format!("file:{}?immutable=1", escape(db_path)),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut stmt = match conn
        .prepare("SELECT symbol_id FROM symbols WHERE kind = 'function' AND module = ?1 AND qualified_name = ?2")
    {
        Ok(s) => s,
        Err(_) => return,
    };

    for f in failures.iter_mut() {
        let (module, qualified) = split_test_path(&f.test_name);
        let rows = stmt.query_map(rusqlite::params![module, qualified], |r| {
            r.get::<_, String>(0)
        });
        let ids: Vec<String> = match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => continue,
        };
        // Unique match only — zero or ambiguous stays null.
        if ids.len() == 1 {
            f.stable_id = Some(ids[0].clone());
        }
    }
}

/// `path` as the path part of an SQLite `file:` URI: `%`, `?` and `#` are
/// percent-encoded (`%25`, `%3F`, `%23`), so a path holding them is neither cut
/// short at a query or fragment nor decoded into a different path. Every other
/// character is passed through.
fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            c => out.push(c),
        }
    }
    out
}

/// Split a cargo test path on its last `::` into `(module, qualified_name)`.
/// `tests::foo` → `("tests", "foo")`; `a::tests::foo` → `("a::tests", "foo")`;
/// a bare `foo` → `("", "foo")` (crate-root test).
fn split_test_path(name: &str) -> (String, String) {
    match name.rsplit_once("::") {
        Some((module, qualified)) => (module.to_string(), qualified.to_string()),
        None => (String::new(), name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_percent_encodes_only_what_ends_or_decodes_a_uri_path() {
        assert_eq!(escape(".murmur/code-graph.db"), ".murmur/code-graph.db");
        assert_eq!(
            escape("murmur-host/srv/a b/x.db"),
            "murmur-host/srv/a b/x.db"
        );
        assert_eq!(escape("a%b?c#d"), "a%25b%3Fc%23d");
        assert_eq!(escape("%3F"), "%253F");
    }

    #[test]
    fn a_database_named_with_uri_characters_still_opens() {
        let dir = std::env::temp_dir().join(format!(
            "tool-test-report-escape-{}-%3F?#",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("code-graph.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (symbol_id TEXT, kind TEXT, module TEXT, qualified_name TEXT);
             INSERT INTO symbols VALUES ('sid-1', 'function', 'tests', 'foo');",
        )
        .unwrap();
        drop(conn);
        let mut failures = vec![Failure {
            test_name: "tests::foo".to_string(),
            file: None,
            line: None,
            exception: "panic".to_string(),
            message: String::new(),
            stable_id: None,
        }];
        resolve_stable_ids(db.to_str().unwrap(), &mut failures);
        assert_eq!(failures[0].stable_id.as_deref(), Some("sid-1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
