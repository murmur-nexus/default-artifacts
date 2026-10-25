//! The operator's merge trailer this tool appends when it merges a pull request.
//!
//! The trailer text comes only from `config.merge_trailer`: nothing in a `pr_merge` call can
//! set or rename it, and with no `merge_trailer` configured no trailer is sent.

use crate::config::MergeMethod;

/// The `commit_message` this tool sends to the merge endpoint, and the trailer line to report
/// back to the caller (`None` when no trailer was sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCommitMessage {
    pub commit_message: Option<String>,
    pub trailer: Option<String>,
}

/// Compose the `commit_message` GitHub's merge endpoint should receive.
///
/// `message_override` is the agent's own `pr_merge` `message`, when given; `trailer` is
/// `config.merge_trailer`, `None` when the operator left it unset or `false`.
///
/// `rebase` never carries a trailer or a custom message: GitHub's merge endpoint accepts
/// neither for that method, and rebase must never block a merge over attribution. For `merge`
/// and `squash` with no trailer, the message override (or nothing) is sent as given; with a
/// trailer, it is joined onto the override, else onto the PR body for `squash` or the PR title
/// for `merge`.
pub fn compose(
    method: MergeMethod,
    pr_title: &str,
    pr_body: &str,
    message_override: Option<&str>,
    trailer: Option<&str>,
) -> MergeCommitMessage {
    if method == MergeMethod::Rebase {
        return MergeCommitMessage {
            commit_message: None,
            trailer: None,
        };
    }

    let Some(trailer) = trailer else {
        return MergeCommitMessage {
            commit_message: message_override.map(str::to_string),
            trailer: None,
        };
    };

    let base = message_override.unwrap_or(match method {
        MergeMethod::Squash => pr_body,
        MergeMethod::Merge => pr_title,
        MergeMethod::Rebase => unreachable!("handled above"),
    });
    MergeCommitMessage {
        commit_message: Some(join_trailer(base, trailer)),
        trailer: Some(trailer.to_string()),
    }
}

/// Whether `line` looks like a trailer (`Key: value`), the same shape `git interpret-trailers`
/// recognizes: an alphanumeric-and-hyphen key, a colon, a space, then content.
pub(crate) fn is_trailer_line(line: &str) -> bool {
    match line.split_once(": ") {
        Some((key, value)) => {
            !key.is_empty()
                && !value.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        }
        None => false,
    }
}

fn ends_with_trailer_block(trimmed: &str) -> bool {
    trimmed.lines().last().is_some_and(is_trailer_line)
}

/// Join `trailer` onto `content`, so it is the whole message when `content` is empty, joins an
/// existing trailing trailer block with no blank line, otherwise gets one blank line of
/// separation, and is never duplicated when `content` already ends with it.
fn join_trailer(content: &str, trailer: &str) -> String {
    let trimmed = content.trim_end_matches('\n');
    if trimmed.is_empty() {
        return trailer.to_string();
    }
    if trimmed.lines().last() == Some(trailer) {
        return trimmed.to_string();
    }
    if ends_with_trailer_block(trimmed) {
        format!("{trimmed}\n{trailer}")
    } else {
        format!("{trimmed}\n\n{trailer}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRAILER: &str = "Co-authored-by: Release Bot <bot@example.com>";

    #[test]
    fn rebase_never_carries_a_trailer_or_message() {
        let result = compose(
            MergeMethod::Rebase,
            "title",
            "body",
            Some("msg"),
            Some(TRAILER),
        );
        assert_eq!(result.commit_message, None);
        assert_eq!(result.trailer, None);
    }

    #[test]
    fn squash_with_no_trailer_configured_sends_none() {
        let result = compose(MergeMethod::Squash, "title", "body", None, None);
        assert_eq!(result.trailer, None);
        assert_eq!(result.commit_message, None);
    }

    #[test]
    fn squash_with_a_trailer_appends_it_to_the_body() {
        let result = compose(MergeMethod::Squash, "title", "body", None, Some(TRAILER));
        assert_eq!(result.trailer.as_deref(), Some(TRAILER));
        assert_eq!(
            result.commit_message.unwrap(),
            "body\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn merge_with_no_trailer_configured_sends_none() {
        let result = compose(MergeMethod::Merge, "title", "body", None, None);
        assert_eq!(result.trailer, None);
        assert_eq!(result.commit_message, None);
    }

    #[test]
    fn merge_with_a_trailer_appends_it_to_the_title() {
        let result = compose(MergeMethod::Merge, "title", "body", None, Some(TRAILER));
        assert_eq!(result.trailer.as_deref(), Some(TRAILER));
        assert_eq!(
            result.commit_message.unwrap(),
            "title\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn empty_body_makes_the_trailer_the_whole_message() {
        let result = compose(MergeMethod::Squash, "title", "", None, Some(TRAILER));
        assert_eq!(result.commit_message.unwrap(), TRAILER);
    }

    #[test]
    fn joins_an_existing_trailer_block_with_no_blank_line() {
        let result = compose(
            MergeMethod::Squash,
            "title",
            "body\nSigned-off-by: Ada <ada@example.com>",
            None,
            Some(TRAILER),
        );
        assert_eq!(
            result.commit_message.unwrap(),
            "body\nSigned-off-by: Ada <ada@example.com>\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn does_not_duplicate_an_existing_exact_trailer_line() {
        let body = "body\n\nCo-authored-by: Release Bot <bot@example.com>";
        let result = compose(MergeMethod::Squash, "title", body, None, Some(TRAILER));
        assert_eq!(result.commit_message.unwrap(), body);
    }

    #[test]
    fn message_override_is_used_as_the_non_trailer_content() {
        let result = compose(
            MergeMethod::Merge,
            "title",
            "body",
            Some("custom"),
            Some(TRAILER),
        );
        assert_eq!(
            result.commit_message.unwrap(),
            "custom\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn trailer_off_with_no_message_sends_no_commit_message() {
        let result = compose(MergeMethod::Merge, "title", "body", None, None);
        assert_eq!(result.commit_message, None);
    }

    #[test]
    fn no_trailer_with_a_message_override_sends_the_override_untouched() {
        let result = compose(MergeMethod::Squash, "title", "body", Some("custom"), None);
        assert_eq!(result.commit_message.as_deref(), Some("custom"));
        assert_eq!(result.trailer, None);
    }

    #[test]
    fn recognises_trailer_lines() {
        assert!(is_trailer_line(TRAILER));
        assert!(is_trailer_line("Signed-off-by: Ada <ada@example.com>"));
        assert!(!is_trailer_line("not a trailer"));
        assert!(!is_trailer_line("Co authored by: someone"));
        assert!(!is_trailer_line("Key:"));
        assert!(!is_trailer_line(""));
    }
}
