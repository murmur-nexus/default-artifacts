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

/// What a tool-built commit message is made from: the pull request's number, title, body and
/// head branch, and the pinned repo's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrText<'a> {
    pub number: u64,
    pub owner: &'a str,
    pub head_ref: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// The whole message of a commit this tool builds itself when `config.commit_identity` is set.
///
/// A non-empty `message_override` (the agent's `pr_merge` `message`) is the whole base text.
/// Otherwise `squash` gives `{title} (#{n})`, then a blank line and the body when the body is
/// not blank; every other method gives `Merge pull request #{n} from {owner}/{head_ref}`, then
/// a blank line and the title when the title is not empty. `trailer` (`config.merge_trailer`)
/// is then joined on by the same rules [`compose`] uses. `rebase` never builds a commit, so it
/// never reaches this function.
pub fn built_message(
    method: MergeMethod,
    pr: PrText<'_>,
    message_override: Option<&str>,
    trailer: Option<&str>,
) -> String {
    let base = match message_override.filter(|message| !message.is_empty()) {
        Some(message) => message.to_string(),
        None if method == MergeMethod::Squash => {
            let body = pr.body.trim_end_matches(['\n', '\r']);
            if body.trim().is_empty() {
                format!("{} (#{})", pr.title, pr.number)
            } else {
                format!("{} (#{})\n\n{body}", pr.title, pr.number)
            }
        }
        None => {
            let head = format!(
                "Merge pull request #{} from {}/{}",
                pr.number, pr.owner, pr.head_ref
            );
            if pr.title.is_empty() {
                head
            } else {
                format!("{head}\n\n{}", pr.title)
            }
        }
    };
    match trailer {
        Some(trailer) => join_trailer(&base, trailer),
        None => base,
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

    fn pr_text() -> PrText<'static> {
        PrText {
            number: 1,
            owner: "acme",
            head_ref: "feature-x",
            title: "Add feature",
            body: "does the thing",
        }
    }

    #[test]
    fn built_merge_message_names_the_pr_and_head_then_the_title_and_trailer() {
        assert_eq!(
            built_message(MergeMethod::Merge, pr_text(), None, Some(TRAILER)),
            "Merge pull request #1 from acme/feature-x\n\nAdd feature\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn built_squash_message_is_title_number_then_body_and_trailer() {
        assert_eq!(
            built_message(MergeMethod::Squash, pr_text(), None, Some(TRAILER)),
            "Add feature (#1)\n\ndoes the thing\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn built_message_without_a_trailer_has_none() {
        assert_eq!(
            built_message(MergeMethod::Merge, pr_text(), None, None),
            "Merge pull request #1 from acme/feature-x\n\nAdd feature"
        );
        assert_eq!(
            built_message(MergeMethod::Squash, pr_text(), None, None),
            "Add feature (#1)\n\ndoes the thing"
        );
    }

    #[test]
    fn built_merge_message_with_an_empty_title_is_the_first_line_alone() {
        let pr = PrText {
            title: "",
            ..pr_text()
        };
        assert_eq!(
            built_message(MergeMethod::Merge, pr, None, None),
            "Merge pull request #1 from acme/feature-x"
        );
    }

    #[test]
    fn built_squash_message_drops_a_blank_body_and_trims_trailing_newlines() {
        for body in ["", "  ", "\n\n", " \n"] {
            let pr = PrText { body, ..pr_text() };
            assert_eq!(
                built_message(MergeMethod::Squash, pr, None, None),
                "Add feature (#1)",
                "{body:?}"
            );
        }
        let pr = PrText {
            body: "does the thing\n\n",
            ..pr_text()
        };
        assert_eq!(
            built_message(MergeMethod::Squash, pr, None, Some(TRAILER)),
            "Add feature (#1)\n\ndoes the thing\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }

    #[test]
    fn a_non_empty_message_override_is_the_whole_base_text() {
        for method in [MergeMethod::Merge, MergeMethod::Squash] {
            assert_eq!(
                built_message(method, pr_text(), Some("custom"), Some(TRAILER)),
                "custom\n\nCo-authored-by: Release Bot <bot@example.com>"
            );
            assert_eq!(
                built_message(method, pr_text(), Some("custom"), None),
                "custom"
            );
        }
    }

    #[test]
    fn an_empty_message_override_falls_back_to_the_default_text() {
        assert_eq!(
            built_message(MergeMethod::Squash, pr_text(), Some(""), None),
            "Add feature (#1)\n\ndoes the thing"
        );
    }

    #[test]
    fn built_message_joins_an_existing_trailer_block_and_never_duplicates() {
        let pr = PrText {
            body: "does the thing\nSigned-off-by: Ada <ada@example.com>",
            ..pr_text()
        };
        assert_eq!(
            built_message(MergeMethod::Squash, pr, None, Some(TRAILER)),
            "Add feature (#1)\n\ndoes the thing\nSigned-off-by: Ada <ada@example.com>\nCo-authored-by: Release Bot <bot@example.com>"
        );
        let pr = PrText {
            body: "does the thing\n\nCo-authored-by: Release Bot <bot@example.com>",
            ..pr_text()
        };
        assert_eq!(
            built_message(MergeMethod::Squash, pr, None, Some(TRAILER)),
            "Add feature (#1)\n\ndoes the thing\n\nCo-authored-by: Release Bot <bot@example.com>"
        );
    }
}
