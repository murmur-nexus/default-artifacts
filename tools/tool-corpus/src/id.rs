//! Record id minting and the prefix rules that decide what an id starts with.
//!
//! Ids are `<prefix>_<32 lowercase hex>`, the shape the Murmur runtime already mints for
//! `ses_`/`tsk_`/`ctx_` (see `crates/capsule-runtime/src/identity.rs`). The 32 hex
//! characters are a UUID v7 laid out by hand: this crate takes no `uuid` dependency, so
//! adding one type of id does not change the wasm component's dependency graph.
//!
//! Because the 48-bit millisecond timestamp is big-endian and leading, ids sort
//! lexicographically in mint order. Retrieval ordering relies on that.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Id prefixes the Murmur runtime mints itself. A corpus record must never collide with
/// one, or a reader cannot tell a corpus id from a runtime id by inspection.
pub const RESERVED_PREFIXES: [&str; 7] = ["ses", "tsk", "ctx", "req", "dep", "evt", "msg"];

/// Characters taken from a type tag when deriving its prefix.
pub const DERIVED_PREFIX_LEN: usize = 3;

/// Upper bound on an explicit `prefix_map` value, matching `^[a-z][a-z0-9]{0,7}$`.
pub const PREFIX_MAX_LEN: usize = 8;

/// Per-invocation sequence feeding `rand_a`, so two ids minted inside one tool call are
/// strictly increasing even when they land in the same millisecond. A fresh component
/// instantiation restarts it at zero, which is harmless: the timestamp leads.
static MINT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether `prefix` is one the runtime reserves for itself.
pub fn is_reserved(prefix: &str) -> bool {
    RESERVED_PREFIXES.contains(&prefix)
}

/// Whether `prefix` is a well-formed explicit prefix: `^[a-z][a-z0-9]{0,7}$`.
pub fn is_valid_explicit_prefix(prefix: &str) -> bool {
    let mut chars = prefix.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    prefix.len() <= PREFIX_MAX_LEN && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The prefix a type tag derives when the operator declares no explicit override:
/// lowercase, keep only `[a-z0-9]`, take the first [`DERIVED_PREFIX_LEN`] characters.
///
/// Two distinct types deriving the same prefix is deliberately allowed. Ids stay unique
/// because of the UUID, and forcing prefix uniqueness would make declaring one new type a
/// breaking change for an unrelated type that happens to share three letters.
pub fn derive_prefix(type_tag: &str) -> Result<String, String> {
    let derived: String = type_tag
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(DERIVED_PREFIX_LEN)
        .collect();
    if derived.is_empty() {
        return Err(format!(
            "type \"{type_tag}\" contains no [a-z0-9] characters to derive an id prefix from; \
             declare an explicit prefix under prefix_map"
        ));
    }
    Ok(derived)
}

/// Mint `<prefix>_<uuid v7, simple form>`.
pub fn mint_id(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let unix_ms = now.as_millis() as u64;
    let seq = MINT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{}", uuid_v7_simple(unix_ms, seq, entropy(now.subsec_nanos(), seq)))
}

/// The 32-hex-character body of an id: 48-bit big-endian millisecond timestamp, version
/// nibble `7`, 12 bits of `rand_a`, variant bits `0b10`, 62 bits of `rand_b`.
fn uuid_v7_simple(unix_ms: u64, seq: u64, rand_b: u64) -> String {
    let mut bytes = [0u8; 16];
    let ms = unix_ms & 0x0000_ffff_ffff_ffff;
    for (i, b) in bytes.iter_mut().enumerate().take(6) {
        *b = (ms >> (40 - 8 * i)) as u8;
    }
    // `rand_a` is the low 12 bits of the mint sequence, so ids minted within one
    // millisecond order by mint order. Wrapping needs 4096 ids in a single millisecond of
    // one tool call, which no operation here can reach.
    let rand_a = (seq & 0x0fff) as u16;
    bytes[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0f);
    bytes[7] = (rand_a & 0x00ff) as u8;
    bytes[8] = 0x80 | ((rand_b >> 58) as u8 & 0x3f);
    for i in 0..7 {
        bytes[9 + i] = (rand_b >> (48 - 8 * i)) as u8;
    }

    let mut out = String::with_capacity(32);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Entropy without a `rand` dependency. `RandomState`'s `BuildHasher` is OS-seeded per
/// process and advances per construction, and each tool call is a fresh component
/// instantiation; the sub-millisecond clock reading and the mint sequence separate two
/// ids minted back to back inside one call.
fn entropy(subsec_nanos: u32, seq: u64) -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(subsec_nanos);
    hasher.write_u64(seq);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn body(id: &str) -> &str {
        id.split_once('_').expect("id has a prefix separator").1
    }

    #[test]
    fn minted_id_has_the_runtime_shape() {
        let id = mint_id("not");
        let (prefix, hex) = id.split_once('_').expect("id has a prefix separator");
        assert_eq!(prefix, "not");
        assert_eq!(hex.len(), 32, "id body must be 32 hex characters: {id}");
        assert!(
            hex.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "id body must be lowercase hex: {id}"
        );
    }

    #[test]
    fn version_nibble_is_seven_and_variant_bits_are_binary_ten() {
        let id = mint_id("not");
        let hex = body(&id);
        assert_eq!(&hex[12..13], "7", "version nibble must be 7: {id}");
        let variant = u8::from_str_radix(&hex[16..18], 16).expect("variant byte is hex");
        assert_eq!(variant & 0xc0, 0x80, "variant bits must be 0b10: {id}");
    }

    #[test]
    fn leading_forty_eight_bits_decode_to_now() {
        let id = mint_id("not");
        let ms = u64::from_str_radix(&body(&id)[0..12], 16).expect("timestamp is hex");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_millis() as u64;
        assert!(
            now.abs_diff(ms) < 1000,
            "id timestamp {ms} is not within a second of {now}"
        );
    }

    #[test]
    fn two_ids_minted_in_one_call_are_strictly_increasing() {
        let first = mint_id("not");
        let second = mint_id("not");
        assert!(first < second, "{first} must sort before {second}");
    }

    #[test]
    fn reserved_prefixes_are_exactly_the_runtime_set() {
        assert_eq!(
            RESERVED_PREFIXES,
            ["ses", "tsk", "ctx", "req", "dep", "evt", "msg"]
        );
    }

    #[test]
    fn derive_prefix_takes_three_filtered_characters() {
        assert_eq!(derive_prefix("session").unwrap(), "ses");
        assert!(is_reserved(&derive_prefix("session").unwrap()));
        assert_eq!(derive_prefix("Note").unwrap(), "not");
        assert_eq!(derive_prefix("a-b-c-d").unwrap(), "abc");
        assert_eq!(derive_prefix("go").unwrap(), "go");
        assert_eq!(derive_prefix("_3d model").unwrap(), "3dm");
    }

    #[test]
    fn derive_prefix_rejects_a_tag_that_filters_to_nothing() {
        let err = derive_prefix("---").expect_err("a tag with no [a-z0-9] must fail");
        assert!(err.contains("prefix_map"), "message must point at prefix_map: {err}");
    }

    #[test]
    fn explicit_prefix_shape_is_enforced() {
        assert!(is_valid_explicit_prefix("rqt"));
        assert!(is_valid_explicit_prefix("a"));
        assert!(is_valid_explicit_prefix("abcdefgh"));
        assert!(!is_valid_explicit_prefix(""));
        assert!(!is_valid_explicit_prefix("abcdefghi"));
        assert!(!is_valid_explicit_prefix("1ab"));
        assert!(!is_valid_explicit_prefix("Ab"));
        assert!(!is_valid_explicit_prefix("a_b"));
    }
}
