//! Make the parity results of two runs over two fresh fixture folders comparable.
//!
//! - Each work-folder path becomes `<WORK>`.
//! - Each distinct 40-hex hash becomes `<C1>`, `<C2>`, … in first-seen order over the whole run,
//!   and an abbreviation of one (a word of 7 to 39 hex digits it starts with) `<Cn:len>`. An
//!   abbreviation of an object whose full hash never appears becomes `<H1>`, `<H2>`, … in
//!   first-seen order.
//! - Each ISO-8601 date-time (`2026-01-01T00:00:00+00:00`, or with `Z`) becomes `<DATE>`.
//!
//! A word is a maximal run of ASCII letters and digits. A full hash, or an abbreviation of no
//! known hash, must be lowercase hex holding at least one digit, so that words like `defaced`
//! stay as they are; an abbreviation of a known hash may be all letters.

use serde_json::Value;

pub struct Normaliser {
    work: Vec<String>,
    full: Vec<String>,
    orphans: Vec<String>,
}

impl Normaliser {
    /// A normaliser for a run whose work folder is reached at each of `work`.
    pub fn new(work: &[String]) -> Self {
        let mut work = work.to_vec();
        work.sort_by_key(|w| std::cmp::Reverse(w.len()));
        Normaliser {
            work,
            full: Vec::new(),
            orphans: Vec::new(),
        }
    }

    /// Normalise every string in `results`, a run's results in call order.
    pub fn run(mut self, results: &[Value]) -> Vec<Value> {
        for r in results {
            self.collect(r);
        }
        results.iter().map(|r| self.value(r)).collect()
    }

    fn collect(&mut self, v: &Value) {
        match v {
            Value::String(s) => {
                for word in words(s) {
                    if word.len() == 40 && is_hashlike(word) && !self.full.iter().any(|f| f == word)
                    {
                        self.full.push(word.to_string());
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| self.collect(x)),
            Value::Object(o) => o.values().for_each(|x| self.collect(x)),
            _ => {}
        }
    }

    fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), self.value(x))).collect())
            }
            other => other.clone(),
        }
    }

    fn text(&mut self, s: &str) -> String {
        let mut s = s.to_string();
        for w in &self.work {
            s = s.replace(w.as_str(), "<WORK>");
        }
        let s = dates(&s);
        let mut out = String::with_capacity(s.len());
        let mut word = String::new();
        for c in s.chars().chain(std::iter::once('\0')) {
            if c.is_ascii_alphanumeric() {
                word.push(c);
                continue;
            }
            out.push_str(&self.word(&word));
            word.clear();
            if c != '\0' {
                out.push(c);
            }
        }
        out
    }

    fn word(&mut self, word: &str) -> String {
        if word.len() < 7 || word.len() > 40 || !is_hex(word) {
            return word.to_string();
        }
        if let Some(i) = self.full.iter().position(|f| f.starts_with(word)) {
            return if word.len() == 40 {
                format!("<C{}>", i + 1)
            } else {
                format!("<C{}:{}>", i + 1, word.len())
            };
        }
        if !is_hashlike(word) {
            return word.to_string();
        }
        let i = match self.orphans.iter().position(|o| o == word) {
            Some(i) => i,
            None => {
                self.orphans.push(word.to_string());
                self.orphans.len() - 1
            }
        };
        format!("<H{}>", i + 1)
    }
}

fn is_hex(word: &str) -> bool {
    word.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_hashlike(word: &str) -> bool {
    is_hex(word) && word.bytes().any(|b| b.is_ascii_digit())
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
}

/// `s` with each `YYYY-MM-DDTHH:MM:SS` followed by `Z` or `±HH:MM` replaced by `<DATE>`.
fn dates(s: &str) -> String {
    const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd";
    let b = s.as_bytes();
    let fits = |at: usize, shape: &[u8]| {
        at + shape.len() <= b.len()
            && shape.iter().enumerate().all(|(i, &want)| {
                let got = b[at + i];
                if want == b'd' {
                    got.is_ascii_digit()
                } else {
                    got == want
                }
            })
    };
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if fits(i, SHAPE) {
            let end = i + SHAPE.len();
            let zone = if end < b.len() && b[end] == b'Z' {
                Some(1)
            } else if end < b.len() && (b[end] == b'+' || b[end] == b'-') && fits(end + 1, b"dd:dd")
            {
                Some(6)
            } else {
                None
            };
            if let Some(z) = zone {
                out.push_str("<DATE>");
                i = end + z;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}
