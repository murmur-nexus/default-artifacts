//! Append-only file access, rooted at a caller-supplied directory.
//!
//! The corpus file is opened in exactly two ways and no others: for append (with
//! `create`), and read-only. There is no code path in this crate that can rewrite a byte
//! already on disk — no whole-file rewrite, no rename-over, no temp-file swap. That is
//! the guarantee the whole artifact exists to provide, and it is enforced by the absence
//! of the alternatives rather than by a check.
//!
//! The state directory is never created here. If the durable-state grant is missing, the
//! guest path `state/` resolves inside the workdir preopen instead, and creating it there
//! would produce a store the agent can rewrite at will — a store that works and is
//! quietly worthless. Failing closed with `state_unavailable` is the correct outcome.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{parse_config, Config};
use crate::ops::{kind, OpError};
use crate::record::Record;

/// The corpus itself, relative to the state directory.
pub const CORPUS_FILE: &str = "corpus.jsonl";
/// The operator config, relative to the state directory.
pub const CONFIG_FILE: &str = "corpus.config.json";

/// Where a withdrawn record's tombstone points: the withdrawing record's id and when it
/// was appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub by: String,
    pub at: String,
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

    /// The operator config's path.
    pub fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    /// Read and validate the operator config.
    pub fn load_config(&self) -> Result<Config, OpError> {
        let path = self.config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(OpError::new(
                    kind::CONFIG_MISSING,
                    format!(
                        "no operator configuration at \"{}\"; the corpus refuses every operation \
                         until an operator declares its record types there",
                        path.display()
                    ),
                ))
            }
            Err(e) => {
                return Err(OpError::new(
                    kind::IO_ERROR,
                    format!("cannot read \"{}\": {e}", path.display()),
                ))
            }
        };
        parse_config(&text).map_err(|message| OpError::new(kind::CONFIG_INVALID, message))
    }

    /// Every record in the corpus, in append order.
    ///
    /// Internal scanning: dedupe, the withdrawal index and search all need the whole file.
    /// No *operation* exposes this — `read_recent` and `search` are capped, and there is
    /// no operation that returns the corpus.
    ///
    /// A line that does not parse as a record fails the whole call with `corpus_corrupt`,
    /// naming the 1-based line number. There is no partial result set: a store that
    /// quietly drops what it cannot read is the accounting failure this artifact exists to
    /// prevent.
    pub fn read_all(&self) -> Result<Vec<Record>, OpError> {
        let path = self.corpus_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(OpError::new(
                    kind::IO_ERROR,
                    format!("cannot read \"{}\": {e}", path.display()),
                ))
            }
        };

        let mut records = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let record: Record = serde_json::from_str(line).map_err(|e| {
                OpError::new(
                    kind::CORPUS_CORRUPT,
                    format!(
                        "{} line {} does not parse as a record: {e}",
                        CORPUS_FILE,
                        index + 1
                    ),
                )
            })?;
            records.push(record);
        }
        Ok(records)
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
