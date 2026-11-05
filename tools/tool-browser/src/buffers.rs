//! The console and network logs a page builds up between the calls that read them.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// The most entries each log keeps; older ones are dropped and counted.
pub const CONSOLE_CAP: usize = 500;
pub const NETWORK_CAP: usize = 200;
/// The longest entry kept, in characters.
pub const ENTRY_MAX_CHARS: usize = 2_000;

/// Saved in `state.json` between calls, unread entries and the dropped count with them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogBuffer {
    entries: VecDeque<String>,
    cap: usize,
    dropped: usize,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap,
            dropped: 0,
        }
    }

    pub fn push(&mut self, entry: &str) {
        if self.entries.len() == self.cap {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries
            .push_back(crate::util::one_line(entry, ENTRY_MAX_CHARS));
    }

    /// Every entry since the last drain, oldest first, behind a line counting what was dropped.
    pub fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.entries.len() + 1);
        if self.dropped > 0 {
            out.push(format!("… {} earlier entries dropped", self.dropped));
        }
        out.extend(self.entries.drain(..));
        self.dropped = 0;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_counts_dropped_entries() {
        let mut b = LogBuffer::new(3);
        for i in 0..5 {
            b.push(&format!("e{i}"));
        }
        assert_eq!(
            b.drain(),
            vec!["… 2 earlier entries dropped", "e2", "e3", "e4"]
        );
        assert!(b.drain().is_empty());
    }

    #[test]
    fn round_trips_through_json() {
        let mut b = LogBuffer::new(2);
        for e in ["a", "b", "c"] {
            b.push(e);
        }
        let mut back: LogBuffer =
            serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back.drain(), vec!["… 1 earlier entries dropped", "b", "c"]);
    }

    #[test]
    fn cuts_long_entries() {
        let mut b = LogBuffer::new(CONSOLE_CAP);
        b.push(&"x".repeat(5_000));
        let entry = &b.drain()[0];
        assert_eq!(entry.chars().count(), ENTRY_MAX_CHARS + 1);
        assert!(entry.ends_with('…'));
    }
}
