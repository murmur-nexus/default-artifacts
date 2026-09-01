//! The glob matcher: patterns over path components, with no regex engine behind
//! them.
//!
//! A regex engine would be both a dependency and a fail-closed hazard. These
//! hooks deny on any error, including an epoch-deadline expiry, so a pathological
//! pattern that outran the deadline would refuse every call for the rest of the
//! run. Both matchers here are the greedy two-pointer wildcard algorithm with a
//! single restart point: worst case `O(pattern × text)`, no recursion, no
//! backtracking stack.
//!
//! Three pattern shapes, decided entirely by where the `/` characters are:
//!
//! | Pattern shape | Matched against |
//! |---|---|
//! | no `/` at all | the basename alone |
//! | trailing `/`, no internal `/` | any directory component of the path |
//! | contains an internal `/` | the whole path, anchored at the workdir root |
//!
//! A pattern with both an internal `/` and a trailing `/` (`src/tests/`) is
//! anchored and matches everything beneath that prefix — it carries an implicit
//! trailing `**`.

use std::fmt;

/// Which part of a path a [`Pattern`] is matched against, derived from the
/// pattern text alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternKind {
    /// No `/` in the pattern: matched against the path's last component.
    Basename,
    /// Trailing `/` and no internal `/`: matched against every component of the
    /// path except the last, so it names a directory wherever it appears.
    DirComponent,
    /// An internal `/`: matched against the whole component list, anchored at the
    /// workdir root.
    Anchored,
}

/// Why a pattern string could not be compiled. Carried into a
/// [`crate::ConfigError`] with the offending key and list index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternError {
    /// The pattern was `""`, or `/` — nothing left after the trailing slash.
    Empty,
    /// The pattern contained a NUL, which no path this policy can judge holds.
    Nul,
    /// Two adjacent slashes, or a leading slash: a component with no text.
    EmptyComponent,
    /// `**` appeared inside a larger component (`a**b`), where it has no meaning:
    /// `**` is a whole component or it is a mistake. Carries that component.
    PartialDoubleStar(String),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a pattern must not be empty"),
            Self::Nul => write!(f, "a pattern must not contain a NUL byte"),
            Self::EmptyComponent => write!(
                f,
                "a pattern must not contain an empty path component (a leading '/' or a '//')"
            ),
            Self::PartialDoubleStar(component) => write!(
                f,
                "'**' must be a whole path component, not part of '{component}'"
            ),
        }
    }
}

/// One compiled protected-path pattern.
///
/// `*` matches any run of characters within a single component and never crosses
/// `/`. `?` matches exactly one non-`/` character. `**` is a whole component and
/// matches zero or more components. There is no escaping and no character class:
/// a `\` or a `[` in a pattern is a literal character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    source: String,
    kind: PatternKind,
    /// One entry per component of the pattern. [`PatternKind::Basename`] and
    /// [`PatternKind::DirComponent`] always hold exactly one.
    segments: Vec<Vec<char>>,
}

impl Pattern {
    /// Compile one pattern string, or say why it cannot be compiled.
    pub fn parse(source: &str) -> Result<Self, PatternError> {
        if source.is_empty() {
            return Err(PatternError::Empty);
        }
        if source.contains('\0') {
            return Err(PatternError::Nul);
        }
        let trailing_slash = source.ends_with('/');
        let core = match source.strip_suffix('/') {
            Some(core) => core,
            None => source,
        };
        if core.is_empty() {
            return Err(PatternError::Empty);
        }
        let kind = match (core.contains('/'), trailing_slash) {
            (true, _) => PatternKind::Anchored,
            (false, true) => PatternKind::DirComponent,
            (false, false) => PatternKind::Basename,
        };

        let mut segments: Vec<Vec<char>> = Vec::new();
        for component in core.split('/') {
            if component.is_empty() {
                return Err(PatternError::EmptyComponent);
            }
            if component.contains("**") && component != "**" {
                return Err(PatternError::PartialDoubleStar(component.to_string()));
            }
            segments.push(component.chars().collect());
        }
        // `src/tests/` names a directory, so it matches everything beneath it.
        if kind == PatternKind::Anchored && trailing_slash {
            segments.push(vec!['*', '*']);
        }

        Ok(Self {
            source: source.to_string(),
            kind,
            segments,
        })
    }

    /// The pattern exactly as the operator wrote it. This is what the refusal
    /// reason names, so it must be the operator's own text and not a normalized
    /// form of it.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Which part of a path this pattern is matched against.
    pub fn kind(&self) -> PatternKind {
        self.kind
    }

    /// Match a normalized component list.
    ///
    /// `absolute` is the named limit of this policy: an absolute path is judged on
    /// basename and directory-component patterns, and a root-anchored pattern
    /// never matches it, because the hook holds no filesystem grant and cannot
    /// know where the workdir sits in the host filesystem.
    pub fn matches_path(&self, components: &[String], absolute: bool) -> bool {
        match self.kind {
            PatternKind::Basename => match (self.segments.first(), components.last()) {
                (Some(segment), Some(base)) => match_component(segment, base),
                _ => false,
            },
            PatternKind::DirComponent => match (self.segments.first(), components.split_last()) {
                (Some(segment), Some((_base, dirs))) => {
                    dirs.iter().any(|dir| match_component(segment, dir))
                }
                _ => false,
            },
            PatternKind::Anchored => !absolute && match_segments(&self.segments, components),
        }
    }

    /// Match a bare name that has no path structure at all — a tool name.
    ///
    /// Only a single-component pattern can match one; a `tools[].match` carrying a
    /// `/` is rejected at parse time rather than silently never matching.
    pub fn matches_name(&self, name: &str) -> bool {
        match self.segments.as_slice() {
            [segment] => match_component(segment, name),
            _ => false,
        }
    }
}

/// True for the `**` segment, which is the only segment that spans components.
fn is_double_star(segment: &[char]) -> bool {
    segment.len() == 2 && segment.first() == Some(&'*') && segment.get(1) == Some(&'*')
}

/// Greedy wildcard match of one pattern component against one path component.
///
/// `star` remembers the last `*` and `star_text` the text position it was first
/// tried at; a mismatch restarts from there with the `*` having eaten one more
/// character. That is one restart point rather than a stack, which is what keeps
/// this out of the exponential-backtracking class regex would put it in.
fn match_component(pattern: &[char], text: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let mut p = 0usize;
    let mut t = 0usize;
    let mut star: Option<usize> = None;
    let mut star_text = 0usize;

    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                star_text = t;
                p = p.saturating_add(1);
            }
            Some('?') => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            Some(c) if text.get(t) == Some(c) => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            _ => match star {
                Some(s) => {
                    p = s.saturating_add(1);
                    star_text = star_text.saturating_add(1);
                    t = star_text;
                }
                None => return false,
            },
        }
    }

    while pattern.get(p) == Some(&'*') {
        p = p.saturating_add(1);
    }
    p >= pattern.len()
}

/// The same greedy algorithm one level up: `**` is the star, and a component is
/// the unit being consumed.
fn match_segments(segments: &[Vec<char>], components: &[String]) -> bool {
    let mut s = 0usize;
    let mut c = 0usize;
    let mut star: Option<usize> = None;
    let mut star_component = 0usize;

    while c < components.len() {
        let advanced = match segments.get(s) {
            Some(segment) if is_double_star(segment) => {
                star = Some(s);
                star_component = c;
                s = s.saturating_add(1);
                true
            }
            Some(segment) => match components.get(c) {
                Some(component) if match_component(segment, component) => {
                    s = s.saturating_add(1);
                    c = c.saturating_add(1);
                    true
                }
                _ => false,
            },
            None => false,
        };
        if !advanced {
            match star {
                Some(k) => {
                    s = k.saturating_add(1);
                    star_component = star_component.saturating_add(1);
                    c = star_component;
                }
                None => return false,
            }
        }
    }

    while segments
        .get(s)
        .map(Vec::as_slice)
        .is_some_and(is_double_star)
    {
        s = s.saturating_add(1);
    }
    s >= segments.len()
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

    fn comps(path: &str) -> Vec<String> {
        path.split('/').map(str::to_string).collect()
    }

    fn matches(pattern: &str, path: &str) -> bool {
        Pattern::parse(pattern)
            .unwrap()
            .matches_path(&comps(path), false)
    }

    #[test]
    fn no_slash_matches_the_basename_only() {
        assert_eq!(
            Pattern::parse("conftest.py").unwrap().kind(),
            PatternKind::Basename
        );
        assert!(matches("conftest.py", "conftest.py"));
        assert!(matches("conftest.py", "pkg/a/conftest.py"));
        assert!(!matches("conftest.py", "conftest.py/x.txt"));
        assert!(matches("test_*", "src/test_auth.py"));
        assert!(!matches("test_*", "src/auth_test.py"));
        assert!(matches("*_test.*", "a/b/auth_test.py"));
        assert!(!matches("*_test.*", "a/b/auth_test"));
    }

    #[test]
    fn trailing_slash_matches_a_directory_component_anywhere() {
        assert_eq!(
            Pattern::parse("tests/").unwrap().kind(),
            PatternKind::DirComponent
        );
        assert!(matches("tests/", "tests/a.py"));
        assert!(matches("tests/", "pkg/tests/a.py"));
        assert!(matches("tests/", "pkg/tests/deep/a.py"));
        // A *file* named `tests` is not a directory component.
        assert!(!matches("tests/", "tests"));
        assert!(!matches("tests/", "src/tests"));
    }

    #[test]
    fn internal_slash_anchors_at_the_workdir_root() {
        assert_eq!(
            Pattern::parse("src/tests/*.py").unwrap().kind(),
            PatternKind::Anchored
        );
        assert!(matches("src/tests/*.py", "src/tests/a.py"));
        assert!(!matches("src/tests/*.py", "pkg/src/tests/a.py"));
        assert!(!matches("src/tests/*.py", "src/tests/deep/a.py"));
    }

    #[test]
    fn anchored_pattern_with_a_trailing_slash_covers_everything_beneath() {
        assert!(matches("src/tests/", "src/tests/a.py"));
        assert!(matches("src/tests/", "src/tests/deep/a.py"));
        assert!(!matches("src/tests/", "src/other/a.py"));
    }

    #[test]
    fn star_never_crosses_a_slash() {
        assert!(!matches("src/*.py", "src/deep/a.py"));
        assert!(matches("src/*.py", "src/a.py"));
    }

    #[test]
    fn double_star_matches_zero_or_more_components() {
        assert!(matches("src/**/test_*.py", "src/test_a.py"));
        assert!(matches("src/**/test_*.py", "src/a/b/test_a.py"));
        assert!(!matches("src/**/test_*.py", "src/a/b/a.py"));
        assert!(matches("**/conftest.py", "conftest.py"));
        assert!(matches("**", "anything/at/all"));
    }

    #[test]
    fn question_mark_matches_exactly_one_character() {
        assert!(matches("test_?.py", "test_a.py"));
        assert!(!matches("test_?.py", "test_ab.py"));
        assert!(!matches("test_?.py", "test_.py"));
    }

    #[test]
    fn root_anchored_pattern_never_matches_an_absolute_path() {
        let anchored = Pattern::parse("tests/a.py").unwrap();
        assert!(anchored.matches_path(&comps("tests/a.py"), false));
        assert!(!anchored.matches_path(&comps("tests/a.py"), true));
        // Basename and directory-component patterns still apply to absolute paths.
        let basename = Pattern::parse("test_*").unwrap();
        assert!(basename.matches_path(&comps("etc/test_a.py"), true));
        let dir = Pattern::parse("tests/").unwrap();
        assert!(dir.matches_path(&comps("home/u/tests/a.py"), true));
    }

    #[test]
    fn a_repeated_star_does_not_blow_up() {
        // The shape that makes a backtracking regex engine explode. Greedy
        // two-pointer matching answers it in linear-ish time.
        let pattern = Pattern::parse("*a*a*a*a*a*a*a*a*b").unwrap();
        let text = "a".repeat(2048);
        assert!(!pattern.matches_path(&[text], false));
    }

    #[test]
    fn malformed_patterns_are_rejected_with_the_reason() {
        assert_eq!(Pattern::parse(""), Err(PatternError::Empty));
        assert_eq!(Pattern::parse("/"), Err(PatternError::Empty));
        assert_eq!(Pattern::parse("a\0b"), Err(PatternError::Nul));
        assert_eq!(Pattern::parse("/tests"), Err(PatternError::EmptyComponent));
        assert_eq!(Pattern::parse("a//b"), Err(PatternError::EmptyComponent));
        assert_eq!(
            Pattern::parse("a**b"),
            Err(PatternError::PartialDoubleStar("a**b".to_string()))
        );
        assert_eq!(
            Pattern::parse("src/**b/x"),
            Err(PatternError::PartialDoubleStar("**b".to_string()))
        );
        assert!(Pattern::parse("**").is_ok());
        assert!(Pattern::parse("src/**/x").is_ok());
    }

    #[test]
    fn partial_double_star_message_names_the_component() {
        let err = Pattern::parse("a**b").unwrap_err();
        assert!(err.to_string().contains("a**b"), "{err}");
        assert!(err.to_string().contains("whole path component"), "{err}");
    }

    #[test]
    fn matches_name_only_accepts_single_component_patterns() {
        assert!(Pattern::parse("murmur-tool-editor")
            .unwrap()
            .matches_name("murmur-tool-editor"));
        assert!(Pattern::parse("murmur-tool-*")
            .unwrap()
            .matches_name("murmur-tool-editor"));
        assert!(!Pattern::parse("murmur-tool-*")
            .unwrap()
            .matches_name("other-editor"));
        assert!(!Pattern::parse("a/b").unwrap().matches_name("a"));
    }

    #[test]
    fn replacement_characters_and_c0_controls_match_literally_and_never_panic() {
        // WIT `list<string>` is UTF-8 by construction, so a literally non-UTF-8
        // argument cannot reach the guest; U+FFFD is what a lossily-converted one
        // becomes on the host side. There is no byte-level case to test here.
        assert!(matches("test_*", "tests/test_\u{fffd}.py"));
        assert!(matches("*", "\u{1}\u{7}\u{1b}"));
        assert!(!matches("test_?.py", "test_\u{fffd}\u{fffd}.py"));
        assert!(matches("test_?.py", "test_\u{fffd}.py"));
    }
}
