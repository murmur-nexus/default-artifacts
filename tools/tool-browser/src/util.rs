//! Small text helpers.

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

/// `text` with every `%XX` escape decoded; an escape that is not two hex digits stays as written.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    fn percent_decoding() {
        assert_eq!(percent_decode("/a%20b/c%23d"), "/a b/c#d");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%C3%A9"), "é");
    }

    #[test]
    fn join_words_forms() {
        assert_eq!(join_words(&["a"], "or"), "a");
        assert_eq!(join_words(&["a", "b"], "or"), "a or b");
        assert_eq!(join_words(&["a", "b", "c"], "and"), "a, b and c");
    }
}
