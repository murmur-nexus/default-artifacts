//! Looking through wrapper programs to the command they run.
//!
//! A command such as `sudo env FOO=1 rm -rf x` is judged as three layers: `sudo …`,
//! `env …` and `rm -rf x`. Each wrapper's options are skipped by the table in [`unwrap`];
//! the first remaining word is the inner program. `find -exec` yields one inner command per
//! clause, and a shell interpreter's literal `-c` script is handed back to be lexed and
//! walked.

use crate::lex::{lex, LexError, Mode, Token};
use crate::words::{basename, is_env_assignment, Arg};

/// The shell interpreters, as murmur's `SHELL_INTERPRETERS` names them. `fish` is read as
/// POSIX shell, best-effort.
pub const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "dash", "zsh", "ksh", "fish"];

/// The display text of the operand `xargs` appends from its input.
pub const XARGS_INPUT: &str = "the arguments xargs reads from its input";

/// Why the guard cannot tell what a command will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unclear {
    /// `eval` with arguments.
    Eval,
    /// The program word holds an expansion or substitution.
    Assembled,
    /// A shell interpreter runs a script that is not literal text.
    ShellScriptNotLiteral,
    /// A shell interpreter reads its script from a pipe or redirection.
    ShellReadsInput,
    /// A word list the guard had to lex (`env -S`) could not be read.
    Lex(LexError),
}

/// What a layer does beyond being a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    /// A shell interpreter's literal `-c` script, to be lexed and walked.
    Script(String),
    Unclear(Unclear),
}

/// One layer of a command.
#[derive(Debug, Clone)]
pub struct Layer {
    /// The program's basename; `None` when the program word is assembled at run time.
    pub program: Option<String>,
    /// The words after the program.
    pub args: Vec<Arg>,
    pub effect: Effect,
}

/// Splits a command into its layers, outermost first. `stdin_fed` says whether the command's
/// standard input is a pipe or redirection; `depth` is the nesting level of the script the
/// command came from.
pub fn unwrap(args: Vec<Arg>, stdin_fed: bool, depth: usize) -> Vec<Layer> {
    let mut layers = Vec::new();
    let mut queue: Vec<Vec<Arg>> = vec![args];
    let mut next = 0;
    while let Some(words) = queue.get_mut(next).map(std::mem::take) {
        next += 1;
        let mut words = words;
        while let Some((first, rest)) = words.split_first() {
            let Some(literal) = first.literal.as_deref() else {
                layers.push(Layer {
                    program: None,
                    args: rest.to_vec(),
                    effect: Effect::Unclear(Unclear::Assembled),
                });
                break;
            };
            let program = basename(literal).to_string();
            let mut layer = Layer {
                program: Some(program.clone()),
                args: rest.to_vec(),
                effect: Effect::None,
            };
            let inner: Option<Vec<Arg>> = match program.as_str() {
                "sudo" => sudo(rest).map(|i| tail(rest, i)),
                "env" => match env(rest, depth) {
                    Ok(inner) => inner,
                    Err(unclear) => {
                        layer.effect = Effect::Unclear(unclear);
                        None
                    }
                },
                "nice" => nice(rest).map(|i| tail(rest, i)),
                "nohup" => nohup(rest).map(|i| tail(rest, i)),
                "timeout" => timeout(rest).map(|i| tail(rest, i)),
                "xargs" => Some(xargs(rest)),
                "command" => command(rest).map(|i| tail(rest, i)),
                "exec" => exec(rest).map(|i| tail(rest, i)),
                "eval" => {
                    if !rest.is_empty() {
                        layer.effect = Effect::Unclear(Unclear::Eval);
                    }
                    None
                }
                "find" => {
                    let (own, clauses) = find(rest);
                    layer.args = own;
                    queue.extend(clauses);
                    None
                }
                p if SHELL_INTERPRETERS.contains(&p) => {
                    layer.effect = interpreter(rest, stdin_fed);
                    None
                }
                _ => None,
            };
            layers.push(layer);
            match inner {
                Some(inner) if !inner.is_empty() => words = inner,
                _ => break,
            }
        }
    }
    layers
}

fn tail(rest: &[Arg], from: usize) -> Vec<Arg> {
    rest.get(from..).map(<[Arg]>::to_vec).unwrap_or_default()
}

fn lit(arg: Option<&Arg>) -> Option<&str> {
    arg.and_then(|a| a.literal.as_deref())
}

/// Index of the inner command in `sudo`'s arguments, or `None` when there is none.
fn sudo(rest: &[Arg]) -> Option<usize> {
    const VALUE_SHORT: &str = "ughprtCDTU";
    const VALUE_LONG: &[&str] = &[
        "--user",
        "--group",
        "--host",
        "--prompt",
        "--role",
        "--type",
        "--close-from",
        "--chdir",
        "--command-timeout",
        "--other-user",
    ];
    let mut i = 0;
    while let Some(arg) = rest.get(i) {
        let Some(text) = arg.literal.as_deref() else {
            break;
        };
        if text == "--" {
            i += 1;
            break;
        }
        if let Some(long) = text.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            if name == "edit" {
                return None;
            }
            let takes_value = VALUE_LONG.contains(&format!("--{name}").as_str());
            i += if takes_value && !attached { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = text.strip_prefix('-').filter(|c| !c.is_empty()) {
            let mut skip = 1;
            for (k, c) in cluster.char_indices() {
                if c == 'e' {
                    return None;
                }
                if VALUE_SHORT.contains(c) {
                    if k + c.len_utf8() == cluster.len() {
                        skip = 2;
                    }
                    break;
                }
            }
            i += skip;
            continue;
        }
        if is_env_assignment(text) {
            i += 1;
            continue;
        }
        break;
    }
    (i < rest.len()).then_some(i)
}

/// The inner command of `env`, or the reason it cannot be read.
fn env(rest: &[Arg], depth: usize) -> Result<Option<Vec<Arg>>, Unclear> {
    let mut i = 0;
    while let Some(arg) = rest.get(i) {
        let Some(text) = arg.literal.as_deref() else {
            break;
        };
        if text == "--" {
            i += 1;
            break;
        }
        if text == "-" {
            i += 1;
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            if name == "split-string" {
                return match value {
                    Some(value) => split_string(Some(value), tail(rest, i + 1), depth),
                    None => split_string(lit(rest.get(i + 1)), tail(rest, i + 2), depth),
                };
            }
            let takes_value = matches!(name, "unset" | "chdir");
            i += if takes_value && value.is_none() { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = text.strip_prefix('-').filter(|c| !c.is_empty()) {
            let mut skip = 1;
            for (k, c) in cluster.char_indices() {
                let after = cluster.get(k + c.len_utf8()..).unwrap_or("");
                if c == 'S' {
                    return if after.is_empty() {
                        split_string(lit(rest.get(i + 1)), tail(rest, i + 2), depth)
                    } else {
                        split_string(Some(after), tail(rest, i + 1), depth)
                    };
                }
                if c == 'u' || c == 'C' {
                    if after.is_empty() {
                        skip = 2;
                    }
                    break;
                }
            }
            i += skip;
            continue;
        }
        if is_env_assignment(text) {
            i += 1;
            continue;
        }
        break;
    }
    Ok((i < rest.len()).then(|| tail(rest, i)))
}

/// `env -S VALUE`: VALUE is split into words that start the inner command, followed by the
/// remaining arguments.
fn split_string(
    value: Option<&str>,
    remaining: Vec<Arg>,
    depth: usize,
) -> Result<Option<Vec<Arg>>, Unclear> {
    let Some(value) = value else {
        return Err(Unclear::Assembled);
    };
    let script = lex(value, Mode::Shell, depth + 1).map_err(Unclear::Lex)?;
    let mut words: Vec<Arg> = script
        .tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word(w) => Some(Arg::from_word(
                w,
                script
                    .source
                    .get(w.span.start..w.span.end)
                    .map(|c| c.iter().collect())
                    .unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    words.extend(remaining);
    Ok((!words.is_empty()).then_some(words))
}

fn nice(rest: &[Arg]) -> Option<usize> {
    let mut i = 0;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            break;
        }
        if text == "-n" || text == "--adjustment" {
            i += 2;
        } else if text.starts_with('-') && text.len() > 1 {
            i += 1;
        } else {
            break;
        }
    }
    (i < rest.len()).then_some(i)
}

fn nohup(rest: &[Arg]) -> Option<usize> {
    let mut i = 0;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            break;
        }
        if text.starts_with('-') && text.len() > 1 {
            i += 1;
        } else {
            break;
        }
    }
    (i < rest.len()).then_some(i)
}

fn timeout(rest: &[Arg]) -> Option<usize> {
    let mut i = 0;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            break;
        }
        if matches!(text, "-s" | "--signal" | "-k" | "--kill-after") {
            i += 2;
        } else if text.starts_with('-') && text.len() > 1 {
            i += 1;
        } else {
            break;
        }
    }
    // One DURATION word, then the inner command.
    i += 1;
    (i < rest.len()).then_some(i)
}

/// The command `xargs` runs: its first non-option word onward (or `echo`), plus one opaque
/// operand for what it reads from its input.
fn xargs(rest: &[Arg]) -> Vec<Arg> {
    const VALUE: &str = "ILnPsdEa";
    const OPTIONAL: &str = "ile";
    const VALUE_LONG: &[&str] = &[
        "--arg-file",
        "--delimiter",
        "--max-args",
        "--max-procs",
        "--max-chars",
        "--process-slot-var",
    ];
    let mut i = 0;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            break;
        }
        if text.starts_with("--") {
            let takes_value = !text.contains('=') && VALUE_LONG.contains(&text);
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = text.strip_prefix('-').filter(|c| !c.is_empty()) {
            let mut skip = 1;
            for (k, c) in cluster.char_indices() {
                if OPTIONAL.contains(c) {
                    break;
                }
                if VALUE.contains(c) {
                    if k + c.len_utf8() == cluster.len() {
                        skip = 2;
                    }
                    break;
                }
            }
            i += skip;
            continue;
        }
        break;
    }
    let mut inner = tail(rest, i);
    if inner.is_empty() {
        inner.push(Arg::plain("echo"));
    }
    inner.push(Arg::opaque(XARGS_INPUT));
    inner
}

fn command(rest: &[Arg]) -> Option<usize> {
    let mut i = 0;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            break;
        }
        match text.strip_prefix('-').filter(|c| !c.is_empty()) {
            Some(cluster) => {
                if cluster.contains(['v', 'V']) {
                    return None;
                }
                i += 1;
            }
            None => break,
        }
    }
    (i < rest.len()).then_some(i)
}

fn exec(rest: &[Arg]) -> Option<usize> {
    let mut i = 0;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            break;
        }
        match text.strip_prefix('-').filter(|c| !c.is_empty()) {
            Some(cluster) => {
                let mut skip = 1;
                for (k, c) in cluster.char_indices() {
                    if c == 'a' {
                        if k + c.len_utf8() == cluster.len() {
                            skip = 2;
                        }
                        break;
                    }
                }
                i += skip;
            }
            None => break,
        }
    }
    (i < rest.len()).then_some(i)
}

/// Splits `find`'s arguments into its own (each `-exec` keyword kept, the clause removed)
/// and one command per `-exec`/`-execdir`/`-ok`/`-okdir` clause, with `{}` opaque.
fn find(rest: &[Arg]) -> (Vec<Arg>, Vec<Vec<Arg>>) {
    let mut own = Vec::new();
    let mut clauses = Vec::new();
    let mut words = rest.iter();
    while let Some(arg) = words.next() {
        own.push(arg.clone());
        if !matches!(
            arg.literal.as_deref(),
            Some("-exec" | "-execdir" | "-ok" | "-okdir")
        ) {
            continue;
        }
        let mut clause = Vec::new();
        for word in words.by_ref() {
            match word.literal.as_deref() {
                Some(";" | "+") => break,
                Some("{}") => clause.push(Arg::opaque("{}")),
                _ => clause.push(word.clone()),
            }
        }
        if !clause.is_empty() {
            clauses.push(clause);
        }
    }
    (own, clauses)
}

/// What a shell interpreter runs, from its arguments.
fn interpreter(rest: &[Arg], stdin_fed: bool) -> Effect {
    let mut i = 0;
    let mut has_c = false;
    let mut has_s = false;
    let mut options_ended = false;
    while let Some(text) = lit(rest.get(i)) {
        if text == "--" {
            i += 1;
            options_ended = true;
            break;
        }
        if text == "-" {
            options_ended = true;
            break;
        }
        if text.starts_with("--") {
            i += if matches!(text, "--rcfile" | "--init-file") {
                2
            } else {
                1
            };
            continue;
        }
        let minus = text.starts_with('-');
        if (minus || text.starts_with('+')) && text.len() > 1 {
            let mut takes_value = false;
            for c in text.chars().skip(1) {
                match c {
                    'c' if minus => has_c = true,
                    's' if minus => has_s = true,
                    'o' | 'O' => takes_value = true,
                    _ => {}
                }
            }
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        break;
    }
    let operand = rest.get(i);
    let reads_input = || {
        if stdin_fed {
            Effect::Unclear(Unclear::ShellReadsInput)
        } else {
            Effect::None
        }
    };
    if has_c {
        return match operand {
            None => Effect::None,
            Some(arg) => match &arg.literal {
                Some(script) => Effect::Script(script.clone()),
                None => Effect::Unclear(Unclear::ShellScriptNotLiteral),
            },
        };
    }
    // An expansion where an option may still stand could be `-c` or `-s`: `bash $OPTS 'rm x'`
    // runs its next word as a script, and `… | bash "$O"` may read its input.
    let opaque_option = !options_ended && operand.is_some_and(|a| a.literal.is_none());
    if opaque_option && rest.len() > i + 1 {
        return Effect::Unclear(Unclear::ShellScriptNotLiteral);
    }
    if has_s {
        return reads_input();
    }
    match operand {
        None => reads_input(),
        Some(arg) if arg.literal.as_deref() == Some("-") => reads_input(),
        Some(arg) if arg.procsub => Effect::Unclear(Unclear::ShellScriptNotLiteral),
        Some(_) if opaque_option => reads_input(),
        Some(_) => Effect::None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::walk::walk;

    fn layers(text: &str, fed: bool) -> Vec<Layer> {
        let script = lex(text, Mode::Shell, 0).unwrap();
        let mut cmds = Vec::new();
        walk(&script, false, &mut cmds);
        let cmd = cmds.remove(0);
        unwrap(cmd.args, fed || cmd.stdin_fed, 0)
    }

    /// Each layer as `program arg arg` (`?` for an opaque word).
    fn shape(text: &str) -> Vec<String> {
        layers(text, false)
            .iter()
            .map(|l| {
                let mut out = l.program.clone().unwrap_or_else(|| "?".into());
                for a in &l.args {
                    out.push(' ');
                    out.push_str(a.literal.as_deref().unwrap_or("?"));
                }
                out
            })
            .collect()
    }

    fn last(text: &str) -> String {
        shape(text).pop().unwrap()
    }

    #[test]
    fn sudo_options() {
        assert_eq!(shape("sudo rm -rf x"), vec!["sudo rm -rf x", "rm -rf x"]);
        assert_eq!(last("sudo -u root -E rm x"), "rm x");
        assert_eq!(last("sudo -uroot rm x"), "rm x");
        assert_eq!(last("sudo --user=root FOO=1 -- rm x"), "rm x");
        assert_eq!(last("sudo --user root rm x"), "rm x");
        assert_eq!(shape("sudo -s").len(), 1);
        assert_eq!(shape("sudo -i").len(), 1);
        assert_eq!(shape("sudo -e /etc/x").len(), 1);
        assert_eq!(shape("sudo --edit /etc/x").len(), 1);
    }

    #[test]
    fn env_nice_nohup_timeout() {
        assert_eq!(
            shape("env FOO=1 nice -n 5 nohup timeout 10 rm -rf x"),
            vec![
                "env FOO=1 nice -n 5 nohup timeout 10 rm -rf x",
                "nice -n 5 nohup timeout 10 rm -rf x",
                "nohup timeout 10 rm -rf x",
                "timeout 10 rm -rf x",
                "rm -rf x"
            ]
        );
        assert_eq!(last("env -i -u X -C /tmp rm x"), "rm x");
        assert_eq!(last("env -S 'rm -rf' x"), "rm -rf x");
        assert_eq!(last("env --split-string='rm -rf' x"), "rm -rf x");
        assert_eq!(last("env -S\"rm -rf\" x"), "rm -rf x");
        assert_eq!(last("nice -10 rm x"), "rm x");
        assert_eq!(last("nice --adjustment=3 rm x"), "rm x");
        assert_eq!(last("timeout -s KILL -k 5 --foreground 1m rm x"), "rm x");
        assert_eq!(last("timeout --signal=KILL 5 rm x"), "rm x");
        assert_eq!(
            layers("env -S \"$X\"", false)[0].effect,
            Effect::Unclear(Unclear::Assembled)
        );
    }

    #[test]
    fn xargs_appends_its_input() {
        assert_eq!(last("xargs rm -rf"), "rm -rf ?");
        assert_eq!(last("xargs -0 -n 1 -I {} rm {}"), "rm {} ?");
        assert_eq!(last("xargs -i rm"), "rm ?");
        assert_eq!(last("xargs"), "echo ?");
        assert_eq!(last("xargs --max-args 2 rm"), "rm ?");
    }

    #[test]
    fn command_exec_eval() {
        assert_eq!(last("command -p rm x"), "rm x");
        assert_eq!(shape("command -v rm").len(), 1);
        assert_eq!(last("exec -a name rm x"), "rm x");
        assert_eq!(last("exec -cl rm x"), "rm x");
        assert_eq!(
            layers("eval \"$X\"", false)[0].effect,
            Effect::Unclear(Unclear::Eval)
        );
        assert_eq!(layers("eval", false)[0].effect, Effect::None);
    }

    #[test]
    fn find_exec_clauses() {
        assert_eq!(
            shape("find . -name t -exec rm -rf {} + -execdir ls \\;"),
            vec!["find . -name t -exec -execdir", "rm -rf ?", "ls"]
        );
    }

    #[test]
    fn interpreters() {
        let effect = |t: &str, fed: bool| layers(t, fed).pop().unwrap().effect;
        assert_eq!(
            effect("bash -c 'rm -rf x'", false),
            Effect::Script("rm -rf x".into())
        );
        assert_eq!(
            effect("sh -lc \"sudo rm x\"", false),
            Effect::Script("sudo rm x".into())
        );
        assert_eq!(
            effect("bash -o pipefail -xec 'ls'", false),
            Effect::Script("ls".into())
        );
        assert_eq!(
            effect("bash -c \"$X\"", false),
            Effect::Unclear(Unclear::ShellScriptNotLiteral)
        );
        assert_eq!(
            effect("sh", true),
            Effect::Unclear(Unclear::ShellReadsInput)
        );
        assert_eq!(
            effect("bash -s -- --flag", true),
            Effect::Unclear(Unclear::ShellReadsInput)
        );
        assert_eq!(
            effect("sh -", true),
            Effect::Unclear(Unclear::ShellReadsInput)
        );
        assert_eq!(effect("sh", false), Effect::None);
        assert_eq!(effect("bash script.sh", true), Effect::None);
        assert_eq!(
            effect("bash <(curl x)", false),
            Effect::Unclear(Unclear::ShellScriptNotLiteral)
        );
        assert_eq!(
            effect("sudo sh", true),
            Effect::Unclear(Unclear::ShellReadsInput)
        );
        // An expansion before the script may be `-c`, so the words after it are unreadable.
        assert_eq!(
            effect("bash $OPTS -c 'rm -rf x'", false),
            Effect::Unclear(Unclear::ShellScriptNotLiteral)
        );
        assert_eq!(
            effect("bash -x \"$O\" 'rm -rf x'", false),
            Effect::Unclear(Unclear::ShellScriptNotLiteral)
        );
        // A lone expansion is a script file, unless the shell's input is fed (it may be `-s`).
        assert_eq!(effect("bash \"$f\"", false), Effect::None);
        assert_eq!(
            effect("bash \"$O\"", true),
            Effect::Unclear(Unclear::ShellReadsInput)
        );
        // After `--` an expansion is the script file and the rest are its arguments.
        assert_eq!(effect("bash -- \"$f\" x", false), Effect::None);
    }

    #[test]
    fn assembled_program() {
        let l = layers("$CMD -rf x", false);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].program, None);
        assert_eq!(l[0].effect, Effect::Unclear(Unclear::Assembled));
        let l = layers("sudo \"$X\" x", false);
        assert_eq!(l[1].effect, Effect::Unclear(Unclear::Assembled));
    }
}
