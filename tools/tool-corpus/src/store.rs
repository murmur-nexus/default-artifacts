//! Append-only file access, rooted at a caller-supplied directory.
//!
//! The corpus file is opened in exactly two ways and no others: for append (with
//! `create`), and read-only. There is no code path in this crate that can rewrite a byte
//! already on disk — no whole-file rewrite, no rename-over, no temp-file swap. That is
//! the guarantee the whole artifact exists to provide, and it is enforced by the absence
//! of the alternatives rather than by a check. [`Store::verify`] is a read: it names the
//! lines a scan cannot use, and repairing them stays a human action on the file.
//!
//! The state directory is never created here. If the durable-state grant is missing, the
//! guest path `state/` resolves inside the workdir preopen instead, and creating it there
//! would produce a store the agent can rewrite at will — a store that works and is
//! quietly worthless. Failing closed with `state_unavailable` is the correct outcome.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ops::{kind, single_line_excerpt, OpError};
use crate::record::Record;

/// The corpus itself, relative to the state directory.
pub const CORPUS_FILE: &str = "corpus.jsonl";

/// Where a withdrawn record's tombstone points: the withdrawing record's id and when it
/// was appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub by: String,
    pub at: String,
}

/// Every record in the corpus, plus the 1-based number of every line that did not parse.
///
/// The two travel together because a result set is only honest alongside what it omits: a
/// caller holding just `records` cannot tell a corpus of three from a corpus of four with
/// one line it could not read.
#[derive(Debug, Clone, PartialEq)]
pub struct Scan {
    pub records: Vec<Record>,
    pub skipped_lines: Vec<u64>,
}

/// One line `verify` could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadLine {
    /// 1-based line number in `corpus.jsonl`, so an operator can go straight to it.
    pub line: u64,
    /// The parse failure, as the JSON parser reported it.
    pub error: String,
    /// The start of the line, collapsed to one line and bounded — enough to recognise what
    /// went wrong without copying an arbitrarily long line into an agent's context.
    pub preview: String,
}

/// What `verify` found: how much of the corpus is readable, and what is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub lines: u64,
    pub records: u64,
    /// Every bad line, uncapped. The display cap lives in `ops`.
    pub bad_lines: Vec<BadLine>,
}

/// A handle on an existing, reachable state directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open the state directory, or fail with `state_unavailable`.
    ///
    /// This only ever *reads* directory metadata. A missing directory is reported, never
    /// repaired.
    pub fn open(state_dir: &Path) -> Result<Self, OpError> {
        match std::fs::metadata(state_dir) {
            Ok(meta) if meta.is_dir() => Ok(Self { root: state_dir.to_path_buf() }),
            Ok(_) => Err(OpError::new(
                kind::STATE_UNAVAILABLE,
                format!(
                    "durable state path \"{}\" is not a directory; the capsule must grant \
                     capabilities.state",
                    state_dir.display()
                ),
            )),
            Err(_) => Err(OpError::new(
                kind::STATE_UNAVAILABLE,
                format!(
                    "durable state directory \"{}\" is not available; the capsule must grant \
                     capabilities.state for this tool to reach the corpus",
                    state_dir.display()
                ),
            )),
        }
    }

    /// The corpus file's path.
    pub fn corpus_path(&self) -> PathBuf {
        self.root.join(CORPUS_FILE)
    }

    /// The corpus file's text, or `None` when no corpus exists yet.
    ///
    /// A corpus that has never been appended to is an empty corpus, not a fault: the file
    /// is created by the first append and by nothing else here.
    fn read_text(&self) -> Result<Option<String>, OpError> {
        let path = self.corpus_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(OpError::new(
                kind::IO_ERROR,
                format!("cannot read \"{}\": {e}", path.display()),
            )),
        }
    }

    /// Every record in the corpus, in append order, alongside the lines that were skipped.
    ///
    /// Internal scanning: dedupe, the withdrawal index and search all need the whole file.
    /// No *operation* exposes this — `read_recent` and `search` are capped, and there is
    /// no operation that returns the corpus.
    ///
    /// A line that does not parse as a record is skipped rather than failing the call, and
    /// its number is carried back in [`Scan::skipped_lines`] for the caller to report.
    /// Skipping is only acceptable because reporting is not optional: every response built
    /// from a scan that skipped something says so, in the envelope and in the summary, so
    /// the damage reaches the agent's context and the trace on the very next call instead
    /// of one bad byte making the whole store unusable.
    pub fn read_all(&self) -> Result<Scan, OpError> {
        let Some(text) = self.read_text()? else {
            return Ok(Scan { records: Vec::new(), skipped_lines: Vec::new() });
        };

        let mut records = Vec::new();
        let mut skipped_lines = Vec::new();
        for (index, line) in text.lines().enumerate() {
            // A blank line is skipped and reported like any other unreadable one.
            // `append_record` writes exactly one `\n` terminator and `str::lines()` yields
            // no trailing element for it, so a blank line means something other than this
            // tool wrote to the file — which is exactly what the operator should be told.
            match serde_json::from_str::<Record>(line) {
                Ok(record) => records.push(record),
                Err(_) => skipped_lines.push(index as u64 + 1),
            }
        }
        Ok(Scan { records, skipped_lines })
    }

    /// Read every line and report the ones that are not records.
    ///
    /// This is a read like any other. There is no repairing counterpart: rewriting the
    /// file would be the only code path in this crate that opens the corpus for something
    /// other than append, and that invariant is worth more than the convenience. Repair is
    /// a human action on `corpus.jsonl`, informed by what this reports.
    pub fn verify(&self) -> Result<VerifyReport, OpError> {
        let Some(text) = self.read_text()? else {
            return Ok(VerifyReport { lines: 0, records: 0, bad_lines: Vec::new() });
        };

        let mut lines = 0;
        let mut records = 0;
        let mut bad_lines = Vec::new();
        for (index, line) in text.lines().enumerate() {
            lines += 1;
            match serde_json::from_str::<Record>(line) {
                Ok(_) => records += 1,
                Err(e) => bad_lines.push(BadLine {
                    line: index as u64 + 1,
                    error: e.to_string(),
                    preview: single_line_excerpt(line),
                }),
            }
        }
        Ok(VerifyReport { lines, records, bad_lines })
    }

    /// Append one record as a single JSON line.
    ///
    /// `serde_json::to_string` on the struct (not through a `Value`) is what preserves the
    /// documented on-disk key order.
    pub fn append_record(&self, record: &Record) -> Result<(), OpError> {
        let path = self.corpus_path();
        let line = serde_json::to_string(record).map_err(|e| {
            OpError::new(kind::IO_ERROR, format!("cannot serialise the record: {e}"))
        })?;

        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| {
                OpError::new(
                    kind::IO_ERROR,
                    format!("cannot open \"{}\" for append: {e}", path.display()),
                )
            })?;

        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
            .map_err(|e| {
                OpError::new(
                    kind::IO_ERROR,
                    format!("cannot append to \"{}\": {e}", path.display()),
                )
            })?;

        // Best-effort durability. Some WASI hosts do not implement fsync on a preopened
        // file; the append already succeeded, so a rejection here must not fail the call.
        let _ = file.sync_all();
        Ok(())
    }
}

/// Which records have been withdrawn, keyed by the withdrawn record's id.
///
/// Withdrawal is terminal: a record that withdraws a withdrawal record removes only that
/// withdrawal record from retrieval, and never restores the original target. That falls
/// out of keying by target id — the original target's entry is never revisited.
pub fn withdrawal_index(records: &[Record]) -> BTreeMap<String, Withdrawal> {
    let mut index = BTreeMap::new();
    for record in records {
        if let Some(target) = &record.withdraws {
            index
                .entry(target.clone())
                .or_insert_with(|| Withdrawal {
                    by: record.id.clone(),
                    at: record.created_at.clone(),
                });
        }
    }
    index
}
