//! Read-write access to `murmur-tool-code-graph`'s existing SQLite database.
//!
//! This tool does NOT own the `files`/`symbols`/`edges` schema — that belongs to
//! `murmur-tool-code-graph`. It only *joins* onto the existing `symbols` table,
//! adding four dynamic-suspicion columns (`suspicion_ef`, `suspicion_ep`,
//! `suspicion_ochiai`, `suspicion_tarantula`) so static reachability and dynamic
//! SBFL scores live on the same row in the same file.
//!
//! Consequently this module never runs `CREATE TABLE` — if the database does not
//! already exist, the caller ([`crate::ops`]) fails with a message pointing at
//! `index_repository` rather than creating a fresh (empty, and therefore wrong)
//! database here.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Directory (relative to a repo root) that holds the code-graph database and
/// this tool's on-disk `data_path` payloads.
pub const STORE_DIR: &str = ".murmur";
pub const DB_FILE: &str = "code-graph.db";

/// Absolute path to the code-graph database for `repo`.
pub fn db_path(repo: &Path) -> PathBuf {
    repo.join(STORE_DIR).join(DB_FILE)
}

/// Open an *existing* code-graph database read-write and ensure the four
/// `suspicion_*` columns exist. The caller MUST have already verified the file
/// exists (see [`db_path`]) — `Connection::open` would otherwise create an empty
/// file, which this tool must never do.
pub fn open(repo: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path(repo))?;
    migrate(&conn)?;
    Ok(conn)
}

/// Whether `table` already has a column named `col`.
///
/// Mirrors `murmur-tool-code-graph`'s `column_exists`: `table` is a fixed
/// internal literal (never user input), so string interpolation is safe here —
/// PRAGMA does not accept a bound parameter.
fn column_exists(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        if name == col {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add this slice's four `suspicion_*` columns to `symbols` if missing, following
/// the identical `column_exists` + `ALTER TABLE ... ADD COLUMN` pattern as
/// `murmur-tool-code-graph`'s own `migrate`.
///
/// Unlike that migration, this one performs NO clear-and-reparse: the four new
/// columns are nullable with no `NOT NULL`/`DEFAULT`, so adding them leaves every
/// existing `files`/`symbols`/`edges` row and value untouched and valid. `NULL`
/// is the deliberate "not observed this run" sentinel, distinct from a computed
/// low score.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    for (col, ddl) in [
        ("suspicion_ef", "ALTER TABLE symbols ADD COLUMN suspicion_ef INTEGER"),
        ("suspicion_ep", "ALTER TABLE symbols ADD COLUMN suspicion_ep INTEGER"),
        ("suspicion_ochiai", "ALTER TABLE symbols ADD COLUMN suspicion_ochiai REAL"),
        ("suspicion_tarantula", "ALTER TABLE symbols ADD COLUMN suspicion_tarantula REAL"),
    ] {
        if !column_exists(conn, "symbols", col)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}
