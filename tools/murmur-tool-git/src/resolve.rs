//! Read-only resolution of a `murmur-tool-code-graph` `symbol_id` to its
//! *current* file path and 1-based line range.
//!
//! This mirrors the read-only resolver pattern already duplicated in
//! `murmur-tool-test-report` and `murmur-tool-code-coverage` (neither sibling
//! crate exposes a `[lib]` target, so the query is copied, not shared). Two
//! deliberate differences from those tools:
//!   1. A missing `.murmur/code-graph.db` is a hard, named error
//!      (`ResolveError::NotIndexed`), not a silent no-op — resolving the range is
//!      `symbol_history`'s entire purpose, not an optional enrichment.
//!   2. It resolves the location too (file + start/end line), not just the id.
//!
//! This tool NEVER writes to the db (read-only `OpenFlags`, and it errors rather
//! than creating a db when one is absent) and NEVER recomputes a `symbol_id` — it
//! reads code-graph's `symbols.symbol_id`/`start_line`/`end_line` verbatim, always
//! from the freshly-indexed current row so a re-indexed (shifted) range is honored.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// A resolved symbol location: current repo-relative file path (forward-slash
/// separated) and 1-based inclusive line range from the `symbols` table.
pub struct SymbolLocation {
    pub file: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// Why resolving a `symbol_id` against the code-graph db failed.
pub enum ResolveError {
    /// No `.murmur/code-graph.db` exists — the repo was never indexed.
    NotIndexed,
    /// The db exists but holds no row for this `symbol_id`.
    NotFound,
    /// An unexpected open/SQLite failure (corrupt or unreadable db, etc.).
    Internal(String),
}

/// Resolve `symbol_id` to its current `(file, start_line, end_line)` by joining
/// `symbols` to `files` in `<repo>/.murmur/code-graph.db`, opened read-only.
///
/// Reference query is `murmur-tool-code-graph`'s own `op_get_symbol`
/// (`src/ops.rs`): `SELECT ... f.path, s.start_line, s.end_line FROM symbols s
/// JOIN files f ON f.id = s.file_id WHERE s.symbol_id = ?1`.
pub fn resolve_symbol_location(repo: &str, symbol_id: &str) -> Result<SymbolLocation, ResolveError> {
    let db = Path::new(repo).join(".murmur").join("code-graph.db");
    if !db.exists() {
        return Err(ResolveError::NotIndexed);
    }

    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| ResolveError::Internal(e.to_string()))?;

    let row = conn.query_row(
        "SELECT f.path, s.start_line, s.end_line
           FROM symbols s JOIN files f ON f.id = s.file_id
          WHERE s.symbol_id = ?1",
        rusqlite::params![symbol_id],
        |r| {
            Ok(SymbolLocation {
                file: r.get::<_, String>(0)?,
                start_line: r.get::<_, i64>(1)?,
                end_line: r.get::<_, i64>(2)?,
            })
        },
    );

    match row {
        Ok(loc) => Ok(loc),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ResolveError::NotFound),
        Err(e) => Err(ResolveError::Internal(e.to_string())),
    }
}
