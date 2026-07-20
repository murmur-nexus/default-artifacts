//! Minimal LCOV (`.info`) parser for line coverage.
//!
//! `cargo llvm-cov --lcov` emits standard LCOV: a stream of records, each
//! introduced by `SF:<source-file>` and terminated by `end_of_record`, carrying
//! per-line hit counts as `DA:<line>,<count>`. For spectrum-based fault
//! localization we need only *which lines were executed* per source file, so
//! this parser reads exactly two record types and ignores the rest.
//!
//! Read:
//!   • `SF:<path>`        — starts a new source-file section.
//!   • `DA:<line>,<hits>` — a line with `<hits> > 0` is a covered line.
//!
//! Ignored (function/branch/summary records not needed for line SBFL):
//!   `TN:`, `FN:`, `FNDA:`, `FNF:`, `FNH:`, `BRDA:`, `BRF:`, `BRH:`, `LF:`,
//!   `LH:`, `end_of_record`, and anything else.
//!
//! A `.info` file with no valid `SF:`/`DA:` records parses to an empty result;
//! the caller treats that as "unparseable / skipped".

/// One source-file section: its LCOV `SF:` path (verbatim, not yet normalized to
/// the repo-relative form) and the set of 1-based line numbers with a non-zero
/// hit count.
pub struct FileCoverage {
    pub source_file: String,
    pub hit_lines: Vec<u32>,
}

/// Parse LCOV text into per-source-file covered-line sets. Returns an empty `Vec`
/// when the text contains no usable `SF:`+`DA:` coverage (the caller reports such
/// a file as skipped). A section whose `DA:` lines are all zero-hit yields an
/// empty `hit_lines` but still counts as a parsed section.
pub fn parse(text: &str) -> Vec<FileCoverage> {
    let mut out: Vec<FileCoverage> = Vec::new();
    let mut current: Option<FileCoverage> = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(sf) = line.strip_prefix("SF:") {
            // A new section begins; flush any prior one.
            if let Some(prev) = current.take() {
                out.push(prev);
            }
            current = Some(FileCoverage { source_file: sf.trim().to_string(), hit_lines: Vec::new() });
        } else if let Some(da) = line.strip_prefix("DA:") {
            // DA:<line>,<hits>[,<checksum>]
            if let Some(cur) = current.as_mut() {
                let mut parts = da.split(',');
                let line_no = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
                let hits = parts.next().and_then(|s| s.trim().parse::<i64>().ok());
                if let (Some(l), Some(h)) = (line_no, hits) {
                    if h > 0 {
                        cur.hit_lines.push(l);
                    }
                }
            }
        } else if line == "end_of_record" {
            if let Some(prev) = current.take() {
                out.push(prev);
            }
        }
        // All other record types are intentionally ignored.
    }
    if let Some(prev) = current.take() {
        out.push(prev);
    }

    // Drop sections that carried an `SF:` but no numeric `DA:` at all — they add
    // no covered lines and only clutter the "which files did this test touch"
    // view. A section retained here has at least one hit line.
    out.retain(|f| !f.hit_lines.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hit_lines_only() {
        let text = "TN:\nSF:/repo/src/lib.rs\nDA:1,3\nDA:2,0\nDA:5,1\nLF:3\nLH:2\nend_of_record\n";
        let cov = parse(text);
        assert_eq!(cov.len(), 1);
        assert_eq!(cov[0].source_file, "/repo/src/lib.rs");
        assert_eq!(cov[0].hit_lines, vec![1, 5]);
    }

    #[test]
    fn multiple_sections() {
        let text = "SF:a.rs\nDA:1,1\nend_of_record\nSF:b.rs\nDA:2,1\nDA:3,1\nend_of_record\n";
        let cov = parse(text);
        assert_eq!(cov.len(), 2);
        assert_eq!(cov[0].source_file, "a.rs");
        assert_eq!(cov[1].hit_lines, vec![2, 3]);
    }

    #[test]
    fn no_records_is_empty() {
        assert!(parse("this is not lcov\njust prose\n").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn all_zero_hits_dropped() {
        assert!(parse("SF:a.rs\nDA:1,0\nDA:2,0\nend_of_record\n").is_empty());
    }
}
