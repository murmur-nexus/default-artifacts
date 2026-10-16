//! The one file this tool writes, and the only two ways it is touched.
//!
//! The state directory is never created here. If the durable-state grant is missing, the
//! guest path `state/` resolves inside the workdir preopen instead, and creating it there
//! would produce a report the agent can rewrite at will — a report that exists and is
//! quietly worthless. Failing closed with `state_unavailable` is the correct outcome.
//!
//! Saving goes through a temp file in the same directory and a rename over `report.json`,
//! so a write that fails partway leaves the previous verdict exactly as it was rather than
//! a truncated file no consumer can read.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ops::{kind, OpError};
use crate::report::ReportDoc;

/// The report itself, relative to the state directory. One fixed name: a consumer finds it
/// without coordinating with the capsule that wrote it.
pub const REPORT_FILE: &str = "report.json";

/// Where a save is staged before it is renamed over [`REPORT_FILE`]. Same directory, so
/// the rename is within one filesystem and one preopen.
pub const TEMP_FILE: &str = "report.json.tmp";

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
        match fs::metadata(state_dir) {
            Ok(meta) if meta.is_dir() => Ok(Self { root: state_dir.to_path_buf() }),
            Ok(_) => Err(OpError::new(
                kind::STATE_UNAVAILABLE,
                format!(
                    "durable state path \"{}\" is not a directory; the capsule must grant \
                     capabilities.state on this tool's entry",
                    state_dir.display()
                ),
            )),
            Err(_) => Err(OpError::new(
                kind::STATE_UNAVAILABLE,
                format!(
                    "durable state directory \"{}\" is not available; the capsule must grant \
                     capabilities.state on this tool's entry for the report to be written \
                     anywhere the consumer can read it",
                    state_dir.display()
                ),
            )),
        }
    }

    /// The report file's path.
    pub fn report_path(&self) -> PathBuf {
        self.root.join(REPORT_FILE)
    }

    fn temp_path(&self) -> PathBuf {
        self.root.join(TEMP_FILE)
    }

    /// The report on disk, or `None` when the capsule has never reported.
    ///
    /// Absence is a state, not a fault — it is the state a consumer must never read as
    /// success. A file that is present and unreadable is a fault: it comes back as
    /// `io_error` rather than as a fresh empty document, because starting over would
    /// discard a verdict that is merely unparseable.
    pub fn load(&self) -> Result<Option<ReportDoc>, OpError> {
        let path = self.report_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(OpError::new(
                    kind::IO_ERROR,
                    format!("cannot read \"{}\": {e}", path.display()),
                ))
            }
        };
        serde_json::from_str::<ReportDoc>(&text)
            .map(Some)
            .map_err(|e| {
                OpError::new(
                    kind::IO_ERROR,
                    format!(
                        "\"{}\" exists but is not a report document ({e}); it was not \
                         overwritten — move it aside to start a fresh report",
                        path.display()
                    ),
                )
            })
    }

    /// Write the report, replacing any previous one atomically.
    ///
    /// Pretty-printed with a trailing newline: this file is read by `cat` as often as by a
    /// parser, and it parses identically either way.
    pub fn save(&self, doc: &ReportDoc) -> Result<(), OpError> {
        let path = self.report_path();
        let temp = self.temp_path();

        let mut text = serde_json::to_string_pretty(doc).map_err(|e| {
            OpError::new(kind::IO_ERROR, format!("cannot serialise the report: {e}"))
        })?;
        text.push('\n');

        self.write_temp(&temp, &text).inspect_err(|_| {
            // A staged write that failed must not leave a stale temp file beside a report
            // that is still the truth.
            let _ = fs::remove_file(&temp);
        })?;

        fs::rename(&temp, &path).map_err(|e| {
            let _ = fs::remove_file(&temp);
            OpError::new(
                kind::IO_ERROR,
                format!(
                    "cannot replace \"{}\" with the staged report: {e}; the previous report \
                     is untouched",
                    path.display()
                ),
            )
        })
    }

    fn write_temp(&self, temp: &Path, text: &str) -> Result<(), OpError> {
        let mut file = fs::File::create(temp).map_err(|e| {
            OpError::new(
                kind::IO_ERROR,
                format!("cannot open \"{}\" for writing: {e}", temp.display()),
            )
        })?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| {
                OpError::new(
                    kind::IO_ERROR,
                    format!("cannot write \"{}\": {e}", temp.display()),
                )
            })?;
        // Best-effort durability. Some WASI hosts do not implement fsync on a preopened
        // file; the write already succeeded, so a rejection here must not fail the call.
        let _ = file.sync_all();
        Ok(())
    }
}
