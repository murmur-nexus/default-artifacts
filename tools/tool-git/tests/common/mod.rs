//! What the tool-git suites share.
//!
//! An [`Env`] is one session's worth of host for a call: the work folder, the host folders the
//! session opens to the tool, and the test host for `murmur:process` (`process`). A call goes
//! through one of two [`Backend`]s:
//!
//! - [`Backend::Component`] runs the compiled `wasm32-wasip2` component in Wasmtime
//!   (`component`) and returns what the model receives: `{status, summary, data, truncated}`, with
//!   `data` parsed back to JSON.
//! - [`Backend::Core`] runs the portable core on the host (`core`) and returns the same fields
//!   from `tool_git::to_tool_result`, plus the envelope's `error_kind`, which murmur never hands
//!   the model.
//!
//! The behaviour suites (`tests/cases/*.rs`) are compiled once per backend: `git_tool.rs` drives
//! the component and `git_tool_core.rs` the core, and so on. [`Env::assert_kind`] asserts an
//! `error_kind` on the core and is a no-op on the component.

// Each test binary compiles this module on its own and uses only part of it.
#![allow(dead_code)]

pub mod component;
pub mod core;
pub mod fixture;
pub mod normalise;
pub mod process;
pub mod sequence;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use process::{Inject, ProcessHost, Record};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Component,
    Core,
}

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty folder under cargo's per-target scratch directory (on the same disk as
/// `target/`, never the system temp dir).
pub fn scratch(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "tool-git/{prefix}-{}-{nanos}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("scratch dir created");
    path
}

/// A scratch folder removed when dropped.
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(prefix: &str) -> Self {
        TestDir {
            path: scratch(prefix),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Run `git <args>` on the host for fixture setup, asserting it succeeds; returns its stdout.
pub fn git_ok(args: &[&str]) -> String {
    let out = Command::new("git").args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {} failed\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub struct Env {
    pub backend: Backend,
    pub work: PathBuf,
    pub roots: Vec<(PathBuf, bool)>,
    pub process: Arc<ProcessHost>,
}

impl Env {
    /// A session whose work folder is `work`, holding no host folder, with `git` granted.
    pub fn new(backend: Backend, work: &Path) -> Self {
        let home = scratch("home");
        Env {
            backend,
            work: work.to_path_buf(),
            roots: Vec::new(),
            process: Arc::new(ProcessHost::new(work.to_path_buf(), home)),
        }
    }

    /// Also open the host folder `path` to the tool, read-write or read-only.
    pub fn root(mut self, path: &Path, writable: bool) -> Self {
        self.roots.push((path.to_path_buf(), writable));
        self
    }

    pub fn shell_allow(self, allow: &[&str]) -> Self {
        self.process.set_shell_allow(allow);
        self
    }

    pub fn inject(&self, when: impl Fn(&[String]) -> bool + Send + Sync + 'static, what: Inject) {
        self.process.inject(when, what);
    }

    /// Every `run` the tool asked for, in order.
    pub fn records(&self) -> Vec<Record> {
        self.process.records()
    }

    /// The argv of every `run` that started a program.
    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.records()
            .into_iter()
            .filter(|r| r.ran)
            .map(|r| r.request.args)
            .collect()
    }

    pub fn call(&self, data: Value) -> Value {
        self.call_raw(data.to_string(), None)
    }

    pub fn call_with_log(&self, data: Value, log_path: &str) -> Value {
        self.call_raw(data.to_string(), Some(log_path.to_string()))
    }

    /// One call with `data` as the raw `tool-input.data` string.
    pub fn call_raw(&self, data: String, log_path: Option<String>) -> Value {
        let parse = |data: Option<String>| match data {
            None => Value::Null,
            Some(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        };
        match self.backend {
            Backend::Component => {
                let a = component::call(&self.process, &self.work, &self.roots, data, log_path);
                json!({
                    "status": a.status,
                    "summary": a.summary,
                    "data": parse(a.data),
                    "truncated": a.truncated,
                })
            }
            Backend::Core => {
                let env = core::HostEnv::new(&self.process, &self.work, &self.roots);
                let envelope = core::call(&env, data, log_path);
                let outcome = tool_git::to_tool_result(&envelope);
                json!({
                    "status": match outcome.status {
                        tool_git::OutcomeStatus::Passed => "passed",
                        tool_git::OutcomeStatus::Failed => "failed",
                        tool_git::OutcomeStatus::Error => "error",
                    },
                    "summary": outcome.summary,
                    "data": parse(outcome.data),
                    "truncated": outcome.truncated,
                    "error_kind": envelope.get("error_kind").cloned().unwrap_or(Value::Null),
                })
            }
        }
    }

    /// Assert the call's `error_kind` on the core; the component's result carries none.
    pub fn assert_kind(&self, res: &Value, kind: Option<&str>) {
        if self.backend == Backend::Core {
            assert_eq!(res["error_kind"].as_str(), kind, "error_kind; got {res}");
        }
    }
}

/// A row of code-graph's `symbols` table: (symbol_id, file_path, start_line, end_line).
pub type Sym<'a> = (&'a str, &'a str, i64, i64);

/// Build `<repo>/.murmur/code-graph.db` with tool-code-graph's schema and the given symbols. Each
/// distinct file path gets one `files` row.
pub fn build_graph_db(repo: &Path, symbols: &[Sym]) {
    let dir = repo.join(".murmur");
    fs::create_dir_all(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("code-graph.db")).unwrap();
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
        "#,
    )
    .unwrap();
    let mut file_ids: Vec<(&str, i64)> = Vec::new();
    for (_, path, _, _) in symbols {
        if !file_ids.iter().any(|(p, _)| p == path) {
            let id = file_ids.len() as i64 + 1;
            conn.execute(
                "INSERT INTO files (id, path, content_hash) VALUES (?1, ?2, 'deadbeef')",
                rusqlite::params![id, path],
            )
            .unwrap();
            file_ids.push((path, id));
        }
    }
    for (sid, path, start, end) in symbols {
        let file_id = file_ids.iter().find(|(p, _)| p == path).unwrap().1;
        conn.execute(
            "INSERT INTO symbols
                (symbol_id, language, package, module, qualified_name, simple_name,
                 signature, kind, file_id, start_line, end_line)
             VALUES (?1, 'rust', 'fixture', '', 'sym', 'sym', '(->())', 'function', ?2, ?3, ?4)",
            rusqlite::params![sid, file_id, start, end],
        )
        .unwrap();
    }
}
