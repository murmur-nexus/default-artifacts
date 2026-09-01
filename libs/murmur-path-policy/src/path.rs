//! Lexical path normalization.
//!
//! The hooks hold no filesystem grant, so nothing here touches the filesystem and
//! nothing is canonicalized: a path is judged as text. Normalization splits on
//! `/`, drops empty and `.` components, and pops on `..`.
//!
//! A relative path is read as relative to the workdir root, which is what a
//! root-anchored pattern anchors to.

/// What normalization made of a write target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathTarget {
    /// Normalized components, relative to the workdir root. Every pattern shape
    /// applies.
    Relative(Vec<String>),
    /// An absolute path. It does not escape — `/..` is `/` — but the hook cannot
    /// know where the workdir sits in the host filesystem, so only basename and
    /// directory-component patterns apply and a root-anchored pattern misses it.
    Absolute(Vec<String>),
    /// `..` popped above the workdir root. A target the policy cannot anchor is
    /// one it cannot judge, and the fail-closed rule decides it: refused outright.
    Escaping,
    /// Nothing left after normalization (`""`, `.`, `/`, `./.`). Names no file, so
    /// there is nothing to protect.
    Empty,
}

impl PathTarget {
    /// The normalized components, or an empty slice when there are none.
    pub fn components(&self) -> &[String] {
        match self {
            Self::Relative(components) | Self::Absolute(components) => components,
            Self::Escaping | Self::Empty => &[],
        }
    }

    /// True when the target was written as an absolute path.
    pub fn is_absolute(&self) -> bool {
        matches!(self, Self::Absolute(_))
    }
}

/// Normalize one raw write target lexically.
pub fn normalize(raw: &str) -> PathTarget {
    let absolute = raw.starts_with('/');
    let mut components: Vec<String> = Vec::new();

    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            // POSIX resolves `/..` to `/`, so an absolute path cannot climb out of
            // itself; a relative one that pops past its own root has no anchor left.
            if components.pop().is_none() && !absolute {
                return PathTarget::Escaping;
            }
            continue;
        }
        components.push(part.to_string());
    }

    if components.is_empty() {
        return PathTarget::Empty;
    }
    if absolute {
        PathTarget::Absolute(components)
    } else {
        PathTarget::Relative(components)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn rel(parts: &[&str]) -> PathTarget {
        PathTarget::Relative(parts.iter().map(|p| (*p).to_string()).collect())
    }

    fn abs(parts: &[&str]) -> PathTarget {
        PathTarget::Absolute(parts.iter().map(|p| (*p).to_string()).collect())
    }

    #[test]
    fn plain_relative_paths_keep_their_components() {
        assert_eq!(normalize("tests/test_x.py"), rel(&["tests", "test_x.py"]));
        assert_eq!(normalize("src/x.py"), rel(&["src", "x.py"]));
    }

    #[test]
    fn dot_and_empty_components_are_dropped() {
        assert_eq!(normalize("./src//x.py"), rel(&["src", "x.py"]));
        assert_eq!(normalize("./tests/../src/x.py"), rel(&["src", "x.py"]));
    }

    #[test]
    fn a_path_that_pops_above_its_own_root_escapes() {
        assert_eq!(normalize("../../etc/passwd"), PathTarget::Escaping);
        assert_eq!(normalize("a/../../b"), PathTarget::Escaping);
        assert_eq!(normalize(".."), PathTarget::Escaping);
        assert_eq!(normalize("src/../.."), PathTarget::Escaping);
    }

    #[test]
    fn an_absolute_path_does_not_escape() {
        assert_eq!(normalize("/etc/passwd"), abs(&["etc", "passwd"]));
        assert_eq!(normalize("/../../etc/passwd"), abs(&["etc", "passwd"]));
        assert!(normalize("/a/b").is_absolute());
        assert!(!normalize("a/b").is_absolute());
    }

    #[test]
    fn a_path_with_nothing_left_is_empty() {
        assert_eq!(normalize(""), PathTarget::Empty);
        assert_eq!(normalize("/"), PathTarget::Empty);
        assert_eq!(normalize("."), PathTarget::Empty);
        assert_eq!(normalize("./."), PathTarget::Empty);
        assert!(normalize("").components().is_empty());
    }

    #[test]
    fn control_characters_and_replacement_characters_survive_as_components() {
        // WIT `list<string>` is UTF-8 by construction — a literally non-UTF-8
        // argument cannot reach the guest, so U+FFFD is the only form a lossily
        // converted argument takes.
        assert_eq!(
            normalize("tests/\u{fffd}.py"),
            rel(&["tests", "\u{fffd}.py"])
        );
        assert_eq!(normalize("a\u{1}b/c\u{7f}"), rel(&["a\u{1}b", "c\u{7f}"]));
    }
}
