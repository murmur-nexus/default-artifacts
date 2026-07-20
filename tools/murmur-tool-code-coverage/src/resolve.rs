//! Best-effort `stable_id` resolution for each failing test's *own* symbol.
//!
//! Duplicated (not shared — both tools are `[[bin]]`-only) from
//! `murmur-tool-test-report`'s `resolve.rs`: identical `split_test_path` and the
//! identical `SELECT symbol_id FROM symbols WHERE kind = 'function' AND module =
//! ?1 AND qualified_name = ?2` join. The one difference: this tool reuses the
//! read-write connection already open for scoring rather than opening its own
//! read-only one, since the query itself needs no special flags.
//!
//! Resolution is soft: a zero-row or ambiguous (>1 row) match resolves to
//! `None`, never an error.

use rusqlite::Connection;

/// Resolve `(test_name, Option<stable_id>)` for each failing-test name. Unique
/// match → `Some`, zero or ambiguous match → `None`.
pub fn resolve_failing_ids(conn: &Connection, names: &[String]) -> Vec<(String, Option<String>)> {
    let mut stmt = match conn.prepare(
        "SELECT symbol_id FROM symbols WHERE kind = 'function' AND module = ?1 AND qualified_name = ?2",
    ) {
        Ok(s) => s,
        // A prepare failure leaves every stable_id null — best-effort.
        Err(_) => return names.iter().map(|n| (n.clone(), None)).collect(),
    };

    names
        .iter()
        .map(|name| {
            let (module, qualified) = split_test_path(name);
            let rows = stmt.query_map(rusqlite::params![module, qualified], |r| r.get::<_, String>(0));
            let ids: Vec<String> = match rows {
                Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
                Err(_) => Vec::new(),
            };
            // Unique match only — zero or ambiguous stays null.
            let stable_id = if ids.len() == 1 { Some(ids[0].clone()) } else { None };
            (name.clone(), stable_id)
        })
        .collect()
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
