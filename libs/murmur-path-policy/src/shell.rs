//! Recognizing the shell write forms this policy knows, from `binary`, `argv` and
//! `script`.
//!
//! Never from `command`: that field is clipped to 200 characters and is display
//! only, so a policy deciding on it decides on a prefix of the truth.
//!
//! This is a tokenizer, not a shell parser, and no sandbox is built out of string
//! matching. It splits a `-c` script into commands on `;`, `&&`, `||`, `|` and
//! newline, and into words on whitespace respecting single and double quotes,
//! then recognizes a fixed set of write forms by the command's binary basename.
//! Anything it does not recognize is not a write as far as this hook is
//! concerned — see the README for the limits that follows from.

use crate::config::PolicyConfig;

/// One write the shell call was recognized as making.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellTarget {
    /// The recognized form names this path.
    Path {
        /// The write form that matched, e.g. `sed -i`, `> redirect`, `tee`.
        form: String,
        /// The path exactly as the command carried it, un-normalized.
        path: String,
    },
    /// A recognized write form whose targets live in data the hook cannot read —
    /// a patch body. A write the policy cannot see the target of is one it cannot
    /// judge, so the fail-closed rule decides it.
    Unreadable {
        /// The write form that matched.
        form: String,
        /// Why its targets cannot be read, for the refusal reason.
        note: String,
    },
}

/// The last `/`-separated component of a path, or the whole string when it has
/// none.
pub fn basename(path: &str) -> &str {
    match path.rsplit('/').next() {
        Some(base) => base,
        None => path,
    }
}

/// True for an argument that is a flag rather than an operand. `-` alone is a
/// conventional stdin/stdout operand, not a flag.
fn is_flag(arg: &str) -> bool {
    arg.starts_with('-') && arg != "-"
}

// ── tokenizer ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// One word, already unquoted.
    Word(String),
    /// A command separator: `;`, `&&`, `||`, `|`, `&`, a newline, or a subshell
    /// parenthesis.
    Sep,
    /// A redirection operator. The next `Word` is its target; `write` says whether
    /// that target is a file the command writes.
    Redirect { op: String, write: bool },
}

#[derive(Default)]
struct Lexer {
    chars: Vec<char>,
    index: usize,
    buf: String,
    /// Set when the current word came out of a quoted or escaped section, so an
    /// empty quoted word (`tee ''`) survives and a quoted `>` stays a word.
    quoted: bool,
    out: Vec<Tok>,
}

impl Lexer {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.index.saturating_add(offset)).copied()
    }

    fn advance(&mut self, count: usize) {
        self.index = self.index.saturating_add(count);
    }

    fn flush_word(&mut self) {
        if !self.buf.is_empty() || self.quoted {
            self.out.push(Tok::Word(std::mem::take(&mut self.buf)));
        }
        self.quoted = false;
    }

    fn single_quote(&mut self) {
        self.advance(1);
        self.quoted = true;
        // An unterminated quote is tokenized as if it closed at end of input.
        while let Some(c) = self.peek(0) {
            self.advance(1);
            if c == '\'' {
                return;
            }
            self.buf.push(c);
        }
    }

    fn double_quote(&mut self) {
        self.advance(1);
        self.quoted = true;
        while let Some(c) = self.peek(0) {
            self.advance(1);
            if c == '"' {
                return;
            }
            if c == '\\' {
                if let Some(next) = self.peek(0) {
                    if matches!(next, '"' | '\\' | '$' | '`' | '\n') {
                        self.advance(1);
                        if next != '\n' {
                            self.buf.push(next);
                        }
                        continue;
                    }
                }
            }
            self.buf.push(c);
        }
    }

    fn escape(&mut self) {
        self.advance(1);
        if let Some(next) = self.peek(0) {
            self.advance(1);
            if next != '\n' {
                self.buf.push(next);
                self.quoted = true;
            }
        }
    }

    /// `2>&1` and `>&2` name a file descriptor, not a file: consume them whole and
    /// emit nothing, so the following word is not misread as a redirect target.
    fn consume_fd_duplication(&mut self) -> bool {
        if self.peek(0) != Some('&') {
            return false;
        }
        self.advance(1);
        while matches!(self.peek(0), Some(c) if c.is_ascii_digit() || c == '-') {
            self.advance(1);
        }
        true
    }

    fn output_redirect(&mut self) {
        // A digit run immediately before `>` is a file-descriptor prefix, not a word.
        let fd =
            if !self.quoted && !self.buf.is_empty() && self.buf.chars().all(|c| c.is_ascii_digit())
            {
                std::mem::take(&mut self.buf)
            } else {
                self.flush_word();
                String::new()
            };
        self.advance(1);
        let mut op = format!("{fd}>");
        match self.peek(0) {
            Some('>') => {
                op.push('>');
                self.advance(1);
            }
            Some('|') => {
                op.push('|');
                self.advance(1);
            }
            _ => {}
        }
        if self.consume_fd_duplication() {
            return;
        }
        self.out.push(Tok::Redirect { op, write: true });
    }

    fn input_redirect(&mut self) {
        self.flush_word();
        self.advance(1);
        let mut op = String::from("<");
        while self.peek(0) == Some('<') {
            op.push('<');
            self.advance(1);
        }
        if self.consume_fd_duplication() {
            return;
        }
        // A read, and a heredoc delimiter: the word that follows is not written.
        self.out.push(Tok::Redirect { op, write: false });
    }

    fn ampersand(&mut self) {
        if self.peek(1) == Some('>') {
            self.flush_word();
            self.advance(2);
            let mut op = String::from("&>");
            if self.peek(0) == Some('>') {
                op.push('>');
                self.advance(1);
            }
            if self.consume_fd_duplication() {
                return;
            }
            self.out.push(Tok::Redirect { op, write: true });
            return;
        }
        self.flush_word();
        self.out.push(Tok::Sep);
        self.advance(if self.peek(1) == Some('&') { 2 } else { 1 });
    }

    fn run(mut self) -> Vec<Tok> {
        while let Some(c) = self.peek(0) {
            match c {
                '\'' => self.single_quote(),
                '"' => self.double_quote(),
                '\\' => self.escape(),
                ' ' | '\t' | '\r' => {
                    self.flush_word();
                    self.advance(1);
                }
                '\n' | ';' | '(' | ')' => {
                    self.flush_word();
                    self.out.push(Tok::Sep);
                    self.advance(1);
                }
                '&' => self.ampersand(),
                '|' => {
                    self.flush_word();
                    self.out.push(Tok::Sep);
                    self.advance(if self.peek(1) == Some('|') { 2 } else { 1 });
                }
                '<' => self.input_redirect(),
                '>' => self.output_redirect(),
                _ => {
                    self.buf.push(c);
                    self.advance(1);
                }
            }
        }
        self.flush_word();
        self.out
    }
}

fn tokenize(script: &str) -> Vec<Tok> {
    Lexer {
        chars: script.chars().collect(),
        ..Lexer::default()
    }
    .run()
}

/// One command out of a script: its words, and the write redirections attached to
/// it as `(operator, target)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Command {
    words: Vec<String>,
    redirects: Vec<(String, String)>,
}

impl Command {
    fn is_empty(&self) -> bool {
        self.words.is_empty() && self.redirects.is_empty()
    }
}

fn split_commands(tokens: &[Tok]) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::new();
    let mut current = Command::default();
    let mut index = 0usize;

    while let Some(token) = tokens.get(index) {
        index = index.saturating_add(1);
        match token {
            Tok::Sep => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            Tok::Word(word) => current.words.push(word.clone()),
            Tok::Redirect { op, write } => {
                // A redirect at the very end of a script (`echo x >`) names no target.
                if let Some(Tok::Word(target)) = tokens.get(index) {
                    index = index.saturating_add(1);
                    if *write {
                        current.redirects.push((op.clone(), target.clone()));
                    }
                }
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

// ── argument parsing ────────────────────────────────────────────────────────

/// An argument list split into the parts a write form needs.
#[derive(Clone, Debug, Default)]
struct ParsedArgs {
    /// Every non-flag argument, in order, from both sides of a `--`.
    positionals: Vec<String>,
    /// The non-flag arguments that appeared after a `--` separator.
    after_separator: Vec<String>,
    /// Flag arguments, verbatim.
    flags: Vec<String>,
    /// `(flag, value)` for each flag named in `value_flags`, in either the
    /// `-o value` or the `--out=value` form.
    values: Vec<(String, String)>,
    /// Whether a `--` separator appeared.
    saw_separator: bool,
}

impl ParsedArgs {
    fn value_of(&self, names: &[&str]) -> Option<&String> {
        self.values
            .iter()
            .find(|(name, _)| names.contains(&name.as_str()))
            .map(|(_, value)| value)
    }

    fn has_flag(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|f| names.contains(&f.as_str()))
    }
}

fn parse_args(args: &[String], value_flags: &[&str]) -> ParsedArgs {
    let mut out = ParsedArgs::default();
    let mut end_of_flags = false;
    let mut index = 0usize;

    while let Some(arg) = args.get(index) {
        index = index.saturating_add(1);
        if end_of_flags {
            out.positionals.push(arg.clone());
            out.after_separator.push(arg.clone());
            continue;
        }
        if arg == "--" {
            end_of_flags = true;
            out.saw_separator = true;
            continue;
        }
        if !is_flag(arg) {
            out.positionals.push(arg.clone());
            continue;
        }
        out.flags.push(arg.clone());
        if let Some((name, value)) = arg.split_once('=') {
            if value_flags.contains(&name) {
                out.values.push((name.to_string(), value.to_string()));
            }
            continue;
        }
        if value_flags.contains(&arg.as_str()) {
            if let Some(value) = args.get(index) {
                out.values.push((arg.clone(), value.clone()));
                index = index.saturating_add(1);
            }
        }
    }
    out
}

/// Strip leading `VAR=value` assignments and a leading `env`, so
/// `FOO=1 sed -i s/a/b/ tests/x.py` is still recognized as `sed -i`.
fn command_head(words: &[String]) -> &[String] {
    let mut rest = words;
    while let Some(first) = rest.first() {
        let is_assignment = match first.split_once('=') {
            Some((name, _)) => {
                !name.is_empty()
                    && !name.starts_with(|c: char| c.is_ascii_digit())
                    && name.chars().all(|c| c.is_alphanumeric() || c == '_')
            }
            None => false,
        };
        if !is_assignment && basename(first) != "env" {
            return rest;
        }
        rest = rest.get(1..).unwrap_or(&[]);
    }
    rest
}

fn path_target(form: &str, path: &str) -> ShellTarget {
    ShellTarget::Path {
        form: form.to_string(),
        path: path.to_string(),
    }
}

fn join(dir: &str, base: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), base)
}

// ── the write forms ─────────────────────────────────────────────────────────

const SED_VALUE_FLAGS: [&str; 4] = ["-e", "-f", "--expression", "--file"];

fn sed_targets(args: &[String], out: &mut Vec<ShellTarget>) {
    let parsed = parse_args(args, &SED_VALUE_FLAGS);
    let short_flag_contains = |needle: char| {
        parsed.flags.iter().any(|f| match f.strip_prefix('-') {
            Some(short) if !short.starts_with('-') => short.contains(needle),
            _ => false,
        })
    };
    let in_place =
        parsed.flags.iter().any(|f| f.starts_with("--in-place")) || short_flag_contains('i');
    if !in_place {
        return;
    }
    // Without an `-e`/`-f` script, sed takes its script from the first operand,
    // which is not a file it writes.
    let script_given =
        !parsed.values.is_empty() || short_flag_contains('e') || short_flag_contains('f');
    let mut files = parsed.positionals.clone();
    if !script_given && !files.is_empty() {
        files.remove(0);
    }
    for file in files {
        out.push(path_target("sed -i", &file));
    }
}

const PATCH_VALUE_FLAGS: [&str; 18] = [
    "-p",
    "--strip",
    "-i",
    "--input",
    "-o",
    "--output",
    "-d",
    "--directory",
    "-D",
    "--ifdef",
    "-B",
    "--prefix",
    "-r",
    "--reject-file",
    "-z",
    "--suffix",
    "-F",
    "--fuzz",
];

fn patch_targets(args: &[String], out: &mut Vec<ShellTarget>) {
    let parsed = parse_args(args, &PATCH_VALUE_FLAGS);
    let mut targets = parsed.positionals.clone();
    if let Some(output) = parsed.value_of(&["-o", "--output"]) {
        targets.push(output.clone());
    }
    if targets.is_empty() {
        out.push(ShellTarget::Unreadable {
            form: "patch".to_string(),
            note: "it names no file to patch, so the files it writes are the ones named inside \
                   the diff, which this hook cannot read"
                .to_string(),
        });
        return;
    }
    for target in targets {
        out.push(path_target("patch", &target));
    }
}

const COPY_VALUE_FLAGS: [&str; 10] = [
    "-t",
    "--target-directory",
    "-S",
    "--suffix",
    "-m",
    "--mode",
    "-o",
    "--owner",
    "-g",
    "--group",
];

fn copy_targets(form: &str, args: &[String], out: &mut Vec<ShellTarget>) {
    let parsed = parse_args(args, &COPY_VALUE_FLAGS);
    if let Some(dir) = parsed.value_of(&["-t", "--target-directory"]) {
        out.push(path_target(form, dir));
        for source in &parsed.positionals {
            out.push(path_target(form, &join(dir, basename(source))));
        }
        return;
    }
    let Some((destination, sources)) = parsed.positionals.split_last() else {
        return;
    };
    if sources.is_empty() {
        // `cp a` alone writes nothing; the tool itself errors.
        return;
    }
    out.push(path_target(form, destination));
    // A destination written with a trailing `/`, or more than one source, is a
    // directory: each source lands beneath it under its own basename.
    if destination.ends_with('/') || sources.len() > 1 {
        for source in sources {
            out.push(path_target(form, &join(destination, basename(source))));
        }
    }
}

fn plain_operand_targets(
    form: &str,
    args: &[String],
    value_flags: &[&str],
    out: &mut Vec<ShellTarget>,
) {
    for operand in parse_args(args, value_flags).positionals {
        out.push(path_target(form, &operand));
    }
}

fn dd_targets(args: &[String], out: &mut Vec<ShellTarget>) {
    for arg in args {
        if let Some(file) = arg.strip_prefix("of=") {
            out.push(path_target("dd of=", file));
        }
    }
}

/// Flags of `git` itself (before the subcommand) that take a separate value.
const GIT_VALUE_FLAGS: [&str; 6] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
];

fn git_targets(args: &[String], out: &mut Vec<ShellTarget>) {
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        if GIT_VALUE_FLAGS.contains(&arg.as_str()) {
            index = index.saturating_add(2);
            continue;
        }
        if is_flag(arg) {
            index = index.saturating_add(1);
            continue;
        }
        break;
    }
    let Some(subcommand) = args.get(index) else {
        return;
    };
    let rest = args.get(index.saturating_add(1)..).unwrap_or(&[]);

    match subcommand.as_str() {
        "checkout" => {
            let parsed = parse_args(
                rest,
                &["-b", "-B", "--orphan", "--conflict", "--pathspec-from-file"],
            );
            if parsed.saw_separator {
                for path in parsed.after_separator {
                    out.push(path_target("git checkout --", &path));
                }
            }
        }
        "restore" => {
            let parsed = parse_args(rest, &["-s", "--source", "--pathspec-from-file"]);
            for path in parsed.positionals {
                out.push(path_target("git restore", &path));
            }
        }
        "apply" => {
            let parsed = parse_args(
                rest,
                &[
                    "--exclude",
                    "--include",
                    "--directory",
                    "--build-fake-ancestor",
                    "--whitespace",
                    "-p",
                    "-C",
                ],
            );
            if !parsed.has_flag(&["--check", "--stat", "--numstat", "--summary"]) {
                out.push(ShellTarget::Unreadable {
                    form: "git apply".to_string(),
                    note: "the files it writes are named inside the patch, which this hook \
                           cannot read"
                        .to_string(),
                });
            }
        }
        _ => {}
    }
}

fn command_targets(config: &PolicyConfig, command: &Command) -> Vec<ShellTarget> {
    let mut out: Vec<ShellTarget> = command
        .redirects
        .iter()
        .map(|(op, target)| path_target(&format!("{op} redirect"), target))
        .collect();

    let words = command_head(&command.words);
    let Some(program) = words.first() else {
        return out;
    };
    let name = basename(program);
    let args = words.get(1..).unwrap_or(&[]);

    match name {
        "sed" => sed_targets(args, &mut out),
        "tee" => plain_operand_targets("tee", args, &[], &mut out),
        "patch" => patch_targets(args, &mut out),
        "cp" | "mv" | "install" | "ln" => copy_targets(name, args, &mut out),
        "rm" => plain_operand_targets("rm", args, &[], &mut out),
        "truncate" => plain_operand_targets(
            "truncate",
            args,
            &["-s", "--size", "-r", "--reference"],
            &mut out,
        ),
        "dd" => dd_targets(args, &mut out),
        "git" => git_targets(args, &mut out),
        other => {
            if config
                .shell_write_binaries
                .iter()
                .any(|configured| basename(configured) == other)
            {
                plain_operand_targets(other, args, &[], &mut out);
            }
        }
    }
    out
}

/// Every write this shell call was recognized as making.
///
/// `script` is the `-c` body when the interpreter form was used, and `none` for
/// every other form; when it is present it is what the call actually runs, so it
/// is what gets tokenized. Otherwise the call is the single command
/// `binary` + `argv`.
pub fn shell_write_targets(
    config: &PolicyConfig,
    binary: &str,
    argv: &[String],
    script: Option<&str>,
) -> Vec<ShellTarget> {
    match script {
        Some(script) => {
            let mut out = Vec::new();
            for command in split_commands(&tokenize(script)) {
                out.extend(command_targets(config, &command));
            }
            out
        }
        None => {
            let mut words = vec![basename(binary).to_string()];
            words.extend(argv.iter().cloned());
            command_targets(
                config,
                &Command {
                    words,
                    redirects: Vec::new(),
                },
            )
        }
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
    use crate::config::{PolicyConfig, PolicySide};

    fn config() -> PolicyConfig {
        PolicyConfig::defaults(PolicySide::Shell)
    }

    fn with_extra(binaries: &[&str]) -> PolicyConfig {
        let mut config = config();
        config.shell_write_binaries = binaries.iter().map(|b| (*b).to_string()).collect();
        config
    }

    /// Run a `bash -c` script through the recognizer.
    fn script(text: &str) -> Vec<ShellTarget> {
        shell_write_targets(
            &config(),
            "/bin/bash",
            &["-c".to_string(), text.to_string()],
            Some(text),
        )
    }

    /// Run a direct (non-interpreter) invocation through the recognizer.
    fn direct(binary: &str, args: &[&str]) -> Vec<ShellTarget> {
        let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        shell_write_targets(&config(), binary, &argv, None)
    }

    fn paths(targets: &[ShellTarget]) -> Vec<(String, String)> {
        targets
            .iter()
            .filter_map(|t| match t {
                ShellTarget::Path { form, path } => Some((form.clone(), path.clone())),
                ShellTarget::Unreadable { .. } => None,
            })
            .collect()
    }

    fn just_paths(targets: &[ShellTarget]) -> Vec<String> {
        paths(targets).into_iter().map(|(_, path)| path).collect()
    }

    #[test]
    fn sed_in_place_names_its_files_and_plain_sed_names_none() {
        assert_eq!(
            paths(&script("sed -i 's/a/b/' tests/test_x.py")),
            vec![("sed -i".to_string(), "tests/test_x.py".to_string())]
        );
        assert_eq!(
            just_paths(&script("sed 's/a/b/' tests/test_x.py")),
            Vec::<String>::new()
        );
        assert_eq!(
            just_paths(&script("sed --in-place 's/a/b/' a.py b.py")),
            vec!["a.py".to_string(), "b.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("sed -i.bak s/a/b/ a.py")),
            vec!["a.py".to_string()]
        );
        // With `-e`, the first operand is a file rather than the script.
        assert_eq!(
            just_paths(&script("sed -i -e s/a/b/ a.py")),
            vec!["a.py".to_string()]
        );
        assert_eq!(
            just_paths(&direct("/usr/bin/sed", &["-i", "s/a/b/", "a.py"])),
            vec!["a.py"]
        );
    }

    #[test]
    fn redirections_of_every_shape_name_their_target() {
        assert_eq!(
            paths(&script("echo x > tests/a.py")),
            vec![("> redirect".to_string(), "tests/a.py".to_string())]
        );
        assert_eq!(
            paths(&script("echo x >>tests/a.py")),
            vec![(">> redirect".to_string(), "tests/a.py".to_string())]
        );
        assert_eq!(
            paths(&script("echo x 2> tests/a.log")),
            vec![("2> redirect".to_string(), "tests/a.log".to_string())]
        );
        assert_eq!(
            paths(&script("echo x &> tests/a.log")),
            vec![("&> redirect".to_string(), "tests/a.log".to_string())]
        );
        assert_eq!(
            paths(&script("echo x >| tests/a.py")),
            vec![(">| redirect".to_string(), "tests/a.py".to_string())]
        );
        // A read is not a write.
        assert!(script("cat < tests/a.py").is_empty());
        // A quoted `>` is a word, not an operator.
        assert!(script("echo '>' tests/a.py").is_empty());
        // An fd duplication names no file, and does not eat the next word.
        assert!(script("pytest 2>&1").is_empty());
    }

    #[test]
    fn a_script_ending_mid_redirect_yields_no_target() {
        assert!(script("echo x >").is_empty());
        assert!(script("echo x >>").is_empty());
        assert!(script("echo x > ").is_empty());
    }

    #[test]
    fn an_empty_script_and_an_empty_argv_yield_no_targets_and_do_not_panic() {
        assert!(script("").is_empty());
        assert!(script("   \n \n ").is_empty());
        assert!(shell_write_targets(&config(), "/bin/true", &[], None).is_empty());
        assert!(shell_write_targets(&config(), "", &[], None).is_empty());
        assert_eq!(direct("/usr/bin/sed", &["-i"]), Vec::new());
    }

    #[test]
    fn an_unterminated_quote_is_closed_at_end_of_input() {
        assert_eq!(
            just_paths(&script("sed -i 's/a/b/ tests/a.py")),
            Vec::<String>::new()
        );
        assert_eq!(
            just_paths(&script("tee 'tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("tee \"tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
    }

    #[test]
    fn tee_names_its_operands_and_not_its_flags() {
        assert_eq!(
            just_paths(&script("pytest | tee -a tests/out.log")),
            vec!["tests/out.log".to_string()]
        );
        assert!(script("pytest | tee").is_empty());
    }

    #[test]
    fn patch_names_a_file_argument_and_is_unreadable_without_one() {
        assert_eq!(
            just_paths(&script("patch -p1 tests/a.py < d.diff")),
            vec!["tests/a.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("patch -o tests/out.py -i d.diff")),
            vec!["tests/out.py".to_string()]
        );
        match script("patch -p1 < d.diff").first() {
            Some(ShellTarget::Unreadable { form, note }) => {
                assert_eq!(form, "patch");
                assert!(note.contains("inside the diff"), "{note}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn copy_forms_name_their_destination() {
        assert_eq!(
            just_paths(&script("cp a.py tests/b.py")),
            vec!["tests/b.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("mv a.py tests/b.py")),
            vec!["tests/b.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("install -m 0644 a.py tests/b.py")),
            vec!["tests/b.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("ln -s a.py tests/b.py")),
            vec!["tests/b.py".to_string()]
        );
        // A directory destination also names what lands inside it.
        assert_eq!(
            just_paths(&script("cp a.py tests/")),
            vec!["tests/".to_string(), "tests/a.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("cp a.py b.py out")),
            vec![
                "out".to_string(),
                "out/a.py".to_string(),
                "out/b.py".to_string()
            ]
        );
        assert_eq!(
            just_paths(&script("cp -t tests a.py")),
            vec!["tests".to_string(), "tests/a.py".to_string()]
        );
        // One operand writes nothing.
        assert!(script("cp a.py").is_empty());
    }

    #[test]
    fn rm_truncate_and_dd_name_their_targets() {
        assert_eq!(
            just_paths(&script("rm -rf tests/a.py tests/b.py")),
            vec!["tests/a.py".to_string(), "tests/b.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("truncate -s 0 tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("dd if=/dev/null of=tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        assert!(script("dd if=tests/a.py").is_empty());
        assert!(script("rm").is_empty());
    }

    #[test]
    fn the_three_git_write_forms_are_recognized() {
        assert_eq!(
            just_paths(&script("git checkout -- tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        // Without a `--`, `git checkout` is a branch switch this policy does not gate.
        assert!(script("git checkout main").is_empty());
        assert_eq!(
            just_paths(&script("git restore tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("git -C repo restore --source=HEAD -- tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        match script("git apply d.diff").first() {
            Some(ShellTarget::Unreadable { form, note }) => {
                assert_eq!(form, "git apply");
                assert!(note.contains("inside the patch"), "{note}");
            }
            other => panic!("{other:?}"),
        }
        // The inspecting modes of `git apply` write nothing.
        assert!(script("git apply --check d.diff").is_empty());
        assert!(script("git apply --numstat d.diff").is_empty());
        // A git subcommand this policy does not know is not a write form.
        assert!(script("git status").is_empty());
        assert!(script("git").is_empty());
    }

    #[test]
    fn commands_are_split_on_every_separator() {
        let targets = just_paths(&script(
            "pytest && sed -i s/a/b/ one.py; rm two.py || tee three.py\ntruncate -s 0 four.py",
        ));
        assert_eq!(
            targets,
            vec![
                "one.py".to_string(),
                "two.py".to_string(),
                "three.py".to_string(),
                "four.py".to_string()
            ]
        );
    }

    #[test]
    fn a_leading_assignment_or_env_does_not_hide_the_write_form() {
        assert_eq!(
            just_paths(&script("FOO=1 rm tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
        assert_eq!(
            just_paths(&script("env rm tests/a.py")),
            vec!["tests/a.py".to_string()]
        );
    }

    #[test]
    fn a_configured_extra_binary_makes_its_operands_targets() {
        let config = with_extra(&["my-writer"]);
        let targets = shell_write_targets(
            &config,
            "/bin/bash",
            &[],
            Some("my-writer --force tests/a.py"),
        );
        assert_eq!(
            paths(&targets),
            vec![("my-writer".to_string(), "tests/a.py".to_string())]
        );
        // An unconfigured binary is not a write form.
        assert!(script("my-writer --force tests/a.py").is_empty());
    }

    #[test]
    fn replacement_characters_and_c0_controls_in_argv_are_handled_without_panicking() {
        // WIT `list<string>` is UTF-8 by construction, so a literally non-UTF-8
        // argument cannot reach the guest: U+FFFD is what a lossily-converted
        // argument becomes, and it is the only such case there is to test.
        let targets = direct("/bin/rm", &["tests/\u{fffd}\u{1}\u{7f}.py"]);
        assert_eq!(
            just_paths(&targets),
            vec!["tests/\u{fffd}\u{1}\u{7f}.py".to_string()]
        );
        assert!(script("rm \u{fffd}").len() == 1);
        assert!(script("\u{1}\u{7}\u{1b}").is_empty());
    }

    #[test]
    fn basename_handles_every_shape_of_path() {
        assert_eq!(basename("/usr/bin/sed"), "sed");
        assert_eq!(basename("sed"), "sed");
        assert_eq!(basename(""), "");
        assert_eq!(basename("/"), "");
    }
}
