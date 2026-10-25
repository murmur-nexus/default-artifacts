//! The operator's footer line this tool appends to a pull request body it creates.

/// Apply `footer` (`config.pr_footer`, `None` when the operator left it unset or `false`) to
/// `body`.
///
/// `None` sends `body` untouched. `Some(line)`: a missing or empty body becomes exactly `line`;
/// a body already ending in exactly `line` is sent unchanged; any other body gets a blank line
/// then `line` appended.
pub fn apply(body: Option<&str>, footer: Option<&str>) -> Option<String> {
    let Some(line) = footer else {
        return body.map(str::to_string);
    };
    let text = body.unwrap_or("");
    if text.is_empty() {
        return Some(line.to_string());
    }
    let trimmed = text.trim_end_matches('\n');
    if trimmed.lines().last() == Some(line) {
        return Some(text.to_string());
    }
    Some(format!("{trimmed}\n\n{line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "Opened by our release bot";

    #[test]
    fn a_set_footer_appends_a_blank_line_then_the_line() {
        assert_eq!(
            apply(Some("hello"), Some(LINE)).unwrap(),
            "hello\n\nOpened by our release bot"
        );
    }

    #[test]
    fn no_footer_sends_the_body_untouched() {
        assert_eq!(apply(Some("hello"), None).unwrap(), "hello");
        assert_eq!(apply(Some("hello\n\n"), None).unwrap(), "hello\n\n");
        assert_eq!(apply(None, None), None);
    }

    #[test]
    fn no_body_at_all_becomes_exactly_the_line() {
        assert_eq!(apply(None, Some(LINE)).unwrap(), LINE);
        assert_eq!(apply(Some(""), Some(LINE)).unwrap(), LINE);
    }

    #[test]
    fn a_body_already_ending_in_the_line_is_not_duplicated() {
        let body = "hello\n\nOpened by our release bot";
        assert_eq!(apply(Some(body), Some(LINE)).unwrap(), body);
    }
}
