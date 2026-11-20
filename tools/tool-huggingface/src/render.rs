//! The text budget every op renders within.
//!
//! The runtime puts no size cap on a WASM tool's result, so `max_output_bytes` is the only
//! one: the rendered text, trailer included, never exceeds it. Upstream text passes through
//! byte for byte apart from that cut. Nothing is sanitised, escaped or fenced here; the
//! runtime's fence around every tool result is what protects the model.
//!
//! Each op renders into a [`Page`] with a byte limit. [`finish`] renders once against the
//! whole budget; when that page fits and nothing was cut, it is the result. Otherwise the full
//! upstream response is spilled, the op renders again against the budget less
//! [`TRAILER_RESERVE`], and a `[truncated: …]` trailer naming what was cut and where the full
//! response went ends the text.

/// The ellipsis a cut ends with.
pub const ELLIPSIS: &str = "…";
/// Bytes kept free for the trailer on a truncated page.
pub const TRAILER_RESERVE: usize = 320;

/// `text` cut to at most `max` bytes at a UTF-8 character boundary, ending in `…` when cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let Some(keep) = max.checked_sub(ELLIPSIS.len()) else {
        return String::new();
    };
    let mut end = keep;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &text[..end])
}

/// `text` cut to at most `max` characters, ending in `…` when cut, and whether it was cut.
pub fn cut_chars(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        None => (text.to_string(), false),
        Some((end, _)) => (format!("{}{ELLIPSIS}", &text[..end]), true),
    }
}

/// Text under construction, never longer than its limit, and the reasons anything was cut.
#[derive(Debug, Clone)]
pub struct Page {
    pub text: String,
    limit: usize,
    pub cuts: Vec<String>,
}

impl Page {
    pub fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            cuts: Vec::new(),
        }
    }

    /// Bytes still free.
    pub fn room(&self) -> usize {
        self.limit.saturating_sub(self.text.len())
    }

    /// Append `part`, cut to the room left when it does not fit.
    pub fn push(&mut self, part: &str, what: &str) {
        if part.len() <= self.room() {
            self.text.push_str(part);
        } else {
            let room = self.room();
            self.text.push_str(&cut(part, room));
            self.note(format!("{what} cut to fit"));
        }
    }

    /// Append `part` only if it fits whole.
    pub fn try_push(&mut self, part: &str) -> bool {
        if part.len() <= self.room() {
            self.text.push_str(part);
            true
        } else {
            false
        }
    }

    /// Record a cut, once per reason.
    pub fn note(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if !self.cuts.contains(&reason) {
            self.cuts.push(reason);
        }
    }
}

/// The rendered text and how much of the response it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub truncated: bool,
    /// Where the full response was written, when it was truncated and the write succeeded.
    pub data_path: Option<String>,
}

/// Render within `budget` bytes, spilling and adding a trailer when anything is cut.
pub fn finish(
    budget: usize,
    render: impl Fn(usize) -> Page,
    spill: impl FnOnce() -> Option<String>,
) -> Rendered {
    let whole = render(budget);
    if whole.cuts.is_empty() && whole.text.len() <= budget {
        return Rendered {
            text: whole.text,
            truncated: false,
            data_path: None,
        };
    }
    let data_path = spill();
    let page = render(budget.saturating_sub(TRAILER_RESERVE));
    let mut text = page.text;
    let mut cuts = page.cuts;
    if cuts.is_empty() {
        cuts.push("output cut to fit".to_string());
    }
    let saved = match &data_path {
        Some(path) => format!("full response in {path}"),
        None => "the full response could not be saved".to_string(),
    };
    // `[truncated: ` + reasons + `; ` + saved + `]`, after a blank line.
    let fixed = 2 + "[truncated: ".len() + 2 + saved.len() + 1;
    let reasons = cut(&cuts.join("; "), TRAILER_RESERVE.saturating_sub(fixed));
    text.push_str(&format!("\n\n[truncated: {reasons}; {saved}]"));
    if text.len() > budget {
        text = cut(&text, budget);
    }
    Rendered {
        text,
        truncated: true,
        data_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_at_char_boundaries() {
        assert_eq!(cut("abcdef", 6), "abcdef");
        assert_eq!(cut("abcdef", 5), "ab…");
        assert_eq!(cut("ééé", 5), "é…");
        assert_eq!(cut("abc", 2), "");
        assert_eq!(cut_chars("abcdef", 3), ("abc…".to_string(), true));
    }

    #[test]
    fn finish_spills_only_when_cut() {
        let fits = finish(
            2048,
            |limit| {
                let mut page = Page::new(limit);
                page.push("short", "x");
                page
            },
            || panic!("no spill"),
        );
        assert!(!fits.truncated);
        let cut = finish(
            2048,
            |limit| {
                let mut page = Page::new(limit);
                page.push(&"x".repeat(5000), "body");
                page
            },
            || Some("hf-results/search-1.json".into()),
        );
        assert!(cut.truncated);
        assert!(cut.text.len() <= 2048);
        assert!(
            cut.text
                .ends_with("full response in hf-results/search-1.json]"),
            "{}",
            cut.text
        );
    }
}
