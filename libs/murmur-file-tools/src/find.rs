//! `tool-find`: search file contents for a regex, within the find limits.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::{
    err, err_result, fail_msg, format_result_size, io_error_kind, ok_with, reject_absolute,
};

// find limits. A search stops at the first one it reaches and still succeeds, returning the
// matches found so far with a `capped` note; see `cap_note`.
pub(crate) const FIND_RESULT_SIZE_LIMIT: usize = 500 * 1024; // 500 KB ceiling for find output
const FIND_MAX_MATCHES: usize = 200;
// Files walked: every file the walk reaches below `dir`, whether or not the `glob` admits it.
const FIND_MAX_FILES: usize = 5_000;
const FIND_MAX_BYTES: u64 = 32 * 1024 * 1024;

// Upper bound on `find` `context_lines`, mirroring the range a `grep -C` user would
// realistically ask for. Beyond this the context stops being "the surrounding function" and
// starts re-transmitting whole files, defeating the bounded-read intent.
pub(crate) const MAX_CONTEXT_LINES: i64 = 20;

/// Search with the production limits.
pub(crate) fn find(op: &Value) -> Value {
    find_with_limits(op, Limits::PRODUCTION)
}

/// The bounds one `find` call works within. Production calls use
/// [`Limits::PRODUCTION`]; unit tests pass smaller ones so a budget can be reached with a
/// fixture of a few files.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) matches: usize,
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    pub(crate) result_size: usize,
}

impl Limits {
    pub(crate) const PRODUCTION: Limits = Limits {
        matches: FIND_MAX_MATCHES,
        files: FIND_MAX_FILES,
        bytes: FIND_MAX_BYTES,
        result_size: FIND_RESULT_SIZE_LIMIT,
    };
}

/// Which limit stopped a search. A stopped search still succeeds, carrying the matches
/// found so far and a `capped` note built by [`cap_note`] that names this limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapReason {
    Matches,
    Files,
    Bytes,
    ResultSize,
}

pub(crate) fn find_with_limits(op: &Value, limits: Limits) -> Value {
    let pattern = match op.get("pattern").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return fail_msg("missing required field: pattern"),
    };
    // Absent or null means the workdir root, the same as "" or ".".
    let dir = match op.get("dir") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(d)) => d.clone(),
        Some(_) => return fail_msg("dir must be a string"),
    };
    if let Some(refusal) = reject_absolute("dir", &dir) {
        return refusal;
    }
    let glob = match op.get("glob") {
        None | Some(Value::Null) => None,
        Some(Value::String(g)) if g.is_empty() => None,
        Some(Value::String(g)) => Some(g.clone()),
        Some(_) => return fail_msg("glob must be a string"),
    };
    if let Some(refusal) = glob.as_deref().and_then(|g| reject_absolute("glob", g)) {
        return refusal;
    }
    let recursive = op
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Optional grep-style context window. Default 0 reproduces the historical match shape
    // exactly (no context keys). Validate before any filesystem walk so a rejected input
    // performs no search. A negative value or one above the cap is rejected — the latter
    // would re-transmit whole files and defeat the bounded-read intent.
    let context_lines = op.get("context_lines").and_then(Value::as_i64).unwrap_or(0);
    if !(0..=MAX_CONTEXT_LINES).contains(&context_lines) {
        return err_result(
            err::CONTEXT_TOO_LARGE,
            format!("context_lines must be between 0 and {MAX_CONTEXT_LINES}, got {context_lines}"),
        );
    }

    let root = match normalize_dir(&dir) {
        Some(n) if n.is_empty() => ".".to_string(),
        Some(n) => n,
        None => {
            return err_result(
                err::OUTSIDE_WORKDIR,
                format!("'dir' resolves outside the capsule workdir: '{dir}'"),
            )
        }
    };

    let re = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => {
            return err_result(
                err::INVALID_PATTERN,
                format!("invalid regex '{pattern}': {e}"),
            )
        }
    };
    // A glob with no `/` is matched against the file name, so `*.py` finds a Python file
    // at any depth; one with a `/` is matched against the path relative to `dir`.
    let glob_filter = match glob.as_deref().map(|g| (glob_to_regex(g), g.contains('/'))) {
        None => None,
        Some((Ok(re), whole_path)) => Some((re, whole_path)),
        Some((Err(e), _)) => {
            return err_result(
                err::INVALID_PATTERN,
                format!("invalid glob '{}': {e}", glob.unwrap_or_default()),
            )
        }
    };

    let root_path = Path::new(&root);
    let file_paths: Box<dyn Iterator<Item = PathBuf>> = if recursive {
        Box::new(
            WalkDir::new(root_path)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !is_skipped_dir(e))
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path()),
        )
    } else {
        match std::fs::read_dir(root_path) {
            Ok(entries) => {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .map(|e| e.path())
                    .collect();
                files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                Box::new(files.into_iter())
            }
            Err(e) => return err_result(io_error_kind(&e), format!("{dir}: {e}")),
        }
    };

    // The result goes out as the whole `ok_with` envelope, in which every match appears
    // twice (nested under `data.matches` and flattened to the top-level `matches`). The
    // running estimate starts from the envelope at its largest — the longest cap note and
    // summary this search could produce — so it only has to add each match's two copies.
    let ceiling = limits.result_size.saturating_sub(METADATA_RESERVE);
    let mut approx_out_size = envelope_overhead(limits);
    let mut matches: Vec<Value> = Vec::new();
    let mut files_searched: usize = 0;
    let mut bytes_read: u64 = 0;
    let mut capped: Option<CapReason> = None;

    // `files_walked` counts the files the walk yielded before this one, whatever the glob,
    // the read or the byte budget made of them; `files_searched` counts those actually read.
    'walk: for (files_walked, file_path) in file_paths.enumerate() {
        if files_walked >= limits.files {
            capped = Some(CapReason::Files);
            break;
        }

        let relative = relative_path(root_path, &file_path);
        if let Some((glob_re, whole_path)) = &glob_filter {
            let subject = if *whole_path {
                relative.as_str()
            } else {
                file_path.file_name().and_then(|n| n.to_str()).unwrap_or("")
            };
            if !glob_re.is_match(subject) {
                continue;
            }
        }

        if matches.len() >= limits.matches {
            capped = Some(CapReason::Matches);
            break;
        }
        let contents = match read_text_file(&file_path, &mut bytes_read, limits.bytes) {
            FileRead::Text(c) => c,
            FileRead::Binary => {
                files_searched += 1;
                continue;
            }
            FileRead::Unreadable => continue,
            FileRead::OverBudget => {
                capped = Some(CapReason::Bytes);
                break;
            }
        };
        files_searched += 1;

        let lines: Vec<&str> = contents.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            if matches.len() >= limits.matches {
                capped = Some(CapReason::Matches);
                break 'walk;
            }
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
            // Measured after context is attached, so a large context window counts. Each
            // copy also takes the comma that separates it from its neighbour.
            let match_size = serde_json::to_string(&match_obj)
                .map(|s| s.len())
                .unwrap_or(0);
            if approx_out_size + 2 * (match_size + 1) > ceiling {
                if matches.is_empty() {
                    return err_result(err::RESULT_SIZE_EXCEEDED, size_exceeded_message(limits));
                }
                capped = Some(CapReason::ResultSize);
                break 'walk;
            }
            approx_out_size += 2 * (match_size + 1);
            matches.push(match_obj);
        }
    }

    // Authoritative ceiling: the exact bytes that go out. The estimate is built to stay
    // above them, so this trims only if it ever undercounts.
    loop {
        let result = find_result(&matches, files_searched, capped, limits);
        let serialized_len = serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0);
        if serialized_len <= ceiling {
            return result;
        }
        if matches.len() <= 1 {
            return err_result(err::RESULT_SIZE_EXCEEDED, size_exceeded_message(limits));
        }
        matches.pop();
        capped = Some(CapReason::ResultSize);
    }
}

/// Room left under the result ceiling for the `metadata.state_effect` that [`crate::run`] attaches
/// to a successful result after the handler returns.
const METADATA_RESERVE: usize = 64;

/// The serialized size of a `find` result with no matches, at its largest: the
/// longest cap note, and a summary whose counts are as wide as the limits allow.
fn envelope_overhead(limits: Limits) -> usize {
    [
        CapReason::Matches,
        CapReason::Files,
        CapReason::Bytes,
        CapReason::ResultSize,
    ]
    .into_iter()
    .map(|reason| {
        let note = cap_note(reason, limits, limits.files);
        let summary = format!(
            "{} matches in {} files (capped)",
            limits.matches, limits.files
        );
        let empty = ok_with(
            summary.clone(),
            json!({ "capped": note, "matches": [] }),
            summary,
        );
        serde_json::to_string(&empty).map(|s| s.len()).unwrap_or(0)
    })
    .max()
    .unwrap_or(0)
}

/// The success envelope for a search: `data` is `{"matches": [...]}`, plus `capped` when a
/// limit stopped the search.
pub(crate) fn find_result(
    matches: &[Value],
    files_searched: usize,
    capped: Option<CapReason>,
    limits: Limits,
) -> Value {
    let match_count = matches.len();
    let file_count = matches
        .iter()
        .filter_map(|m| m["path"].as_str())
        .collect::<HashSet<_>>()
        .len();
    let mut summary = format!("{match_count} matches in {file_count} files");
    let data = match capped {
        None => json!({ "matches": matches }),
        Some(reason) => {
            summary.push_str(" (capped)");
            json!({
                "capped": cap_note(reason, limits, files_searched),
                "matches": matches,
            })
        }
    };
    ok_with(summary.clone(), data, summary)
}

/// The `capped` note: which limit stopped the search and how many files it read.
pub(crate) fn cap_note(reason: CapReason, limits: Limits, files_read: usize) -> String {
    let limit = match reason {
        CapReason::Matches => format!("the limit of {} matches", limits.matches),
        CapReason::Files => format!("the limit of {} files walked", limits.files),
        CapReason::Bytes => format!("the limit of {} read", format_bytes(limits.bytes)),
        CapReason::ResultSize => format!(
            "the {} result size limit",
            format_result_size(limits.result_size)
        ),
    };
    let files = if files_read == 1 { "file" } else { "files" };
    format!("Search stopped at {limit} after reading {files_read} {files}.")
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn size_exceeded_message(limits: Limits) -> String {
    format!(
        "a single match exceeds the find result size limit of {} bytes ({}).",
        limits.result_size,
        format_result_size(limits.result_size)
    )
}

/// What [`read_text_file`] found.
enum FileRead {
    Text(String),
    /// A NUL in the first [`BINARY_SNIFF_BYTES`], or not UTF-8. Read, so it counts as read
    /// and toward the byte budget, but never searched.
    Binary,
    /// Could not be opened or read; skipped, counted as walked but not as read.
    Unreadable,
    /// Its length would take the search past the byte budget; nothing was read.
    OverBudget,
}

/// The leading bytes inspected for a NUL when deciding a file is binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Read `path` as text, adding the bytes read to `bytes_read`. The file's length is checked
/// against `budget` before any byte of it is read.
fn read_text_file(path: &Path, bytes_read: &mut u64, budget: u64) -> FileRead {
    let Ok(file) = std::fs::File::open(path) else {
        return FileRead::Unreadable;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return FileRead::Unreadable;
    };
    if *bytes_read + len > budget {
        return FileRead::OverBudget;
    }
    // Read at most the length checked above, so a file still being written cannot carry
    // the search past its byte budget.
    let mut bytes = Vec::with_capacity(len as usize);
    if file.take(len).read_to_end(&mut bytes).is_err() {
        return FileRead::Unreadable;
    }
    *bytes_read += bytes.len() as u64;
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return FileRead::Binary;
    }
    match String::from_utf8(bytes) {
        Ok(text) => FileRead::Text(text),
        Err(_) => FileRead::Binary,
    }
}

/// A directory the recursive walk does not descend into: hidden (`.git`, `.venv`, …),
/// `target` or `node_modules`. Only applied below the search root, so naming one of these
/// as `dir` still searches it.
fn is_skipped_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || name == "target" || name == "node_modules"
}

/// `file` relative to the search root, `/`-separated. A `dir` that names a file reports
/// that file's name.
fn relative_path(root: &Path, file: &Path) -> String {
    let relative = file
        .strip_prefix(root)
        .unwrap_or(file)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if relative.is_empty() {
        file.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        relative
    }
}

/// `dir` with its `.` and empty segments dropped and each `..` applied to the segment
/// before it, `/`-joined. `""` is the workdir root. `None` when a `..` has no segment left
/// to pop, i.e. the value climbs above the workdir.
pub(crate) fn normalize_dir(dir: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in dir.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            s => segments.push(s),
        }
    }
    Some(segments.join("/"))
}

/// An anchored regex for `glob`. `**/` matches zero or more whole segments, a trailing `**`
/// after `/` matches everything below, and a `**` that is the whole glob matches anything.
/// `*` matches within one segment and `?` one non-`/` character; every other character is
/// literal.
pub(crate) fn glob_to_regex(glob: &str) -> Result<Regex, regex::Error> {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                let at_segment_start = i == 0 || chars[i - 1] == '/';
                let next = chars.get(i + 2);
                if at_segment_start && next == Some(&'/') {
                    out.push_str("(?:[^/]+/)*");
                    i += 3;
                    continue;
                }
                if at_segment_start && next.is_none() {
                    out.push_str(".*");
                } else {
                    out.push_str("[^/]*");
                }
                i += 2;
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            c => out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
        i += 1;
    }
    out.push('$');
    Regex::new(&out)
}
