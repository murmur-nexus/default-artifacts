//! Keeping `state/reports/` bounded without removing a running session's file.
//!
//! Every session writes its own file, so the directory grows by one file per session that
//! ever called this tool. After each successful save the store sweeps it: while more than
//! [`MAX_REPORT_FILES`] report files exist, the ones last written longest ago go first — but
//! never the caller's own, and never one written within [`REPORT_RETENTION_GRACE`].
//!
//! A component cannot see whether another session is still running: it holds only its
//! `state` and workdir preopens and its own environment. Ordering by last write puts every
//! session still calling this tool at the young end, and the grace turns that into a
//! guarantee: a session that called `report` or `progress` within the grace keeps its file
//! however many other sessions run meanwhile. The residual case is a session still running
//! that has not called for longer than the grace while at least [`MAX_REPORT_FILES`] other
//! sessions wrote; its file can go, and its next call starts a fresh one. Closing that gap
//! needs a liveness signal the runtime does not give a component.
//!
//! The policy is the pure [`select_for_removal`]; [`sweep`] only reads the directory and
//! acts on its answer, best-effort throughout.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::store::{valid_session_id, REPORT_EXTENSION};

/// How many report files `state/reports/` may hold before the oldest are removed.
///
/// It sits above the sessions of one capsule running at once on one machine plus the
/// finished sessions whose report has not been read yet. Both cases that run sessions at once
/// — cards sharing a Transit stage, formation members sharing a capsule — are single-digit,
/// so this leaves an order of magnitude of headroom, and 64 recent verdicts is enough history
/// to audit a capsule by. Each file is already capped inside, so a fixed count bounds the
/// store.
pub const MAX_REPORT_FILES: usize = 64;

/// How recently a report file must have been written to be immune from removal, whatever
/// the count. Sessions of the cases this tool serves write within hours, so a week keeps
/// every one of them.
pub const REPORT_RETENTION_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One report file as retention sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    /// The file name, `<session_id>.json`.
    pub name: String,
    /// When the file was last written.
    pub modified: SystemTime,
}

/// Which of `entries` to remove: nothing while `max` or fewer exist, otherwise the oldest
/// `modified` first, ties broken by name, skipping `own_name` and every entry younger than
/// `grace`, until `max` would remain or the candidates run out.
///
/// An entry modified after `now` counts as age zero, so a clock that disagrees with a file's
/// mtime keeps the file rather than removing it.
pub fn select_for_removal(
    entries: &[ReportEntry],
    own_name: &str,
    now: SystemTime,
    max: usize,
    grace: Duration,
) -> Vec<String> {
    if entries.len() <= max {
        return Vec::new();
    }
    let excess = entries.len() - max;

    let mut candidates: Vec<&ReportEntry> = entries
        .iter()
        .filter(|entry| entry.name != own_name && !within_grace(entry.modified, now, grace))
        .collect();
    candidates.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.name.cmp(&b.name))
    });

    candidates
        .into_iter()
        .take(excess)
        .map(|entry| entry.name.clone())
        .collect()
}

/// Trim `reports_dir` after a successful save by the session whose file is `own_name`.
///
/// Only `<valid session id>.json` regular files are counted or removed: a temp file, a
/// foreign name and anything outside `reports_dir` are invisible here. Each selected file's
/// mtime is read again just before removal and the file is kept if it was written within the
/// grace since the listing. Every error — a file another session already removed included —
/// is ignored: retention never changes the outcome of the call that triggered it.
pub fn sweep(reports_dir: &Path, own_name: &str) {
    let Ok(listing) = fs::read_dir(reports_dir) else {
        return;
    };
    let entries: Vec<ReportEntry> = listing
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !is_report_file_name(&name) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(ReportEntry {
                name,
                modified: meta.modified().ok()?,
            })
        })
        .collect();

    let selected = select_for_removal(
        &entries,
        own_name,
        SystemTime::now(),
        MAX_REPORT_FILES,
        REPORT_RETENTION_GRACE,
    );
    for name in selected {
        let path = reports_dir.join(&name);
        // Another session may have written this file since the listing; a write inside the
        // grace makes it immune again.
        let Ok(modified) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
            continue;
        };
        if within_grace(modified, SystemTime::now(), REPORT_RETENTION_GRACE) {
            continue;
        }
        let _ = fs::remove_file(&path);
    }
}

/// Whether `name` is one this tool writes: `<valid session id>.json`.
fn is_report_file_name(name: &str) -> bool {
    name.strip_suffix(REPORT_EXTENSION)
        .is_some_and(valid_session_id)
}

fn within_grace(modified: SystemTime, now: SystemTime, grace: Duration) -> bool {
    now.duration_since(modified).map_or(true, |age| age < grace)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    /// An entry `days_old` days before [`now`].
    fn entry(name: &str, days_old: u64) -> ReportEntry {
        ReportEntry {
            name: name.to_string(),
            modified: now() - DAY * days_old as u32,
        }
    }

    #[test]
    fn at_the_bound_nothing_is_selected() {
        let entries: Vec<ReportEntry> = (0..4).map(|n| entry(&format!("s{n}.json"), 30)).collect();
        assert!(select_for_removal(&entries, "s0.json", now(), 4, DAY * 7).is_empty());
    }

    #[test]
    fn above_the_bound_the_oldest_go_first_ties_broken_by_name() {
        let entries = vec![
            entry("d.json", 10),
            entry("b.json", 20),
            entry("a.json", 20),
            entry("c.json", 30),
            entry("own.json", 8),
        ];
        assert_eq!(
            select_for_removal(&entries, "own.json", now(), 2, DAY * 7),
            vec!["c.json", "a.json", "b.json"]
        );
    }

    #[test]
    fn the_own_file_is_never_selected_even_when_it_is_the_oldest() {
        let entries = vec![
            entry("own.json", 100),
            entry("a.json", 50),
            entry("b.json", 40),
        ];
        assert_eq!(
            select_for_removal(&entries, "own.json", now(), 1, DAY * 7),
            vec!["a.json", "b.json"]
        );
        assert_eq!(
            select_for_removal(&entries, "own.json", now(), 0, DAY * 7),
            vec!["a.json", "b.json"]
        );
    }

    #[test]
    fn entries_inside_the_grace_are_never_selected_even_over_the_bound() {
        let entries = vec![
            entry("a.json", 6),
            entry("b.json", 1),
            entry("c.json", 0),
            entry("old.json", 8),
            ReportEntry {
                name: "future.json".into(),
                modified: now() + DAY,
            },
        ];
        assert_eq!(
            select_for_removal(&entries, "c.json", now(), 1, DAY * 7),
            vec!["old.json"]
        );
    }

    #[test]
    fn selection_stops_once_max_would_remain() {
        let entries: Vec<ReportEntry> = (0..10)
            .map(|n| entry(&format!("s{n}.json"), 100 - n))
            .collect();
        assert_eq!(
            select_for_removal(&entries, "s9.json", now(), 7, DAY * 7),
            vec!["s0.json", "s1.json", "s2.json"]
        );
    }

    #[test]
    fn only_names_this_tool_writes_count_as_report_files() {
        assert!(is_report_file_name(
            "ses_0192f3a4b5c67d8e9f0a1b2c3d4e5f60.json"
        ));
        for name in [
            "ses_x.json.tmp",
            "notes.txt",
            "bad name.json",
            ".json",
            "a.b.json",
            "../x.json",
        ] {
            assert!(!is_report_file_name(name), "{name}");
        }
    }
}
