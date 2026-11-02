//! What one command's words mean to a rule: literal text, flags, operands and paths.
//!
//! A word is either literal (its whole value is known before the command runs) or opaque
//! (some part is an expansion or substitution). An opaque word never satisfies or blocks a
//! flag condition; it counts as an operand whose value is unknown.

use std::collections::BTreeSet;

use crate::lex::{Part, Word};

/// A path, normalized lexically. Paths compare only within the same anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathKey {
    /// `/`-rooted; the components below `/`.
    Root(Vec<String>),
    /// Anchored at the home folder (`~`, `~/…`, `$HOME`, `${HOME}`); the components below it.
    Home(Vec<String>),
    /// Relative to the working folder; the components below it.
    Rel(Vec<String>),
    /// Not known until the command runs, or not expressible (`~user`, `..` above the start).
    Opaque,
}

impl PathKey {
    /// True when `self` names exactly `config`.
    pub fn is(&self, config: &PathKey) -> bool {
        !matches!(self, PathKey::Opaque) && self == config
    }

    /// True when `self` is `config` or lies beneath it.
    pub fn is_under(&self, config: &PathKey) -> bool {
        match (self, config) {
            (PathKey::Root(a), PathKey::Root(b))
            | (PathKey::Home(a), PathKey::Home(b))
            | (PathKey::Rel(a), PathKey::Rel(b)) => a.starts_with(b),
            _ => false,
        }
    }
}

/// One argument of a command, the program word included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// The full value when every part of the word is literal.
    pub literal: Option<String>,
    /// The value read as a path.
    pub path: PathKey,
    /// The word as written, for refusal reasons.
    pub display: String,
    /// True when the word is exactly one process substitution (`<(…)`).
    pub procsub: bool,
}

impl Arg {
    /// An argument the program receives verbatim, with no shell in between: no tilde,
    /// expansion or glob applies.
    pub fn plain(text: &str) -> Arg {
        let chars: Vec<(char, bool)> = text.chars().map(|c| (c, true)).collect();
        Arg {
            literal: Some(text.to_string()),
            path: path_from_chars(Anchor::None, &chars),
            display: text.to_string(),
            procsub: false,
        }
    }

    /// An argument whose value is not known until the command runs.
    pub fn opaque(display: &str) -> Arg {
        Arg {
            literal: None,
            path: PathKey::Opaque,
            display: display.to_string(),
            procsub: false,
        }
    }

    /// The argument a shell word becomes. `display` is the word's source text.
    pub fn from_word(word: &Word, display: String) -> Arg {
        Arg {
            literal: word_literal(word),
            path: word_path(word),
            display,
            procsub: matches!(word.parts.as_slice(), [Part::ProcSub(_)]),
        }
    }
}

/// The word's value when every part is literal text, quoted or not.
pub fn word_literal(word: &Word) -> Option<String> {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            Part::Lit { text, .. } => out.push_str(text),
            _ => return None,
        }
    }
    Some(out)
}

/// The word's value when every part is unquoted literal text: the only form in which a
/// reserved word such as `if` or `done` is recognized.
pub fn word_unquoted(word: &Word) -> Option<String> {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            Part::Lit {
                text,
                quoted: false,
            } => out.push_str(text),
            _ => return None,
        }
    }
    Some(out)
}

/// True for a shell assignment word: `NAME=…`, `NAME+=…` or `NAME[…]=…`, with the name
/// unquoted.
pub fn is_shell_assignment(word: &Word) -> bool {
    match word.parts.first() {
        Some(Part::Lit {
            text,
            quoted: false,
        }) => assignment_prefix_len(text).is_some(),
        _ => false,
    }
}

/// True for `NAME=value`, the form `env` and `sudo` read as an environment assignment.
pub fn is_env_assignment(text: &str) -> bool {
    match text.split_once('=') {
        Some((name, _)) => is_identifier(name),
        None => false,
    }
}

/// The length of the `NAME=`, `NAME+=` or `NAME[…]=` prefix of `text`, if it has one.
pub fn assignment_prefix_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().peekable();
    match chars.next() {
        Some((_, c)) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return None,
    }
    while let Some(&(i, c)) = chars.peek() {
        if c == '_' || c.is_ascii_alphanumeric() {
            chars.next();
            continue;
        }
        return match c {
            '=' => Some(i + 1),
            '+' => match text.get(i + 1..i + 2) {
                Some("=") => Some(i + 2),
                _ => None,
            },
            '[' => {
                let close = text.get(i..)?.find(']')? + i;
                match text.get(close + 1..close + 2) {
                    Some("=") => Some(close + 2),
                    Some("+") if text.get(close + 2..close + 3) == Some("=") => Some(close + 3),
                    _ => None,
                }
            }
            _ => None,
        };
    }
    None
}

/// True for a shell variable name.
pub fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The last `/`-separated component of `path`.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Flags and operands of one command's arguments (the program word excluded).
#[derive(Debug, Default)]
pub struct Parsed {
    /// Every flag spelling present: `--name` for `--name` and `--name=value`, and for `-abc`
    /// each of `-a`, `-b`, `-c` and `-abc` itself.
    pub flags: BTreeSet<String>,
    /// The operands, in order.
    pub operands: Vec<Operand>,
}

/// One operand: an index into the argument list, and whether the word directly before it
/// is a flag word (so it could be that flag's value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operand {
    pub index: usize,
    pub follows_flag: bool,
}

/// Splits arguments into flags and operands. Flags are read from literal words before a
/// bare `--`; a lone `-` and every word after `--` is an operand, and so is an opaque word.
pub fn parse_args(args: &[Arg]) -> Parsed {
    let mut parsed = Parsed::default();
    let mut after_dashdash = false;
    let mut prev_flag = false;
    for (index, arg) in args.iter().enumerate() {
        let text = match (&arg.literal, after_dashdash) {
            (Some(text), false) => text,
            _ => {
                parsed.operands.push(Operand {
                    index,
                    follows_flag: prev_flag,
                });
                prev_flag = false;
                continue;
            }
        };
        if text == "--" {
            after_dashdash = true;
            prev_flag = false;
        } else if let Some(long) = text.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            parsed.flags.insert(format!("--{name}"));
            prev_flag = true;
        } else if let Some(cluster) = text.strip_prefix('-').filter(|c| !c.is_empty()) {
            for c in cluster.chars() {
                parsed.flags.insert(format!("-{c}"));
            }
            parsed.flags.insert(text.clone());
            prev_flag = true;
        } else {
            parsed.operands.push(Operand {
                index,
                follows_flag: prev_flag,
            });
            prev_flag = false;
        }
    }
    parsed
}

/// Where a path's text starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// The text itself says: `/` for root, anything else relative.
    None,
    /// The home folder; the text, when non-empty, starts with `/`.
    Home,
}

/// Reads a shell word as a path: tilde and `$HOME` anchoring, then lexical normalization,
/// with an unquoted glob cutting the path at the directory before its first glob component.
fn word_path(word: &Word) -> PathKey {
    let mut parts = word.parts.iter();
    let mut chars: Vec<(char, bool)> = Vec::new();
    let anchor = match word.parts.first() {
        Some(Part::Param { name, .. }) if name == "HOME" => {
            parts.next();
            Anchor::Home
        }
        Some(Part::Lit {
            text,
            quoted: false,
        }) if text.starts_with('~') => {
            parts.next();
            let rest = text.get(1..).unwrap_or("");
            if !rest.is_empty() && !rest.starts_with('/') {
                return PathKey::Opaque;
            }
            if rest.is_empty() && word.parts.len() > 1 {
                // `~"x"` and `~$X`: the tilde prefix is not a plain `~`.
                return PathKey::Opaque;
            }
            chars.extend(rest.chars().map(|c| (c, false)));
            Anchor::Home
        }
        _ => Anchor::None,
    };
    for part in parts {
        match part {
            Part::Lit { text, quoted } => chars.extend(text.chars().map(|c| (c, *quoted))),
            _ => return PathKey::Opaque,
        }
    }
    path_from_chars(anchor, &chars)
}

/// Normalizes `chars` (each with its quoted marker) under `anchor`.
fn path_from_chars(anchor: Anchor, chars: &[(char, bool)]) -> PathKey {
    let rooted = anchor == Anchor::None && matches!(chars.first(), Some(('/', _)));
    if anchor == Anchor::Home && !chars.is_empty() && !matches!(chars.first(), Some(('/', _))) {
        return PathKey::Opaque;
    }
    let mut components: Vec<String> = Vec::new();
    for component in chars.split(|(c, _)| *c == '/') {
        if component
            .iter()
            .any(|(c, quoted)| !quoted && matches!(c, '*' | '?' | '['))
        {
            break;
        }
        let text: String = component.iter().map(|(c, _)| *c).collect();
        match text.as_str() {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() && !rooted {
                    return PathKey::Opaque;
                }
            }
            _ => components.push(text),
        }
    }
    match (anchor, rooted) {
        (Anchor::Home, _) => PathKey::Home(components),
        (Anchor::None, true) => PathKey::Root(components),
        (Anchor::None, false) => PathKey::Rel(components),
    }
}

/// Reads a configured path. `~` and `~/…` are home-anchored, `/…` is rooted, anything else
/// is relative. Glob characters are taken literally.
pub fn config_path(text: &str) -> Result<PathKey, String> {
    if text.is_empty() {
        return Err("must not be empty".to_string());
    }
    if text.contains('$') {
        return Err("a path cannot contain `$`; write the home folder as `~`".to_string());
    }
    let (anchor, rest) = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => (Anchor::Home, rest),
        Some(_) => {
            return Err("`~` is allowed only as `~` or `~/…`, not `~user`".to_string());
        }
        None => (Anchor::None, text),
    };
    let chars: Vec<(char, bool)> = rest.chars().map(|c| (c, true)).collect();
    match path_from_chars(anchor, &chars) {
        PathKey::Opaque => Err("`..` rises above the start of the path".to_string()),
        key => Ok(key),
    }
}

/// The reassembled command line of a direct call: the basename of `binary`, then each argv
/// element, separated by single spaces, with an element holding whitespace or a shell
/// metacharacter single-quoted.
pub fn reassemble(binary: &str, argv: &[String]) -> String {
    let mut out = basename(binary).to_string();
    for arg in argv {
        out.push(' ');
        out.push_str(&quote(arg));
    }
    out
}

fn quote(arg: &str) -> String {
    let needs = arg.is_empty()
        || arg.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '|' | '&'
                        | ';'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '$'
                        | '`'
                        | '\\'
                        | '"'
                        | '\''
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '#'
                        | '~'
                        | '{'
                        | '}'
                        | '!'
                )
        });
    if needs {
        format!("'{}'", arg.replace('\'', "'\\''"))
    } else {
        arg.to_string()
    }
}

/// The longest text a refusal quotes, in characters, the ellipsis included.
pub const CLIP_CHARS: usize = 200;

/// `text` with every run of whitespace collapsed to one space, clipped to [`CLIP_CHARS`]
/// characters with a trailing `…`.
pub fn collapse(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= CLIP_CHARS {
        return joined;
    }
    let mut out: String = joined.chars().take(CLIP_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::lex::{lex, Mode, Token};

    fn words(text: &str) -> Vec<Arg> {
        let script = lex(text, Mode::Shell, 0).unwrap();
        script
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(Arg::from_word(w, String::new())),
                _ => None,
            })
            .collect()
    }

    fn path(text: &str) -> PathKey {
        words(text).remove(0).path
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rooted_paths_normalize() {
        assert_eq!(path("/"), PathKey::Root(vec![]));
        assert_eq!(path("/etc/"), PathKey::Root(v(&["etc"])));
        assert_eq!(path("/usr/./lib/.."), PathKey::Root(v(&["usr"])));
        assert_eq!(path("//usr//bin"), PathKey::Root(v(&["usr", "bin"])));
        assert_eq!(path("/.."), PathKey::Root(vec![]));
    }

    #[test]
    fn home_paths_anchor() {
        assert_eq!(path("~"), PathKey::Home(vec![]));
        assert_eq!(path("~/"), PathKey::Home(vec![]));
        assert_eq!(path("~/a/b"), PathKey::Home(v(&["a", "b"])));
        assert_eq!(path("$HOME"), PathKey::Home(vec![]));
        assert_eq!(path("\"$HOME\""), PathKey::Home(vec![]));
        assert_eq!(path("${HOME}/x"), PathKey::Home(v(&["x"])));
        assert_eq!(path("\"${HOME}/x\""), PathKey::Home(v(&["x"])));
        assert_eq!(path("~bob"), PathKey::Opaque);
        assert_eq!(path("$HOMEX"), PathKey::Opaque);
        assert_eq!(path("~/.."), PathKey::Opaque);
        assert_eq!(path("'~'"), PathKey::Rel(v(&["~"])));
    }

    #[test]
    fn relative_paths() {
        assert_eq!(path("./build"), PathKey::Rel(v(&["build"])));
        assert_eq!(path("a/../b"), PathKey::Rel(v(&["b"])));
        assert_eq!(path("../x"), PathKey::Opaque);
        assert_eq!(path("$d"), PathKey::Opaque);
    }

    #[test]
    fn globs_cut_at_the_literal_directory() {
        assert_eq!(path("/*"), PathKey::Root(vec![]));
        assert_eq!(path("~/*"), PathKey::Home(vec![]));
        assert_eq!(path("/etc/*.conf"), PathKey::Root(v(&["etc"])));
        assert_eq!(path("'/*'"), PathKey::Root(v(&["*"])));
        assert_eq!(path("/us?/lib"), PathKey::Root(vec![]));
    }

    #[test]
    fn plain_args_take_no_expansion() {
        assert_eq!(Arg::plain("~").path, PathKey::Rel(v(&["~"])));
        assert_eq!(Arg::plain("/*").path, PathKey::Root(v(&["*"])));
        assert_eq!(Arg::plain("/etc").path, PathKey::Root(v(&["etc"])));
    }

    #[test]
    fn path_comparison() {
        let root = PathKey::Root(vec![]);
        let usr = PathKey::Root(v(&["usr"]));
        let usr_lib = PathKey::Root(v(&["usr", "lib"]));
        assert!(usr.is(&usr));
        assert!(!usr_lib.is(&usr));
        assert!(usr_lib.is_under(&usr));
        assert!(usr_lib.is_under(&root));
        assert!(!usr.is_under(&usr_lib));
        assert!(!PathKey::Home(vec![]).is_under(&root));
        assert!(!PathKey::Opaque.is(&PathKey::Opaque));
    }

    #[test]
    fn config_paths() {
        assert_eq!(config_path("/").unwrap(), PathKey::Root(vec![]));
        assert_eq!(config_path("~").unwrap(), PathKey::Home(vec![]));
        assert_eq!(config_path("~/.ssh").unwrap(), PathKey::Home(v(&[".ssh"])));
        assert_eq!(config_path("build").unwrap(), PathKey::Rel(v(&["build"])));
        assert!(config_path("").is_err());
        assert!(config_path("~bob").is_err());
        assert!(config_path("$HOME").is_err());
        assert!(config_path("../x").is_err());
    }

    #[test]
    fn flags_and_operands() {
        let args = words("-rf --force=yes - -- -x name");
        let parsed = parse_args(&args);
        for flag in ["-r", "-f", "-rf", "--force"] {
            assert!(parsed.flags.contains(flag), "{flag}");
        }
        assert!(!parsed.flags.contains("-x"));
        let ops: Vec<usize> = parsed.operands.iter().map(|o| o.index).collect();
        assert_eq!(ops, vec![2, 4, 5]);
        assert!(parsed.operands[0].follows_flag);
        assert!(!parsed.operands[1].follows_flag);
    }

    #[test]
    fn opaque_words_are_operands() {
        let parsed = parse_args(&words("\"$F\" -r"));
        assert_eq!(parsed.operands.len(), 1);
        assert!(parsed.flags.contains("-r"));
    }

    #[test]
    fn assignments() {
        assert_eq!(assignment_prefix_len("A=1"), Some(2));
        assert_eq!(assignment_prefix_len("A+=1"), Some(3));
        assert_eq!(assignment_prefix_len("a[2]=x"), Some(5));
        assert_eq!(assignment_prefix_len("1A=x"), None);
        assert_eq!(assignment_prefix_len("A"), None);
        assert!(is_env_assignment("FOO=1"));
        assert!(!is_env_assignment("-u"));
    }

    #[test]
    fn reassembly_quotes_metacharacters() {
        let argv = v(&["-rf", "./x", "a b", "|", "it's", ""]);
        assert_eq!(
            reassemble("/usr/bin/rm", &argv),
            "rm -rf ./x 'a b' '|' 'it'\\''s' ''"
        );
    }

    #[test]
    fn collapse_clips() {
        assert_eq!(collapse("a \n\t b"), "a b");
        let long = "x".repeat(500);
        let clipped = collapse(&long);
        assert_eq!(clipped.chars().count(), CLIP_CHARS);
        assert!(clipped.ends_with('…'));
    }
}
