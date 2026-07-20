//! Structured file editor tool, packaged as a `wasm32-wasip2` component exporting
//! `murmur:tool/run` (world `tool`).
//!
//! The dispatch logic (operation parsing, the on-disk read cache, the file operations,
//! and the old-protocol output envelope) is deliberately split into a `cfg`-independent
//! [`logic`] module so it can be unit-tested on the host with `cargo test` — exactly the
//! split every hook crate uses (see `hooks/murmur-hook-compact/src/lib.rs`). The
//! `wasm_tool` module (compiled only for `wasm32`) is a thin adapter: it rewraps the
//! `murmur:tool/run` `ToolInput` into the stdin-envelope shape [`logic::run`] already
//! parses, then maps the returned old-protocol JSON `Value` back to a WIT `ToolResult`.

// ── Pure, host-testable dispatch logic (no WASM bindings, no `cfg`) ────────────
pub mod logic {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use regex::Regex;
    use serde_json::{json, Value};
    use walkdir::WalkDir;

    // ── Configuration ──────────────────────────────────────────────────────────

    const FIND_RESULT_SIZE_LIMIT: usize = 500 * 1024; // 500 KB ceiling for find_in_files output

    // On-disk read-cache location, relative to the capsule workdir (the component's CWD /
    // preopened `.` at dispatch time). A plain relative path is how sibling artifacts scope
    // per-session state to the workdir — `murmur-hook-compact` writes `checkpoints/`, and
    // `murmur-hook-memory-jsonl` writes `memory-log.jsonl` the same way — so the cache is
    // automatically isolated per session/capsule and never leaks across unrelated ones.
    // The location is overridable via `MURMUR_TOOL_EDITOR_CACHE_DIR`, mirroring the
    // manifest-driven WASI-env override pattern already used by `murmur-hook-grafana`
    // (`MURMUR_OTEL_ENDPOINT`), `murmur-hook-eval` (`MURMUR_EVAL_CONFIG`), and
    // `murmur-hook-memory-jsonl` (`MURMUR_MEMORY_LOG_PATH`).
    const CACHE_DIR_ENV: &str = "MURMUR_TOOL_EDITOR_CACHE_DIR";
    const DEFAULT_CACHE_DIR: &str = ".murmur-tool-editor-cache";

    // Bound on the number of on-disk cache entries. Each entry is a small (~100-byte) JSON
    // file, so 1024 entries cap the cache at roughly a hundred KB. When the bound is reached
    // we evict oldest-by-file-mtime entries before writing a new one. A hard bound (rather
    // than unbounded growth) keeps a long-running session's workdir from accumulating a
    // stale pointer for every file ever read; the cache is best-effort, so eviction only
    // costs an occasional re-read of a long-untouched file.
    const MAX_CACHE_ENTRIES: usize = 1024;

    // ── Error kind constants ────────────────────────────────────────────────────

    mod err {
        pub const NOT_FOUND: &str = "not_found";
        pub const PERMISSION_DENIED: &str = "permission_denied";
        pub const IO_ERROR: &str = "io_error";
        pub const STRING_NOT_FOUND: &str = "string_not_found";
        pub const INVALID_PATTERN: &str = "invalid_pattern";
        pub const SEARCH_TOO_BROAD: &str = "search_too_broad";
        pub const RESULT_SIZE_EXCEEDED: &str = "result_size_exceeded";
    }

    // ── Read cache: keyed by (path, byte_range, mtime), persisted on disk ────────
    //
    // The tool is a one-shot dispatch — one operation per component instantiation — so an
    // in-memory cache could never see a second `read_file` call. The cache therefore lives
    // on disk in the workdir, keyed by (path, byte_range, mtime), so a *later* invocation
    // against an unchanged file returns a `cache_ref` pointer instead of re-transmitting the
    // content.
    //
    // Retrieval is keyed by (path, byte_range, mtime) directly; the generated `cache_id`
    // (`content.len() ^ mtime`) is only an opaque label returned to the caller and is never
    // used as a lookup key, so its collision-proneness is not exploitable.

    #[derive(Clone, Copy)]
    struct ByteRange {
        start: Option<usize>,
        end: Option<usize>,
    }

    impl ByteRange {
        fn whole_file() -> Self {
            ByteRange { start: None, end: None }
        }
    }

    // Resolve the cache directory: the `MURMUR_TOOL_EDITOR_CACHE_DIR` override wins when set
    // and non-empty; otherwise the default workdir-relative directory is used.
    fn resolve_cache_dir() -> PathBuf {
        match std::env::var(CACHE_DIR_ENV) {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => PathBuf::from(DEFAULT_CACHE_DIR),
        }
    }

    // FNV-1a 64-bit hash — stable and dependency-free — used only to derive a cache filename
    // from the lookup key. Hash collisions are harmless: each entry stores its full key and
    // is re-validated on read, so a colliding lookup is simply treated as a miss.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash
    }

    fn cache_key_string(path: &str, range: ByteRange, mtime: u64) -> String {
        let start = range.start.map(|n| n.to_string()).unwrap_or_default();
        let end = range.end.map(|n| n.to_string()).unwrap_or_default();
        // NUL separators can't appear in any component, so the encoding is unambiguous.
        format!("{path}\u{0}{start}\u{0}{end}\u{0}{mtime}")
    }

    fn cache_entry_path(cache_dir: &Path, key: &str) -> PathBuf {
        cache_dir.join(format!("{:016x}.json", fnv1a(key.as_bytes())))
    }

    // A per-write unique-ish token for the atomic-publish temp filename. The native binary
    // used `std::process::id()` for this; on `wasm32-wasip2` that call *traps* (it is not a
    // supported syscall under the sandboxed component model), which would abort every
    // cache-miss `read_file`. So we derive uniqueness portably instead: a process-wide
    // monotonic counter (unique across writes within one instantiation) mixed with a
    // `RandomState` seed (backed by `wasi:random` under wasip2, OS entropy on the host, so
    // it differs across instantiations). This changes only the temp-name source — the
    // write-temp-then-atomic-rename publish story is unchanged.
    fn unique_token() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        seed ^ COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    // Look up a cached cache_id for this exact key. Returns None on a miss, on a corrupt or
    // partially-written entry file (fails to parse), or on a hash collision (stored key does
    // not match) — all of which the caller safely handles as a cache miss.
    fn cache_lookup(cache_dir: &Path, key: &str) -> Option<String> {
        let raw = std::fs::read_to_string(cache_entry_path(cache_dir, key)).ok()?;
        let v: Value = serde_json::from_str(&raw).ok()?;
        if v.get("key").and_then(Value::as_str) == Some(key) {
            v.get("cache_id").and_then(Value::as_str).map(str::to_string)
        } else {
            None
        }
    }

    // Persist a cache entry. Best-effort: any I/O failure just means the next read re-reads.
    fn cache_store(cache_dir: &Path, key: &str, cache_id: &str) {
        if std::fs::create_dir_all(cache_dir).is_err() {
            return;
        }
        evict_if_needed(cache_dir);

        let payload = json!({ "key": key, "cache_id": cache_id }).to_string();

        // Atomic publish: write to a unique temp file, then rename into place. Two
        // invocations racing to cache the same key produce identical payloads (cache_id is a
        // deterministic function of content length and mtime), so last-writer-wins is safe,
        // and a reader never observes a torn file because rename is atomic on POSIX. The
        // temp name mixes a portable per-write token (see `unique_token`) with the key hash.
        let tmp = cache_dir.join(format!(
            ".tmp-{:016x}-{:016x}",
            unique_token(),
            fnv1a(key.as_bytes())
        ));
        if std::fs::write(&tmp, payload).is_ok() {
            let _ = std::fs::rename(&tmp, cache_entry_path(cache_dir, key));
        }
    }

    // Evict oldest-by-mtime entries when the cache is at capacity, leaving room for one more.
    fn evict_if_needed(cache_dir: &Path) {
        let entries: Vec<PathBuf> = match std::fs::read_dir(cache_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
                .collect(),
            Err(_) => return,
        };
        if entries.len() < MAX_CACHE_ENTRIES {
            return;
        }
        let mut by_mtime: Vec<(std::time::SystemTime, PathBuf)> = entries
            .into_iter()
            .filter_map(|p| {
                let mt = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                Some((mt, p))
            })
            .collect();
        by_mtime.sort_by_key(|(mt, _)| *mt);
        let remove_count = by_mtime.len().saturating_sub(MAX_CACHE_ENTRIES - 1);
        for (_, p) in by_mtime.into_iter().take(remove_count) {
            let _ = std::fs::remove_file(p);
        }
    }

    // ── Dispatch entry point ────────────────────────────────────────────────────

    /// Parse a stdin-envelope string `{"data": <string-or-object>, "log_path": ...}`,
    /// dispatch on `data.operation`, and return the old-protocol result `Value`. This is
    /// the exact contract the native binary's `main` fed from stdin; the `wasm_tool`
    /// adapter reconstructs the same envelope from the WIT `ToolInput` so behavior is
    /// identical across the port.
    pub fn run(raw: &str) -> Value {
        if raw.trim().is_empty() {
            return fail_msg("missing input on stdin");
        }

        let envelope: Value = match serde_json::from_str(raw) {
            Ok(e) => e,
            Err(e) => return fail_msg(format!("invalid stdin JSON: {e}")),
        };

        let data_value = match envelope.get("data") {
            None | Some(Value::Null) => return fail_msg("missing data field"),
            Some(v) => v.clone(),
        };

        // data may be a JSON-encoded string (double-encoded) or a JSON object directly
        let op: Value = match &data_value {
            Value::String(s) => match serde_json::from_str(s) {
                Ok(v) => v,
                Err(e) => return fail_msg(format!("invalid data JSON string: {e}")),
            },
            Value::Object(_) => data_value.clone(),
            _ => return fail_msg("data must be a JSON string or object"),
        };

        let operation = op.get("operation").and_then(|v| v.as_str()).unwrap_or("");

        // Declare each operation's effect on the resource it addressed via the runtime's
        // reserved `state_effect` metadata key (see the host's wit/tool.wit). This is what
        // lets `mur trace` redundant-call detection reason about these operations without
        // hardcoding any of their names. Only successful calls declare an effect — a failed
        // read did not read, and a failed write did not mutate, so those stay undeclared.
        match operation {
            "read_file" => with_state_effect(op_read_file(&op), "read"),
            "write_file" => with_state_effect(op_write_file(&op), "mutate"),
            "replace_in_file" => with_state_effect(op_replace_in_file(&op), "mutate"),
            "find_in_files" => with_state_effect(op_find_in_files(&op), "read"),
            other => fail_msg(format!("unknown operation: {other}")),
        }
    }

    /// Attach the reserved `state_effect` metadata key to a successful result. Failures are
    /// left untouched (metadata stays `null`), so a call that did not complete declares no
    /// effect.
    fn with_state_effect(mut result: Value, effect: &str) -> Value {
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            result["metadata"] = json!({ "state_effect": effect });
        }
        result
    }

    // ── FILE operations ─────────────────────────────────────────────────────────

    fn op_read_file(op: &Value) -> Value {
        let path = match op.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return fail_msg("missing required field: path"),
        };

        // Get file metadata for mtime
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return err_result(io_error_kind(&e), format!("{path}: {e}")),
        };

        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let byte_range = ByteRange::whole_file();
        let cache_dir = resolve_cache_dir();
        let key = cache_key_string(&path, byte_range, mtime);

        // Cache hit: the file is unchanged since a prior invocation cached it, so return the
        // pointer only and skip re-transmitting the content.
        if let Some(cache_id) = cache_lookup(&cache_dir, &key) {
            return ok_with(
                format!("read {path} (cached)"),
                json!({ "cache_ref": cache_id }),
                format!("cache hit: {cache_id}"),
            );
        }

        // Cache miss: read the file, persist the pointer for the next invocation.
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let byte_count = content.len();
                let cache_id = format!("cache_{:x}", content.len() ^ (mtime as usize));

                cache_store(&cache_dir, &key, &cache_id);

                ok_with(
                    format!("read {path}"),
                    json!({ "content": content, "cache_ref": cache_id }),
                    format!("{byte_count} bytes"),
                )
            }
            Err(e) => err_result(io_error_kind(&e), format!("{path}: {e}")),
        }
    }

    fn op_write_file(op: &Value) -> Value {
        let path = match op.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return fail_msg("missing required field: path"),
        };
        let content = match op.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return fail_msg("missing required field: content"),
        };

        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return err_result(io_error_kind(&e), format!("failed to create directories for {path}: {e}"));
                }
            }
        }

        let byte_count = content.len();
        match std::fs::write(&path, content) {
            Ok(()) => ok_summary_only(format!("wrote {path}"), format!("{byte_count} bytes written")),
            Err(e) => err_result(io_error_kind(&e), format!("{path}: {e}")),
        }
    }

    fn op_replace_in_file(op: &Value) -> Value {
        let path = match op.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return fail_msg("missing required field: path"),
        };
        let old_string = match op.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return fail_msg("missing required field: old_string"),
        };
        let new_string = match op.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return fail_msg("missing required field: new_string"),
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return err_result(io_error_kind(&e), format!("{path}: {e}")),
        };

        // Count BEFORE replacing; bail out without writing if absent.
        let count = contents.matches(old_string.as_str()).count();
        if count == 0 {
            return err_result(
                err::STRING_NOT_FOUND,
                format!("old_string not found in {path}"),
            );
        }

        let new_contents = contents.replace(old_string.as_str(), new_string.as_str());

        match std::fs::write(&path, new_contents) {
            Ok(()) => ok_with(
                format!("{count} replacement(s) in {path}"),
                json!({ "count": count }),
                format!("{count} replacements"),
            ),
            Err(e) => err_result(io_error_kind(&e), format!("{path}: {e}")),
        }
    }

    fn op_find_in_files(op: &Value) -> Value {
        let pattern = match op.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return fail_msg("missing required field: pattern"),
        };
        // Distinguish an absent `dir` field (missing required field) from a present-but-empty
        // one. An empty string, ".", or "./" all resolve to "no scope narrower than repo
        // root" and must be rejected identically to an explicit repo-root search.
        let dir = match op.get("dir").and_then(|v| v.as_str()) {
            Some(d) => d.to_string(),
            None => return fail_msg("missing required field: dir"),
        };
        let recursive = op.get("recursive").and_then(|v| v.as_bool()).unwrap_or(true);

        // Scope check: reject any input that does not narrow the search below repo root.
        let scope = dir.trim();
        if scope.is_empty() || scope == "." || scope == "./" {
            return err_result(
                err::SEARCH_TOO_BROAD,
                "find_in_files requires a specific subdirectory scope, not repo root. Provide a more specific path.",
            );
        }

        let re = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(e) => {
                return err_result(
                    err::INVALID_PATTERN,
                    format!("invalid regex '{pattern}': {e}"),
                )
            }
        };

        let file_paths: Vec<std::path::PathBuf> = if recursive {
            WalkDir::new(&dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect()
        } else {
            match std::fs::read_dir(&dir) {
                Ok(entries) => entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .map(|e| e.path())
                    .collect(),
                Err(e) => return err_result(io_error_kind(&e), format!("{dir}: {e}")),
            }
        };

        let mut matches: Vec<Value> = Vec::new();
        let mut files_matched = HashSet::new();
        // Running lower bound on the final serialized output. `ok_with` emits each match
        // object twice — nested under `data.matches` and flattened at the top-level
        // `matches` field — so every match contributes ~2x its serialized length to the
        // real payload. The original code counted it once, under-bounding the true output by
        // roughly half. This early-exit guard bounds memory on pathological inputs; the
        // authoritative ceiling check below measures the exact bytes that go out the door.
        let mut approx_out_size: usize = 0;

        for file_path in file_paths {
            let contents = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => continue, // skip binary or unreadable files
            };

            let relative = file_path
                .strip_prefix(&dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| file_path.to_string_lossy().to_string());
            // Normalize away leading slash that strip_prefix can produce on some paths.
            let relative = relative.trim_start_matches('/').to_string();

            for (idx, line) in contents.lines().enumerate() {
                if re.is_match(line) {
                    let match_obj = json!({
                        "path": relative,
                        "line": idx + 1,
                        "text": line,
                    });
                    approx_out_size += 2 * serde_json::to_string(&match_obj).unwrap_or_default().len();
                    if approx_out_size > FIND_RESULT_SIZE_LIMIT {
                        return err_result(err::RESULT_SIZE_EXCEEDED, size_exceeded_message());
                    }

                    matches.push(match_obj);
                    files_matched.insert(relative.clone());
                }
            }
        }

        let match_count = matches.len();
        let file_count = files_matched.len();
        let summary = format!("{match_count} matches in {file_count} files");
        let result = ok_with(summary.clone(), json!({ "matches": matches }), summary);

        // Authoritative ceiling: measure the actual serialized output that the tool will
        // return, not an intermediate representation. This catches boundary cases where the
        // running lower bound stayed under the limit but the full envelope (both copies of
        // the matches array plus the wrapper fields) tips it over.
        let serialized_len = serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0);
        if serialized_len > FIND_RESULT_SIZE_LIMIT {
            return err_result(err::RESULT_SIZE_EXCEEDED, size_exceeded_message());
        }

        result
    }

    fn size_exceeded_message() -> String {
        format!(
            "search result would exceed size limit of {FIND_RESULT_SIZE_LIMIT} bytes. Try a more specific pattern or narrower scope."
        )
    }

    // ── I/O error mapping ───────────────────────────────────────────────────────

    fn io_error_kind(e: &std::io::Error) -> &'static str {
        match e.kind() {
            std::io::ErrorKind::NotFound => err::NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => err::PERMISSION_DENIED,
            _ => err::IO_ERROR,
        }
    }

    // ── Output constructors ─────────────────────────────────────────────────────
    //
    // Mirrors the git-tool protocol so the capsule runtime can extract data/summary.
    // Fields: ok, message, status, summary, data, data_path, metadata.
    // Error results additionally carry error_kind at the top level.

    fn ok_with(message: impl Into<String>, data: Value, summary: impl Into<String>) -> Value {
        let msg = message.into();
        let sum = summary.into();
        let data_clone = data.clone();
        let mut obj = json!({
            "ok": true,
            "message": &msg,
            "status": "passed",
            "summary": &sum,
            "data": data_clone,
            "data_path": null,
            "metadata": null,
        });
        // Flatten data fields at the top level for new-protocol callers.
        if let Value::Object(map) = data {
            for (k, v) in map {
                obj[k] = v;
            }
        }
        obj
    }

    fn ok_summary_only(message: impl Into<String>, summary: impl Into<String>) -> Value {
        let msg = message.into();
        let sum = summary.into();
        json!({
            "ok": true,
            "message": &msg,
            "status": "passed",
            "summary": &sum,
            "data": null,
            "data_path": null,
            "metadata": null,
        })
    }

    fn fail_msg(message: impl Into<String>) -> Value {
        let msg = message.into();
        json!({
            "ok": false,
            "message": &msg,
            "status": "error",
            "summary": &msg,
            "data": null,
            "data_path": null,
            "metadata": null,
        })
    }

    fn err_result(error_kind: &str, message: impl Into<String>) -> Value {
        let msg = message.into();
        json!({
            "ok": false,
            "error_kind": error_kind,
            "message": &msg,
            "status": "error",
            "summary": &msg,
            "data": null,
            "data_path": null,
            "metadata": null,
        })
    }

    // ── Unit tests ──────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        #[test]
        fn fail_msg_returns_ok_false() {
            let out = fail_msg("something went wrong");
            assert_eq!(out["ok"], false);
            assert_eq!(out["message"], "something went wrong");
        }

        #[test]
        fn err_result_includes_error_kind() {
            let out = err_result(err::NOT_FOUND, "file.txt: not found");
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::NOT_FOUND);
            assert_eq!(out["message"], "file.txt: not found");
        }

        #[test]
        fn err_constants_are_distinct() {
            let kinds = [
                err::NOT_FOUND,
                err::PERMISSION_DENIED,
                err::IO_ERROR,
                err::STRING_NOT_FOUND,
                err::INVALID_PATTERN,
                err::SEARCH_TOO_BROAD,
                err::RESULT_SIZE_EXCEEDED,
            ];
            for (i, a) in kinds.iter().enumerate() {
                for (j, b) in kinds.iter().enumerate() {
                    if i != j {
                        assert_ne!(a, b, "error kind constants must be unique");
                    }
                }
            }
        }

        #[test]
        fn ok_with_flattens_data_fields() {
            let out = ok_with("done", json!({ "count": 3 }), "3 replacements");
            assert_eq!(out["ok"], true);
            assert_eq!(out["message"], "done");
            assert_eq!(out["summary"], "3 replacements");
            assert_eq!(out["data"]["count"], 3);
            assert_eq!(out["count"], 3); // flattened
        }

        #[test]
        fn run_returns_error_for_empty_input() {
            let out = run("");
            assert_eq!(out["ok"], false);
            assert!(out["message"].as_str().unwrap().contains("missing input"));
        }

        #[test]
        fn run_returns_error_for_unknown_operation() {
            let input = r#"{"data":{"operation":"bogus_op_xyz"}}"#;
            let out = run(input);
            assert_eq!(out["ok"], false);
            assert!(out["message"].as_str().unwrap().contains("unknown operation"));
        }

        #[test]
        fn with_state_effect_declares_on_success_only() {
            let ok = with_state_effect(ok_summary_only("done", "done"), "mutate");
            assert_eq!(ok["metadata"]["state_effect"], "mutate");

            let failed = with_state_effect(fail_msg("nope"), "mutate");
            assert!(
                failed["metadata"].is_null(),
                "a failed op must not declare a state effect"
            );
        }

        #[test]
        fn read_file_declares_read_effect() {
            let dir = std::env::temp_dir().join("murmur_editor_state_effect_read");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("hello.txt");
            fs::write(&path, "hi\n").unwrap();
            let envelope = json!({
                "data": { "operation": "read_file", "path": path.to_str().unwrap() },
                "log_path": null,
            });
            let out = run(&envelope.to_string());
            assert_eq!(out["ok"], true, "read should succeed: {out:?}");
            assert_eq!(out["metadata"]["state_effect"], "read");
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn write_file_declares_mutate_effect() {
            let dir = std::env::temp_dir().join("murmur_editor_state_effect_write");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("out.txt");
            let envelope = json!({
                "data": { "operation": "write_file", "path": path.to_str().unwrap(), "content": "x" },
                "log_path": null,
            });
            let out = run(&envelope.to_string());
            assert_eq!(out["ok"], true, "write should succeed: {out:?}");
            assert_eq!(out["metadata"]["state_effect"], "mutate");
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn run_handles_double_encoded_data() {
            let inner = r#"{"operation":"bogus_double_enc"}"#;
            let envelope = format!(
                r#"{{"data":"{}","log_path":null}}"#,
                inner.replace('"', "\\\"")
            );
            let out = run(&envelope);
            assert_eq!(out["ok"], false);
            assert!(out["message"].as_str().unwrap().contains("unknown operation"));
        }

        // ── Scoped search tests ─────────────────────────────────────────────────

        #[test]
        fn find_in_files_rejects_repo_root_dot() {
            let op = json!({
                "operation": "find_in_files",
                "pattern": "test",
                "dir": ".",
                "recursive": true,
            });
            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::SEARCH_TOO_BROAD);
            assert!(out["message"]
                .as_str()
                .unwrap()
                .contains("specific subdirectory"));
        }

        #[test]
        fn find_in_files_rejects_empty_dir() {
            // Regression: dir="" means "no scope narrower than repo root", exactly like
            // dir=".", so it must return error_kind=search_too_broad — not a bare
            // missing-field message. The old test only checked ok==false and so passed even
            // while the branch returned the wrong error kind.
            let op = json!({
                "operation": "find_in_files",
                "pattern": "test",
                "dir": "",
                "recursive": true,
            });
            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::SEARCH_TOO_BROAD);
        }

        #[test]
        fn find_in_files_rejects_dot_slash_dir() {
            let op = json!({
                "operation": "find_in_files",
                "pattern": "test",
                "dir": "./",
                "recursive": true,
            });
            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::SEARCH_TOO_BROAD);
        }

        #[test]
        fn find_in_files_absent_dir_is_missing_field() {
            // An absent dir field is distinct from an empty one: it is a missing required
            // field, not a too-broad scope.
            let op = json!({
                "operation": "find_in_files",
                "pattern": "test",
            });
            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], false);
            assert!(out["error_kind"].is_null());
            assert!(out["message"].as_str().unwrap().contains("missing required field"));
        }

        #[test]
        fn find_in_files_enforces_size_ceiling() {
            // This fixture is sized to catch the ~2x accounting bug specifically: the sum of
            // the individual match objects (the quantity the OLD code counted) stays UNDER
            // the 500KB ceiling, but the real serialized output — in which `ok_with` emits
            // the matches array twice — is comfortably OVER it. The old accounting therefore
            // passed this input; correct accounting must reject it.
            let temp_dir = std::env::temp_dir().join("murmur_test_find_2x_ceiling");
            let _ = fs::remove_dir_all(&temp_dir);
            fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

            let test_file = temp_dir.join("large_match_file.txt");
            // Each line -> a match object of ~230 bytes serialized. 1400 lines -> single-count
            // sum ~320KB (< 500KB, old code passes), doubled ~640KB (> 500KB, new code fails).
            let padding = "x".repeat(200);
            let mut content = String::new();
            for i in 0..1400 {
                content.push_str(&format!("line {i} has marker {padding}\n"));
            }
            fs::write(&test_file, content).expect("failed to write test file");

            // Independently reconstruct the single-count sum the OLD code measured and assert
            // it is under the ceiling — proving this fixture would have passed the old check.
            let single_count_sum: usize = std::fs::read_to_string(&test_file)
                .unwrap()
                .lines()
                .enumerate()
                .filter(|(_, l)| l.contains("marker"))
                .map(|(idx, line)| {
                    serde_json::to_string(&json!({
                        "path": "large_match_file.txt",
                        "line": idx + 1,
                        "text": line,
                    }))
                    .unwrap()
                    .len()
                })
                .sum();
            assert!(
                single_count_sum < FIND_RESULT_SIZE_LIMIT,
                "fixture invalid: single-count sum {single_count_sum} is not below the ceiling, \
                 so it would not distinguish the old 1x accounting from correct 2x accounting"
            );

            let dir_name = temp_dir.to_string_lossy().to_string();
            let op = json!({
                "operation": "find_in_files",
                "pattern": "marker",
                "dir": &dir_name,
                "recursive": false,
            });

            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::RESULT_SIZE_EXCEEDED);
            assert!(out["message"].as_str().unwrap().contains("size limit"));

            let _ = fs::remove_dir_all(&temp_dir);
        }

        #[test]
        fn find_in_files_respects_specific_dir() {
            // This test verifies that find works with a valid specific directory
            let temp_dir = std::env::temp_dir().join("murmur_test_find_valid");
            let _ = fs::remove_dir_all(&temp_dir);
            fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

            let test_file = temp_dir.join("test.txt");
            fs::write(&test_file, "hello world\nfoo bar\n").expect("failed to write test file");

            let dir_name = temp_dir.to_string_lossy().to_string();
            let op = json!({
                "operation": "find_in_files",
                "pattern": "world",
                "dir": &dir_name,
                "recursive": false,
            });

            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], true);
            assert!(out["matches"].is_array());
            assert_eq!(out["matches"].as_array().unwrap().len(), 1);

            // Cleanup
            let _ = fs::remove_dir_all(&temp_dir);
        }

        #[test]
        fn read_file_returns_content_and_cache_ref() {
            let temp_dir = std::env::temp_dir().join("murmur_test_read");
            let _ = fs::remove_dir_all(&temp_dir);
            fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

            let test_file = temp_dir.join("test.txt");
            fs::write(&test_file, "test content").expect("failed to write test file");

            // Point the cache at a fresh temp dir so this test is isolated from any other.
            let _guard = cache_env_guard(&temp_dir.join("cache"));

            let path = test_file.to_string_lossy().to_string();
            let op = json!({
                "operation": "read_file",
                "path": &path,
            });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], true);
            assert_eq!(out["content"], "test content");
            assert!(out["cache_ref"].is_string());

            let _ = fs::remove_dir_all(&temp_dir);
        }

        // ── On-disk cache mechanism (unit-level) ────────────────────────────────
        //
        // These exercise the disk cache primitives directly with an explicit cache dir, so
        // they don't rely on process-global state. The authoritative proof that the cache
        // works the way it is actually invoked — across two separate component
        // instantiations — lives in tests/wasm_component.rs, since that is the only shape
        // that reflects the one-op-per-dispatch reality of this tool.

        #[test]
        fn cache_store_then_lookup_roundtrips() {
            let dir = std::env::temp_dir().join("murmur_test_cache_roundtrip");
            let _ = fs::remove_dir_all(&dir);
            let key = cache_key_string("some/file.rs", ByteRange::whole_file(), 12345);

            assert!(cache_lookup(&dir, &key).is_none(), "empty cache must miss");
            cache_store(&dir, &key, "cache_abc123");
            assert_eq!(cache_lookup(&dir, &key).as_deref(), Some("cache_abc123"));

            // A different key (different mtime) must miss.
            let other = cache_key_string("some/file.rs", ByteRange::whole_file(), 99999);
            assert!(cache_lookup(&dir, &other).is_none());

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn cache_lookup_treats_corrupt_entry_as_miss() {
            let dir = std::env::temp_dir().join("murmur_test_cache_corrupt");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let key = cache_key_string("x.txt", ByteRange::whole_file(), 1);

            // Write a truncated/garbage file at the entry path.
            fs::write(cache_entry_path(&dir, &key), b"{ this is not json").unwrap();
            assert!(cache_lookup(&dir, &key).is_none());

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn cache_eviction_bounds_entry_count() {
            let dir = std::env::temp_dir().join("murmur_test_cache_evict");
            let _ = fs::remove_dir_all(&dir);
            // Store more than MAX_CACHE_ENTRIES distinct keys; the directory must stay bounded.
            for i in 0..(MAX_CACHE_ENTRIES + 50) {
                let key = cache_key_string("f.txt", ByteRange::whole_file(), i as u64);
                cache_store(&dir, &key, &format!("cache_{i:x}"));
            }
            let count = fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
                .count();
            assert!(
                count <= MAX_CACHE_ENTRIES,
                "cache grew past the bound: {count} > {MAX_CACHE_ENTRIES}"
            );

            let _ = fs::remove_dir_all(&dir);
        }

        // Serializes env-var mutation across the cache-env tests so parallel tests don't race
        // on the shared process environment. Returns a guard that restores the prior value.
        static CACHE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        struct CacheEnvGuard {
            _lock: std::sync::MutexGuard<'static, ()>,
            prev: Option<String>,
        }

        impl Drop for CacheEnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var(CACHE_DIR_ENV, v),
                    None => std::env::remove_var(CACHE_DIR_ENV),
                }
            }
        }

        fn cache_env_guard(dir: &std::path::Path) -> CacheEnvGuard {
            let lock = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var(CACHE_DIR_ENV).ok();
            std::env::set_var(CACHE_DIR_ENV, dir);
            CacheEnvGuard { _lock: lock, prev }
        }

        #[test]
        fn read_file_cache_hit_returns_ref_only() {
            let temp_dir = std::env::temp_dir().join("murmur_test_read_cache");
            let _ = fs::remove_dir_all(&temp_dir);
            fs::create_dir_all(&temp_dir).expect("failed to create temp dir");
            let _guard = cache_env_guard(&temp_dir.join("cache"));

            let test_file = temp_dir.join("cached.txt");
            fs::write(&test_file, "cached content").expect("failed to write test file");

            let path = test_file.to_string_lossy().to_string();
            let op = json!({ "operation": "read_file", "path": &path });

            let out1 = op_read_file(&op);
            assert_eq!(out1["ok"], true);
            let cache_ref1 = out1["cache_ref"].as_str().unwrap().to_string();
            assert!(out1["content"].is_string());

            // Second read of the unchanged file hits the on-disk cache: ref, no content.
            let out2 = op_read_file(&op);
            assert_eq!(out2["ok"], true);
            assert_eq!(out2["cache_ref"].as_str().unwrap(), cache_ref1);
            assert!(out2["content"].is_null());

            let _ = fs::remove_dir_all(&temp_dir);
        }

        #[test]
        fn read_file_cache_miss_on_mtime_change() {
            let temp_dir = std::env::temp_dir().join("murmur_test_read_mtime");
            let _ = fs::remove_dir_all(&temp_dir);
            fs::create_dir_all(&temp_dir).expect("failed to create temp dir");
            let _guard = cache_env_guard(&temp_dir.join("cache"));

            let test_file = temp_dir.join("mtime.txt");
            fs::write(&test_file, "original content").expect("failed to write test file");

            let path = test_file.to_string_lossy().to_string();
            let op = json!({ "operation": "read_file", "path": &path });

            let out1 = op_read_file(&op);
            assert_eq!(out1["ok"], true);
            assert_eq!(out1["content"], "original content");

            // Filesystem mtime can have 1-second granularity, so wait before rewriting.
            std::thread::sleep(std::time::Duration::from_millis(1500));
            fs::write(&test_file, "modified content").expect("failed to modify test file");
            std::thread::sleep(std::time::Duration::from_millis(100));

            // The mtime changed, so the key changed: this is a miss and returns fresh content.
            let out2 = op_read_file(&op);
            assert_eq!(out2["ok"], true);
            assert_eq!(out2["content"], "modified content");

            let _ = fs::remove_dir_all(&temp_dir);
        }
    }
}

// ── WASM adapter: WIT bindings + envelope/result mapping (wasm32 only) ─────────
#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use serde_json::Value;

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The host places the tool-call arguments (the `data` field the native binary
            // read from its stdin envelope) directly in `input.data`. Rewrap them into the
            // same `{"data": ...}` envelope `logic::run` parses so the ported component
            // reproduces the native dispatch exactly — including the double-encoded-string
            // path (a JSON string value re-parses inside `logic::run`).
            let raw = match input.data.as_deref() {
                Some(d) => {
                    let data_val = serde_json::from_str::<Value>(d)
                        .unwrap_or_else(|_| Value::String(d.to_string()));
                    serde_json::json!({ "data": data_val }).to_string()
                }
                // Absent data reproduces the native "missing data field" error: an envelope
                // with no `data` key.
                None => "{}".to_string(),
            };

            let result = crate::logic::run(&raw);

            let status = match result.get("status").and_then(Value::as_str) {
                Some("passed") => Status::Passed,
                Some("failed") => Status::Failed,
                _ => Status::Error,
            };
            let summary = result
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_string);
            // Success declares its `state_effect` via the reserved metadata key; failures
            // leave metadata null and so emit an empty list.
            let metadata = result
                .get("metadata")
                .and_then(|m| m.get("state_effect"))
                .and_then(Value::as_str)
                .map(|effect| vec![("state_effect".to_string(), effect.to_string())])
                .unwrap_or_default();
            // Map `data` exactly as the host's native dispatch did from the tool's stdout
            // (crates/capsule-runtime dispatch_native_tool): the envelope's `data` *field* —
            // a string used verbatim, else a non-null value re-serialized, else None. This
            // reproduces the pre-port `ToolResult.data` byte-for-byte (e.g. read miss ->
            // `{"content":...,"cache_ref":...}`, cache hit -> `{"cache_ref":...}`,
            // write/error -> None). Status/summary/state_effect carry the rest.
            let data = result
                .get("data")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    result
                        .get("data")
                        .filter(|d| !d.is_null())
                        .map(|d| d.to_string())
                });

            ToolResult {
                status,
                summary,
                data,
                data_path: None,
                truncated: false,
                metadata,
            }
        }
    }

    export!(Component);
}
