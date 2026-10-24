//! One session's report file, and the only two ways it is touched.
//!
//! Every session of a capsule shares the durable state store, so each session writes its own
//! file, `<state>/reports/<session_id>.json`, and never reads another's. Two sessions running
//! at once — two cards in one Transit stage, two formation members on one capsule — then
//! cannot overwrite each other's verdict, and a new session never starts on top of the last
//! one's file.
//!
//! The state directory is never created here. If the durable-state grant is missing, the
//! guest path `state/` resolves inside the workdir preopen instead, and creating it there
//! would produce a report the agent can rewrite at will — a report that exists and is
//! quietly worthless. Failing closed with `state_unavailable` is the correct outcome. Only
//! `reports/` inside an existing state directory is created, and only by a save.
//!
//! Saving goes through a per-session temp file in the same directory and a rename over the
//! report, so a write that fails partway leaves the previous verdict exactly as it was rather
//! than a truncated file no consumer can read.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ops::{kind, OpError};
use crate::report::ReportDoc;
use crate::retention;

/// The directory, relative to the state directory, that holds one report file per session.
pub const REPORTS_DIR: &str = "reports";

/// Extension of a report file. Its stem is the session id, used verbatim.
pub const REPORT_EXTENSION: &str = ".json";

/// Appended to a report file's name to stage a save before it is renamed over the report.
/// Per session, so two sessions saving at once never share a temp file; same directory, so
/// the rename stays within one filesystem and one preopen.
pub const TEMP_SUFFIX: &str = ".tmp";

/// The longest session id this tool will name a file after. The runtime's ids are
/// `ses_<32 hex>`, 36 characters; this leaves room for any other host while keeping the
/// name far below every filesystem's component limit.
pub const MAX_SESSION_ID_CHARS: usize = 128;

/// Whether `id` can name a report file: non-empty, at most [`MAX_SESSION_ID_CHARS`], and
/// only ASCII alphanumerics, `_` and `-`.
///
/// The character rule is what keeps a hostile value such as `../x` from naming a path
/// outside `reports/`, and it is the same rule retention uses to decide which files are this
/// tool's to remove.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The file name a session's report is written under: `<id>.json`. Only meaningful for an id
/// that passes [`valid_session_id`].
pub fn report_file_name(session_id: &str) -> String {
    format!("{session_id}{REPORT_EXTENSION}")
}

/// A handle on an existing, reachable state directory and the one session writing to it.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    session_id: String,
}

impl Store {
    /// Open the state directory for one session, or fail with `state_unavailable` (checked
    /// first) or `session_unavailable`.
    ///
    /// This only ever *reads* directory metadata. A missing directory is reported, never
    /// repaired, and nothing is created on either refusal.
    pub fn open(state_dir: &Path, session_id: &str) -> Result<Self, OpError> {
        match fs::metadata(state_dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(OpError::new(
                    kind::STATE_UNAVAILABLE,
                    format!(
                        "durable state path \"{}\" is not a directory; the capsule must grant \
                         capabilities.state on this tool's entry",
                        state_dir.display()
                    ),
                ))
            }
            Err(_) => {
                return Err(OpError::new(
                    kind::STATE_UNAVAILABLE,
                    format!(
                        "durable state directory \"{}\" is not available; the capsule must \
                         grant capabilities.state on this tool's entry for the report to be \
                         written anywhere the consumer can read it",
                        state_dir.display()
                    ),
                ))
            }
        }
        if !valid_session_id(session_id) {
            return Err(OpError::new(
                kind::SESSION_UNAVAILABLE,
                if session_id.is_empty() {
                    "no session id; the report is written to state/reports/<session_id>.json and \
                     the runtime sets MURMUR_SESSION_ID in every guest it launches"
                        .to_string()
                } else {
                    format!(
                        "the session id is not usable as a report file name (it must be 1 to \
                         {MAX_SESSION_ID_CHARS} characters of A-Z, a-z, 0-9, _ and -); nothing \
                         was written"
                    )
                },
            ));
        }
        Ok(Self {
            root: state_dir.to_path_buf(),
            session_id: session_id.to_string(),
        })
    }

    /// The directory every session's report sits in.
    pub fn reports_dir(&self) -> PathBuf {
        self.root.join(REPORTS_DIR)
    }

    /// This session's report file: `<state>/reports/<session_id>.json`.
    pub fn report_path(&self) -> PathBuf {
        self.reports_dir().join(report_file_name(&self.session_id))
    }

    fn temp_path(&self) -> PathBuf {
        self.reports_dir().join(format!(
            "{}{TEMP_SUFFIX}",
            report_file_name(&self.session_id)
        ))
    }

    /// This session's report on disk, or `None` when the session has never reported.
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

    /// Write this session's report, replacing any previous one atomically, then run the
    /// best-effort [`retention::sweep`] over `reports/`. Only the write can fail the call.
    ///
    /// `reports/` is created here on first use, with `create_dir` and never `create_dir_all`:
    /// the state directory itself must already exist, and a recursive create would conjure
    /// it inside the workdir when the grant is missing.
    ///
    /// Pretty-printed with a trailing newline: this file is read by `cat` as often as by a
    /// parser, and it parses identically either way.
    pub fn save(&self, doc: &ReportDoc) -> Result<(), OpError> {
        let dir = self.reports_dir();
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(OpError::new(
                    kind::IO_ERROR,
                    format!("cannot create \"{}\": {e}", dir.display()),
                ))
            }
        }

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
        })?;

        retention::sweep(&dir, &report_file_name(&self.session_id));
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_session_id_and_the_test_ids_are_valid() {
        for id in [
            "ses_0192f3a4b5c67d8e9f0a1b2c3d4e5f60",
            "sess-abc",
            "sess-wasm-1",
        ] {
            assert!(valid_session_id(id), "{id}");
        }
        assert!(valid_session_id(&"a".repeat(MAX_SESSION_ID_CHARS)));
    }

    #[test]
    fn an_empty_traversing_dotted_spaced_or_over_long_id_is_refused() {
        let too_long = "a".repeat(MAX_SESSION_ID_CHARS + 1);
        for id in [
            "",
            "../x",
            "a/b",
            "a.b",
            "a b",
            "a\\b",
            "é",
            too_long.as_str(),
        ] {
            assert!(!valid_session_id(id), "{id:?}");
        }
    }

    #[test]
    fn the_file_name_is_the_id_verbatim_plus_json() {
        assert_eq!(report_file_name("ses_ABC-1"), "ses_ABC-1.json");
    }
}
