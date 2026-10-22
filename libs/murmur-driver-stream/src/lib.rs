//! Slot tables a driver grows from indexes a provider's stream names.
//!
//! A streamed turn addresses its parts by index: a chat-completions chunk names the tool call
//! a delta belongs to, an Anthropic event names the content block it starts. A driver keeps
//! one slot per index in a `Vec`, so the index sizes an allocation. The index is the
//! provider's, not the driver's, and nothing in the protocol bounds it: a stream naming index
//! 4 294 967 296 would have the guest allocate billions of slots and die. [`slot_mut`] is the
//! one place a table grows from such an index, and it refuses anything past
//! [`MAX_SLOTS_PER_TURN`] before it allocates.

use std::fmt;

/// The most slots one turn's stream may name: indexes 0 through 1023.
///
/// A fixed ceiling, because the risk is allocation size, and a fixed ceiling bounds it
/// absolutely — 1024 slots of a few dozen bytes each. No real turn approaches it: a turn's
/// slots are its tool calls or content blocks, far fewer than this.
pub const MAX_SLOTS_PER_TURN: usize = 1024;

/// An index at or past [`MAX_SLOTS_PER_TURN`], refused by [`slot_mut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexPastLimit {
    /// The index the stream named, as it arrived.
    pub index: u64,
}

impl fmt::Display for IndexPastLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "index {} is past the limit of {MAX_SLOTS_PER_TURN} per turn",
            self.index
        )
    }
}

/// The slot at `index`, growing `slots` with `T::default()` up to it when it is not there yet.
///
/// Refuses an index at or past [`MAX_SLOTS_PER_TURN`] with [`IndexPastLimit`] and leaves
/// `slots` untouched, so nothing is allocated for it. The comparison is made on the `u64`
/// before any conversion, because `usize` is 32 bits on wasm32. Slots already present are
/// returned as they are, never reset, and a lower index never shrinks the table.
pub fn slot_mut<T: Default>(slots: &mut Vec<T>, index: u64) -> Result<&mut T, IndexPastLimit> {
    if index >= MAX_SLOTS_PER_TURN as u64 {
        return Err(IndexPastLimit { index });
    }
    let index = index as usize;
    if slots.len() <= index {
        slots.resize_with(index + 1, T::default);
    }
    Ok(&mut slots[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_at_or_past_the_limit_is_refused_without_allocating() {
        for index in [1024, 4_294_967_296, u64::MAX] {
            let mut slots: Vec<u32> = Vec::new();
            assert_eq!(slot_mut(&mut slots, index), Err(IndexPastLimit { index }));
            assert_eq!(slots.len(), 0);
            assert_eq!(slots.capacity(), 0);
        }
    }

    #[test]
    fn the_last_index_under_the_limit_is_accepted() {
        let mut slots: Vec<u32> = Vec::new();
        *slot_mut(&mut slots, 1023).unwrap() = 7;
        assert_eq!(slots.len(), 1024);
        assert_eq!(slots[1023], 7);
    }

    #[test]
    fn index_zero_on_an_empty_table_adds_one_slot() {
        let mut slots: Vec<u32> = Vec::new();
        assert_eq!(*slot_mut(&mut slots, 0).unwrap(), 0);
        assert_eq!(slots.len(), 1);
    }

    #[test]
    fn growing_past_a_filled_slot_keeps_its_value() {
        let mut slots: Vec<String> = Vec::new();
        slot_mut(&mut slots, 0).unwrap().push_str("first");
        slot_mut(&mut slots, 3).unwrap().push_str("fourth");
        assert_eq!(slots, ["first", "", "", "fourth"]);
    }

    #[test]
    fn a_lower_index_after_a_higher_one_neither_shrinks_nor_resets() {
        let mut slots: Vec<String> = Vec::new();
        slot_mut(&mut slots, 2).unwrap().push_str("third");
        slot_mut(&mut slots, 1).unwrap().push_str("second");
        assert_eq!(slots, ["", "second", "third"]);
        slot_mut(&mut slots, 1).unwrap().push('!');
        assert_eq!(slots, ["", "second!", "third"]);
    }

    #[test]
    fn a_refused_index_leaves_a_filled_table_unchanged() {
        let mut slots: Vec<String> = vec!["kept".to_string()];
        assert!(slot_mut(&mut slots, u64::MAX).is_err());
        assert_eq!(slots, ["kept"]);
    }

    #[test]
    fn the_refusal_names_the_index_and_the_limit() {
        assert_eq!(
            IndexPastLimit {
                index: 4_294_967_296
            }
            .to_string(),
            "index 4294967296 is past the limit of 1024 per turn"
        );
    }
}
