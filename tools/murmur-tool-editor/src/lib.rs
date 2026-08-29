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

    // Upper bound on a single ranged `read_file` request, in lines. A bounded read exists to
    // let an agent inspect a function instead of a whole module, so the cap enforces by
    // construction that a ranged read is always *smaller* than the whole-file read it
    // replaces. 2000 lines is generous enough to cover any single function or type while
    // still being a small fraction of a large module.
    const MAX_READ_RANGE_LINES: usize = 2000;

    // Upper bound on `find_in_files` `context_lines`, mirroring the range a `grep -C` user
    // would realistically ask for. Beyond this the context stops being "the surrounding
    // function" and starts re-transmitting whole files, defeating the bounded-read intent.
    const MAX_CONTEXT_LINES: i64 = 20;

    // On-disk read-cache location, relative to the capsule workdir (the component's CWD /
    // preopened `.` at dispatch time). A plain relative path is how sibling artifacts scope
    // per-session state to the workdir — `murmur-hook-compact` writes `checkpoints/` the
    // same way — so the cache is automatically isolated per session/capsule and never leaks
    // across unrelated ones.
    // The location is overridable via `MURMUR_TOOL_EDITOR_CACHE_DIR`, mirroring the
    // manifest-driven WASI-env override pattern already used by `murmur-hook-grafana`
    // (`MURMUR_OTEL_ENDPOINT`) and `murmur-hook-eval` (`MURMUR_EVAL_CONFIG`).
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
        // read_file range validation: start_line < 1, an inverted span (start > end), or a
        // start_line past the end of the file. All three are caller mistakes about *which*
        // lines to read, so they share one kind; the message states which one occurred.
        pub const INVALID_RANGE: &str = "invalid_range";
        // read_file resolved span exceeds MAX_READ_RANGE_LINES — a valid but too-wide range.
        pub const RANGE_TOO_LARGE: &str = "range_too_large";
        // find_in_files context_lines is negative or exceeds MAX_CONTEXT_LINES.
        pub const CONTEXT_TOO_LARGE: &str = "context_too_large";
    }

    // ── Read cache: keyed by (path, line_range, mtime), persisted on disk ────────
    //
    // The tool is a one-shot dispatch — one operation per component instantiation — so an
    // in-memory cache could never see a second `read_file` call. The cache therefore lives
    // on disk in the workdir, keyed by (path, line_range, mtime), so a *later* invocation
    // against an unchanged file returns a `cache_ref` pointer instead of re-transmitting the
    // content. A whole-file read keys on `LineRange::whole_file()` (both fields `None`), so
    // ranged reads of the same file cache under distinct keys and never collide with the
    // whole-file entry or each other.
    //
    // Retrieval is keyed by (path, line_range, mtime) directly; the generated `cache_id`
    // (`content.len() ^ mtime`) is only an opaque label returned to the caller and is never
    // used as a lookup key, so its collision-proneness is not exploitable.

    // A resolved 1-based, inclusive line span, or the whole file when both fields are `None`.
    // (Historically named `ByteRange`; its two `Option<usize>` fields now carry line numbers,
    // which is all the cache key ever needs.)
    #[derive(Clone, Copy)]
    struct LineRange {
        start: Option<usize>,
        end: Option<usize>,
    }

    impl LineRange {
        fn whole_file() -> Self {
            LineRange { start: None, end: None }
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

    fn cache_key_string(path: &str, range: LineRange, mtime: u64) -> String {
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

        let cache_dir = resolve_cache_dir();

        // Optional 1-based, inclusive line bounds. Reading through `as_i64` (not `as_u64`) is
        // deliberate: a negative or zero `start_line` must surface as an INVALID_RANGE error,
        // not be silently dropped as "absent". A field present as anything non-integer (or
        // JSON null) is treated as absent.
        let start_arg = op.get("start_line").and_then(Value::as_i64);
        let end_arg = op.get("end_line").and_then(Value::as_i64);

        // Whole-file path: byte-for-byte the pre-slice behavior and the pre-slice cache key,
        // so cache entries written before this feature keep hitting. The only addition is the
        // `total_lines` field on a cache miss (additive — omitted on a hit, where the file is
        // not read), letting an agent learn a file's size for a follow-up ranged read.
        if start_arg.is_none() && end_arg.is_none() {
            let key = cache_key_string(&path, LineRange::whole_file(), mtime);

            if let Some(cache_id) = cache_lookup(&cache_dir, &key) {
                return ok_with(
                    format!("read {path} (cached)"),
                    json!({ "cache_ref": cache_id }),
                    format!("cache hit: {cache_id}"),
                );
            }

            return match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let byte_count = content.len();
                    let total_lines = content.lines().count();
                    let cache_id = format!("cache_{:x}", content.len() ^ (mtime as usize));

                    cache_store(&cache_dir, &key, &cache_id);

                    ok_with(
                        format!("read {path}"),
                        json!({
                            "content": content,
                            "cache_ref": cache_id,
                            "total_lines": total_lines,
                        }),
                        format!("{byte_count} bytes"),
                    )
                }
                Err(e) => err_result(io_error_kind(&e), format!("{path}: {e}")),
            };
        }

        // Ranged path. The file must be read to count its lines and validate the span, even on
        // a cache hit — but the cache still earns its keep by returning only a `cache_ref`
        // (never the content) once a given (path, resolved-range, mtime) has been seen, so the
        // saving is the transmission of the slice, not the local disk read.
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return err_result(io_error_kind(&e), format!("{path}: {e}")),
        };
        let total_lines = content.lines().count();

        let start = start_arg.unwrap_or(1);
        if start < 1 {
            return err_result(
                err::INVALID_RANGE,
                format!("start_line must be >= 1, got {start}"),
            );
        }
        // start beyond EOF errors (states the real line count) rather than silently falling
        // back to the whole file. An empty file (0 lines) rejects any start_line here.
        if start as usize > total_lines {
            return err_result(
                err::INVALID_RANGE,
                format!("start_line {start} is beyond end of file ({total_lines} lines)"),
            );
        }
        // end defaults to EOF; an explicit end past EOF *clamps* (read-to-EOF is what an agent
        // means) — the one case that clamps instead of erroring.
        let mut end = end_arg.unwrap_or(total_lines as i64);
        if end > total_lines as i64 {
            end = total_lines as i64;
        }
        if start > end {
            return err_result(
                err::INVALID_RANGE,
                format!("inverted range: start_line {start} is greater than end_line {end}"),
            );
        }

        let start = start as usize;
        let end = end as usize;
        let span = end - start + 1;
        if span > MAX_READ_RANGE_LINES {
            return err_result(
                err::RANGE_TOO_LARGE,
                format!(
                    "requested range of {span} lines exceeds the maximum of {MAX_READ_RANGE_LINES}; narrow start_line/end_line"
                ),
            );
        }

        // Key on the *resolved* range so different ranges of the same file cache independently
        // and never collide with the whole-file entry.
        let key = cache_key_string(
            &path,
            LineRange { start: Some(start), end: Some(end) },
            mtime,
        );

        if let Some(cache_id) = cache_lookup(&cache_dir, &key) {
            return ok_with(
                format!("read {path} lines {start}-{end} (cached)"),
                json!({
                    "cache_ref": cache_id,
                    "total_lines": total_lines,
                    "start_line": start,
                    "end_line": end,
                }),
                format!("cache hit: {cache_id}"),
            );
        }

        // Slice `[start-1, end)` (0-based half-open), join with "\n". `lines()` already strips
        // line terminators, so the join reproduces the original text of the span.
        let sliced: String = content
            .lines()
            .skip(start - 1)
            .take(end - start + 1)
            .collect::<Vec<&str>>()
            .join("\n");
        let byte_count = sliced.len();
        let cache_id = format!("cache_{:x}", sliced.len() ^ (mtime as usize));

        cache_store(&cache_dir, &key, &cache_id);

        ok_with(
            format!("read {path} lines {start}-{end}"),
            json!({
                "content": sliced,
                "cache_ref": cache_id,
                "total_lines": total_lines,
                "start_line": start,
                "end_line": end,
            }),
            format!("{byte_count} bytes (lines {start}-{end} of {total_lines})"),
        )
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

        // Optional grep-style context window. Default 0 reproduces the historical match shape
        // exactly (no context keys). Validate before any filesystem walk so a rejected input
        // performs no search. A negative value or one above the cap is rejected — the latter
        // would re-transmit whole files and defeat the bounded-read intent.
        let context_lines = op.get("context_lines").and_then(Value::as_i64).unwrap_or(0);
        if !(0..=MAX_CONTEXT_LINES).contains(&context_lines) {
            return err_result(
                err::CONTEXT_TOO_LARGE,
                format!(
                    "context_lines must be between 0 and {MAX_CONTEXT_LINES}, got {context_lines}"
                ),
            );
        }

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

            let lines: Vec<&str> = contents.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    let mut match_obj = json!({
                        "path": relative,
                        "line": idx + 1,
                        "text": line,
                    });
                    // Attach context only when requested, so the default shape is byte-for-byte
                    // unchanged (no empty arrays). Each side is clamped to the lines that exist
                    // — a match near a file boundary gets a shorter array, never padding.
                    if context_lines > 0 {
                        let cl = context_lines as usize;
                        let before_start = idx.saturating_sub(cl);
                        let after_end = std::cmp::min(idx + 1 + cl, lines.len());
                        match_obj["context_before"] = json!(&lines[before_start..idx]);
                        match_obj["context_after"] = json!(&lines[idx + 1..after_end]);
                    }
                    // The running lower bound is measured *after* context is attached, so a
                    // large context window inflating the payload is caught here (and again by
                    // the authoritative post-serialization check below).
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
                err::INVALID_RANGE,
                err::RANGE_TOO_LARGE,
                err::CONTEXT_TOO_LARGE,
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

        // ── Bounded read_file (start_line / end_line) ────────────────────────────
        //
        // Each test isolates the disk cache to its own temp dir via `cache_env_guard`, so a
        // cache hit from a prior test can never mask a miss here. A shared helper writes a
        // file of `n` numbered lines ("line 1".."line n"), the fixture every range test slices.

        fn write_numbered_file(dir: &std::path::Path, name: &str, n: usize) -> String {
            let _ = fs::remove_dir_all(dir);
            fs::create_dir_all(dir).expect("failed to create temp dir");
            let mut content = String::new();
            for i in 1..=n {
                content.push_str(&format!("line {i}\n"));
            }
            let path = dir.join(name);
            fs::write(&path, content).expect("failed to write test file");
            path.to_string_lossy().to_string()
        }

        #[test]
        fn read_file_whole_file_reports_total_lines() {
            // A whole-file read (no start/end) keeps its content and cache_ref unchanged, and
            // now additionally reports total_lines — but adds NO start_line/end_line keys.
            let dir = std::env::temp_dir().join("murmur_test_read_wholefile_total");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 5);
            let op = json!({ "operation": "read_file", "path": &path });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            assert_eq!(out["content"], "line 1\nline 2\nline 3\nline 4\nline 5\n");
            assert_eq!(out["total_lines"], 5);
            assert!(out["cache_ref"].is_string());
            assert!(out["start_line"].is_null(), "whole-file read must not add start_line");
            assert!(out["end_line"].is_null(), "whole-file read must not add end_line");

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_happy_path() {
            let dir = std::env::temp_dir().join("murmur_test_read_ranged");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 10);
            let op = json!({ "operation": "read_file", "path": &path, "start_line": 2, "end_line": 4 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            // Sliced span, joined with "\n" (no trailing newline — lines() strips terminators).
            assert_eq!(out["content"], "line 2\nline 3\nline 4");
            assert_eq!(out["total_lines"], 10);
            assert_eq!(out["start_line"], 2);
            assert_eq!(out["end_line"], 4);
            assert!(out["cache_ref"].is_string());

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_start_only_reads_to_eof() {
            let dir = std::env::temp_dir().join("murmur_test_read_start_only");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 6);
            let op = json!({ "operation": "read_file", "path": &path, "start_line": 5 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            assert_eq!(out["content"], "line 5\nline 6");
            assert_eq!(out["start_line"], 5);
            assert_eq!(out["end_line"], 6); // resolved to total_lines

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_end_only_reads_from_top() {
            let dir = std::env::temp_dir().join("murmur_test_read_end_only");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 6);
            let op = json!({ "operation": "read_file", "path": &path, "end_line": 3 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            assert_eq!(out["content"], "line 1\nline 2\nline 3");
            assert_eq!(out["start_line"], 1); // resolved default
            assert_eq!(out["end_line"], 3);

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_inverted_span_errors() {
            let dir = std::env::temp_dir().join("murmur_test_read_inverted");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 10);
            let op = json!({ "operation": "read_file", "path": &path, "start_line": 5, "end_line": 2 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::INVALID_RANGE);
            assert!(out["message"].as_str().unwrap().contains("inverted"));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_start_below_one_errors() {
            let dir = std::env::temp_dir().join("murmur_test_read_start_zero");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 10);
            let op = json!({ "operation": "read_file", "path": &path, "start_line": 0 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::INVALID_RANGE);
            assert!(out["message"].as_str().unwrap().contains(">= 1"));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_start_beyond_eof_errors() {
            // start past EOF errors and states the real line count — never silently falls back
            // to the whole file.
            let dir = std::env::temp_dir().join("murmur_test_read_start_eof");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 4);
            let op = json!({ "operation": "read_file", "path": &path, "start_line": 99 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::INVALID_RANGE);
            let msg = out["message"].as_str().unwrap();
            assert!(msg.contains("beyond end of file"), "{msg}");
            assert!(msg.contains("4 lines"), "message must state real line count: {msg}");

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_end_beyond_eof_clamps() {
            // The one asymmetric case: end past EOF clamps to total_lines rather than erroring.
            let dir = std::env::temp_dir().join("murmur_test_read_end_eof");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 4);
            let op = json!({ "operation": "read_file", "path": &path, "start_line": 3, "end_line": 999 });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            assert_eq!(out["content"], "line 3\nline 4");
            assert_eq!(out["end_line"], 4); // clamped, reported as resolved
            assert_eq!(out["total_lines"], 4);

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_oversized_span_errors() {
            let dir = std::env::temp_dir().join("murmur_test_read_oversized");
            let _guard = cache_env_guard(&dir.join("cache"));
            // A file larger than the cap, requested whole via an explicit span.
            let path = write_numbered_file(&dir, "big.txt", MAX_READ_RANGE_LINES + 500);
            let op = json!({
                "operation": "read_file",
                "path": &path,
                "start_line": 1,
                "end_line": MAX_READ_RANGE_LINES + 500,
            });

            let out = op_read_file(&op);
            assert_eq!(out["ok"], false);
            assert_eq!(out["error_kind"], err::RANGE_TOO_LARGE);
            assert!(out["message"].as_str().unwrap().contains("exceeds the maximum"));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_file_ranged_cache_ref_differs_from_whole_file() {
            // Different ranges of the same file cache under distinct keys, so a whole-file read
            // and a ranged read return different cache_refs and never collide.
            let dir = std::env::temp_dir().join("murmur_test_read_cache_distinct");
            let _guard = cache_env_guard(&dir.join("cache"));
            let path = write_numbered_file(&dir, "f.txt", 10);

            let whole = op_read_file(&json!({ "operation": "read_file", "path": &path }));
            let ranged = op_read_file(&json!({
                "operation": "read_file", "path": &path, "start_line": 2, "end_line": 4
            }));
            assert_eq!(whole["ok"], true);
            assert_eq!(ranged["ok"], true);
            assert_ne!(
                whole["cache_ref"].as_str().unwrap(),
                ranged["cache_ref"].as_str().unwrap(),
                "whole-file and ranged reads must not share a cache_ref"
            );

            // And the ranged read is itself cacheable: a second identical ranged read hits.
            let ranged2 = op_read_file(&json!({
                "operation": "read_file", "path": &path, "start_line": 2, "end_line": 4
            }));
            assert_eq!(ranged2["cache_ref"], ranged["cache_ref"]);
            assert!(ranged2["content"].is_null(), "ranged cache hit must not resend content");

            let _ = fs::remove_dir_all(&dir);
        }

        // ── find_in_files context_lines ─────────────────────────────────────────

        fn write_find_fixture(dir: &std::path::Path, name: &str, content: &str) -> String {
            let _ = fs::remove_dir_all(dir);
            fs::create_dir_all(dir).expect("failed to create temp dir");
            fs::write(dir.join(name), content).expect("failed to write fixture");
            dir.to_string_lossy().to_string()
        }

        #[test]
        fn find_in_files_default_has_no_context_keys() {
            // context_lines omitted → the match shape is byte-for-byte the historical one:
            // {path, line, text} with NO context_before/context_after keys at all.
            let dir = std::env::temp_dir().join("murmur_test_find_no_ctx");
            let dir_name = write_find_fixture(&dir, "a.txt", "one\ntwo needle\nthree\n");
            let op = json!({ "operation": "find_in_files", "pattern": "needle", "dir": &dir_name, "recursive": false });

            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            let m = &out["matches"][0];
            assert_eq!(m["line"], 2);
            assert_eq!(m["text"], "two needle");
            assert!(m.get("context_before").is_none(), "no context key when omitted");
            assert!(m.get("context_after").is_none(), "no context key when omitted");

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn find_in_files_context_lines_happy_path() {
            let dir = std::env::temp_dir().join("murmur_test_find_ctx");
            let dir_name = write_find_fixture(&dir, "a.txt", "L1\nL2\nL3 needle\nL4\nL5\n");
            let op = json!({
                "operation": "find_in_files", "pattern": "needle",
                "dir": &dir_name, "recursive": false, "context_lines": 1
            });

            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            let m = &out["matches"][0];
            assert_eq!(m["line"], 3);
            assert_eq!(m["context_before"], json!(["L2"]));
            assert_eq!(m["context_after"], json!(["L4"]));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn find_in_files_context_clamped_at_boundaries() {
            // A match on the first line gets an empty context_before (clamped, never padded)
            // and a context_after shortened to the lines that exist.
            let dir = std::env::temp_dir().join("murmur_test_find_ctx_boundary");
            let dir_name = write_find_fixture(&dir, "a.txt", "head needle\nb\nc\n");
            let op = json!({
                "operation": "find_in_files", "pattern": "needle",
                "dir": &dir_name, "recursive": false, "context_lines": 5
            });

            let out = op_find_in_files(&op);
            assert_eq!(out["ok"], true, "{out:?}");
            let m = &out["matches"][0];
            assert_eq!(m["line"], 1);
            assert_eq!(m["context_before"], json!([]), "top-of-file match: empty before");
            assert_eq!(m["context_after"], json!(["b", "c"]), "clamped to existing lines");

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn find_in_files_rejects_oversized_context() {
            let dir = std::env::temp_dir().join("murmur_test_find_ctx_toobig");
            let dir_name = write_find_fixture(&dir, "a.txt", "needle\n");
            for bad in [MAX_CONTEXT_LINES + 1, -1] {
                let op = json!({
                    "operation": "find_in_files", "pattern": "needle",
                    "dir": &dir_name, "recursive": false, "context_lines": bad
                });
                let out = op_find_in_files(&op);
                assert_eq!(out["ok"], false, "context_lines {bad} must be rejected");
                assert_eq!(out["error_kind"], err::CONTEXT_TOO_LARGE);
            }

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn find_in_files_size_ceiling_accounts_for_context() {
            // The fixture is under the 500KB ceiling with NO context, but the same search with
            // a non-zero context window blows past it — proving the size accounting measures
            // the context text, not just the match lines.
            let dir = std::env::temp_dir().join("murmur_test_find_ctx_ceiling");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("failed to create temp dir");

            let padding = "x".repeat(200);
            let mut content = String::new();
            // 1200 lines; every 4th is a match. The 900 non-matching lines are heavy padding
            // that only enters the payload once context is requested.
            for i in 0..1200 {
                if i % 4 == 0 {
                    content.push_str(&format!("marker {padding}\n"));
                } else {
                    content.push_str(&format!("{padding}\n"));
                }
            }
            fs::write(dir.join("big.txt"), content).expect("failed to write fixture");
            let dir_name = dir.to_string_lossy().to_string();

            // Without context: comfortably under the ceiling.
            let no_ctx = op_find_in_files(&json!({
                "operation": "find_in_files", "pattern": "marker",
                "dir": &dir_name, "recursive": false, "context_lines": 0
            }));
            assert_eq!(no_ctx["ok"], true, "no-context search should fit: {}", no_ctx["message"]);

            // With context: the added neighbor text tips it over, and it is rejected.
            let with_ctx = op_find_in_files(&json!({
                "operation": "find_in_files", "pattern": "marker",
                "dir": &dir_name, "recursive": false, "context_lines": 3
            }));
            assert_eq!(with_ctx["ok"], false);
            assert_eq!(with_ctx["error_kind"], err::RESULT_SIZE_EXCEEDED);
            assert!(with_ctx["message"].as_str().unwrap().contains("size limit"));

            let _ = fs::remove_dir_all(&dir);
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
            let key = cache_key_string("some/file.rs", LineRange::whole_file(), 12345);

            assert!(cache_lookup(&dir, &key).is_none(), "empty cache must miss");
            cache_store(&dir, &key, "cache_abc123");
            assert_eq!(cache_lookup(&dir, &key).as_deref(), Some("cache_abc123"));

            // A different key (different mtime) must miss.
            let other = cache_key_string("some/file.rs", LineRange::whole_file(), 99999);
            assert!(cache_lookup(&dir, &other).is_none());

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn cache_lookup_treats_corrupt_entry_as_miss() {
            let dir = std::env::temp_dir().join("murmur_test_cache_corrupt");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let key = cache_key_string("x.txt", LineRange::whole_file(), 1);

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
                let key = cache_key_string("f.txt", LineRange::whole_file(), i as u64);
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
