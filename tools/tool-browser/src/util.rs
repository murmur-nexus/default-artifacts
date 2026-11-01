//! Small helpers shared by the call process and the supervisor.

use std::io::Read;

/// `text` on one line: newlines and tabs become spaces, cut at `max` characters with `…`.
pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cut_chars(flat.trim(), max)
}

/// `text` cut at `max` characters, ending in `…` when cut.
pub fn cut_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((i, _)) => format!("{}…", &text[..i]),
    }
}

/// `n` random bytes from `/dev/urandom`, which every murmur tier carries.
pub fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `a`, `a and b`, `a, b and c`, with `word` in place of `and`.
pub fn join_words(items: &[&str], word: &str) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [init @ .., last] => format!("{} {word} {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_flattens_and_cuts() {
        assert_eq!(one_line("a\nb\tc", 10), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
    }

    #[test]
    fn join_words_forms() {
        assert_eq!(join_words(&["a"], "or"), "a");
        assert_eq!(join_words(&["a", "b"], "or"), "a or b");
        assert_eq!(join_words(&["a", "b", "c"], "and"), "a, b and c");
    }
}
