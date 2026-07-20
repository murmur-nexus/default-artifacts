//! The six structured operations.
//!
//! Each `op_*` takes the parsed operation object and returns a fully-formed
//! response envelope (see [`crate::out`]). Symbol-addressed operations
//! (`get_symbol`, `slice_symbol`, `explain_path`) declare an explicit
//! `resource_id` (the stable `symbol_id`) so the `mur trace` redundant-call
//! detector works without a filesystem path to sniff.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db;
use crate::out::{errored, failed, passed, Meta};
use crate::parse;

// ── Input helpers ─────────────────────────────────────────────────────────────

fn str_field<'a>(op: &'a Value, key: &str) -> Option<&'a str> {
    op.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

fn req_str<'a>(op: &'a Value, key: &str, operation: &str) -> Result<&'a str, Value> {
    str_field(op, key).ok_or_else(|| failed(format!("{operation}: missing required field '{key}'")))
}

fn opt_i64(op: &Value, key: &str, default: i64) -> i64 {
    op.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
}

fn req_i64(op: &Value, key: &str, operation: &str) -> Result<i64, Value> {
    op.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| failed(format!("{operation}: missing or non-integer required field '{key}'")))
}

/// Resolve and validate `repo_path`; defaults to the current directory. Returns
/// a `failed` envelope if the path is missing, not found, or not a directory.
fn resolve_repo(op: &Value, operation: &str) -> Result<PathBuf, Value> {
    let raw = str_field(op, "repo_path")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| failed(format!("{operation}: could not determine repo_path")))?;
    if !raw.exists() {
        return Err(failed(format!("{operation}: repo_path does not exist: {}", raw.display())));
    }
    if !raw.is_dir() {
        return Err(failed(format!("{operation}: repo_path is not a directory: {}", raw.display())));
    }
    Ok(raw.canonicalize().unwrap_or(raw))
}

fn open_db(repo: &Path, operation: &str) -> Result<Connection, Value> {
    db::open(repo).map_err(|e| errored(format!("{operation}: failed to open code graph db: {e}")))
}

// ── op: index_repository ──────────────────────────────────────────────────────

pub fn op_index_repository(op: &Value) -> Value {
    let repo = match resolve_repo(op, "index_repository") {
        Ok(r) => r,
        Err(e) => return e,
    };
    let mut conn = match open_db(&repo, "index_repository") {
        Ok(c) => c,
        Err(e) => return e,
    };
    match index_repo(&mut conn, &repo) {
        Ok(stats) => {
            let summary = format!(
                "{} changed, {} unchanged, {} removed ({} symbols, {} edges)",
                stats.changed, stats.unchanged, stats.removed, stats.total_symbols, stats.total_edges
            );
            let data = json!({
                "repo_path": repo.display().to_string(),
                "changed_files": stats.changed,
                "unchanged_files": stats.unchanged,
                "removed_files": stats.removed,
                "total_symbols": stats.total_symbols,
                "total_edges": stats.total_edges,
            });
            // A no-op reindex (nothing changed) is observationally a read, so a
            // repeat is flagged redundant; a reindex that touched the graph is a
            // mutate that clears any prior read of this repo.
            let meta = if stats.changed == 0 && stats.removed == 0 {
                Meta::read(format!("repo:{}", repo.display()))
            } else {
                Meta::mutate(format!("repo:{}", repo.display()))
            };
            passed(summary, data, None, Some(meta))
        }
        Err(e) => errored(format!("index_repository: {e}")),
    }
}

struct IndexStats {
    changed: usize,
    unchanged: usize,
    removed: usize,
    total_symbols: i64,
    total_edges: i64,
}

fn index_repo(conn: &mut Connection, repo: &Path) -> Result<IndexStats, String> {
    let files = collect_rust_files(repo);
    let mut on_disk: Vec<String> = Vec::new();
    let mut changed = 0usize;
    let mut unchanged = 0usize;

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut pkg_cache: HashMap<PathBuf, (String, PathBuf)> = HashMap::new();

    for file in &files {
        let rel = rel_path(repo, file);
        on_disk.push(rel.clone());

        let bytes = match std::fs::read(file) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hash = sha256_hex(&bytes);

        let existing: Option<String> = tx
            .query_row("SELECT content_hash FROM files WHERE path = ?1", params![rel], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;

        if existing.as_deref() == Some(hash.as_str()) {
            unchanged += 1;
            continue;
        }
        changed += 1;

        // Replace any prior rows for this file (symbols, their fts entries, and
        // edges originating from them) before inserting fresh ones.
        delete_file_rows(&tx, &rel).map_err(|e| e.to_string())?;

        let file_id = upsert_file(&tx, &rel, &hash).map_err(|e| e.to_string())?;

        let (package, module) = package_and_module(repo, file, &mut pkg_cache);
        let source = String::from_utf8_lossy(&bytes).into_owned();
        let parsed = parse::parse_file(&source, &package, &module);
        insert_parsed(&tx, file_id, &parsed).map_err(|e| e.to_string())?;
    }

    // Drop files that vanished from disk (cascades to their symbols).
    let removed = delete_missing_files(&tx, &on_disk).map_err(|e| e.to_string())?;

    db::resolve_edges(&tx).map_err(|e| e.to_string())?;

    let total_symbols: i64 =
        tx.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    let total_edges: i64 =
        tx.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0)).map_err(|e| e.to_string())?;

    tx.commit().map_err(|e| e.to_string())?;

    Ok(IndexStats { changed, unchanged, removed, total_symbols, total_edges })
}

fn delete_file_rows(conn: &Connection, rel: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM symbols_fts WHERE rowid IN
            (SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1)",
        params![rel],
    )?;
    conn.execute(
        "DELETE FROM edges WHERE src_symbol_id IN
            (SELECT s.symbol_id FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1)",
        params![rel],
    )?;
    conn.execute(
        "DELETE FROM symbols WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
        params![rel],
    )?;
    Ok(())
}

fn upsert_file(conn: &Connection, rel: &str, hash: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO files (path, content_hash, language) VALUES (?1, ?2, 'rust')
            ON CONFLICT(path) DO UPDATE SET content_hash = excluded.content_hash",
        params![rel, hash],
    )?;
    conn.query_row("SELECT id FROM files WHERE path = ?1", params![rel], |r| r.get(0))
}

fn insert_parsed(conn: &Connection, file_id: i64, parsed: &parse::Parsed) -> rusqlite::Result<()> {
    for s in &parsed.symbols {
        let changed = conn.execute(
            "INSERT OR IGNORE INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name,
                 signature, kind, file_id, start_line, end_line, doc_comment, visibility, attributes)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                s.symbol_id, s.language, s.package, s.module, s.qualified_name, s.simple_name,
                s.signature, s.kind, file_id, s.start_line, s.end_line, s.doc_comment,
                s.visibility, s.attributes
            ],
        )?;
        if changed == 1 {
            let rowid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO symbols_fts (rowid, symbol_id, qualified_name, doc_comment)
                 VALUES (?1, ?2, ?3, ?4)",
                params![rowid, s.symbol_id, s.qualified_name, s.doc_comment],
            )?;
        }
    }
    for e in &parsed.edges {
        // `contains` edges are structurally exact → definite at insert time.
        // `calls` edges get a placeholder recomputed by db::resolve_edges after
        // dst_symbol_id resolution completes for the whole repo.
        let confidence = if e.edge_kind == "contains" { "definite" } else { "unresolved" };
        conn.execute(
            "INSERT OR IGNORE INTO edges (src_symbol_id, dst_symbol_id, dst_name, edge_kind, call_style, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![e.src_symbol_id, e.dst_symbol_id, e.dst_name, e.edge_kind, e.call_style, confidence],
        )?;
    }
    Ok(())
}

fn delete_missing_files(conn: &Connection, on_disk: &[String]) -> rusqlite::Result<usize> {
    let known: Vec<String> =
        {
            let mut stmt = conn.prepare("SELECT path FROM files")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
    let mut removed = 0;
    for path in known {
        if !on_disk.contains(&path) {
            delete_file_rows(conn, &path)?;
            conn.execute("DELETE FROM files WHERE path = ?1", params![path])?;
            removed += 1;
        }
    }
    Ok(removed)
}

// ── op: find_symbol ───────────────────────────────────────────────────────────

pub fn op_find_symbol(op: &Value) -> Value {
    let repo = match resolve_repo(op, "find_symbol") {
        Ok(r) => r,
        Err(e) => return e,
    };
    let query = match req_str(op, "query", "find_symbol") {
        Ok(q) => q,
        Err(e) => return e,
    };
    let limit = opt_i64(op, "limit", 20).clamp(1, 500);
    let conn = match open_db(&repo, "find_symbol") {
        Ok(c) => c,
        Err(e) => return e,
    };

    let Some(match_expr) = fts_match_expr(query) else {
        // No searchable tokens — an empty result, not an error.
        return passed(
            "find_symbol: 0 matches",
            json!({ "query": query, "count": 0, "matches": [] }),
            None,
            Some(Meta::read(format!("find:{}:{}", repo.display(), query))),
        );
    };

    let sql = "SELECT s.symbol_id, s.qualified_name, s.kind, s.signature, s.module, f.path,
                      s.start_line, s.doc_comment
                 FROM symbols_fts fts
                 JOIN symbols s ON s.id = fts.rowid
                 JOIN files f ON f.id = s.file_id
                WHERE symbols_fts MATCH ?1
                ORDER BY rank
                LIMIT ?2";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => return errored(format!("find_symbol: {e}")),
    };
    let rows = stmt.query_map(params![match_expr, limit], |r| {
        Ok(json!({
            "symbol_id": r.get::<_, String>(0)?,
            "qualified_name": r.get::<_, String>(1)?,
            "kind": r.get::<_, String>(2)?,
            "signature": r.get::<_, String>(3)?,
            "module": r.get::<_, String>(4)?,
            "file": r.get::<_, String>(5)?,
            "start_line": r.get::<_, i64>(6)?,
            "doc_comment": r.get::<_, String>(7)?,
        }))
    });
    let matches: Vec<Value> = match rows {
        Ok(iter) => match iter.collect::<rusqlite::Result<Vec<_>>>() {
            Ok(v) => v,
            Err(e) => return errored(format!("find_symbol: {e}")),
        },
        Err(e) => return errored(format!("find_symbol: {e}")),
    };

    passed(
        format!("find_symbol: {} match(es) for '{}'", matches.len(), query),
        json!({ "query": query, "count": matches.len(), "matches": matches }),
        None,
        Some(Meta::read(format!("find:{}:{}", repo.display(), query))),
    )
}

/// Build an FTS5 MATCH expression: alnum/underscore tokens turned into prefix
/// queries (`tok*`) AND-ed together. Returns `None` if the query has no usable
/// tokens (so the caller can short-circuit to an empty result).
fn fts_match_expr(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(|t| format!("{t}*"))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

// ── op: get_symbol ────────────────────────────────────────────────────────────

pub fn op_get_symbol(op: &Value) -> Value {
    let repo = match resolve_repo(op, "get_symbol") {
        Ok(r) => r,
        Err(e) => return e,
    };
    let symbol_id = match req_str(op, "symbol_id", "get_symbol") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let conn = match open_db(&repo, "get_symbol") {
        Ok(c) => c,
        Err(e) => return e,
    };

    let detail = conn
        .query_row(
            "SELECT s.symbol_id, s.language, s.package, s.module, s.qualified_name, s.signature,
                    s.kind, f.path, s.start_line, s.end_line, s.doc_comment, s.visibility, s.attributes
               FROM symbols s JOIN files f ON f.id = s.file_id
              WHERE s.symbol_id = ?1",
            params![symbol_id],
            |r| {
                Ok(json!({
                    "symbol_id": r.get::<_, String>(0)?,
                    "language": r.get::<_, String>(1)?,
                    "package": r.get::<_, String>(2)?,
                    "module": r.get::<_, String>(3)?,
                    "qualified_name": r.get::<_, String>(4)?,
                    "signature": r.get::<_, String>(5)?,
                    "kind": r.get::<_, String>(6)?,
                    "file": r.get::<_, String>(7)?,
                    "start_line": r.get::<_, i64>(8)?,
                    "end_line": r.get::<_, i64>(9)?,
                    "doc_comment": r.get::<_, String>(10)?,
                    "visibility": r.get::<_, String>(11)?,
                    "attributes": r.get::<_, String>(12)?,
                }))
            },
        )
        .optional();

    let mut detail = match detail {
        Ok(Some(d)) => d,
        Ok(None) => {
            // By design: an old id no longer resolving after that symbol's own
            // signature changed is a not-found, not a defect.
            return failed(format!("get_symbol: symbol not found: {symbol_id}"));
        }
        Err(e) => return errored(format!("get_symbol: {e}")),
    };

    let callees = neighbor_names(&conn, symbol_id, "callees").unwrap_or_default();
    let callers = neighbor_names(&conn, symbol_id, "callers").unwrap_or_default();
    detail["callees"] = json!(callees);
    detail["callers"] = json!(callers);

    passed(
        format!("get_symbol: {symbol_id}"),
        detail,
        None,
        Some(Meta::read(symbol_id)),
    )
}

fn neighbor_names(conn: &Connection, symbol_id: &str, which: &str) -> rusqlite::Result<Vec<String>> {
    let sql = if which == "callees" {
        "SELECT DISTINCT dst_symbol_id FROM edges
          WHERE src_symbol_id = ?1 AND edge_kind = 'calls' AND dst_symbol_id IS NOT NULL"
    } else {
        "SELECT DISTINCT src_symbol_id FROM edges
          WHERE dst_symbol_id = ?1 AND edge_kind = 'calls'"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![symbol_id], |r| r.get::<_, String>(0))?;
    rows.collect()
}

// ── op: slice_symbol ──────────────────────────────────────────────────────────

pub fn op_slice_symbol(op: &Value) -> Value {
    let repo = match resolve_repo(op, "slice_symbol") {
        Ok(r) => r,
        Err(e) => return e,
    };
    let symbol_id = match req_str(op, "symbol_id", "slice_symbol") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let max_depth = opt_i64(op, "max_depth", 3).clamp(0, 64);
    let max_nodes = opt_i64(op, "max_nodes", 50).clamp(1, 10_000);
    let direction = str_field(op, "direction").unwrap_or("callees");
    if direction != "callees" && direction != "callers" {
        return failed("slice_symbol: direction must be 'callees' or 'callers'");
    }
    let conn = match open_db(&repo, "slice_symbol") {
        Ok(c) => c,
        Err(e) => return e,
    };

    if !symbol_exists(&conn, symbol_id) {
        return failed(format!("slice_symbol: symbol not found: {symbol_id}"));
    }

    // The depth predicate (`depth < max_depth`) is the real safety valve against
    // cyclic call graphs; UNION dedup alone would not guarantee termination.
    let cte = if direction == "callees" {
        "WITH RECURSIVE reach(sid, depth) AS (
             SELECT ?1, 0
             UNION
             SELECT e.dst_symbol_id, r.depth + 1
               FROM reach r JOIN edges e ON e.src_symbol_id = r.sid
              WHERE e.dst_symbol_id IS NOT NULL AND r.depth < ?2
         )
         SELECT sid, MIN(depth) AS depth FROM reach GROUP BY sid ORDER BY depth, sid LIMIT ?3"
    } else {
        "WITH RECURSIVE reach(sid, depth) AS (
             SELECT ?1, 0
             UNION
             SELECT e.src_symbol_id, r.depth + 1
               FROM reach r JOIN edges e ON e.dst_symbol_id = r.sid
              WHERE r.depth < ?2
         )
         SELECT sid, MIN(depth) AS depth FROM reach GROUP BY sid ORDER BY depth, sid LIMIT ?3"
    };

    // Fetch one past the cap to detect truncation.
    let mut stmt = match conn.prepare(cte) {
        Ok(s) => s,
        Err(e) => return errored(format!("slice_symbol: {e}")),
    };
    let rows = stmt.query_map(params![symbol_id, max_depth, max_nodes + 1], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    });
    let mut node_rows: Vec<(String, i64)> = match rows {
        Ok(iter) => match iter.collect::<rusqlite::Result<Vec<_>>>() {
            Ok(v) => v,
            Err(e) => return errored(format!("slice_symbol: {e}")),
        },
        Err(e) => return errored(format!("slice_symbol: {e}")),
    };
    let truncated = node_rows.len() as i64 > max_nodes;
    node_rows.truncate(max_nodes as usize);

    // Enrich each node with detail, and depth-index for the payload.
    let mut depth_by_id: HashMap<String, i64> = HashMap::new();
    let mut nodes_full: Vec<Value> = Vec::new();
    let mut nodes_brief: Vec<Value> = Vec::new();
    for (sid, depth) in &node_rows {
        depth_by_id.insert(sid.clone(), *depth);
        let d = conn
            .query_row(
                "SELECT s.qualified_name, s.kind, s.signature, s.module, f.path, s.start_line
                   FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.symbol_id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .unwrap_or(None);
        let (qn, kind, sig, module, file, line) =
            d.unwrap_or_else(|| ("<unresolved>".into(), "".into(), "".into(), "".into(), "".into(), 0));
        nodes_brief.push(json!({ "symbol_id": sid, "qualified_name": qn, "depth": depth }));
        nodes_full.push(json!({
            "symbol_id": sid, "qualified_name": qn, "kind": kind, "signature": sig,
            "module": module, "file": file, "start_line": line, "depth": depth,
        }));
    }

    // Edges internal to the slice.
    let mut slice_edges: Vec<Value> = Vec::new();
    if !depth_by_id.is_empty() {
        if let Ok(mut es) = conn.prepare(
            "SELECT src_symbol_id, dst_symbol_id, edge_kind FROM edges WHERE dst_symbol_id IS NOT NULL",
        ) {
            if let Ok(iter) = es.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            }) {
                for e in iter.flatten() {
                    if depth_by_id.contains_key(&e.0) && depth_by_id.contains_key(&e.1) {
                        slice_edges.push(json!({ "src": e.0, "dst": e.1, "kind": e.2 }));
                    }
                }
            }
        }
    }

    let full = json!({
        "root": symbol_id,
        "direction": direction,
        "max_depth": max_depth,
        "max_nodes": max_nodes,
        "truncated": truncated,
        "node_count": nodes_full.len(),
        "edge_count": slice_edges.len(),
        "nodes": nodes_full,
        "edges": slice_edges,
    });

    // Write the full slice to disk and return a bounded summary inline, rather
    // than dumping the whole graph into the response.
    let data_path = write_slice_file(&repo, symbol_id, &full);
    let summary = format!(
        "slice_symbol: {} node(s), {} edge(s), depth<={}{} ({})",
        nodes_brief.len(),
        slice_edges.len(),
        max_depth,
        if truncated { format!(", truncated at {max_nodes}") } else { String::new() },
        direction
    );
    let inline = json!({
        "root": symbol_id,
        "direction": direction,
        "max_depth": max_depth,
        "max_nodes": max_nodes,
        "truncated": truncated,
        "node_count": nodes_brief.len(),
        "edge_count": slice_edges.len(),
        "nodes": nodes_brief,
        "data_path": data_path,
    });

    passed(summary, inline, data_path, Some(Meta::read(symbol_id)))
}

fn write_slice_file(repo: &Path, symbol_id: &str, full: &Value) -> Option<String> {
    let dir = repo.join(db::STORE_DIR).join("slices");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!("slice-{}.json", sha256_hex(symbol_id.as_bytes()));
    let path = dir.join(name);
    let body = serde_json::to_string_pretty(full).ok()?;
    std::fs::write(&path, body).ok()?;
    Some(path.display().to_string())
}

// ── op: explain_path ──────────────────────────────────────────────────────────

pub fn op_explain_path(op: &Value) -> Value {
    let repo = match resolve_repo(op, "explain_path") {
        Ok(r) => r,
        Err(e) => return e,
    };
    let from = match req_str(op, "from", "explain_path") {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    let to = match req_str(op, "to", "explain_path") {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    let max_depth = opt_i64(op, "max_depth", 6).clamp(1, 64);
    let conn = match open_db(&repo, "explain_path") {
        Ok(c) => c,
        Err(e) => return e,
    };

    if !symbol_exists(&conn, &from) {
        return failed(format!("explain_path: 'from' symbol not found: {from}"));
    }
    if !symbol_exists(&conn, &to) {
        return failed(format!("explain_path: 'to' symbol not found: {to}"));
    }

    let resource = format!("{from}=>{to}");

    if from == to {
        return passed(
            "explain_path: from and to are the same symbol (path length 0)",
            json!({ "from": from, "to": to, "found": true, "depth": 0, "path": [from] }),
            None,
            Some(Meta::read(resource)),
        );
    }

    // Cycle-safe shortest path: the `instr` guard forbids revisiting a node
    // already on the path, and the depth bound guarantees termination.
    let sql = "WITH RECURSIVE paths(sid, trail, depth) AS (
                   SELECT ?1, char(10) || ?1 || char(10), 0
                   UNION
                   SELECT e.dst_symbol_id, p.trail || e.dst_symbol_id || char(10), p.depth + 1
                     FROM paths p JOIN edges e ON e.src_symbol_id = p.sid
                    WHERE e.dst_symbol_id IS NOT NULL
                      AND p.depth < ?2
                      AND instr(p.trail, char(10) || e.dst_symbol_id || char(10)) = 0
               )
               SELECT trail, depth FROM paths WHERE sid = ?3 ORDER BY depth LIMIT 1";
    let found: Option<(String, i64)> = conn
        .query_row(sql, params![from, max_depth, to], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .optional()
        .unwrap_or(None);

    match found {
        Some((trail, depth)) => {
            let path: Vec<String> =
                trail.split('\n').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
            passed(
                format!("explain_path: path found in {depth} hop(s)"),
                json!({ "from": from, "to": to, "found": true, "depth": depth, "path": path }),
                None,
                Some(Meta::read(resource)),
            )
        }
        None => passed(
            format!("explain_path: no path found within depth {max_depth}"),
            json!({ "from": from, "to": to, "found": false, "depth": max_depth, "path": [] }),
            None,
            Some(Meta::read(resource)),
        ),
    }
}

fn symbol_exists(conn: &Connection, symbol_id: &str) -> bool {
    conn.query_row("SELECT 1 FROM symbols WHERE symbol_id = ?1", params![symbol_id], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

// ── op: impact_analysis ───────────────────────────────────────────────────────

/// Callee simple names that heuristically suggest a persistence operation. This
/// vocabulary is deliberately small, Level-1, and both over- and under-matches:
/// it flags any symbol that *calls* one of these names, so it will hit generic
/// `execute`/`query` helpers that have nothing to do with a database, and will
/// miss ORM abstractions that never call these specific names. Documented as
/// best-effort, not authoritative.
const PERSISTENCE_VOCAB: [&str; 9] = [
    "execute", "execute_batch", "query", "query_row", "query_map", "prepare", "transaction",
    "commit", "rollback",
];

/// Routing-attribute substrings (common Rust web frameworks: axum/actix/rocket
/// style). Matched only against a symbol's captured `#[...]` attribute text, at
/// an attribute-path boundary, so a function merely *named* `get` is not
/// flagged. Best-effort and incomplete by design.
const ROUTE_MARKERS: [&str; 6] = ["get(", "post(", "put(", "delete(", "patch(", "route("];

pub fn op_impact_analysis(op: &Value) -> Value {
    let repo = match resolve_repo(op, "impact_analysis") {
        Ok(r) => r,
        Err(e) => return e,
    };
    let file = match req_str(op, "file", "impact_analysis") {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    let start_line = match req_i64(op, "start_line", "impact_analysis") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let end_line = match req_i64(op, "end_line", "impact_analysis") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if end_line < start_line {
        return failed(format!(
            "impact_analysis: end_line ({end_line}) must be >= start_line ({start_line})"
        ));
    }
    let max_depth = opt_i64(op, "max_depth", 3).clamp(0, 64);
    let max_nodes = opt_i64(op, "max_nodes", 50).clamp(1, 10_000);
    let conn = match open_db(&repo, "impact_analysis") {
        Ok(c) => c,
        Err(e) => return e,
    };

    let rel = normalize_rel(&repo, &file);
    let resource = format!("impact:{rel}:{start_line}-{end_line}");

    // The file must be present in the index. A never-indexed / misspelled path is
    // a hard failure (distinct from a valid file whose range overlaps nothing,
    // which is a passed-but-empty result below).
    let file_id: Option<i64> = conn
        .query_row("SELECT id FROM files WHERE path = ?1", params![rel], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    let Some(file_id) = file_id else {
        return failed(format!(
            "impact_analysis: file not indexed: '{rel}' (run index_repository first, or check the path)"
        ));
    };

    // Roots: every symbol in that file whose [start_line, end_line] overlaps the
    // requested range. These are exactly what was edited.
    let root_ids: Vec<String> = {
        let sql = "SELECT symbol_id FROM symbols
                    WHERE file_id = ?1 AND start_line <= ?2 AND end_line >= ?3
                    ORDER BY symbol_id";
        match conn.prepare(sql) {
            Ok(mut stmt) => match stmt
                .query_map(params![file_id, end_line, start_line], |r| r.get::<_, String>(0))
            {
                Ok(iter) => match iter.collect::<rusqlite::Result<Vec<_>>>() {
                    Ok(v) => v,
                    Err(e) => return errored(format!("impact_analysis: {e}")),
                },
                Err(e) => return errored(format!("impact_analysis: {e}")),
            },
            Err(e) => return errored(format!("impact_analysis: {e}")),
        }
    };

    if root_ids.is_empty() {
        let full = json!({
            "file": rel, "start_line": start_line, "end_line": end_line,
            "max_depth": max_depth, "max_nodes": max_nodes, "truncated": false,
            "roots": [], "callers": [],
            "tests": [], "routes": [], "persistence_operations": [], "public_apis": [],
        });
        return passed(
            format!("impact_analysis: no symbol overlaps {rel}:{start_line}-{end_line}"),
            full,
            None,
            Some(Meta::read(resource)),
        );
    }

    // Reverse call-graph walk from every root. Unlike slice_symbol's callers
    // arm, this filters explicitly to `edge_kind = 'calls'` (and resolved
    // targets only): impact means call-graph impact, NOT containment ancestry,
    // so a symbol's structural parent must not show up as a "caller". Each node
    // carries the running weakest confidence (lowest rank) along the path that
    // reached it; the final MIN(minconf) is the conservative worst case across
    // ALL discovered paths within max_depth. UNION (not UNION ALL) dedups
    // (sid,depth,minconf) triples so the walk terminates and stays bounded.
    let seed = root_ids.iter().map(|_| "SELECT ?, 0, 2").collect::<Vec<_>>().join(" UNION ");
    let cte = format!(
        "WITH RECURSIVE reach(sid, depth, minconf) AS (
             {seed}
             UNION
             SELECT e.src_symbol_id, r.depth + 1,
                    MIN(r.minconf, CASE e.confidence
                        WHEN 'definite' THEN 2 WHEN 'possible' THEN 1 ELSE 0 END)
               FROM reach r JOIN edges e ON e.dst_symbol_id = r.sid
              WHERE e.edge_kind = 'calls' AND e.dst_symbol_id IS NOT NULL AND r.depth < ?
         )
         SELECT sid, MIN(depth) AS depth, MIN(minconf) AS minconf
           FROM reach GROUP BY sid ORDER BY depth, sid LIMIT ?"
    );

    let mut cte_params: Vec<rusqlite::types::Value> = Vec::new();
    for id in &root_ids {
        cte_params.push(rusqlite::types::Value::Text(id.clone()));
    }
    cte_params.push(rusqlite::types::Value::Integer(max_depth));
    cte_params.push(rusqlite::types::Value::Integer(max_nodes + 1));

    let mut stmt = match conn.prepare(&cte) {
        Ok(s) => s,
        Err(e) => return errored(format!("impact_analysis: {e}")),
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(cte_params.iter()), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    });
    let mut node_rows: Vec<(String, i64, i64)> = match rows {
        Ok(iter) => match iter.collect::<rusqlite::Result<Vec<_>>>() {
            Ok(v) => v,
            Err(e) => return errored(format!("impact_analysis: {e}")),
        },
        Err(e) => return errored(format!("impact_analysis: {e}")),
    };
    let truncated = node_rows.len() as i64 > max_nodes;
    node_rows.truncate(max_nodes as usize);

    // Set of symbols that make a persistence-suggestive call (computed once).
    let persistence_srcs = persistence_sources(&conn);
    let root_set: std::collections::HashSet<&String> = root_ids.iter().collect();

    let mut roots_out: Vec<Value> = Vec::new();
    let mut callers_out: Vec<Value> = Vec::new();
    let mut tests: Vec<Value> = Vec::new();
    let mut routes: Vec<Value> = Vec::new();
    let mut persistence: Vec<Value> = Vec::new();
    let mut public_apis: Vec<Value> = Vec::new();

    for (sid, depth, minconf) in &node_rows {
        let is_root = root_set.contains(sid);
        // Roots are exactly what was named — not inferred — so they always report
        // depth 0 and definite confidence, regardless of any reverse edge that
        // also happens to reach them.
        let (out_depth, confidence) =
            if is_root { (0, "definite") } else { (*depth, rank_label(*minconf)) };

        let detail = conn
            .query_row(
                "SELECT s.qualified_name, s.kind, f.path, s.start_line, s.visibility,
                        s.attributes, s.module
                   FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.symbol_id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()
            .unwrap_or(None);
        let (qn, kind, fpath, sline, vis, attrs, module) = detail.unwrap_or_else(|| {
            ("<unresolved>".into(), "".into(), "".into(), 0, "".into(), "".into(), "".into())
        });

        let entry = json!({
            "symbol_id": sid, "qualified_name": qn, "kind": kind, "file": fpath,
            "start_line": sline, "depth": out_depth, "confidence": confidence,
        });

        if is_root {
            roots_out.push(entry.clone());
        } else {
            callers_out.push(entry.clone());
        }
        if vis == "pub" {
            public_apis.push(entry.clone());
        }
        if is_test(&attrs, &module, &fpath) {
            tests.push(entry.clone());
        }
        if is_route(&attrs) {
            routes.push(entry.clone());
        }
        if persistence_srcs.contains(sid) {
            persistence.push(entry.clone());
        }
    }

    let full = json!({
        "file": rel, "start_line": start_line, "end_line": end_line,
        "max_depth": max_depth, "max_nodes": max_nodes, "truncated": truncated,
        "root_count": roots_out.len(), "caller_count": callers_out.len(),
        "roots": roots_out, "callers": callers_out,
        "tests": tests, "routes": routes,
        "persistence_operations": persistence, "public_apis": public_apis,
    });

    let data_path = write_impact_file(&repo, &rel, start_line, end_line, &full);
    let summary = format!(
        "impact_analysis: {} root(s), {} caller(s); {} test(s), {} route(s), {} persistence, {} public api(s){}",
        roots_out.len(), callers_out.len(), tests.len(), routes.len(),
        persistence.len(), public_apis.len(),
        if truncated { format!(", truncated at {max_nodes}") } else { String::new() }
    );

    let mut inline = full;
    inline["data_path"] = json!(data_path);
    passed(summary, inline, data_path, Some(Meta::read(resource)))
}

/// Symbols whose own outgoing `calls` edges hit a persistence vocabulary name.
fn persistence_sources(conn: &Connection) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let placeholders = PERSISTENCE_VOCAB.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT DISTINCT src_symbol_id FROM edges
          WHERE edge_kind = 'calls' AND dst_name IN ({placeholders})"
    );
    let params: Vec<rusqlite::types::Value> =
        PERSISTENCE_VOCAB.iter().map(|s| rusqlite::types::Value::Text((*s).to_string())).collect();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(iter) =
            stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| r.get::<_, String>(0))
        {
            for v in iter.flatten() {
                set.insert(v);
            }
        }
    }
    set
}

/// Map a confidence rank (definite=2 > possible=1 > heuristic=0) back to a label.
fn rank_label(rank: i64) -> &'static str {
    match rank {
        r if r >= 2 => "definite",
        1 => "possible",
        _ => "heuristic",
    }
}

/// Heuristic test detection: a test attribute, a `tests`/`test` module segment,
/// or a test-shaped file path.
fn is_test(attributes: &str, module: &str, file: &str) -> bool {
    if attributes.contains("#[test]")
        || attributes.contains("tokio::test")
        || attributes.contains("async_std::test")
        || attr_has_marker(attributes, "test(")
    {
        return true;
    }
    if module.split("::").any(|seg| seg == "tests" || seg == "test") {
        return true;
    }
    let last = file.rsplit('/').next().unwrap_or(file);
    if last == "tests.rs" || last.ends_with("_test.rs") || last.ends_with("_tests.rs") {
        return true;
    }
    file.starts_with("tests/") || file.contains("/tests/")
}

/// Heuristic route detection: a routing-attribute macro in the captured `#[...]`.
fn is_route(attributes: &str) -> bool {
    ROUTE_MARKERS.iter().any(|m| attr_has_marker(attributes, m))
}

/// Whether `marker` appears in `attrs` at an attribute-path boundary — i.e. the
/// preceding character is not part of an identifier. This keeps `#[widget(...)]`
/// from matching `get(` while still matching `#[get(...)]`, `#[web::get(...)]`,
/// and `#[actix_web::get(...)]`.
fn attr_has_marker(attrs: &str, marker: &str) -> bool {
    let bytes = attrs.as_bytes();
    let mut from = 0;
    while let Some(pos) = attrs[from..].find(marker) {
        let abs = from + pos;
        let boundary = abs == 0 || {
            let c = bytes[abs - 1];
            !(c.is_ascii_alphanumeric() || c == b'_')
        };
        if boundary {
            return true;
        }
        from = abs + marker.len();
    }
    false
}

/// Normalize a caller-supplied `file` to the stored repo-relative form: forward
/// slashes, no leading `./`, and any absolute repo prefix stripped. Delegates
/// the actual repo-relative join to `rel_path` — the same function indexing
/// uses to populate `files.path` — so a lookup here always agrees with what
/// was stored, instead of risking drift between two independent
/// normalizations of the same path.
fn normalize_rel(repo: &Path, file: &str) -> String {
    let mut f = file.replace('\\', "/");
    while let Some(s) = f.strip_prefix("./") {
        f = s.to_string();
    }
    rel_path(repo, Path::new(&f))
}

/// Persist the full impact payload under `.murmur/impacts/` and return its path,
/// mirroring `write_slice_file`.
fn write_impact_file(repo: &Path, rel: &str, start: i64, end: i64, full: &Value) -> Option<String> {
    let dir = repo.join(db::STORE_DIR).join("impacts");
    std::fs::create_dir_all(&dir).ok()?;
    let key = format!("{rel}:{start}-{end}");
    let name = format!("impact-{}.json", sha256_hex(key.as_bytes()));
    let path = dir.join(name);
    let body = serde_json::to_string_pretty(full).ok()?;
    std::fs::write(&path, body).ok()?;
    Some(path.display().to_string())
}

// ── Filesystem / crate-layout helpers ─────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn rel_path(repo: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(repo).unwrap_or(file);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Recursively collect `*.rs` files, skipping build output, VCS, and the tool's
/// own `.murmur` store and any hidden directory.
fn collect_rust_files(repo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if name == "target" || name == ".git" || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Determine the crate package name and module path for `file`.
///
/// - package: `name` from the nearest ancestor `Cargo.toml` that has a
///   `[package]` section; falls back to the repo directory name.
/// - module: path from the crate's `src/` to the file, with `lib.rs`/`main.rs`/
///   `mod.rs` collapsing to their parent module; `""` at the crate root.
fn package_and_module(
    repo: &Path,
    file: &Path,
    cache: &mut HashMap<PathBuf, (String, PathBuf)>,
) -> (String, String) {
    let dir = file.parent().unwrap_or(repo).to_path_buf();
    let (package, crate_root) = nearest_crate(repo, &dir, cache);
    let module = module_path(&crate_root, file);
    (package, module)
}

fn nearest_crate(
    repo: &Path,
    start: &Path,
    cache: &mut HashMap<PathBuf, (String, PathBuf)>,
) -> (String, PathBuf) {
    if let Some(hit) = cache.get(start) {
        return hit.clone();
    }
    let mut cur = Some(start.to_path_buf());
    while let Some(dir) = cur {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            if let Some(name) = package_name(&text) {
                let result = (name, dir.clone());
                cache.insert(start.to_path_buf(), result.clone());
                return result;
            }
        }
        if dir == repo {
            break;
        }
        cur = dir.parent().map(|p| p.to_path_buf());
        // Never ascend above the repo root.
        if let Some(ref c) = cur {
            if !c.starts_with(repo) {
                break;
            }
        }
    }
    let fallback = (
        repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "crate".into()),
        repo.to_path_buf(),
    );
    cache.insert(start.to_path_buf(), fallback.clone());
    fallback
}

/// Extract `name` from the `[package]` table of a `Cargo.toml`. Returns `None`
/// for workspace-only manifests (no `[package]`).
fn package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = t.strip_prefix("name") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    let v = rest.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

fn module_path(crate_root: &Path, file: &Path) -> String {
    let rel = match file.strip_prefix(crate_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let mut comps: Vec<String> =
        rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if comps.is_empty() {
        return String::new();
    }
    // Drop a leading `src/`.
    if comps.first().map(|s| s.as_str()) == Some("src") {
        comps.remove(0);
    }
    if comps.is_empty() {
        return String::new();
    }
    // Strip the `.rs` extension from the final component.
    let last = comps.len() - 1;
    if let Some(stem) = comps[last].strip_suffix(".rs") {
        comps[last] = stem.to_string();
    }
    // Collapse entry-point / module-root file names to their parent module.
    if matches!(comps[last].as_str(), "lib" | "main" | "mod") {
        comps.pop();
    }
    comps.join("::")
}
