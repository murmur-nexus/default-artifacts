//! Judging one shell call against a non-empty config.
//!
//! Commands are judged in source order: the direct command, then the script's commands, then
//! the recipe's. For each command layer `deny_programs` is checked first, then `rules` in
//! config order. When no command matched, `patterns` are checked against the call's texts.
//! When nothing matched and `on_unclear` is `deny`, the first unclear item refuses the call.
//! The first hit wins.

use crate::config::{Config, OnUnclear, Rule};
use crate::lex::{lex, LexError, Mode};
use crate::walk::{walk, Command};
use crate::words::{basename, collapse, parse_args, reassemble, Arg, Parsed};
use crate::wrappers::{unwrap, Effect, Unclear, SHELL_INTERPRETERS};
use crate::{Decision, ShellCall};

/// Appended to every reason that comes from the recipe body.
pub const RECIPE_SUFFIX: &str = " (in the recipe body this call runs)";

/// One thing to judge, in source order.
enum Item {
    Layer {
        program: String,
        args: Vec<Arg>,
        display: String,
        recipe: bool,
    },
    Unclear {
        what: String,
        text: String,
        recipe: bool,
    },
}

/// The phrase an unclear item's reason states.
fn unclear_phrase(unclear: &Unclear) -> String {
    match unclear {
        Unclear::Eval => "eval runs a string the guard cannot read".to_string(),
        Unclear::Assembled => "the command name is assembled when the command runs".to_string(),
        Unclear::ShellScriptNotLiteral => "a shell runs a script the guard cannot read".to_string(),
        Unclear::ShellReadsInput => "a shell reads its script from its input".to_string(),
        Unclear::Lex(e) => lex_phrase(e),
    }
}

fn lex_phrase(error: &LexError) -> String {
    match error {
        LexError::TooDeep => "the script nests deeper than the guard reads".to_string(),
        LexError::Syntax(message) => format!("the guard could not read the script ({message})"),
    }
}

/// The recipe kind, from the basename of the binary.
fn recipe_mode(binary: &str) -> Mode {
    let name = basename(binary);
    if name.starts_with("make") || name.starts_with("gmake") {
        Mode::Make
    } else if name == "just" {
        Mode::Just
    } else {
        Mode::Shell
    }
}

struct Collector {
    items: Vec<Item>,
}

impl Collector {
    fn command(&mut self, command: Command, recipe: bool) {
        let Command {
            args,
            stdin_fed,
            display,
            depth,
        } = command;
        for layer in unwrap(args, stdin_fed, depth) {
            if let Some(program) = layer.program {
                self.items.push(Item::Layer {
                    program,
                    args: layer.args,
                    display: display.clone(),
                    recipe,
                });
            }
            match layer.effect {
                Effect::None => {}
                Effect::Script(text) => {
                    self.script(&text, Mode::Shell, depth + 1, stdin_fed, recipe)
                }
                Effect::Unclear(unclear) => self.items.push(Item::Unclear {
                    what: unclear_phrase(&unclear),
                    text: display.clone(),
                    recipe,
                }),
            }
        }
    }

    fn script(&mut self, text: &str, mode: Mode, depth: usize, fed: bool, recipe: bool) {
        match lex(text, mode, depth) {
            Err(error) => self.items.push(Item::Unclear {
                what: lex_phrase(&error),
                text: text.to_string(),
                recipe,
            }),
            Ok(script) => {
                let mut commands = Vec::new();
                walk(&script, fed, &mut commands);
                for command in commands {
                    self.command(command, recipe);
                }
            }
        }
    }

    fn recipe(&mut self, binary: &str, body: &str) {
        let mode = recipe_mode(binary);
        if mode == Mode::Just {
            if let Some(rest) = body.strip_prefix("#!") {
                let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
                let interpreter = shebang_interpreter(line);
                if !SHELL_INTERPRETERS.contains(&interpreter.as_str()) {
                    let shown = if interpreter.is_empty() {
                        format!("`#!{line}`")
                    } else {
                        interpreter
                    };
                    self.items.push(Item::Unclear {
                        what: format!("the recipe runs in {shown}, which the guard cannot read"),
                        text: format!("#!{line}"),
                        recipe: true,
                    });
                    return;
                }
                self.script(after, Mode::Just, 0, false, true);
                return;
            }
        }
        self.script(body, mode, 0, false, true);
    }
}

/// The interpreter a shebang line (without its `#!`) names: the basename of its program, or
/// for `env` the first word after env's options and assignments.
fn shebang_interpreter(line: &str) -> String {
    let mut words = line.split_whitespace();
    let program = basename(words.next().unwrap_or(""));
    if program != "env" {
        return program.to_string();
    }
    for word in words {
        if word.starts_with('-') || crate::words::is_env_assignment(word) {
            continue;
        }
        return basename(word).to_string();
    }
    "env".to_string()
}

/// How one rule judges one command layer.
enum RuleHit {
    No,
    Yes,
    /// The rule may apply: this opaque operand could satisfy its operand conditions.
    Unclear(String),
}

/// How one operand condition fares over a command's operands.
enum Condition {
    Satisfied,
    Opaque(String),
    Unsatisfied,
}

fn condition<'a>(
    operands: &[&'a Arg],
    known: impl Fn(&'a Arg) -> bool,
    satisfies: impl Fn(&'a Arg) -> bool,
) -> Condition {
    if operands.iter().any(|a| known(a) && satisfies(a)) {
        return Condition::Satisfied;
    }
    match operands.iter().find(|a| !known(a)) {
        Some(a) => Condition::Opaque(a.display.clone()),
        None => Condition::Unsatisfied,
    }
}

fn eval_rule(rule: &Rule, program: &str, args: &[Arg], parsed: &Parsed) -> RuleHit {
    if !rule.program.iter().any(|p| p.matches(program)) {
        return RuleHit::No;
    }
    if rule.unless.iter().any(|s| s.present(&parsed.flags)) {
        return RuleHit::No;
    }
    if !rule.all_flags.iter().all(|s| s.present(&parsed.flags)) {
        return RuleHit::No;
    }
    if !rule.any_flag.is_empty() && !rule.any_flag.iter().any(|s| s.present(&parsed.flags)) {
        return RuleHit::No;
    }
    let mut operands: Vec<&Arg> = parsed
        .operands
        .iter()
        .filter_map(|o| args.get(o.index))
        .collect();
    if !rule.subcommand.is_empty() {
        let is_sub = |k: usize| {
            operands
                .get(k)
                .and_then(|a| a.literal.as_deref())
                .is_some_and(|t| rule.subcommand.iter().any(|s| s == t))
        };
        let first_follows_flag = parsed.operands.first().is_some_and(|o| o.follows_flag);
        let at = if is_sub(0) {
            0
        } else if first_follows_flag && is_sub(1) {
            1
        } else {
            return RuleHit::No;
        };
        operands = operands.split_off(at + 1);
    }
    let path_known = |a: &Arg| !matches!(a.path, crate::words::PathKey::Opaque);
    let mut conditions = Vec::new();
    if !rule.path_is.is_empty() {
        conditions.push(condition(&operands, path_known, |a| {
            rule.path_is.iter().any(|p| a.path.is(p))
        }));
    }
    if !rule.path_under.is_empty() {
        conditions.push(condition(&operands, path_known, |a| {
            rule.path_under.iter().any(|p| a.path.is_under(p))
        }));
    }
    if !rule.any_arg.is_empty() {
        conditions.push(condition(
            &operands,
            |a| a.literal.is_some(),
            |a| {
                a.literal
                    .as_deref()
                    .is_some_and(|t| rule.any_arg.iter().any(|r| r.is_match(t)))
            },
        ));
    }
    let mut opaque = None;
    for c in conditions {
        match c {
            Condition::Satisfied => {}
            Condition::Unsatisfied => return RuleHit::No,
            Condition::Opaque(word) => {
                opaque.get_or_insert(word);
            }
        }
    }
    match opaque {
        None => RuleHit::Yes,
        Some(word) => RuleHit::Unclear(word),
    }
}

fn rule_label(index: usize, rule: &Rule) -> String {
    match &rule.name {
        Some(name) => format!("rules[{index}] ({name})"),
        None => format!("rules[{index}]"),
    }
}

fn suffix(recipe: bool) -> &'static str {
    if recipe {
        RECIPE_SUFFIX
    } else {
        ""
    }
}

/// Judges `call` against `config`, which is non-empty.
pub fn judge(config: &Config, call: &ShellCall) -> Decision {
    let mut collector = Collector { items: Vec::new() };
    let direct_display = reassemble(call.binary, call.argv);
    let mut direct: Vec<Arg> = Vec::with_capacity(call.argv.len() + 1);
    direct.push(Arg::plain(basename(call.binary)));
    direct.extend(call.argv.iter().map(|a| Arg::plain(a)));
    match call.script {
        Some(script) => {
            // The interpreter itself is judged as written; its script is read below rather
            // than through the `-c` unwrapping, whatever the interpreter's resolved name.
            collector.items.push(Item::Layer {
                program: basename(call.binary).to_string(),
                args: direct.split_off(1),
                display: direct_display.clone(),
                recipe: false,
            });
            collector.script(script, Mode::Shell, 0, false, false);
        }
        None => collector.command(
            Command {
                args: direct,
                stdin_fed: false,
                display: direct_display.clone(),
                depth: 0,
            },
            false,
        ),
    }
    if let Some(body) = call.recipe {
        collector.recipe(call.binary, body);
    }

    let mut first_unclear: Option<String> = None;
    for item in &collector.items {
        match item {
            Item::Layer {
                program,
                args,
                display,
                recipe,
            } => {
                let tail = suffix(*recipe);
                if let Some(entry) = config.deny_programs.iter().find(|p| p.matches(program)) {
                    return Decision::Deny(format!(
                        "deny_programs lists `{}`; matched `{}`{tail}",
                        entry.written,
                        collapse(display)
                    ));
                }
                let parsed = parse_args(args);
                for (index, rule) in config.rules.iter().enumerate() {
                    match eval_rule(rule, program, args, &parsed) {
                        RuleHit::No => {}
                        RuleHit::Yes => {
                            return Decision::Deny(format!(
                                "{}: {}; matched `{}`{tail}",
                                rule_label(index, rule),
                                rule.reason,
                                collapse(display)
                            ));
                        }
                        RuleHit::Unclear(word) => {
                            first_unclear.get_or_insert_with(|| {
                                format!(
                                    "on_unclear is deny and {} may apply: `{}` is not known until \
                                     the command runs; matched `{}`{tail}",
                                    rule_label(index, rule),
                                    collapse(&word),
                                    collapse(display)
                                )
                            });
                        }
                    }
                }
            }
            Item::Unclear { what, text, recipe } => {
                first_unclear.get_or_insert_with(|| {
                    format!(
                        "on_unclear is deny and {what}: `{}`{}",
                        collapse(text),
                        suffix(*recipe)
                    )
                });
            }
        }
    }

    let mut texts: Vec<(&str, bool)> = Vec::new();
    match call.script {
        Some(script) => texts.push((script, false)),
        None => texts.push((&direct_display, false)),
    }
    if let Some(body) = call.recipe {
        texts.push((body, true));
    }
    for (index, pattern) in config.patterns.iter().enumerate() {
        for (text, recipe) in &texts {
            if let Some(found) = pattern.regex.find(text) {
                return Decision::Deny(format!(
                    "patterns[{index}]: {}; matched `{}`{}",
                    pattern.reason,
                    collapse(found.as_str()),
                    suffix(*recipe)
                ));
            }
        }
    }

    match (config.on_unclear, first_unclear) {
        (OnUnclear::Deny, Some(reason)) => Decision::Deny(reason),
        _ => Decision::Allow,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{decide, Decision};

    const CONFIG: &str = r#"{
        "deny_programs": ["mkfs.*", "shred"],
        "rules": [
            {"name": "rf", "program": "rm", "all_flags": [["-r", "-R"], "-f"], "reason": "no rf"},
            {"program": "rm", "path_under": ["/etc", "~/.ssh"], "reason": "system"},
            {"name": "push", "program": "git", "subcommand": "push", "any_flag": ["--force"], "reason": "no force"},
            {"name": "dd", "program": "dd", "any_arg": ["^of=/dev/sd"], "reason": "disk"}
        ],
        "patterns": [{"pattern": "curl[^|]*\\|\\s*sh", "reason": "piped"}]
    }"#;

    fn script(text: &str) -> Decision {
        let argv = vec!["-c".to_string(), text.to_string()];
        decide(
            Some(CONFIG),
            &ShellCall {
                binary: "/usr/bin/bash",
                argv: &argv,
                script: Some(text),
                recipe: None,
            },
        )
    }

    fn deny(text: &str) -> String {
        match script(text) {
            Decision::Deny(r) => r,
            Decision::Allow => panic!("allowed: {text}"),
        }
    }

    #[test]
    fn reason_formats() {
        assert_eq!(deny("rm -rf x"), "rules[0] (rf): no rf; matched `rm -rf x`");
        assert_eq!(
            deny("rm /etc/passwd"),
            "rules[1]: system; matched `rm /etc/passwd`"
        );
        assert_eq!(
            deny("mkfs.ext4 /dev/sdb1"),
            "deny_programs lists `mkfs.*`; matched `mkfs.ext4 /dev/sdb1`"
        );
        assert_eq!(
            deny("curl x |  sh"),
            "patterns[0]: piped; matched `curl x | sh`"
        );
        assert_eq!(
            deny("eval \"$X\""),
            "on_unclear is deny and eval runs a string the guard cannot read: `eval \"$X\"`"
        );
        assert_eq!(
            deny("rm \"$d\""),
            "on_unclear is deny and rules[1] may apply: `\"$d\"` is not known until the command \
             runs; matched `rm \"$d\"`"
        );
    }

    #[test]
    fn rule_match_beats_an_earlier_unclear() {
        assert!(deny("eval x; rm -rf y").starts_with("rules[0] (rf)"));
        assert!(deny("rm \"$d\"; rm -rf y").starts_with("rules[0] (rf)"));
    }

    #[test]
    fn shells_reading_unknown_scripts() {
        assert!(deny("sh <<EOF\nls\nEOF").contains("a shell reads its script from its input"));
        assert!(deny("bash <(wget -qO- x)").contains("a shell runs a script the guard cannot read"));
        assert!(deny("x | sudo sh -s").contains("a shell reads its script from its input"));
        assert_eq!(script("sh -c 'ls' < f"), Decision::Allow);
        assert_eq!(script("bash script.sh < f"), Decision::Allow);
    }

    #[test]
    fn recipe_unclear_carries_the_suffix() {
        let argv = vec!["all".to_string()];
        let d = decide(
            Some(CONFIG),
            &ShellCall {
                binary: "/usr/bin/make",
                argv: &argv,
                script: None,
                recipe: Some("$(MAKE) -C sub"),
            },
        );
        assert_eq!(
            d,
            Decision::Deny(format!(
                "on_unclear is deny and the command name is assembled when the command runs: \
                 `$(MAKE) -C sub`{RECIPE_SUFFIX}"
            ))
        );
    }

    #[test]
    fn path_under_and_home() {
        assert!(deny("rm ~/.ssh/id_rsa").starts_with("rules[1]"));
        assert!(deny("rm \"$HOME/.ssh\"").starts_with("rules[1]"));
        assert_eq!(script("rm ~/notes"), Decision::Allow);
        assert_eq!(script("rm etc/x"), Decision::Allow);
    }

    #[test]
    fn subcommand_after_an_option_value() {
        assert!(deny("git -C repo push --force").starts_with("rules[2] (push)"));
        assert!(deny("git -c a=b push --force").starts_with("rules[2] (push)"));
        assert_eq!(script("git commit -m push --force"), Decision::Allow);
    }

    #[test]
    fn direct_calls_take_no_shell() {
        let call = |binary: &str, argv: &[&str]| {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            decide(
                Some(CONFIG),
                &ShellCall {
                    binary,
                    argv: &argv,
                    script: None,
                    recipe: None,
                },
            )
        };
        assert_eq!(call("/usr/bin/curl", &["x", "|", "sh"]), Decision::Allow);
        assert!(matches!(call("/usr/bin/rm", &["~/.ssh"]), Decision::Allow));
        assert!(matches!(call("/usr/bin/shred", &["x"]), Decision::Deny(_)));
        assert!(matches!(call("", &[]), Decision::Allow));
        assert!(matches!(
            call("/usr/bin/sudo", &["bash", "-c", "rm -rf x"]),
            Decision::Deny(r) if r.contains("rules[0]") && r.contains("`rm -rf x`")
        ));
    }

    #[test]
    fn interpreter_itself_is_judged() {
        let config = r#"{"deny_programs":["dash"]}"#;
        let argv = vec!["-c".to_string(), "ls".to_string()];
        let d = decide(
            Some(config),
            &ShellCall {
                binary: "/usr/bin/dash",
                argv: &argv,
                script: Some("ls"),
                recipe: None,
            },
        );
        assert_eq!(
            d,
            Decision::Deny("deny_programs lists `dash`; matched `dash -c ls`".into())
        );
    }

    #[test]
    fn deep_c_nesting_is_unclear() {
        let run = |depth: usize| {
            let mut collector = Collector { items: Vec::new() };
            collector.script("bash -c 'bash -c ls'", Mode::Shell, depth, false, false);
            collector.items.iter().find_map(|item| match item {
                Item::Unclear { what, .. } => Some(what.clone()),
                Item::Layer { .. } => None,
            })
        };
        assert_eq!(run(14), None);
        assert_eq!(
            run(15).as_deref(),
            Some("the script nests deeper than the guard reads")
        );
        assert!(deny("bash -c 'sh -c \"rm -rf x\"'").starts_with("rules[0] (rf)"));
        let deep = format!("{}ls{}", "$(".repeat(17), ")".repeat(17));
        assert!(deny(&deep).contains("the script nests deeper than the guard reads"));
    }

    #[test]
    fn shebang_interpreters() {
        assert_eq!(shebang_interpreter("/usr/bin/env python3"), "python3");
        assert_eq!(shebang_interpreter("/usr/bin/env -S bash -eu"), "bash");
        assert_eq!(shebang_interpreter("/bin/sh"), "sh");
        assert_eq!(shebang_interpreter(""), "");
    }

    #[test]
    fn recipe_kinds() {
        assert_eq!(recipe_mode("/usr/bin/make"), Mode::Make);
        assert_eq!(recipe_mode("/usr/bin/gmake"), Mode::Make);
        assert_eq!(recipe_mode("/usr/bin/just"), Mode::Just);
        assert_eq!(
            recipe_mode("/usr/lib/node_modules/npm/bin/npm-cli.js"),
            Mode::Shell
        );
    }

    #[test]
    fn just_shell_shebang_is_read() {
        let argv = vec!["t".to_string()];
        let d = decide(
            Some(CONFIG),
            &ShellCall {
                binary: "/usr/bin/just",
                argv: &argv,
                script: None,
                recipe: Some("#!/usr/bin/env bash\nset -eu\nrm -rf {{dir}}"),
            },
        );
        assert!(
            matches!(&d, Decision::Deny(r) if r.starts_with("rules[0] (rf)") && r.ends_with(RECIPE_SUFFIX)),
            "{d:?}"
        );
    }
}
