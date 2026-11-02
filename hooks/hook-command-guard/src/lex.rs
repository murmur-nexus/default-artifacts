//! A POSIX/bash shell lexer that yields words made of parts, operators and redirections.
//!
//! The lexer is the guard's only strict stage: an unterminated quote, `$(`, `${` or backtick
//! is an error, and so is nesting deeper than [`MAX_DEPTH`] levels of substitution or `-c`.
//! Either makes the whole script unclear. Heredoc bodies are data and are skipped.
//!
//! Input is first turned into units. In shell mode a unit is one character. In make and just
//! mode, a make expansion (`$(…)`, `${…}`, `$X`) or a just interpolation (`{{…}}`) is one
//! opaque unit, and make's `$$` is one `$` unit, so the lexer itself never sees template
//! syntax. Every unit keeps its span in the source, so a command's display text is always the
//! text as written.

use std::rc::Rc;

/// The deepest nesting of substitutions and `-c` scripts the guard reads.
pub const MAX_DEPTH: usize = 16;

/// How the input is read before lexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Plain shell text: a `-c` script or an npm script.
    Shell,
    /// A make recipe: line prefixes `@`, `-`, `+` are dropped, `$$` is `$`, and every other
    /// `$` expansion is an opaque fragment.
    Make,
    /// A just recipe: line prefixes `@` and `-` are dropped, and `{{…}}` is an opaque fragment.
    Just,
}

/// Why a script could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexError {
    /// A syntax error, described in a short phrase.
    Syntax(String),
    /// Substitutions or `-c` scripts nest deeper than [`MAX_DEPTH`].
    TooDeep,
}

fn syntax(text: &str) -> LexError {
    LexError::Syntax(text.to_string())
}

/// A lexed script. Nested scripts (substitutions) share their parent's source.
#[derive(Debug, Clone)]
pub struct Script {
    /// The text spans point into.
    pub source: Rc<[char]>,
    pub tokens: Vec<Token>,
    /// Nesting level: 0 for a top-level script.
    pub depth: usize,
}

/// A half-open range of character positions in a script's source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub enum Token {
    Word(Word),
    Op(Op),
    Redir(Redir),
    /// An arithmetic command `(( … ))`, with the substitutions found inside it.
    Arith(Vec<Script>, Span),
}

#[derive(Debug, Clone)]
pub struct Word {
    pub parts: Vec<Part>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Part {
    /// Literal text. `quoted` is true inside quotes and for a backslash-escaped character.
    Lit { text: String, quoted: bool },
    /// `$NAME`, `$1`, `${…}`: `name` is the variable name when it is a plain one, and the
    /// substitutions found inside `${…}` are kept.
    Param { name: String, subs: Vec<Script> },
    /// `$(…)` or `` `…` ``.
    CmdSub(Script),
    /// `<(…)` or `>(…)`.
    ProcSub(Script),
    /// `$((…))`, with the substitutions found inside it.
    Arith(Vec<Script>),
    /// The parenthesized list of an array assignment `NAME=(…)`. Its words are values, not
    /// commands; only their substitutions run.
    Array(Script),
    /// A make expansion or just interpolation.
    Fragment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Pipe,
    PipeBoth,
    Or,
    Amp,
    And,
    Semi,
    DSemi,
    SemiAmp,
    DSemiAmp,
    LParen,
    RParen,
    Newline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redir {
    pub op: RedirOp,
    /// The file-descriptor number or `{name}` written before the operator.
    pub fd: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirOp {
    In,
    Out,
    Append,
    Clobber,
    ReadWrite,
    DupIn,
    DupOut,
    OutErr,
    AppendErr,
    Heredoc,
    HeredocStrip,
    Herestring,
}

impl RedirOp {
    /// True for an operator that replaces standard input when no descriptor is written.
    pub fn reads_stdin(self) -> bool {
        matches!(
            self,
            RedirOp::In
                | RedirOp::ReadWrite
                | RedirOp::DupIn
                | RedirOp::Heredoc
                | RedirOp::HeredocStrip
                | RedirOp::Herestring
        )
    }
}

/// Lexes `text` read in `mode`, at nesting level `depth`.
pub fn lex(text: &str, mode: Mode, depth: usize) -> Result<Script, LexError> {
    if depth > MAX_DEPTH {
        return Err(LexError::TooDeep);
    }
    let source: Rc<[char]> = text.chars().collect::<Vec<_>>().into();
    let units = units(&source, mode)?;
    let budget = units.len().saturating_mul(8).saturating_add(4096);
    let mut lexer = Lexer {
        source: source.clone(),
        units,
        pos: 0,
        budget,
        heredocs: Vec::new(),
    };
    let tokens = lexer.list(depth, false)?;
    Ok(Script {
        source,
        tokens,
        depth,
    })
}

/// One input unit: a character, or an opaque template fragment.
#[derive(Debug, Clone, Copy)]
struct Unit {
    c: char,
    frag: bool,
    start: usize,
    end: usize,
}

impl Unit {
    fn is(&self, c: char) -> bool {
        !self.frag && self.c == c
    }
}

fn units(source: &[char], mode: Mode) -> Result<Vec<Unit>, LexError> {
    let mut out = Vec::with_capacity(source.len());
    let mut i = 0;
    let mut line_start = true;
    let at = |k: usize| source.get(k).copied();
    while let Some(c) = at(i) {
        if line_start && mode != Mode::Shell {
            let prefixes: &[char] = if mode == Mode::Make {
                &['@', '-', '+']
            } else {
                &['@', '-']
            };
            if c == ' ' || c == '\t' || prefixes.contains(&c) {
                i += 1;
                continue;
            }
        }
        line_start = false;
        let plain = Unit {
            c,
            frag: false,
            start: i,
            end: i + 1,
        };
        match (mode, c) {
            (Mode::Make | Mode::Just, '\n') => {
                let continued = i > 0 && at(i - 1) == Some('\\');
                line_start = !continued;
                out.push(plain);
                i += 1;
            }
            (Mode::Make, '$') => match at(i + 1) {
                Some('$') => {
                    out.push(Unit {
                        c: '$',
                        frag: false,
                        start: i,
                        end: i + 2,
                    });
                    i += 2;
                }
                Some(open @ ('(' | '{')) => {
                    let close = if open == '(' { ')' } else { '}' };
                    let mut level = 0usize;
                    let mut k = i + 1;
                    let end = loop {
                        match at(k) {
                            None => return Err(syntax("unterminated make expansion")),
                            Some(ch) if ch == open => level += 1,
                            Some(ch) if ch == close => {
                                level -= 1;
                                if level == 0 {
                                    break k + 1;
                                }
                            }
                            Some(_) => {}
                        }
                        k += 1;
                    };
                    out.push(Unit {
                        c: '$',
                        frag: true,
                        start: i,
                        end,
                    });
                    i = end;
                }
                Some(_) => {
                    out.push(Unit {
                        c: '$',
                        frag: true,
                        start: i,
                        end: i + 2,
                    });
                    i += 2;
                }
                None => {
                    out.push(plain);
                    i += 1;
                }
            },
            (Mode::Just, '{') if at(i + 1) == Some('{') => {
                if at(i + 2) == Some('{') && at(i + 3) == Some('{') {
                    for k in 0..2 {
                        out.push(Unit {
                            c: '{',
                            frag: false,
                            start: i + 2 * k,
                            end: i + 2 * k + 2,
                        });
                    }
                    i += 4;
                    continue;
                }
                let mut k = i + 2;
                let end = loop {
                    match at(k) {
                        None => return Err(syntax("unterminated `{{`")),
                        Some('}') if at(k + 1) == Some('}') => break k + 2,
                        Some(_) => k += 1,
                    }
                };
                out.push(Unit {
                    c: '{',
                    frag: true,
                    start: i,
                    end,
                });
                i = end;
            }
            _ => {
                out.push(plain);
                i += 1;
            }
        }
    }
    Ok(out)
}

struct PendingHeredoc {
    delimiter: String,
    strip_tabs: bool,
}

struct Lexer {
    source: Rc<[char]>,
    units: Vec<Unit>,
    pos: usize,
    /// Units the lexer may still step over. Backtracking spends it too, so no input can make
    /// the lexer run long.
    budget: usize,
    heredocs: Vec<PendingHeredoc>,
}

/// Words that leave the lexer at command position, where `((` starts an arithmetic command.
const COMMAND_POSITION_WORDS: &[&str] = &[
    "!", "{", "if", "then", "elif", "else", "while", "until", "do", "time", "for",
];

impl Lexer {
    fn peek(&self) -> Option<Unit> {
        self.units.get(self.pos).copied()
    }

    fn peek_at(&self, k: usize) -> Option<Unit> {
        self.units.get(self.pos + k).copied()
    }

    fn peek_is(&self, k: usize, c: char) -> bool {
        self.peek_at(k).is_some_and(|u| u.is(c))
    }

    fn bump(&mut self) {
        self.pos += 1;
        self.budget = self.budget.saturating_sub(1);
    }

    fn bump_n(&mut self, n: usize) {
        for _ in 0..n {
            self.bump();
        }
    }

    fn check_budget(&self) -> Result<(), LexError> {
        if self.budget == 0 {
            Err(syntax("the script is too complex to read"))
        } else {
            Ok(())
        }
    }

    /// End position (exclusive) of the unit before the cursor.
    fn last_end(&self, fallback: usize) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|k| self.units.get(k))
            .map_or(fallback, |u| u.end)
    }

    fn here_start(&self) -> usize {
        self.peek()
            .map_or_else(|| self.last_end(self.source.len()), |u| u.start)
    }

    fn skip_blanks(&mut self) {
        while let Some(u) = self.peek() {
            if u.is(' ') || u.is('\t') {
                self.bump();
            } else if u.is('\\') && self.peek_is(1, '\n') {
                self.bump_n(2);
            } else {
                break;
            }
        }
    }

    /// Lexes a command list up to the end of input, or, when `nested`, up to the `)` that
    /// closes the substitution the cursor is inside.
    fn list(&mut self, depth: usize, nested: bool) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        let mut balance = 0usize;
        let mut case_depth = 0usize;
        let mut command_position = true;
        let mut want_delimiter: Option<bool> = None;
        loop {
            self.check_budget()?;
            self.skip_blanks();
            let Some(u) = self.peek() else {
                if want_delimiter.is_some() {
                    return Err(syntax("a heredoc has no delimiter"));
                }
                if nested {
                    return Err(syntax("unterminated `$(`"));
                }
                return Ok(tokens);
            };
            let start = u.start;
            if !u.frag {
                match u.c {
                    '#' => {
                        while self.peek().is_some_and(|u| !u.is('\n')) {
                            self.bump();
                        }
                        continue;
                    }
                    '\n' => {
                        if want_delimiter.is_some() {
                            return Err(syntax("a heredoc has no delimiter"));
                        }
                        self.bump();
                        tokens.push(Token::Op(Op::Newline));
                        self.read_heredocs();
                        command_position = true;
                        continue;
                    }
                    ')' => {
                        if want_delimiter.is_some() {
                            return Err(syntax("a heredoc has no delimiter"));
                        }
                        self.bump();
                        if nested && balance == 0 && case_depth == 0 {
                            return Ok(tokens);
                        }
                        balance = balance.saturating_sub(1);
                        tokens.push(Token::Op(Op::RParen));
                        command_position = true;
                        continue;
                    }
                    '(' => {
                        if want_delimiter.is_some() {
                            return Err(syntax("a heredoc has no delimiter"));
                        }
                        if command_position && self.peek_is(1, '(') {
                            let saved = (self.pos, self.heredocs.len());
                            self.bump_n(2);
                            if let Some(subs) = self.arith(depth + 1, "unterminated `((`")? {
                                let end = self.last_end(start);
                                tokens.push(Token::Arith(subs, Span { start, end }));
                                command_position = false;
                                continue;
                            }
                            self.pos = saved.0;
                            self.heredocs.truncate(saved.1);
                        }
                        self.bump();
                        balance += 1;
                        tokens.push(Token::Op(Op::LParen));
                        command_position = true;
                        continue;
                    }
                    '<' | '>' if !self.peek_is(1, '(') => {
                        if want_delimiter.is_some() {
                            return Err(syntax("a heredoc has no delimiter"));
                        }
                        let redir = self.redirection(None, start);
                        if matches!(redir.op, RedirOp::Heredoc | RedirOp::HeredocStrip) {
                            want_delimiter = Some(redir.op == RedirOp::HeredocStrip);
                        }
                        tokens.push(Token::Redir(redir));
                        command_position = false;
                        continue;
                    }
                    '|' | '&' | ';' => {
                        if want_delimiter.is_some() {
                            return Err(syntax("a heredoc has no delimiter"));
                        }
                        if u.c == '&' && self.peek_is(1, '>') {
                            let redir = self.redirection(None, start);
                            tokens.push(Token::Redir(redir));
                            command_position = false;
                            continue;
                        }
                        let op = self.operator(u.c);
                        tokens.push(Token::Op(op));
                        command_position = true;
                        continue;
                    }
                    _ => {}
                }
            }
            let word = self.word(depth)?;
            if let Some(strip_tabs) = want_delimiter.take() {
                self.heredocs.push(PendingHeredoc {
                    delimiter: delimiter_text(&word),
                    strip_tabs,
                });
                tokens.push(Token::Word(word));
                command_position = false;
                continue;
            }
            let next_is_redir = self
                .peek()
                .is_some_and(|n| (n.is('<') || n.is('>')) && !self.peek_is(1, '('));
            if next_is_redir && is_fd_word(&word) {
                let fd = crate::words::word_literal(&word);
                let redir = self.redirection(fd, word.span.start);
                if matches!(redir.op, RedirOp::Heredoc | RedirOp::HeredocStrip) {
                    want_delimiter = Some(redir.op == RedirOp::HeredocStrip);
                }
                tokens.push(Token::Redir(redir));
                command_position = false;
                continue;
            }
            let unquoted = crate::words::word_unquoted(&word);
            if command_position {
                match unquoted.as_deref() {
                    Some("case") => case_depth += 1,
                    Some("esac") => case_depth = case_depth.saturating_sub(1),
                    _ => {}
                }
            }
            command_position = command_position
                && unquoted
                    .as_deref()
                    .is_some_and(|w| COMMAND_POSITION_WORDS.contains(&w));
            tokens.push(Token::Word(word));
        }
    }

    fn operator(&mut self, first: char) -> Op {
        self.bump();
        match first {
            '|' => {
                if self.peek_is(0, '&') {
                    self.bump();
                    Op::PipeBoth
                } else if self.peek_is(0, '|') {
                    self.bump();
                    Op::Or
                } else {
                    Op::Pipe
                }
            }
            '&' => {
                if self.peek_is(0, '&') {
                    self.bump();
                    Op::And
                } else {
                    Op::Amp
                }
            }
            _ => {
                if self.peek_is(0, ';') {
                    self.bump();
                    if self.peek_is(0, '&') {
                        self.bump();
                        Op::DSemiAmp
                    } else {
                        Op::DSemi
                    }
                } else if self.peek_is(0, '&') {
                    self.bump();
                    Op::SemiAmp
                } else {
                    Op::Semi
                }
            }
        }
    }

    /// Lexes a redirection operator at the cursor (`<`, `>` or `&>`).
    fn redirection(&mut self, fd: Option<String>, start: usize) -> Redir {
        let first = self.peek().map_or(' ', |u| u.c);
        self.bump();
        let op = match first {
            '&' => {
                self.bump();
                if self.peek_is(0, '>') {
                    self.bump();
                    RedirOp::AppendErr
                } else {
                    RedirOp::OutErr
                }
            }
            '<' => {
                if self.peek_is(0, '<') {
                    self.bump();
                    if self.peek_is(0, '<') {
                        self.bump();
                        RedirOp::Herestring
                    } else if self.peek_is(0, '-') {
                        self.bump();
                        RedirOp::HeredocStrip
                    } else {
                        RedirOp::Heredoc
                    }
                } else if self.peek_is(0, '>') {
                    self.bump();
                    RedirOp::ReadWrite
                } else if self.peek_is(0, '&') {
                    self.bump();
                    RedirOp::DupIn
                } else {
                    RedirOp::In
                }
            }
            _ => {
                if self.peek_is(0, '>') {
                    self.bump();
                    RedirOp::Append
                } else if self.peek_is(0, '|') {
                    self.bump();
                    RedirOp::Clobber
                } else if self.peek_is(0, '&') {
                    self.bump();
                    RedirOp::DupOut
                } else {
                    RedirOp::Out
                }
            }
        };
        let end = self.last_end(start);
        Redir {
            op,
            fd,
            span: Span { start, end },
        }
    }

    /// Skips the body of every pending heredoc, starting at the cursor (just past a newline).
    fn read_heredocs(&mut self) {
        let pending = std::mem::take(&mut self.heredocs);
        for heredoc in pending {
            loop {
                if self.peek().is_none() {
                    return;
                }
                let mut line = String::new();
                while let Some(u) = self.peek() {
                    self.bump();
                    if u.is('\n') {
                        break;
                    }
                    line.push(if u.frag { '\u{FFFD}' } else { u.c });
                }
                let line = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == heredoc.delimiter {
                    break;
                }
            }
        }
    }

    fn word(&mut self, depth: usize) -> Result<Word, LexError> {
        let start = self.here_start();
        let mut parts: Vec<Part> = Vec::new();
        let mut consumed = false;
        while let Some(u) = self.peek() {
            self.check_budget()?;
            if u.frag {
                self.bump();
                parts.push(Part::Fragment);
                consumed = true;
                continue;
            }
            match u.c {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | ')' => break,
                '(' => {
                    if extglob_prefix(&parts).is_some() {
                        self.extglob(&mut parts)?;
                        continue;
                    }
                    if is_array_assignment_prefix(&parts) {
                        self.bump();
                        if depth + 1 > MAX_DEPTH {
                            return Err(LexError::TooDeep);
                        }
                        let tokens = self.list(depth + 1, true)?;
                        parts.push(Part::Array(self.script(tokens, depth + 1)));
                        continue;
                    }
                    break;
                }
                '<' | '>' => {
                    if parts.is_empty() && !consumed && self.peek_is(1, '(') {
                        self.bump_n(2);
                        if depth + 1 > MAX_DEPTH {
                            return Err(LexError::TooDeep);
                        }
                        let tokens = self.list(depth + 1, true)?;
                        parts.push(Part::ProcSub(self.script(tokens, depth + 1)));
                        consumed = true;
                        continue;
                    }
                    break;
                }
                '\'' => {
                    self.bump();
                    consumed = true;
                    self.single_quoted(&mut parts)?;
                }
                '"' => {
                    self.bump();
                    consumed = true;
                    self.double_quoted(&mut parts, depth)?;
                }
                '\\' => {
                    self.bump();
                    consumed = true;
                    match self.peek() {
                        None => push_lit(&mut parts, '\\', false),
                        Some(n) if n.is('\n') => self.bump(),
                        Some(n) if n.frag => {}
                        Some(n) => {
                            self.bump();
                            push_lit(&mut parts, n.c, true);
                        }
                    }
                }
                '$' => {
                    consumed = true;
                    if let Some(part) = self.dollar(depth, false)? {
                        parts.push(part);
                    }
                }
                '`' => {
                    consumed = true;
                    let script = self.backtick(depth)?;
                    parts.push(Part::CmdSub(script));
                }
                c => {
                    self.bump();
                    consumed = true;
                    push_lit(&mut parts, c, false);
                }
            }
        }
        if parts.is_empty() {
            parts.push(Part::Lit {
                text: String::new(),
                quoted: true,
            });
        }
        let end = self.last_end(start);
        Ok(Word {
            parts,
            span: Span { start, end },
        })
    }

    fn script(&self, tokens: Vec<Token>, depth: usize) -> Script {
        Script {
            source: self.source.clone(),
            tokens,
            depth,
        }
    }

    /// Consumes a balanced `( … )` after an extglob operator as unquoted literal text.
    fn extglob(&mut self, parts: &mut Vec<Part>) -> Result<(), LexError> {
        let mut level = 0usize;
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax("unterminated extended glob"));
            };
            self.bump();
            if u.frag {
                parts.push(Part::Fragment);
                continue;
            }
            push_lit(parts, u.c, false);
            if u.c == '(' {
                level += 1;
            } else if u.c == ')' {
                level -= 1;
                if level == 0 {
                    return Ok(());
                }
            }
        }
    }

    fn single_quoted(&mut self, parts: &mut Vec<Part>) -> Result<(), LexError> {
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax("unterminated single quote"));
            };
            self.bump();
            if u.frag {
                parts.push(Part::Fragment);
            } else if u.c == '\'' {
                return Ok(());
            } else {
                push_lit(parts, u.c, true);
            }
        }
    }

    fn double_quoted(&mut self, parts: &mut Vec<Part>, depth: usize) -> Result<(), LexError> {
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax("unterminated double quote"));
            };
            if u.frag {
                self.bump();
                parts.push(Part::Fragment);
                continue;
            }
            match u.c {
                '"' => {
                    self.bump();
                    return Ok(());
                }
                '\\' => {
                    self.bump();
                    match self.peek() {
                        Some(n) if n.is('\n') => self.bump(),
                        Some(n) if !n.frag && matches!(n.c, '$' | '`' | '"' | '\\') => {
                            self.bump();
                            push_lit(parts, n.c, true);
                        }
                        _ => push_lit(parts, '\\', true),
                    }
                }
                '$' => {
                    if let Some(part) = self.dollar(depth, true)? {
                        parts.push(part);
                    }
                }
                '`' => {
                    let script = self.backtick(depth)?;
                    parts.push(Part::CmdSub(script));
                }
                c => {
                    self.bump();
                    push_lit(parts, c, true);
                }
            }
        }
    }

    /// Lexes what follows a `$` at the cursor. Returns `None` when the `$` only opens a
    /// locale-quoted string (`$"…"`), which the caller then reads as a double-quoted string.
    fn dollar(&mut self, depth: usize, in_dquote: bool) -> Result<Option<Part>, LexError> {
        let Some(next) = self.peek_at(1).filter(|n| !n.frag) else {
            self.bump();
            return Ok(Some(lit_part("$", in_dquote)));
        };
        match next.c {
            '(' => {
                if self.peek_is(2, '(') {
                    let saved = (self.pos, self.heredocs.len());
                    self.bump_n(3);
                    if let Some(subs) = self.arith(depth + 1, "unterminated `$((`")? {
                        return Ok(Some(Part::Arith(subs)));
                    }
                    self.pos = saved.0;
                    self.heredocs.truncate(saved.1);
                }
                self.bump_n(2);
                if depth + 1 > MAX_DEPTH {
                    return Err(LexError::TooDeep);
                }
                let tokens = self.list(depth + 1, true)?;
                Ok(Some(Part::CmdSub(self.script(tokens, depth + 1))))
            }
            '{' => {
                self.bump_n(2);
                self.braced_param(depth + 1, in_dquote).map(Some)
            }
            '\'' if !in_dquote => {
                self.bump_n(2);
                self.ansi_c().map(Some)
            }
            '"' if !in_dquote => {
                self.bump();
                Ok(None)
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                self.bump();
                let mut name = String::new();
                while let Some(u) = self.peek() {
                    if !u.frag && (u.c == '_' || u.c.is_ascii_alphanumeric()) {
                        name.push(u.c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                Ok(Some(Part::Param {
                    name,
                    subs: Vec::new(),
                }))
            }
            c if c.is_ascii_digit() || matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!') => {
                self.bump_n(2);
                Ok(Some(Part::Param {
                    name: c.to_string(),
                    subs: Vec::new(),
                }))
            }
            _ => {
                self.bump();
                Ok(Some(lit_part("$", in_dquote)))
            }
        }
    }

    /// Scans an arithmetic body after `$((` or `((` up to its `))`. Returns `None` when a
    /// lone `)` shows the text is not arithmetic, so the caller rewinds and reads it as
    /// nested parentheses.
    fn arith(&mut self, depth: usize, unterminated: &str) -> Result<Option<Vec<Script>>, LexError> {
        if depth > MAX_DEPTH {
            return Err(LexError::TooDeep);
        }
        let mut level = 0usize;
        let mut subs = Vec::new();
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax(unterminated));
            };
            if u.frag {
                self.bump();
                continue;
            }
            match u.c {
                '(' => {
                    level += 1;
                    self.bump();
                }
                ')' => {
                    if level == 0 {
                        if self.peek_is(1, ')') {
                            self.bump_n(2);
                            return Ok(Some(subs));
                        }
                        return Ok(None);
                    }
                    level -= 1;
                    self.bump();
                }
                '\\' => self.bump_n(2),
                '\'' => {
                    self.bump();
                    loop {
                        let Some(q) = self.peek() else {
                            return Err(syntax("unterminated single quote"));
                        };
                        self.bump();
                        if q.is('\'') {
                            break;
                        }
                    }
                }
                '$' => {
                    if let Some(part) = self.dollar(depth, true)? {
                        collect_part_subs(part, &mut subs);
                    }
                }
                '`' => subs.push(self.backtick(depth)?),
                _ => self.bump(),
            }
        }
    }

    /// Scans `${…}` after its opening brace.
    fn braced_param(&mut self, depth: usize, in_dquote: bool) -> Result<Part, LexError> {
        if depth > MAX_DEPTH {
            return Err(LexError::TooDeep);
        }
        let mut level = 0usize;
        let mut raw = String::new();
        let mut plain = true;
        let mut subs = Vec::new();
        let mut in_double = false;
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax("unterminated `${`"));
            };
            if u.frag {
                self.bump();
                plain = false;
                continue;
            }
            match u.c {
                '}' if level == 0 && !in_double => {
                    self.bump();
                    break;
                }
                '}' if !in_double => {
                    level -= 1;
                    plain = false;
                    self.bump();
                }
                '{' if !in_double => {
                    level += 1;
                    plain = false;
                    self.bump();
                }
                '\\' => {
                    plain = false;
                    self.bump_n(2);
                }
                '\'' if !in_dquote && !in_double => {
                    plain = false;
                    self.bump();
                    loop {
                        let Some(q) = self.peek() else {
                            return Err(syntax("unterminated single quote"));
                        };
                        self.bump();
                        if q.is('\'') {
                            break;
                        }
                    }
                }
                '"' => {
                    plain = false;
                    in_double = !in_double;
                    self.bump();
                }
                '$' => {
                    plain = false;
                    if let Some(part) = self.dollar(depth, true)? {
                        collect_part_subs(part, &mut subs);
                    }
                }
                '`' => {
                    plain = false;
                    subs.push(self.backtick(depth)?);
                }
                c => {
                    raw.push(c);
                    self.bump();
                }
            }
        }
        let name = if plain { raw } else { String::new() };
        Ok(Part::Param { name, subs })
    }

    /// Reads `$'…'` after its opening quote.
    fn ansi_c(&mut self) -> Result<Part, LexError> {
        let mut text = String::new();
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax("unterminated `$'`"));
            };
            self.bump();
            if u.frag {
                text.push('\u{FFFD}');
                continue;
            }
            match u.c {
                '\'' => break,
                '\\' => {
                    let Some(e) = self.peek().filter(|e| !e.frag) else {
                        text.push('\\');
                        continue;
                    };
                    self.bump();
                    match e.c {
                        'n' => text.push('\n'),
                        't' => text.push('\t'),
                        'r' => text.push('\r'),
                        'a' => text.push('\u{7}'),
                        'b' => text.push('\u{8}'),
                        'e' | 'E' => text.push('\u{1b}'),
                        'f' => text.push('\u{c}'),
                        'v' => text.push('\u{b}'),
                        '\\' | '\'' | '"' | '?' => text.push(e.c),
                        'x' => {
                            let value = self.digits(16, 2);
                            text.push(value.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
                        }
                        'u' => {
                            let value = self.digits(16, 4);
                            text.push(value.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
                        }
                        'U' => {
                            let value = self.digits(16, 8);
                            text.push(value.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
                        }
                        'c' => {
                            if let Some(x) = self.peek().filter(|x| !x.frag) {
                                self.bump();
                                let code = (x.c as u32) & 0x1f;
                                text.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                            }
                        }
                        d @ '0'..='7' => {
                            let mut value = d.to_digit(8).unwrap_or(0);
                            for _ in 0..2 {
                                match self
                                    .peek()
                                    .filter(|x| !x.frag)
                                    .and_then(|x| x.c.to_digit(8))
                                {
                                    Some(digit) => {
                                        value = value * 8 + digit;
                                        self.bump();
                                    }
                                    None => break,
                                }
                            }
                            text.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
                        }
                        other => {
                            text.push('\\');
                            text.push(other);
                        }
                    }
                }
                c => text.push(c),
            }
        }
        Ok(Part::Lit { text, quoted: true })
    }

    /// Reads up to `max` digits in `radix`; `None` when there is none.
    fn digits(&mut self, radix: u32, max: usize) -> Option<u32> {
        let mut value: Option<u32> = None;
        for _ in 0..max {
            let Some(d) = self
                .peek()
                .filter(|u| !u.frag)
                .and_then(|u| u.c.to_digit(radix))
            else {
                break;
            };
            self.bump();
            value = Some(value.unwrap_or(0).wrapping_mul(radix).wrapping_add(d));
        }
        value
    }

    /// Reads `` `…` `` at the cursor and lexes its unescaped body as a nested script.
    fn backtick(&mut self, depth: usize) -> Result<Script, LexError> {
        let inner_depth = depth + 1;
        if inner_depth > MAX_DEPTH {
            return Err(LexError::TooDeep);
        }
        self.bump();
        let mut body = Vec::new();
        loop {
            self.check_budget()?;
            let Some(u) = self.peek() else {
                return Err(syntax("unterminated backtick"));
            };
            self.bump();
            if u.is('`') {
                break;
            }
            if u.is('\\') {
                if let Some(n) = self.peek() {
                    if !n.frag && matches!(n.c, '`' | '$' | '\\') {
                        self.bump();
                        body.push(n);
                        continue;
                    }
                }
            }
            body.push(u);
        }
        let mut inner = Lexer {
            source: self.source.clone(),
            units: body,
            pos: 0,
            budget: self.budget,
            heredocs: Vec::new(),
        };
        let tokens = inner.list(inner_depth, false);
        self.budget = inner.budget;
        Ok(self.script(tokens?, inner_depth))
    }
}

fn lit_part(text: &str, quoted: bool) -> Part {
    Part::Lit {
        text: text.to_string(),
        quoted,
    }
}

/// Appends `c` to the word, merging with a preceding literal of the same quotedness.
fn push_lit(parts: &mut Vec<Part>, c: char, quoted: bool) {
    if let Some(Part::Lit { text, quoted: q }) = parts.last_mut() {
        if *q == quoted {
            text.push(c);
            return;
        }
    }
    parts.push(Part::Lit {
        text: c.to_string(),
        quoted,
    });
}

fn extglob_prefix(parts: &[Part]) -> Option<char> {
    match parts.last() {
        Some(Part::Lit {
            text,
            quoted: false,
        }) => text
            .chars()
            .last()
            .filter(|c| matches!(c, '@' | '!' | '+' | '*' | '?')),
        _ => None,
    }
}

fn is_array_assignment_prefix(parts: &[Part]) -> bool {
    match parts {
        [Part::Lit {
            text,
            quoted: false,
        }] => crate::words::assignment_prefix_len(text) == Some(text.len()),
        _ => false,
    }
}

fn collect_part_subs(part: Part, subs: &mut Vec<Script>) {
    match part {
        Part::CmdSub(s) | Part::ProcSub(s) => subs.push(s),
        Part::Param { subs: inner, .. } | Part::Arith(inner) => subs.extend(inner),
        _ => {}
    }
}

/// A heredoc delimiter is the word with its quoting removed.
fn delimiter_text(word: &Word) -> String {
    let mut out = String::new();
    for part in &word.parts {
        if let Part::Lit { text, .. } = part {
            out.push_str(text);
        }
    }
    out
}

/// True for a word written as a redirection's descriptor: unquoted digits or `{name}`.
fn is_fd_word(word: &Word) -> bool {
    let Some(text) = crate::words::word_unquoted(word) else {
        return false;
    };
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    text.strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .is_some_and(crate::words::is_identifier)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<Token> {
        lex(text, Mode::Shell, 0).unwrap().tokens
    }

    fn word_texts(text: &str) -> Vec<String> {
        tokens(text)
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(crate::words::word_literal(w).unwrap_or("?".into())),
                _ => None,
            })
            .collect()
    }

    fn err(text: &str) -> LexError {
        lex(text, Mode::Shell, 0).unwrap_err()
    }

    #[test]
    fn quoting() {
        assert_eq!(
            word_texts(r#"a 'b c' "d $"  e\ f $'g\nh' \rm"#),
            vec!["a", "b c", "d $", "e f", "g\nh", "rm"]
        );
        assert_eq!(word_texts("'' \"\""), vec!["", ""]);
        assert_eq!(word_texts("a\\\nb"), vec!["ab"]);
        assert_eq!(word_texts("\\"), vec!["\\"]);
    }

    #[test]
    fn operators() {
        let ops: Vec<Op> = tokens("a | b |& c || d & e && f ; g ;; h ;& i ;;& j\n( k )")
            .iter()
            .filter_map(|t| match t {
                Token::Op(op) => Some(*op),
                _ => None,
            })
            .collect();
        assert_eq!(
            ops,
            vec![
                Op::Pipe,
                Op::PipeBoth,
                Op::Or,
                Op::Amp,
                Op::And,
                Op::Semi,
                Op::DSemi,
                Op::SemiAmp,
                Op::DSemiAmp,
                Op::Newline,
                Op::LParen,
                Op::RParen
            ]
        );
    }

    #[test]
    fn redirections() {
        let redirs: Vec<(RedirOp, Option<String>)> =
            tokens("a <x >y >>z >|w <>v <&0 >&2 &>u &>>t <<<s 2>e {fd}>f")
                .into_iter()
                .filter_map(|t| match t {
                    Token::Redir(r) => Some((r.op, r.fd)),
                    _ => None,
                })
                .collect();
        let ops: Vec<RedirOp> = redirs.iter().map(|r| r.0).collect();
        assert_eq!(
            ops,
            vec![
                RedirOp::In,
                RedirOp::Out,
                RedirOp::Append,
                RedirOp::Clobber,
                RedirOp::ReadWrite,
                RedirOp::DupIn,
                RedirOp::DupOut,
                RedirOp::OutErr,
                RedirOp::AppendErr,
                RedirOp::Herestring,
                RedirOp::Out,
                RedirOp::Out
            ]
        );
        assert_eq!(redirs[10].1.as_deref(), Some("2"));
        assert_eq!(redirs[11].1.as_deref(), Some("{fd}"));
    }

    #[test]
    fn substitutions_nest() {
        let toks = tokens("echo $(ls `pwd`) <(a) $((1 + $(b))) ${x:-$(c)}");
        let Token::Word(w) = &toks[1] else { panic!() };
        let Part::CmdSub(inner) = &w.parts[0] else {
            panic!()
        };
        assert_eq!(inner.depth, 1);
        let Token::Word(w) = &inner.tokens[1] else {
            panic!()
        };
        assert!(matches!(&w.parts[0], Part::CmdSub(s) if s.depth == 2));
        let Token::Word(w) = &toks[2] else { panic!() };
        assert!(matches!(&w.parts[0], Part::ProcSub(_)));
        let Token::Word(w) = &toks[3] else { panic!() };
        assert!(matches!(&w.parts[0], Part::Arith(s) if s.len() == 1));
        let Token::Word(w) = &toks[4] else { panic!() };
        assert!(matches!(&w.parts[0], Part::Param { subs, .. } if subs.len() == 1));
    }

    #[test]
    fn params() {
        let toks = tokens("$HOME ${HOME} \"$HOME\" $1 $@");
        let names: Vec<String> = toks
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => w.parts.iter().find_map(|p| match p {
                    Part::Param { name, .. } => Some(name.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["HOME", "HOME", "HOME", "1", "@"]);
    }

    #[test]
    fn heredoc_bodies_are_skipped() {
        assert_eq!(
            word_texts("cat <<'EOF'\nrm -rf /\nEOF\nls"),
            vec!["cat", "EOF", "ls"]
        );
        assert_eq!(
            word_texts("cat <<-E\n\trm\n\tE\nls"),
            vec!["cat", "E", "ls"]
        );
        assert_eq!(word_texts("cat <<E\nrm -rf /"), vec!["cat", "E"]);
        assert!(matches!(err("cat <<"), LexError::Syntax(_)));
    }

    #[test]
    fn comments() {
        assert_eq!(word_texts("# rm -rf /\nls a#b"), vec!["ls", "a#b"]);
    }

    #[test]
    fn errors() {
        for (text, phrase) in [
            ("rm 'x", "unterminated single quote"),
            ("rm \"x", "unterminated double quote"),
            ("echo $(ls", "unterminated `$(`"),
            ("echo ${x", "unterminated `${`"),
            ("echo `ls", "unterminated backtick"),
            ("echo $'x", "unterminated `$'`"),
        ] {
            assert_eq!(err(text), LexError::Syntax(phrase.to_string()), "{text}");
        }
    }

    #[test]
    fn depth_limit() {
        let ok = format!("{}x{}", "$(".repeat(16), ")".repeat(16));
        assert!(lex(&ok, Mode::Shell, 0).is_ok());
        let deep = format!("{}x{}", "$(".repeat(17), ")".repeat(17));
        assert_eq!(err(&deep), LexError::TooDeep);
        let ticks = "echo $(a `b $(c)`)";
        assert!(lex(ticks, Mode::Shell, 15).is_err());
        assert!(lex(ticks, Mode::Shell, 13).is_ok());
    }

    #[test]
    fn arithmetic_command_and_fallback() {
        let toks = tokens("(( x << 2 )); ls");
        assert!(matches!(toks[0], Token::Arith(_, _)));
        let toks = tokens("((cd a) && ls)");
        assert!(matches!(toks[0], Token::Op(Op::LParen)));
        assert!(matches!(toks[1], Token::Op(Op::LParen)));
        let toks = tokens("echo $( (ls) )");
        assert_eq!(toks.len(), 2);
        let toks = tokens("echo $((ls) | cat)");
        let Token::Word(w) = &toks[1] else { panic!() };
        assert!(matches!(&w.parts[0], Part::CmdSub(_)));
    }

    #[test]
    fn case_patterns_inside_substitution() {
        let toks = tokens("echo $(case x in a) rm -rf y;; esac) z");
        assert_eq!(toks.len(), 3);
    }

    #[test]
    fn array_assignment_and_extglob() {
        let toks = tokens("a=( \"$x\" $(ls) ) rm !(keep)");
        let Token::Word(w) = &toks[0] else { panic!() };
        assert!(matches!(&w.parts[1], Part::Array(_)));
        assert_eq!(word_texts("rm !(keep)"), vec!["rm", "!(keep)"]);
    }

    #[test]
    fn make_mode() {
        let script = lex("@rm -rf $(DIR) $$HOME '$$x'", Mode::Make, 0).unwrap();
        let words: Vec<&Word> = script
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w),
                _ => None,
            })
            .collect();
        assert_eq!(crate::words::word_literal(words[0]).as_deref(), Some("rm"));
        assert!(matches!(words[2].parts[0], Part::Fragment));
        assert!(matches!(&words[3].parts[0], Part::Param { name, .. } if name == "HOME"));
        assert_eq!(crate::words::word_literal(words[4]).as_deref(), Some("$x"));
        let text: String = script.source[words[2].span.start..words[2].span.end]
            .iter()
            .collect();
        assert_eq!(text, "$(DIR)");
    }

    #[test]
    fn just_mode() {
        let script = lex("-rm -rf {{dir}} {{{{x", Mode::Just, 0).unwrap();
        let words: Vec<&Word> = script
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w),
                _ => None,
            })
            .collect();
        assert_eq!(crate::words::word_literal(words[0]).as_deref(), Some("rm"));
        assert!(matches!(words[2].parts[0], Part::Fragment));
        assert_eq!(crate::words::word_literal(words[3]).as_deref(), Some("{{x"));
        assert!(lex("echo {{x", Mode::Just, 0).is_err());
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        let inputs = [
            String::new(),
            "   \n\t".into(),
            "a;".repeat(32 * 1024),
            "$(".repeat(200),
            "(".repeat(200),
            format!("{}{}", "(".repeat(200), ")".repeat(200)),
            "$((".repeat(200),
            "`".repeat(201),
            "\\".into(),
            "\u{0}\u{1}\u{1f} \u{FFFD}".into(),
            "${".repeat(200),
            "<(".repeat(200),
            "a=(".repeat(200),
        ];
        for input in inputs {
            let _ = lex(&input, Mode::Shell, 0);
            let _ = lex(&input, Mode::Make, 0);
            let _ = lex(&input, Mode::Just, 0);
        }
    }
}
