//! The report document, exactly as it sits on disk, and the run identity stamped into it.
//!
//! Field order here is the on-disk key order — `serde_json` emits struct fields in
//! declaration order — so a person who cats the file reads the verdict before its history:
//! which run wrote it, whether it concluded, what it concluded, then the evidence, then
//! everything it superseded.
//!
//! The document is deserialised strictly. A file this tool cannot read is an `io_error`
//! rather than a fresh empty document, because "start over" and "the previous verdict is
//! unreadable" are not the same thing and only one of them is safe to act on.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Format version of the document, stamped into every file this build writes. A consumer
/// reads it before anything else; a later shape bumps it.
pub const REPORT_VERSION: u64 = 1;

/// How many superseded reports the file carries. Older ones fall off the front while
/// `superseded_count` keeps counting, so the file is bounded and still honest about how
/// many verdicts came before. The most recent revisions are the ones kept: they are the
/// ones a consumer is deciding between.
pub const MAX_SUPERSEDED: usize = 20;

/// How many progress notes the file carries, on the same terms as [`MAX_SUPERSEDED`] —
/// oldest dropped first, `progress_count` always the true total.
pub const MAX_PROGRESS_NOTES: usize = 100;

/// Which run produced a report.
///
/// Both fields are best-effort: the runtime pushes them into every guest environment, and
/// a call made without them records `""` rather than failing. The store outlives every
/// session, so an unstamped report would be indistinguishable from a stale one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunIdentity {
    pub session_id: String,
    pub capsule: String,
}

impl RunIdentity {
    /// Build an identity from what the environment offered, substituting `""` for either
    /// variable the host did not set.
    pub fn new(session_id: Option<String>, capsule: Option<String>) -> Self {
        Self {
            session_id: session_id.unwrap_or_default(),
            capsule: capsule.unwrap_or_default(),
        }
    }
}

/// One deliverable, held by reference and never by value.
///
/// `uri` is a workspace path or a URI. It is never an inline payload — a report is a
/// pointer to work, not a copy of it, and a tool that accepted bodies here would become a
/// second, unreviewable place the work lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliverable {
    pub name: String,
    /// Operator-defined kind tag. Opaque: nothing in this crate branches on its value.
    pub kind: String,
    pub uri: String,
}

/// One note addressed at a downstream stage.
///
/// `stage` is an opaque operator-defined string — this artifact never interprets it, and
/// assumes no board, no stage name and no card, because "what did this capsule conclude"
/// is asked by CI, a parent capsule and a person reading a trace equally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteFor {
    pub stage: String,
    pub body: String,
}

/// One progress note and when it was filed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressNote {
    /// RFC 3339 UTC, millisecond precision.
    pub at: String,
    pub note: String,
}

/// A conclusion that a later `report` call replaced.
///
/// A second verdict is legitimate and wins, but the one it replaced stays readable in the
/// same file: a silently overwritten first verdict is how a consumer ends up trusting the
/// wrong one without ever learning that the capsule changed its mind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededReport {
    pub outcome: String,
    pub summary: String,
    pub deliverables: Vec<Deliverable>,
    pub notes_for: Vec<NoteFor>,
    pub reported_at: String,
}

/// The whole of `state/report.json`.
///
/// `concluded` is the field a consumer branches on, and it is the only one that has to be
/// read to tell the three states apart. `outcome`, `summary` and `reported_at` are `null`
/// together until a `report` call sets them together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportDoc {
    pub report_version: u64,
    pub capsule: String,
    pub session_id: String,
    pub concluded: bool,
    pub outcome: Option<String>,
    pub summary: Option<String>,
    pub deliverables: Vec<Deliverable>,
    pub notes_for: Vec<NoteFor>,
    pub reported_at: Option<String>,
    /// How many terminal reports have been filed. `0` until the first one lands, so a
    /// progress-only file and a concluded file are distinguishable by this too.
    pub revision: u64,
    pub superseded: Vec<SupersededReport>,
    /// Every superseded report ever pushed, including those dropped by [`MAX_SUPERSEDED`].
    pub superseded_count: u64,
    pub progress: Vec<ProgressNote>,
    /// Every progress note ever filed, including those dropped by [`MAX_PROGRESS_NOTES`].
    pub progress_count: u64,
}

impl ReportDoc {
    /// The document a state directory with no report yet stands for: nothing concluded,
    /// nothing filed, stamped with the run that is about to write to it.
    pub fn new(identity: &RunIdentity) -> Self {
        Self {
            report_version: REPORT_VERSION,
            capsule: identity.capsule.clone(),
            session_id: identity.session_id.clone(),
            concluded: false,
            outcome: None,
            summary: None,
            deliverables: Vec::new(),
            notes_for: Vec::new(),
            reported_at: None,
            revision: 0,
            superseded: Vec::new(),
            superseded_count: 0,
            progress: Vec::new(),
            progress_count: 0,
        }
    }

    /// Stamp the run that is writing now. Every call restamps: the store outlives the
    /// session, so the last writer is the one a consumer needs to be able to identify.
    pub fn stamp(&mut self, identity: &RunIdentity) {
        self.capsule = identity.capsule.clone();
        self.session_id = identity.session_id.clone();
    }

    /// File a terminal verdict, pushing any previous one onto `superseded`.
    pub fn conclude(
        &mut self,
        outcome: String,
        summary: String,
        deliverables: Vec<Deliverable>,
        notes_for: Vec<NoteFor>,
        reported_at: String,
    ) {
        if let (Some(previous_outcome), Some(previous_summary), Some(previous_at)) = (
            self.outcome.take(),
            self.summary.take(),
            self.reported_at.take(),
        ) {
            self.superseded.push(SupersededReport {
                outcome: previous_outcome,
                summary: previous_summary,
                deliverables: std::mem::take(&mut self.deliverables),
                notes_for: std::mem::take(&mut self.notes_for),
                reported_at: previous_at,
            });
            self.superseded_count += 1;
            trim_front(&mut self.superseded, MAX_SUPERSEDED);
        }

        self.concluded = true;
        self.outcome = Some(outcome);
        self.summary = Some(summary);
        self.deliverables = deliverables;
        self.notes_for = notes_for;
        self.reported_at = Some(reported_at);
        self.revision += 1;
    }

    /// File a progress note. It concludes nothing: `concluded`, `outcome`, `summary`,
    /// `reported_at` and `revision` are all left exactly as they were.
    pub fn add_progress(&mut self, note: String, at: String) {
        self.progress.push(ProgressNote { at, note });
        self.progress_count += 1;
        trim_front(&mut self.progress, MAX_PROGRESS_NOTES);
    }
}

/// Drop entries from the front until at most `cap` remain, keeping the newest.
fn trim_front<T>(items: &mut Vec<T>, cap: usize) {
    if items.len() > cap {
        items.drain(..items.len() - cap);
    }
}

/// The current wall-clock instant as RFC 3339 UTC with millisecond precision.
///
/// `SystemTime::now()` resolves through `wasi:clocks/wall-clock` in a `wasm32-wasip2`
/// guest, so this works identically on the host and in the component. Formatted by hand;
/// a date-time crate would be a dependency bought for one `format!`.
pub fn now_rfc3339_millis() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339_millis(since_epoch.as_millis() as i64)
}

/// RFC 3339 UTC rendering of a Unix millisecond timestamp.
pub fn format_rfc3339_millis(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let seconds_of_day = ms_of_day / 1000;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
        ms_of_day % 1000
    )
}

/// Days since the Unix epoch to a proleptic Gregorian `(year, month, day)`, via Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> RunIdentity {
        RunIdentity::new(Some("sess-1".into()), Some("capsule-1".into()))
    }

    fn deliverable(name: &str) -> Deliverable {
        Deliverable { name: name.into(), kind: "diff".into(), uri: "out/a.patch".into() }
    }

    #[test]
    fn a_fresh_document_has_concluded_false_and_every_terminal_field_null() {
        let doc = ReportDoc::new(&identity());
        assert!(!doc.concluded);
        assert_eq!(doc.outcome, None);
        assert_eq!(doc.summary, None);
        assert_eq!(doc.reported_at, None);
        assert_eq!(doc.revision, 0);
        assert_eq!(doc.report_version, REPORT_VERSION);
    }

    #[test]
    fn a_second_conclusion_pushes_the_first_onto_superseded() {
        let mut doc = ReportDoc::new(&identity());
        doc.conclude(
            "success".into(),
            "first".into(),
            vec![deliverable("patch")],
            Vec::new(),
            "2026-01-01T00:00:00.000Z".into(),
        );
        doc.conclude(
            "blocked".into(),
            "second".into(),
            Vec::new(),
            Vec::new(),
            "2026-01-01T00:01:00.000Z".into(),
        );

        assert_eq!(doc.outcome.as_deref(), Some("blocked"));
        assert_eq!(doc.summary.as_deref(), Some("second"));
        assert_eq!(doc.revision, 2);
        assert_eq!(doc.superseded.len(), 1);
        assert_eq!(doc.superseded_count, 1);
        assert_eq!(doc.superseded[0].outcome, "success");
        assert_eq!(doc.superseded[0].deliverables, vec![deliverable("patch")]);
    }

    #[test]
    fn superseded_is_capped_while_its_count_stays_true() {
        let mut doc = ReportDoc::new(&identity());
        for n in 0..MAX_SUPERSEDED + 5 {
            doc.conclude(
                "success".into(),
                format!("verdict {n}"),
                Vec::new(),
                Vec::new(),
                format!("2026-01-01T00:00:{n:02}.000Z"),
            );
        }
        assert_eq!(doc.revision as usize, MAX_SUPERSEDED + 5);
        assert_eq!(doc.superseded.len(), MAX_SUPERSEDED);
        assert_eq!(doc.superseded_count as usize, MAX_SUPERSEDED + 4);
        // The newest kept, the oldest dropped.
        assert_eq!(doc.superseded.last().unwrap().summary, format!("verdict {}", MAX_SUPERSEDED + 3));
    }

    #[test]
    fn progress_is_capped_while_its_count_stays_true() {
        let mut doc = ReportDoc::new(&identity());
        for n in 0..MAX_PROGRESS_NOTES + 3 {
            doc.add_progress(format!("note {n}"), "2026-01-01T00:00:00.000Z".into());
        }
        assert_eq!(doc.progress.len(), MAX_PROGRESS_NOTES);
        assert_eq!(doc.progress_count as usize, MAX_PROGRESS_NOTES + 3);
        assert_eq!(doc.progress[0].note, "note 3");
    }

    #[test]
    fn progress_does_not_conclude_and_a_conclusion_keeps_the_notes() {
        let mut doc = ReportDoc::new(&identity());
        doc.add_progress("one".into(), "2026-01-01T00:00:00.000Z".into());
        assert!(!doc.concluded);
        assert_eq!(doc.revision, 0);

        doc.conclude(
            "blocked".into(),
            "stuck".into(),
            Vec::new(),
            Vec::new(),
            "2026-01-01T00:00:01.000Z".into(),
        );
        assert!(doc.concluded);
        assert_eq!(doc.progress.len(), 1);
        assert_eq!(doc.progress[0].note, "one");
    }

    #[test]
    fn timestamps_render_as_rfc_3339_utc() {
        assert_eq!(format_rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_millis(1_767_225_600_123), "2026-01-01T00:00:00.123Z");
    }
}
