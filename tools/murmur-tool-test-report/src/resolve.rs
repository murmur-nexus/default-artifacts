//! Best-effort `stable_id` resolution against an existing code-graph database.
//!
//! This is the join key `murmur-tool-code-coverage` will use to connect a
//! dynamic `cargo_test` failure back to the static symbol graph. Resolution is
//! soft: a missing `repo_path`, missing db file, a corrupt/unreadable db, or a
//! zero/multiple-row match all leave `stable_id` at `None`. This tool NEVER
//! writes to the db (read-only `OpenFlags`), and NEVER recomputes the identity
//! itself — it reads code-graph's `symbols.symbol_id` verbatim.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::parse::Failure;

/// Resolve `stable_id` for each `cargo_test` failure in place. Silent on every
/// failure mode (best-effort by design).
pub fn resolve_stable_ids(repo_path: &str, failures: &mut [Failure]) {
    let db = Path::new(repo_path).join(".murmur").join("code-graph.db");
    if !db.exists() {
        return;
    }
    let conn = match Connection::open_with_flags(
        &db,
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
        let rows = stmt.query_map(rusqlite::params![module, qualified], |r| r.get::<_, String>(0));
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

/// Split a cargo test path on its last `::` into `(module, qualified_name)`.
/// `tests::foo` → `("tests", "foo")`; `a::tests::foo` → `("a::tests", "foo")`;
/// a bare `foo` → `("", "foo")` (crate-root test).
fn split_test_path(name: &str) -> (String, String) {
    match name.rsplit_once("::") {
        Some((module, qualified)) => (module.to_string(), qualified.to_string()),
        None => (String::new(), name.to_string()),
    }
}
