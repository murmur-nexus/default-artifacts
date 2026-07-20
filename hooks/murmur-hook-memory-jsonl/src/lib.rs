//! murmur-hook-memory-jsonl — durable, structured, per-Turn Memory Log.
//!
//! Binds to three Lifecycle Events (via the default `All` binding — see `murmur.yaml`):
//!   - `on-task-start` — reloads the Memory Log and seeds the new task's context via
//!     `hook-output::replace-context`, then records a `task_open` marker.
//!   - `on-inference`  — appends the current Turn as one `turn` record (per-Turn
//!     granularity — one JSONL line per Turn, not a bulk dump at task end).
//!   - `on-task-end`   — appends a `task_close` marker for the completed task.
//!
//! Storage is a plain JSONL file in the capsule workdir (`memory-log.jsonl` by
//! default). The location is swappable via the `MURMUR_MEMORY_LOG_PATH` environment
//! variable, which the host injects from the capsule manifest — the same
//! WASI-environment mechanism `murmur-hook-grafana` (`MURMUR_OTEL_ENDPOINT`) and
//! `murmur-hook-eval` (`MURMUR_EVAL_CONFIG`) already use to honor manifest-declared
//! config. The log is *append-only* and grows without bound on purpose: it is meant
//! to outlive any single Session Loop and accumulate across resumed launches, since
//! the workdir persists.
//!
//! This slice does logging only — no selection, tagging, or summarization of logged
//! Turns. The reload is a verbatim concatenation of prior Turn records, never a
//! summary.

/// Pure, host-compilable Memory Log logic. Kept outside the `wasm32`-gated module so
/// it can be unit-tested on the host toolchain (the WASM `Guest` impl is a thin
/// adapter that converts WIT event records into calls on this module).
pub mod memlog {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    /// Default log location, relative to the capsule workdir (the component's CWD).
    pub const DEFAULT_LOG_PATH: &str = "memory-log.jsonl";

    /// Environment variable the host injects when the manifest declares an alternate
    /// storage target for the Memory Log.
    pub const LOG_PATH_ENV: &str = "MURMUR_MEMORY_LOG_PATH";

    /// One line of the JSONL Memory Log. The `kind` tag discriminates the three
    /// record shapes.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum LogRecord {
        /// Written at `on-task-start`, before the reload's prior content. Delimits the
        /// start of a task's Turns and carries the task identity that the per-Turn
        /// `inference-event` does not itself include.
        TaskOpen {
            task_id: String,
            context_id: String,
            source: String,
        },
        /// Written once per `on-inference` — the per-Turn record.
        Turn {
            turn: u32,
            decision: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_name: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output: Option<String>,
            input_tokens: u64,
            output_tokens: u64,
        },
        /// The close-out marker written at `on-task-end`.
        TaskClose {
            task_id: String,
            exit_status: String,
        },
    }

    /// Resolve the storage target. An explicit, non-blank override (from the manifest
    /// via `LOG_PATH_ENV`) wins; otherwise the default workdir JSONL path is used.
    /// The default is never hardcoded at the write site — it is only produced here, so
    /// a manifest declaration can always redirect it.
    pub fn resolve_log_path(override_path: Option<&str>) -> PathBuf {
        match override_path {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => PathBuf::from(DEFAULT_LOG_PATH),
        }
    }

    /// Append one record as a single JSONL line. Always create-and-append, never
    /// truncate — this is what makes the log persist and keep growing across resumed
    /// launches. Creates parent directories for an override path if needed.
    pub fn append_record(path: &Path, record: &LogRecord) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create log directory {}: {e}", parent.display()))?;
            }
        }
        let line = serde_json::to_string(record).map_err(|e| format!("failed to serialize record: {e}"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
        writeln!(file, "{line}").map_err(|e| format!("failed to write record: {e}"))
    }

    /// Read every well-formed record from the log. A missing file yields an empty log
    /// (the expected state for the very first task in a fresh capsule). Blank lines and
    /// any line that fails to parse are skipped defensively rather than aborting the
    /// reload — a partial log is still useful context.
    pub fn read_records(path: &Path) -> Vec<LogRecord> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<LogRecord>(line).ok())
            .collect()
    }

    /// A reconstructed context message: `(role, content)`. The WASM adapter maps these
    /// onto WIT `message` records for `hook-output::replace-context`.
    pub type ContextMessage = (String, String);

    /// Build the replace-context seed from prior Turn records. Returns a single `user`
    /// framing message whose body is the verbatim concatenation of prior Turns, so the
    /// new task is seeded with the log's contents. Returns an empty vector when there
    /// are no prior Turns (e.g. the first task in a fresh capsule) so the caller can
    /// emit `hook-output::none` instead of an empty replacement.
    ///
    /// This is deliberately a plain concatenation, not a summary — selection and
    /// curation of which Turns to include is out of scope for this slice.
    pub fn reload_context(path: &Path) -> Vec<ContextMessage> {
        let lines: Vec<String> = read_records(path)
            .iter()
            .filter_map(format_turn)
            .collect();
        if lines.is_empty() {
            return Vec::new();
        }
        let header = format!(
            "Memory Log — {} prior turn(s) recorded in this capsule's working history:\n\n{}",
            lines.len(),
            lines.join("\n")
        );
        vec![("user".to_string(), header)]
    }

    /// Render one Turn record as a single reload line; non-Turn records contribute
    /// nothing to the reconstructed context.
    fn format_turn(record: &LogRecord) -> Option<String> {
        match record {
            LogRecord::Turn {
                turn,
                decision,
                tool_name,
                output,
                ..
            } => {
                let tool = tool_name
                    .as_deref()
                    .map(|t| format!(" tool={t}"))
                    .unwrap_or_default();
                let body = output.as_deref().unwrap_or("");
                Some(format!("[turn {turn} · {decision}{tool}] {body}"))
            }
            _ => None,
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use crate::memlog::{self, LogRecord};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, Message, SessionContext,
        SessionEndEvent, ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    };

    pub struct MemoryLog;

    /// Resolve the log path from the WASI environment (`MURMUR_MEMORY_LOG_PATH`),
    /// falling back to the default workdir JSONL file.
    fn log_path() -> std::path::PathBuf {
        let env_value = std::env::var(memlog::LOG_PATH_ENV).ok();
        memlog::resolve_log_path(env_value.as_deref())
    }

    impl Guest for MemoryLog {
        // ── The three bound events ──────────────────────────────────────────────

        fn on_task_start(event: TaskStartEvent) -> Result<HookOutput, String> {
            let path = log_path();

            // Reload prior Turns *before* recording this task's open marker so the
            // seed reflects only earlier tasks' history.
            let seed = memlog::reload_context(&path);

            memlog::append_record(
                &path,
                &LogRecord::TaskOpen {
                    task_id: event.task_id,
                    context_id: event.context_id,
                    source: event.source,
                },
            )?;

            if seed.is_empty() {
                Ok(HookOutput::None)
            } else {
                let messages = seed
                    .into_iter()
                    .map(|(role, content)| Message { role, content })
                    .collect();
                Ok(HookOutput::ReplaceContext(messages))
            }
        }

        fn on_inference(event: InferenceEvent) -> Result<HookOutput, String> {
            memlog::append_record(
                &log_path(),
                &LogRecord::Turn {
                    turn: event.turn,
                    decision: event.decision,
                    tool_name: event.tool_name,
                    output: event.output,
                    input_tokens: event.input_tokens,
                    output_tokens: event.output_tokens,
                },
            )?;
            Ok(HookOutput::None)
        }

        fn on_task_end(event: TaskEndEvent) -> Result<HookOutput, String> {
            memlog::append_record(
                &log_path(),
                &LogRecord::TaskClose {
                    task_id: event.task_id,
                    exit_status: event.exit_status,
                },
            )?;
            Ok(HookOutput::None)
        }

        // ── Unbound events — the Memory Log ignores them ────────────────────────

        fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_tool_call(_event: ToolEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_shell(_event: ShellEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    export!(MemoryLog);
}

#[cfg(test)]
mod tests {
    use crate::memlog::{
        append_record, read_records, reload_context, resolve_log_path, LogRecord, DEFAULT_LOG_PATH,
    };
    use tempfile::tempdir;

    fn turn(n: u32, decision: &str, tool: Option<&str>, output: Option<&str>) -> LogRecord {
        LogRecord::Turn {
            turn: n,
            decision: decision.to_string(),
            tool_name: tool.map(str::to_string),
            output: output.map(str::to_string),
            input_tokens: 100 + u64::from(n),
            output_tokens: 20 + u64::from(n),
        }
    }

    // Evidence: on-task-start correctly reloads and seeds context from an existing log.
    #[test]
    fn on_task_start_reloads_and_seeds_context() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory-log.jsonl");

        // A prior task's history already on disk.
        append_record(
            &path,
            &LogRecord::TaskOpen {
                task_id: "task-1".into(),
                context_id: "ctx-1".into(),
                source: "task_md".into(),
            },
        )
        .unwrap();
        append_record(&path, &turn(1, "tool_call", Some("murmur-tool-editor"), Some("edited main.rs"))).unwrap();
        append_record(&path, &turn(2, "final", None, Some("all tests pass"))).unwrap();
        append_record(
            &path,
            &LogRecord::TaskClose {
                task_id: "task-1".into(),
                exit_status: "ok".into(),
            },
        )
        .unwrap();

        let seed = reload_context(&path);
        assert_eq!(seed.len(), 1, "reload collapses prior turns into one framing message");
        let (role, content) = &seed[0];
        assert_eq!(role, "user");
        // Both prior turns' verbatim content is present, in order.
        assert!(content.contains("2 prior turn(s)"), "reports the turn count: {content}");
        assert!(content.contains("edited main.rs"));
        assert!(content.contains("all tests pass"));
        assert!(content.contains("tool=murmur-tool-editor"));
        assert!(
            content.find("edited main.rs").unwrap() < content.find("all tests pass").unwrap(),
            "turns preserved in log order"
        );
        // Markers do not leak into the seed.
        assert!(!content.contains("task_close"));
    }

    // Sanity: reloading a fresh/empty capsule yields no seed (so the hook emits `none`,
    // not an empty replacement). Covers the coinciding none/single first-task case.
    #[test]
    fn on_task_start_on_empty_log_yields_no_seed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory-log.jsonl");
        assert!(reload_context(&path).is_empty(), "no file → empty seed");

        // A log that exists but has only markers (no turns yet) also seeds nothing.
        append_record(
            &path,
            &LogRecord::TaskOpen {
                task_id: "t".into(),
                context_id: "c".into(),
                source: "task_md".into(),
            },
        )
        .unwrap();
        assert!(reload_context(&path).is_empty(), "markers alone → empty seed");
    }

    // Evidence: on-inference appends a Turn record with the expected structure.
    #[test]
    fn on_inference_appends_turn_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory-log.jsonl");

        append_record(&path, &turn(1, "tool_call", Some("murmur-tool-git"), Some("committed"))).unwrap();

        let records = read_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0],
            LogRecord::Turn {
                turn: 1,
                decision: "tool_call".into(),
                tool_name: Some("murmur-tool-git".into()),
                output: Some("committed".into()),
                input_tokens: 101,
                output_tokens: 21,
            }
        );

        // The on-disk line is real JSONL: a single line tagged `turn`.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(value["kind"], "turn");
        assert_eq!(value["turn"], 1);
        assert_eq!(value["tool_name"], "murmur-tool-git");
    }

    // A turn with no tool / no output omits those fields rather than emitting nulls.
    #[test]
    fn turn_record_omits_absent_optional_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory-log.jsonl");
        append_record(&path, &turn(3, "final", None, None)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert!(value.get("tool_name").is_none(), "absent tool_name omitted");
        assert!(value.get("output").is_none(), "absent output omitted");
        // Round-trips back to the same record.
        assert_eq!(read_records(&path), vec![turn(3, "final", None, None)]);
    }

    // Evidence: on-task-end writes a close-out marker.
    #[test]
    fn on_task_end_writes_close_out_marker() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory-log.jsonl");

        append_record(&path, &turn(1, "final", None, Some("done"))).unwrap();
        append_record(
            &path,
            &LogRecord::TaskClose {
                task_id: "task-42".into(),
                exit_status: "ok".into(),
            },
        )
        .unwrap();

        let records = read_records(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[1],
            LogRecord::TaskClose {
                task_id: "task-42".into(),
                exit_status: "ok".into(),
            }
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        let last: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(last["kind"], "task_close");
        assert_eq!(last["exit_status"], "ok");
    }

    // Evidence: the log persists and continues appending across a simulated resumed
    // launch — it is not reset. Unbounded growth is the intended behavior.
    #[test]
    fn log_persists_and_appends_across_resumed_launch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory-log.jsonl");

        // ── Launch 1 ──
        append_record(
            &path,
            &LogRecord::TaskOpen { task_id: "t1".into(), context_id: "c1".into(), source: "task_md".into() },
        )
        .unwrap();
        append_record(&path, &turn(1, "final", None, Some("launch-1 turn"))).unwrap();
        append_record(&path, &LogRecord::TaskClose { task_id: "t1".into(), exit_status: "ok".into() }).unwrap();
        let after_first = read_records(&path).len();
        assert_eq!(after_first, 3);

        // ── Launch 2 — same workdir/path, no truncation between launches ──
        append_record(
            &path,
            &LogRecord::TaskOpen { task_id: "t2".into(), context_id: "c2".into(), source: "a2a".into() },
        )
        .unwrap();
        append_record(&path, &turn(1, "final", None, Some("launch-2 turn"))).unwrap();

        let all = read_records(&path);
        assert_eq!(all.len(), 5, "launch 2 appended to launch 1's log, not a fresh file");
        // Launch 1's content is still first, launch 2's content follows.
        assert_eq!(all[1], turn(1, "final", None, Some("launch-1 turn")));
        assert_eq!(all[4], turn(1, "final", None, Some("launch-2 turn")));

        // A reload on the resumed launch sees both launches' turns.
        let seed = reload_context(&path);
        let (_, content) = &seed[0];
        assert!(content.contains("launch-1 turn"));
        assert!(content.contains("launch-2 turn"));
        assert!(content.contains("2 prior turn(s)"));
    }

    // Evidence: manifest-declared alternate storage is honored — the write target is
    // never hardcoded to the default.
    #[test]
    fn manifest_declared_alternate_storage_is_honored() {
        // Resolution: a non-blank override wins; blank/absent falls back to the default.
        assert_eq!(resolve_log_path(Some("custom/mem.jsonl")).to_str().unwrap(), "custom/mem.jsonl");
        assert_eq!(resolve_log_path(None).to_str().unwrap(), DEFAULT_LOG_PATH);
        assert_eq!(resolve_log_path(Some("   ")).to_str().unwrap(), DEFAULT_LOG_PATH);

        // End-to-end: writing via an override path lands the file at the override
        // location (creating parent dirs), and the default path is not touched.
        let dir = tempdir().unwrap();
        let override_path = dir.path().join("alt").join("store").join("mem.jsonl");
        let resolved = resolve_log_path(Some(override_path.to_str().unwrap()));
        append_record(&resolved, &turn(1, "final", None, Some("stored in override"))).unwrap();

        assert!(override_path.exists(), "override path was written");
        assert!(!dir.path().join(DEFAULT_LOG_PATH).exists(), "default path untouched");
        assert_eq!(read_records(&override_path).len(), 1);
    }
}
