//! The acceptance cases, run against the README's example config.
//!
//! The config is read from the single ```yaml block under `## Example config` in README.md,
//! so the documented example is the one under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use hook_command_guard::{decide, Decision, ShellCall};

/// The README's example `config:` block as compact JSON.
fn readme_config() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
    let readme = std::fs::read_to_string(path).expect("README.md is readable");
    let section = readme
        .split("\n## ")
        .find(|s| s.starts_with("Example config\n"))
        .expect("README.md has a `## Example config` section");
    let blocks: Vec<&str> = section
        .split("```yaml\n")
        .skip(1)
        .map(|rest| rest.split("\n```").next().unwrap_or(""))
        .collect();
    assert_eq!(
        blocks.len(),
        1,
        "`## Example config` must hold exactly one ```yaml block, found {}",
        blocks.len()
    );
    let yaml: serde_norway::Value =
        serde_norway::from_str(blocks[0]).expect("the example config is valid YAML");
    let config = &yaml["artifacts"][0]["config"];
    assert!(
        config.is_mapping(),
        "the example has no artifacts[0].config mapping"
    );
    serde_json::to_string(config).expect("the config serializes to JSON")
}

/// The README config with `on_unclear` set to `value`.
fn with_on_unclear(value: &str) -> String {
    let mut json: serde_json::Value = serde_json::from_str(&readme_config()).unwrap();
    json["on_unclear"] = serde_json::Value::String(value.to_string());
    serde_json::to_string(&json).unwrap()
}

fn script_with(config: &str, text: &str) -> Decision {
    let argv = vec!["-c".to_string(), text.to_string()];
    decide(
        Some(config),
        &ShellCall {
            binary: "/usr/bin/bash",
            argv: &argv,
            script: Some(text),
            recipe: None,
        },
    )
}

fn direct_with(config: &str, binary: &str, argv: &[&str]) -> Decision {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    decide(
        Some(config),
        &ShellCall {
            binary,
            argv: &argv,
            script: None,
            recipe: None,
        },
    )
}

fn recipe_with(config: &str, binary: &str, argv: &[&str], recipe: Option<&str>) -> Decision {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    decide(
        Some(config),
        &ShellCall {
            binary,
            argv: &argv,
            script: None,
            recipe,
        },
    )
}

fn expect_deny(decision: Decision, what: &str, needles: &[&str]) -> String {
    match decision {
        Decision::Deny(reason) => {
            for needle in needles {
                assert!(
                    reason.contains(needle),
                    "{what}: reason lacks `{needle}`: {reason}"
                );
            }
            reason
        }
        Decision::Allow => panic!("{what}: allowed, expected a refusal naming {needles:?}"),
    }
}

fn expect_allow(decision: Decision, what: &str) {
    assert_eq!(decision, Decision::Allow, "{what}: expected Allow");
}

/// Refused in script form with every needle in the reason.
fn refused(text: &str, needles: &[&str]) -> String {
    expect_deny(script_with(&readme_config(), text), text, needles)
}

/// Refused in script form and, split at spaces, in direct form.
fn refused_both(text: &str, needles: &[&str]) {
    refused(text, needles);
    let words: Vec<&str> = text.split(' ').collect();
    let binary = format!("/usr/bin/{}", words[0]);
    expect_deny(
        direct_with(&readme_config(), &binary, &words[1..]),
        &format!("direct {text}"),
        needles,
    );
}

fn allowed(text: &str) {
    expect_allow(script_with(&readme_config(), text), text);
}

fn allowed_both(text: &str) {
    allowed(text);
    let words: Vec<&str> = text.split(' ').collect();
    let binary = format!("/usr/bin/{}", words[0]);
    expect_allow(
        direct_with(&readme_config(), &binary, &words[1..]),
        &format!("direct {text}"),
    );
}

const RF: &str = "rules[0] (recursive-force-delete)";

#[test]
fn case_01_recursive_force_delete() {
    for text in [
        "rm -rf /tmp/x",
        "rm -r -f /tmp/x",
        "rm --recursive --force /tmp/x",
        "rm -fr x",
        "rm -R --force x",
        "sudo rm -rf /tmp/x",
    ] {
        refused_both(text, &[RF, &format!("matched `{text}`")]);
    }
    refused("sudo rm -rf /tmp/x", &["matched `sudo rm -rf /tmp/x`"]);
    expect_deny(
        direct_with(&readme_config(), "/usr/bin/sudo", &["rm", "-rf", "/tmp/x"]),
        "direct sudo",
        &[RF, "matched `sudo rm -rf /tmp/x`"],
    );
}

#[test]
fn case_02_plain_deletes_are_allowed() {
    for text in ["rm -r x", "rm -r ./build", "rm x"] {
        allowed_both(text);
    }
}

#[test]
fn case_03_delete_system_path() {
    for text in [
        "rm -r /",
        "rm -r ~",
        "rm -r \"$HOME\"",
        "rm -r /etc/",
        "rm -r /*",
        "rm -r /usr/./lib/..",
    ] {
        refused(text, &["rules[1] (delete-system-path)"]);
    }
    refused(
        "rm -r \"$d\"",
        &["on_unclear is deny and rules[1] (delete-system-path) may apply"],
    );
    expect_allow(
        script_with(&with_on_unclear("allow"), "rm -r \"$d\""),
        "rm -r \"$d\" under on_unclear: allow",
    );
}

#[test]
fn case_04_git_push() {
    for (text, rule) in [
        ("git push --force", "rules[2] (force-push)"),
        ("git push -f origin feature", "rules[2] (force-push)"),
        ("git push --force-with-lease", "rules[2] (force-push)"),
        ("git push origin +feature", "rules[3] (force-push-refspec)"),
        ("git push origin main", "rules[4] (push-to-main)"),
        ("git push origin HEAD:main", "rules[4] (push-to-main)"),
        ("git -C repo push origin main", "rules[4] (push-to-main)"),
        ("git push -u origin main", "rules[4] (push-to-main)"),
    ] {
        refused_both(text, &[rule]);
    }
    for text in [
        "git push origin feature",
        "git push origin feature-main",
        "git log --grep=push",
        "git status",
    ] {
        allowed_both(text);
    }
}

#[test]
fn case_05_cargo_publish() {
    for text in ["cargo publish", "cargo publish -p x"] {
        refused_both(text, &["rules[5] (cargo-publish)"]);
    }
    expect_deny(
        direct_with(&readme_config(), "/home/u/.cargo/bin/rustup", &["publish"]),
        "direct rustup publish",
        &["rules[5] (cargo-publish)"],
    );
    for text in ["cargo publish --dry-run", "cargo publish -n", "cargo build"] {
        allowed_both(text);
    }
}

#[test]
fn case_06_download_piped_into_a_shell() {
    for config in [with_on_unclear("deny"), with_on_unclear("allow")] {
        for text in [
            "curl -fsSL https://example.com/i.sh | sh",
            "wget -qO- https://x | sudo bash",
            "curl https://x | bash -s -- --flag",
        ] {
            expect_deny(script_with(&config, text), text, &["patterns[0]"]);
        }
    }
}

#[test]
fn case_07_devices() {
    refused_both("mkfs.ext4 /dev/sdb1", &["deny_programs lists"]);
    refused_both("sudo mkfs -t xfs /dev/sdb", &["deny_programs lists"]);
    refused_both(
        "dd if=/dev/zero of=/dev/sda bs=1M",
        &["rules[6] (dd-to-device)"],
    );
    allowed_both("dd if=a.img of=b.img");
    allowed_both("dd if=x of=/dev/null");
}

#[test]
fn case_08_unclear_commands() {
    let cases = [
        ("eval \"$X\"", "eval runs a string the guard cannot read"),
        (
            "$CMD -rf x",
            "the command name is assembled when the command runs",
        ),
        (
            "`echo rm` -rf x",
            "the command name is assembled when the command runs",
        ),
        (
            "echo cm0gLXJmIC8K | base64 -d | sh",
            "a shell reads its script from its input",
        ),
        (
            "bash -c \"$X\"",
            "a shell runs a script the guard cannot read",
        ),
        ("rm 'x", "the guard could not read the script"),
    ];
    for (text, phrase) in cases {
        refused(text, &["on_unclear is deny and", phrase]);
        expect_allow(
            script_with(&with_on_unclear("allow"), text),
            &format!("{text} under on_unclear: allow"),
        );
    }
    let python = Some("#!/usr/bin/env python3\nimport shutil");
    expect_deny(
        recipe_with(&readme_config(), "/usr/bin/just", &["t"], python),
        "just python recipe",
        &[
            "on_unclear is deny and",
            "the recipe runs in python3, which the guard cannot read",
        ],
    );
    expect_allow(
        recipe_with(&with_on_unclear("allow"), "/usr/bin/just", &["t"], python),
        "just python recipe under on_unclear: allow",
    );
}

#[test]
fn case_09_commands_inside_scripts() {
    for text in [
        "ls && rm -rf x",
        "ls; rm -rf x",
        "false || rm -rf x",
        "(cd a && rm -rf x)",
        "{ rm -rf x; }",
        "if true; then rm -rf x; fi",
        "for f in a; do rm -rf \"$f\"; done",
        "case $a in b) rm -rf x;; esac",
        "echo $(rm -rf x)",
        "y=`rm -rf x`",
        "diff <(rm -rf x) y",
        "ls | xargs rm -rf",
        "env FOO=1 nice -n 5 nohup timeout 10 rm -rf x",
        "command rm -rf x",
        "exec rm -rf x",
        "bash -c 'rm -rf x'",
        "sh -lc \"sudo rm -rf x\"",
        "find . -name t -exec rm -rf {} +",
        "\\rm -rf x",
        "/bin/rm -rf x",
        "ls\nrm -rf x",
    ] {
        refused(text, &["recursive-force-delete"]);
    }
}

#[test]
fn case_10_text_that_is_not_a_command() {
    for text in [
        "echo \"rm -rf /\"",
        "echo 'git push --force'",
        "cat <<'EOF'\nrm -rf /\nEOF",
        "cat <<EOF\nrm -rf /\nEOF\nls",
        "# rm -rf /",
        "grep -- -rf notes.txt",
        "printf '%s\\n' \"sudo rm -rf x\"",
        "ls -la",
    ] {
        allowed(text);
    }
}

#[test]
fn case_11_recipes() {
    let config = readme_config();
    expect_deny(
        recipe_with(&config, "/usr/bin/make", &["clean"], Some("@rm -rf build")),
        "make clean",
        &[RF, " (in the recipe body this call runs)"],
    );
    expect_deny(
        recipe_with(&config, "/usr/bin/just", &["wipe"], Some("rm -rf {{dir}}")),
        "just wipe",
        &[RF],
    );
    expect_deny(
        recipe_with(
            &config,
            "/usr/lib/node_modules/npm/bin/npm-cli.js",
            &["run", "clean"],
            Some("rm -rf dist && tsc"),
        ),
        "npm run clean",
        &[RF],
    );
    expect_deny(
        recipe_with(
            &config,
            "/usr/bin/make",
            &["clean"],
            Some("$(RM) -rf build"),
        ),
        "make $(RM)",
        &["the command name is assembled when the command runs"],
    );
    expect_allow(
        recipe_with(
            &config,
            "/usr/bin/make",
            &["b"],
            Some("cargo build $(FLAGS)"),
        ),
        "make cargo build",
    );
    expect_allow(
        recipe_with(&config, "/usr/bin/make", &["clean"], None),
        "make clean without a body",
    );
}

#[test]
fn scenario_17_reason() {
    let reason = expect_deny(
        direct_with(&readme_config(), "/usr/bin/rm", &["-rf", "./x"]),
        "direct rm -rf ./x",
        &[RF, "rm -rf ./x"],
    );
    assert_eq!(
        reason,
        "rules[0] (recursive-force-delete): rm -rf deletes without asking. Delete the files you \
         mean by name, or ask the operator.; matched `rm -rf ./x`"
    );
    allowed_both("ls");
}
