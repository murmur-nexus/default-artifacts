# Fixtures

| File | What it is |
|---|---|
| `native-0.5.0-cases.json` | Hand-written inputs: per case, the code-graph symbols (or `null` for no database), the folder whose `.murmur/code-graph.db` is built (`db_at`), extra files, the LCOV files (`@SCRATCH@` stands for the case's scratch folder) and the calls, in order |
| `native-0.5.0-results.json` | What the native 0.5.0 tool returned for every call of every case, recorded with the recipe below: what the model saw, the capped-list file when one was written, and the database's suspicion columns after the call. `tests/wasm_component.rs` (`native_0_5_0_results`) replays every case through the component and requires the same |

## The cases

They mirror the native suite's scenarios 1, 3, 5 (two calls, `cov1` then `cov2`), 6, 7, 8, 9, 10
(a missing relative `missing-cov`, then a file), 11 (each malformed input as its own call), 12, 13,
14 (`limit: 2`) and 15, then two more:

- `repo_omitted_absolute_sf`: no `repo_path`, the database in the work folder itself, absolute
  `SF:` paths; a second call with `limit: 1` writes the capped list;
- `relative_sf`: no `repo_path`, the database in the work folder itself, `SF:src/lib.rs` and
  `SF:./src/lib.rs`.

Scenario 16 (a repository named through a symlink) is not recorded, because a symlink spelling
cannot be recorded portably; `tests/code_coverage_integration.rs` covers it.

## Recording `native-0.5.0-results.json`

Never edit the results by hand; re-record them. The recipe builds the last native release of the
tool (commit `95e17e1`, `tool-code-coverage` 0.5.0) in a scratch tree, adds the recorder below to
that tree as a test (so it can use rusqlite and `CARGO_BIN_EXE_tool-code-coverage`), and runs it.
For each case the recorder:

1. creates a fresh scratch folder under `RUNS` and canonicalizes it;
2. builds the database with the native suite's schema (`files` and `symbols` without the four
   suspicion columns, `files` row `(1, 'src/lib.rs', 'deadbeef')`) in `<scratch>/<db_at>/.murmur/`;
3. writes the extra files and the LCOV files, with `@SCRATCH@` replaced by the scratch path;
4. runs each call with the scratch folder as the working directory, sending
   `{"data": "<arguments as a JSON string>", "log_path": null}` on stdin as murmur did;
5. records `status`, `summary` and `data` the way murmur's native dispatch (`dispatch_native_tool`
   in `crates/capsule-runtime/src/runtime.rs`) handed them to the model (`status` is `passed` or
   `failed`, anything else `error`; `data` when not null), the parsed file `data.data_path` names
   as `data_path_file`, and the database snapshot as `db_after`: `{columns: [PRAGMA
   table_info(symbols) names], rows: [[symbol_id, suspicion_ef, suspicion_ep, suspicion_ochiai,
   suspicion_tarantula], …]}` ordered by `symbol_id`, absent columns as `null`, or `null` with no
   database;
6. replaces every occurrence of the scratch path in the recorded JSON with `@SCRATCH@`.

The replay rewrites `@SCRATCH@/` to nothing and then `@SCRATCH@` to `.`, which is how the
component names the same paths, relative to the work folder, and masks the 64-hex hash in
`localize-<hash>.json`, which native took from the canonical host path.

Run it from the repository root, with the scratch paths anywhere you like:

```bash
SCRATCH=/space/scratch/3e39-native            # any empty folder outside the repo
mkdir -p "$SCRATCH/src" "$SCRATCH/runs"
git archive 95e17e1 | tar -x -C "$SCRATCH/src"
# Save the recorder below as $SCRATCH/src/tools/tool-code-coverage/tests/record_native.rs, then:
FIXTURES="$PWD/tools/tool-code-coverage/tests/fixtures"
( cd "$SCRATCH/src" && CARGO_TARGET_DIR="$SCRATCH/target" \
    CASES="$FIXTURES/native-0.5.0-cases.json" OUT="$FIXTURES/native-0.5.0-results.json" \
    RUNS="$SCRATCH/runs" \
    cargo test --release --locked -p tool-code-coverage --test record_native -- --nocapture )
```

It prints `recorded 15 cases`. This file was recorded that way, with `SCRATCH=/space/scratch/3e39-native`.

### The recorder (`tests/record_native.rs` in the scratch tree)

```rust
//! Throwaway recorder: runs every case of `native-0.5.0-cases.json` through the native 0.5.0
//! binary and writes what the model saw, plus the database's suspicion columns after each call,
//! to `native-0.5.0-results.json`. Run only by the recipe in tests/fixtures/README.md.
//!
//! Environment: CASES (the cases file), OUT (the results file to write), RUNS (an existing
//! directory to create one scratch directory per case in).

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

fn build_graph_db(dir: &Path, symbols: &[Value]) {
    let store = dir.join(".murmur");
    fs::create_dir_all(&store).unwrap();
    let conn = Connection::open(store.join("code-graph.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, content_hash TEXT NOT NULL,
            language TEXT NOT NULL DEFAULT 'rust'
        );
        CREATE TABLE symbols (
            id INTEGER PRIMARY KEY, symbol_id TEXT NOT NULL UNIQUE, language TEXT NOT NULL,
            package TEXT NOT NULL, module TEXT NOT NULL, qualified_name TEXT NOT NULL,
            simple_name TEXT NOT NULL, signature TEXT NOT NULL, kind TEXT NOT NULL,
            file_id INTEGER NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
            doc_comment TEXT NOT NULL DEFAULT '', visibility TEXT NOT NULL DEFAULT '',
            attributes TEXT NOT NULL DEFAULT ''
        );
        INSERT INTO files (id, path, content_hash) VALUES (1, 'src/lib.rs', 'deadbeef');
        "#,
    )
    .unwrap();
    for row in symbols {
        let r = row.as_array().unwrap();
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name,
                 signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture', ?2, ?3, ?3, '(->())', ?4, 1, ?5, ?6)",
            rusqlite::params![
                r[0].as_str().unwrap(),
                r[1].as_str().unwrap(),
                r[2].as_str().unwrap(),
                r[3].as_str().unwrap(),
                r[4].as_i64().unwrap(),
                r[5].as_i64().unwrap()
            ],
        )
        .unwrap();
    }
}

/// `{columns, rows}` of `symbols`: every column name in order, then per symbol (by symbol_id)
/// its id and the four suspicion columns, null where a column is absent.
fn snapshot(db: &Path) -> Value {
    if !db.exists() {
        return Value::Null;
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let columns: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(symbols)").unwrap();
        let names = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
        names.map(Result::unwrap).collect()
    };
    let pick = |c: &str| {
        if columns.iter().any(|n| n == c) {
            c.to_string()
        } else {
            "NULL".to_string()
        }
    };
    let sql = format!(
        "SELECT symbol_id, {}, {}, {}, {} FROM symbols ORDER BY symbol_id",
        pick("suspicion_ef"),
        pick("suspicion_ep"),
        pick("suspicion_ochiai"),
        pick("suspicion_tarantula")
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?
            ]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    json!({ "columns": columns, "rows": rows })
}

#[test]
fn record() {
    let cases: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(std::env::var("CASES").unwrap()).unwrap())
            .unwrap();
    let runs = Path::new(&std::env::var("RUNS").unwrap()).canonicalize().unwrap();
    let mut results = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let scratch = runs.join(name);
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();
        let scratch = scratch.canonicalize().unwrap();
        let scratch_text = scratch.display().to_string();

        let db_dir = scratch.join(case["db_at"].as_str().unwrap());
        if let Some(symbols) = case["symbols"].as_array() {
            build_graph_db(&db_dir, symbols);
        }
        let mut files: Vec<(&String, &Value)> = Vec::new();
        files.extend(case["extra_files"].as_object().unwrap());
        files.extend(case["coverage"].as_object().unwrap());
        for (rel, body) in files {
            let path = scratch.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let body = body.as_str().unwrap().replace("@SCRATCH@", &scratch_text);
            fs::write(path, body).unwrap();
        }

        let db = db_dir.join(".murmur").join("code-graph.db");
        let mut calls = Vec::new();
        for args in case["calls"].as_array().unwrap() {
            // murmur hands a native tool the call arguments as a JSON string in `data`.
            let envelope = json!({ "data": args.to_string(), "log_path": null }).to_string();
            let mut child = Command::new(env!("CARGO_BIN_EXE_tool-code-coverage"))
                .current_dir(&scratch)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(envelope.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            let out: Value = serde_json::from_slice(&output.stdout).unwrap();
            // dispatch_native_tool's mapping: only status, summary and non-null data.
            let status = match out["status"].as_str() {
                Some(s @ ("passed" | "failed")) => s,
                _ => "error",
            };
            let data = out.get("data").cloned().unwrap_or(Value::Null);
            let data_path_file = match data.get("data_path").and_then(Value::as_str) {
                Some(path) => serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap(),
                None => Value::Null,
            };
            calls.push(json!({
                "args": args,
                "status": status,
                "summary": out["summary"],
                "data": data,
                "data_path_file": data_path_file,
                "db_after": snapshot(&db),
            }));
        }
        let recorded = json!({ "name": name, "calls": calls }).to_string();
        results.push(
            serde_json::from_str::<Value>(&recorded.replace(&scratch_text, "@SCRATCH@")).unwrap(),
        );
    }
    let mut text = serde_json::to_string_pretty(&results).unwrap();
    text.push('\n');
    fs::write(std::env::var("OUT").unwrap(), text).unwrap();
    println!("recorded {} cases", results.len());
}
```
