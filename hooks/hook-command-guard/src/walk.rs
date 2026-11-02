//! Walks a lexed script into the simple commands it runs.
//!
//! The walker is lenient: it never fails. It finds every simple command in pipelines, lists,
//! subshells, `{ }` groups, `if`/`while`/`until`/`for`/`select`/`case` bodies, function bodies
//! and substitutions, and skips the syntax around them: assignments, reserved words, `for`
//! headers, `case` patterns, `[[ … ]]` and `(( … ))`. Substitutions inside skipped syntax
//! are still walked.

use crate::lex::{Op, Part, Script, Token, Word};
use crate::words::{is_shell_assignment, word_unquoted, Arg};

/// One simple command found in a script.
#[derive(Debug, Clone)]
pub struct Command {
    /// The program word first, then its arguments; assignments and redirection targets are
    /// removed.
    pub args: Vec<Arg>,
    /// True when the command's standard input comes from a pipe or a redirection rather than
    /// the shell's own input.
    pub stdin_fed: bool,
    /// The command's source text, assignments and redirections included.
    pub display: String,
    /// Nesting level of the script the command was found in.
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    /// Inside `for NAME in …` / `select …`, up to `do`.
    ForHeader,
    /// After `case`, waiting for the subject word.
    CaseSubject,
    /// After the `case` subject, waiting for `in`.
    CaseIn,
    /// Inside a `case` pattern, up to its `)`.
    CasePattern,
    /// Inside `[[ … ]]`.
    DoubleBracket,
}

struct Walker<'a> {
    script: &'a Script,
    base_fed: bool,
    out: &'a mut Vec<Command>,
    words: Vec<&'a Word>,
    subs: Vec<&'a Script>,
    span: Option<(usize, usize)>,
    stdin_redirected: bool,
    expect_target: bool,
    after_time: bool,
    stage: usize,
    frames: Vec<usize>,
    case_depth: usize,
    mode: Mode,
}

/// Appends every simple command in `script`, in source order, to `out`. `base_fed` is true
/// when the script's own standard input is a pipe or a redirection.
pub fn walk(script: &Script, base_fed: bool, out: &mut Vec<Command>) {
    let mut walker = Walker {
        script,
        base_fed,
        out,
        words: Vec::new(),
        subs: Vec::new(),
        span: None,
        stdin_redirected: false,
        expect_target: false,
        after_time: false,
        stage: 0,
        frames: Vec::new(),
        case_depth: 0,
        mode: Mode::Normal,
    };
    walker.run();
}

impl<'a> Walker<'a> {
    fn run(&mut self) {
        let tokens = &self.script.tokens;
        let mut i = 0;
        while let Some(token) = tokens.get(i) {
            i += 1;
            match token {
                Token::Op(op) => self.op(*op),
                Token::Redir(redir) => {
                    if self.mode != Mode::Normal {
                        continue;
                    }
                    self.extend_span(redir.span.start, redir.span.end);
                    let on_stdin = matches!(redir.fd.as_deref(), None | Some("0"));
                    if on_stdin && redir.op.reads_stdin() {
                        self.stdin_redirected = true;
                    }
                    self.expect_target = true;
                }
                Token::Arith(subs, span) => {
                    self.subs.extend(subs.iter());
                    if self.mode == Mode::Normal && !self.words.is_empty() {
                        self.extend_span(span.start, span.end);
                    }
                }
                Token::Word(word) => {
                    let skip = self.word(word, tokens.get(i), tokens.get(i + 1));
                    i += skip;
                }
            }
        }
        self.finish();
    }

    /// Base pipeline stage of the innermost compound command.
    fn base_stage(&self) -> usize {
        self.frames.last().copied().unwrap_or(0)
    }

    fn open(&mut self) {
        self.frames.push(self.stage);
    }

    fn close(&mut self) {
        self.stage = self.frames.pop().unwrap_or(0);
    }

    fn op(&mut self, op: Op) {
        match self.mode {
            Mode::ForHeader | Mode::DoubleBracket | Mode::CaseSubject | Mode::CaseIn => {
                return;
            }
            Mode::CasePattern => {
                if op == Op::RParen {
                    self.finish();
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Normal => {}
        }
        self.expect_target = false;
        self.finish();
        match op {
            Op::Pipe | Op::PipeBoth => self.stage += 1,
            Op::LParen => self.open(),
            Op::RParen => self.close(),
            Op::Semi | Op::Amp | Op::And | Op::Or | Op::Newline => {
                self.stage = self.base_stage();
            }
            Op::DSemi | Op::SemiAmp | Op::DSemiAmp => {
                self.stage = self.base_stage();
                if self.case_depth > 0 {
                    self.mode = Mode::CasePattern;
                }
            }
        }
    }

    /// Handles one word; returns how many following tokens it consumed.
    fn word(&mut self, word: &'a Word, next: Option<&Token>, next2: Option<&Token>) -> usize {
        let reserved = word_unquoted(word);
        let reserved = reserved.as_deref();
        match self.mode {
            Mode::DoubleBracket => {
                self.collect_subs(word);
                if reserved == Some("]]") {
                    self.mode = Mode::Normal;
                }
                return 0;
            }
            Mode::CasePattern => {
                if reserved == Some("esac") {
                    self.close_case();
                } else {
                    self.collect_subs(word);
                }
                return 0;
            }
            Mode::CaseSubject => {
                self.collect_subs(word);
                self.mode = Mode::CaseIn;
                return 0;
            }
            Mode::CaseIn => {
                self.mode = Mode::CasePattern;
                if reserved != Some("in") {
                    self.collect_subs(word);
                }
                return 0;
            }
            Mode::ForHeader => {
                if reserved == Some("do") {
                    self.mode = Mode::Normal;
                    self.finish();
                } else {
                    self.collect_subs(word);
                }
                return 0;
            }
            Mode::Normal => {}
        }
        if self.expect_target {
            self.expect_target = false;
            self.collect_subs(word);
            self.extend_span(word.span.start, word.span.end);
            return 0;
        }
        if !self.words.is_empty() {
            self.push_word(word);
            return 0;
        }
        if self.after_time {
            self.after_time = false;
            if reserved == Some("-p") {
                return 0;
            }
        }
        match reserved {
            Some("!" | "then" | "else" | "elif" | "do") => return 0,
            Some("{" | "if" | "while" | "until") => {
                self.open();
                return 0;
            }
            Some("}" | "fi" | "done") => {
                self.close();
                return 0;
            }
            Some("time") => {
                self.after_time = true;
                return 0;
            }
            Some("for" | "select") => {
                self.open();
                self.mode = Mode::ForHeader;
                return 0;
            }
            Some("case") => {
                self.open();
                self.case_depth += 1;
                self.mode = Mode::CaseSubject;
                return 0;
            }
            Some("esac") => {
                if self.case_depth > 0 {
                    self.close_case();
                }
                return 0;
            }
            Some("[[") => {
                self.mode = Mode::DoubleBracket;
                return 0;
            }
            Some("function") => {
                // `function name`: the name is skipped; `()` after it opens and closes nothing.
                return usize::from(matches!(next, Some(Token::Word(_))));
            }
            _ => {}
        }
        if is_shell_assignment(word) {
            self.collect_subs(word);
            self.extend_span(word.span.start, word.span.end);
            return 0;
        }
        if matches!(next, Some(Token::Op(Op::LParen)))
            && matches!(next2, Some(Token::Op(Op::RParen)))
        {
            // A function header `name ()`; the body that follows is walked.
            return 2;
        }
        self.push_word(word);
        0
    }

    fn close_case(&mut self) {
        self.finish();
        self.case_depth = self.case_depth.saturating_sub(1);
        self.close();
        self.mode = Mode::Normal;
    }

    fn push_word(&mut self, word: &'a Word) {
        self.collect_subs(word);
        self.extend_span(word.span.start, word.span.end);
        self.words.push(word);
    }

    fn extend_span(&mut self, start: usize, end: usize) {
        self.span = Some(match self.span {
            Some((s, e)) => (s.min(start), e.max(end)),
            None => (start, end),
        });
    }

    fn collect_subs(&mut self, word: &'a Word) {
        collect_word_subs(word, &mut self.subs);
    }

    /// Ends the current simple command: emits it, then walks the substitutions found in it.
    fn finish(&mut self) {
        let fed = self.base_fed || self.stage > 0;
        if !self.words.is_empty() {
            let source = &self.script.source;
            let args = self
                .words
                .iter()
                .map(|w| Arg::from_word(w, text(source, w.span.start, w.span.end)))
                .collect();
            let display = self
                .span
                .map(|(s, e)| text(source, s, e))
                .unwrap_or_default();
            self.out.push(Command {
                args,
                stdin_fed: fed || self.stdin_redirected,
                display,
                depth: self.script.depth,
            });
        }
        self.words.clear();
        self.span = None;
        self.stdin_redirected = false;
        self.after_time = false;
        for sub in std::mem::take(&mut self.subs) {
            walk(sub, fed, self.out);
        }
    }
}

/// The substitutions a word runs when it is expanded.
fn collect_word_subs<'a>(word: &'a Word, subs: &mut Vec<&'a Script>) {
    for part in &word.parts {
        match part {
            Part::CmdSub(s) | Part::ProcSub(s) => subs.push(s),
            Part::Param { subs: inner, .. } | Part::Arith(inner) => subs.extend(inner.iter()),
            Part::Array(list) => {
                for token in &list.tokens {
                    match token {
                        Token::Word(w) => collect_word_subs(w, subs),
                        Token::Arith(inner, _) => subs.extend(inner.iter()),
                        _ => {}
                    }
                }
            }
            Part::Lit { .. } | Part::Fragment => {}
        }
    }
}

fn text(source: &[char], start: usize, end: usize) -> String {
    source
        .get(start..end)
        .map(|chars| chars.iter().collect())
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::lex::{lex, Mode};

    fn commands(text: &str) -> Vec<Command> {
        let script = lex(text, Mode::Shell, 0).unwrap();
        let mut out = Vec::new();
        walk(&script, false, &mut out);
        out
    }

    /// Each command as its literal words joined by spaces (`?` for an opaque word).
    fn shapes(text: &str) -> Vec<String> {
        commands(text)
            .iter()
            .map(|c| {
                c.args
                    .iter()
                    .map(|a| a.literal.clone().unwrap_or_else(|| "?".into()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    #[test]
    fn lists_and_pipelines() {
        assert_eq!(
            shapes("a 1 | b && c || d; e & f\ng"),
            vec!["a 1", "b", "c", "d", "e", "f", "g"]
        );
    }

    #[test]
    fn groups_and_compound_commands() {
        assert_eq!(shapes("(cd a && rm x)"), vec!["cd a", "rm x"]);
        assert_eq!(shapes("{ rm x; }"), vec!["rm x"]);
        assert_eq!(
            shapes("if a; then b; elif c; then d; else e; fi"),
            vec!["a", "b", "c", "d", "e"]
        );
        assert_eq!(shapes("while a; do b; done"), vec!["a", "b"]);
        assert_eq!(shapes("until ! a; do b; done"), vec!["a", "b"]);
        assert_eq!(
            shapes("for f in $(ls) b; do rm \"$f\"; done"),
            vec!["ls", "rm ?"]
        );
        assert_eq!(shapes("for ((i=0;i<3;i++)); do b; done"), vec!["b"]);
        assert_eq!(
            shapes("case $a in b|c) x;; (d) y;& *) z;;& esac; w"),
            vec!["x", "y", "z", "w"]
        );
        assert_eq!(shapes("case $a in\n b) x\nesac"), vec!["x"]);
        assert_eq!(shapes("[[ -n $x && ( $y == z ) ]] && b"), vec!["b"]);
        assert_eq!(shapes("(( x > 2 )) && b"), vec!["b"]);
        assert_eq!(shapes("time -p a; ! b"), vec!["a", "b"]);
    }

    #[test]
    fn functions() {
        assert_eq!(shapes("f() { rm x; }; f"), vec!["rm x", "f"]);
        assert_eq!(shapes("function g { rm y; }"), vec!["rm y"]);
        assert_eq!(shapes("function g () { rm y; }"), vec!["rm y"]);
    }

    #[test]
    fn assignments_and_redirections() {
        assert_eq!(shapes("A=1 B=\"x y\" rm -f x >out 2>&1"), vec!["rm -f x"]);
        let cmds = commands("A=1 rm x > out");
        assert_eq!(cmds[0].display, "A=1 rm x > out");
        assert_eq!(shapes("y=`rm -rf x`"), vec!["rm -rf x"]);
        assert_eq!(shapes("cat <<EOF\nrm -rf /\nEOF\nls"), vec!["cat", "ls"]);
        assert_eq!(shapes("a=( $(rm x) ) ls"), vec!["ls", "rm x"]);
    }

    #[test]
    fn substitutions_are_walked() {
        assert_eq!(shapes("echo $(rm -rf x)"), vec!["echo ?", "rm -rf x"]);
        assert_eq!(shapes("diff <(rm -rf x) y"), vec!["diff ? y", "rm -rf x"]);
        assert_eq!(shapes("cat > $(rm a)"), vec!["cat", "rm a"]);
        assert_eq!(
            shapes("echo $((1 + $(rm b))) ${x:-$(rm c)}"),
            vec!["echo ? ?", "rm b", "rm c"]
        );
        assert_eq!(shapes("(( $(rm d) ))"), vec!["rm d"]);
    }

    #[test]
    fn stdin_feeding() {
        let cmds = commands("a | b; c < f; d; e <<< x; (f | (g; h)); i 2< x");
        let fed: Vec<bool> = cmds.iter().map(|c| c.stdin_fed).collect();
        assert_eq!(
            fed,
            vec![false, true, true, false, true, false, true, true, false]
        );
        let cmds = commands("x | while read l; do sh; done");
        assert!(cmds[2].stdin_fed);
    }

    #[test]
    fn display_is_source_text() {
        let cmds = commands("ls &&  sudo rm  -rf /tmp/x ; echo");
        assert_eq!(cmds[1].display, "sudo rm  -rf /tmp/x");
    }
}
