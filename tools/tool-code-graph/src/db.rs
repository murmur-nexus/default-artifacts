//! SQLite-backed symbol/edge graph storage.
//!
//! The database lives at `<repo>/.murmur/code-graph.db`. The schema carries an
//! explicit `language` column on both `files` and `symbols` so additional
//! languages are additive later rather than a breaking change — MVP writes only
//! `"rust"`.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Directory (relative to a repo root) that holds the code-graph database and
/// any on-disk slice payloads referenced by `data_path`.
pub const STORE_DIR: &str = ".murmur";
pub const DB_FILE: &str = "code-graph.db";

/// Absolute path to the code-graph database for `repo`.
pub fn db_path(repo: &Path) -> PathBuf {
    repo.join(STORE_DIR).join(DB_FILE)
}

/// Open (creating if needed) the database for `repo` and ensure the schema
/// exists. The `.murmur` directory is created on demand.
pub fn open(repo: &Path) -> rusqlite::Result<Connection> {
    let dir = repo.join(STORE_DIR);
    // Best-effort directory creation; a failure surfaces as an open error below.
    let _ = std::fs::create_dir_all(&dir);
    let conn = Connection::open(db_path(repo))?;
    // Foreign keys must be enabled per-connection for ON DELETE CASCADE to fire.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    init_schema(&conn)?;
    Ok(conn)
}

/// The exact shipped schema. Kept as one string so the build summary can quote
/// it verbatim and later cards can rely on the column set.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id            INTEGER PRIMARY KEY,
    path          TEXT NOT NULL UNIQUE,   -- repo-relative, forward-slash separated
    content_hash  TEXT NOT NULL,          -- sha256 hex of the file's bytes
    language      TEXT NOT NULL DEFAULT 'rust'
);

CREATE TABLE IF NOT EXISTS symbols (
    id             INTEGER PRIMARY KEY,
    symbol_id      TEXT NOT NULL UNIQUE,  -- stable identity (see symbol_id format)
    language       TEXT NOT NULL,
    package        TEXT NOT NULL,
    module         TEXT NOT NULL,         -- '' for crate root
    qualified_name TEXT NOT NULL,         -- e.g. Foo::bar
    simple_name    TEXT NOT NULL,         -- last '::' segment of qualified_name
    signature      TEXT NOT NULL,         -- e.g. (&str)->Value ; () for non-fns
    kind           TEXT NOT NULL,         -- function|struct|enum|trait|mod|const|static|type|union|macro
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    start_line     INTEGER NOT NULL,      -- 1-based
    end_line       INTEGER NOT NULL,
    doc_comment    TEXT NOT NULL DEFAULT '',
    visibility     TEXT NOT NULL DEFAULT '',   -- 'pub' (bare) | 'pub(crate)' | 'pub(super)' | 'pub(in ..)' | '' private
    attributes     TEXT NOT NULL DEFAULT ''    -- raw #[...] outer-attribute text, newline-joined
);

CREATE INDEX IF NOT EXISTS idx_symbols_file        ON symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_simple_name ON symbols(simple_name);

CREATE TABLE IF NOT EXISTS edges (
    id            INTEGER PRIMARY KEY,
    src_symbol_id TEXT NOT NULL,          -- always a real symbol in this repo
    dst_symbol_id TEXT,                   -- resolved target, NULL when unresolved (external/std)
    dst_name      TEXT NOT NULL,          -- callee/child simple name as written
    edge_kind     TEXT NOT NULL,          -- calls|contains
    call_style    TEXT NOT NULL DEFAULT '',        -- free|path|method ('' for contains)
    confidence    TEXT NOT NULL DEFAULT 'definite', -- definite|possible|heuristic|unresolved
    UNIQUE(src_symbol_id, dst_name, edge_kind)
);

CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src_symbol_id);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst_symbol_id);

CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    symbol_id UNINDEXED,
    qualified_name,
    doc_comment,
    tokenize = 'unicode61'
);
"#;

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    migrate(conn)
}

/// Whether `table` already has a column named `col`.
fn column_exists(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    // `table` is a fixed internal literal (never user input), so string
    // interpolation here is safe — PRAGMA does not accept a bound parameter.
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

/// Bring a database created by an earlier version of this tool (one with no
/// `visibility`/`attributes`/`confidence`/`call_style` columns) up
/// to the current schema. Adds any missing columns and — only when a column was
/// actually added, i.e. this was a genuinely pre-slice database — clears every
/// row so the next `index_repository` treats all files as changed and fully
/// reparses them, populating the new columns. Without the clear, the per-file
/// `content_hash` no-op fast path would permanently skip already-indexed files
/// and leave their new columns blank forever.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut altered = false;
    for (table, col, ddl) in [
        ("symbols", "visibility", "ALTER TABLE symbols ADD COLUMN visibility TEXT NOT NULL DEFAULT ''"),
        ("symbols", "attributes", "ALTER TABLE symbols ADD COLUMN attributes TEXT NOT NULL DEFAULT ''"),
        ("edges", "confidence", "ALTER TABLE edges ADD COLUMN confidence TEXT NOT NULL DEFAULT 'definite'"),
        ("edges", "call_style", "ALTER TABLE edges ADD COLUMN call_style TEXT NOT NULL DEFAULT ''"),
    ] {
        if !column_exists(conn, table, col)? {
            conn.execute(ddl, [])?;
            altered = true;
        }
    }
    if altered {
        conn.execute_batch(
            "DELETE FROM edges; DELETE FROM symbols_fts; DELETE FROM symbols; DELETE FROM files;",
        )?;
    }
    Ok(())
}

/// Re-resolve every `calls` edge's `dst_symbol_id` by matching `dst_name`
/// against the `simple_name` of any indexed symbol. Run after every index pass
/// so cross-file call targets (and targets that only appeared after a later
/// file was parsed) are picked up, and stale resolutions are recomputed.
///
/// Ambiguous names (multiple symbols share a simple name) resolve to the
/// lexicographically-first `symbol_id` — deterministic, documented as a known
/// approximation of Rust's real name resolution.
pub fn resolve_edges(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE edges SET dst_symbol_id = NULL WHERE edge_kind = 'calls'", [])?;
    conn.execute(
        "UPDATE edges
            SET dst_symbol_id = (
                SELECT s.symbol_id FROM symbols s
                 WHERE s.simple_name = edges.dst_name
                 ORDER BY s.symbol_id
                 LIMIT 1
            )
          WHERE edge_kind = 'calls'",
        [],
    )?;
    assign_confidence(conn)
}

/// Assign a per-edge `confidence` for every `calls` edge, from two
/// tree-sitter-observable facts recorded at parse time: the call syntax
/// (`call_style`) and whether the callee `dst_name` was unique or ambiguous
/// across the repo at resolution time. Runs once per index pass, right after
/// `dst_symbol_id` is (re)resolved, inside the same transaction.
///
/// Levels (`contains` edges are always `definite`, set at insert time and left
/// untouched here):
/// - unresolved (`dst_symbol_id IS NULL`) → `'unresolved'` (never surfaces in a
///   traversal, which all filter `dst_symbol_id IS NOT NULL`; keeps the column
///   non-NULL).
/// - method call → `'heuristic'` always: the receiver's concrete type is unknown
///   at Level 1, so even a unique method name may bind the wrong impl.
/// - free/path call to a *unique* name → `'definite'`.
/// - free/path call to an *ambiguous* name (the deterministic first-id tie-break
///   in the resolver above fired) → `'possible'`.
fn assign_confidence(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE edges SET confidence = 'unresolved'
          WHERE edge_kind = 'calls' AND dst_symbol_id IS NULL",
        [],
    )?;
    conn.execute(
        "UPDATE edges SET confidence = 'heuristic'
          WHERE edge_kind = 'calls' AND dst_symbol_id IS NOT NULL AND call_style = 'method'",
        [],
    )?;
    conn.execute(
        "UPDATE edges SET confidence = 'definite'
          WHERE edge_kind = 'calls' AND dst_symbol_id IS NOT NULL AND call_style IN ('free', 'path')",
        [],
    )?;
    // Downgrade free/path calls whose name was ambiguous. The ambiguous-name set
    // is computed once (GROUP BY ... HAVING COUNT(*) > 1), not per edge.
    conn.execute(
        "UPDATE edges SET confidence = 'possible'
          WHERE edge_kind = 'calls' AND dst_symbol_id IS NOT NULL AND call_style IN ('free', 'path')
            AND dst_name IN (SELECT simple_name FROM symbols GROUP BY simple_name HAVING COUNT(*) > 1)",
        [],
    )?;
    Ok(())
}
