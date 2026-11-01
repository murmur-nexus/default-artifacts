//! `MURMUR_ARTIFACT_CONFIG`: the operator's `launch` and `confirm` settings.

use serde_json::Value;

use crate::grants::Grants;
use crate::input::Action;

/// The most programs `launch` may hold.
pub const MAX_LAUNCH: usize = 16;

/// The checked config. An unset variable is the default: nothing launched, nothing held.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    /// Each program's argv, started in order once the display is up.
    pub launch: Vec<Vec<String>>,
    /// Action kinds this tool holds and never runs.
    pub confirm: Vec<Action>,
}

fn err(line: String) -> String {
    format!("tool-computer: config {line}")
}

/// Parses and checks `raw` (the variable's value, `None` when unset) against the capsule's grants.
pub fn parse(raw: Option<&str>, grants: &Grants) -> Result<Config, String> {
    let Some(raw) = raw else {
        return Ok(Config::default());
    };
    let value: Value = serde_json::from_str(raw).map_err(|e| {
        err(format!(
            "is not JSON ({e}); it is an object with launch and confirm"
        ))
    })?;
    let Value::Object(map) = value else {
        return Err(err(
            "is not an object; it is an object with launch and confirm".to_string(),
        ));
    };
    for key in map.keys() {
        if key != "launch" && key != "confirm" {
            return Err(err(format!(
                "key '{key}' is not a setting; the settings are launch and confirm"
            )));
        }
    }

    let mut config = Config::default();
    if let Some(launch) = map.get("launch") {
        let Value::Array(programs) = launch else {
            return Err(err(
                "launch is not a list of programs; give it as [[program, argument, …], …]"
                    .to_string(),
            ));
        };
        if programs.len() > MAX_LAUNCH {
            return Err(err(format!(
                "launch holds {} programs; at most {MAX_LAUNCH} are allowed",
                programs.len()
            )));
        }
        for (i, program) in programs.iter().enumerate() {
            let Value::Array(argv) = program else {
                return Err(err(format!(
                    "launch[{i}] is not a list; give the program and its arguments as a list of \
                     strings"
                )));
            };
            if argv.is_empty() {
                return Err(err(format!(
                    "launch[{i}] is empty; give the program and its arguments as a list of strings"
                )));
            }
            let mut strings = Vec::with_capacity(argv.len());
            for (j, arg) in argv.iter().enumerate() {
                match arg {
                    Value::String(s) => strings.push(s.clone()),
                    _ => return Err(err(format!("launch[{i}][{j}] is not a string"))),
                }
            }
            let program = &strings[0];
            if program.is_empty() {
                return Err(err(format!("launch[{i}] names no program")));
            }
            if program.contains('/') {
                return Err(err(format!(
                    "launch[{i}] names a path ({program}); give the program's name as \
                     capabilities.shell.allow lists it"
                )));
            }
            if !grants.granted(program) {
                return Err(err(format!(
                    "launch[{i}] runs {program}, which capabilities.shell.allow does not grant"
                )));
            }
            config.launch.push(strings);
        }
    }

    if let Some(confirm) = map.get("confirm") {
        let Value::Array(names) = confirm else {
            return Err(err("confirm is not a list of action names".to_string()));
        };
        for name in names {
            let Value::String(name) = name else {
                return Err(err(format!("confirm lists {name}, which is not an action")));
            };
            let action = Action::parse(name)
                .ok_or_else(|| err(format!("confirm lists '{name}', which is not an action")))?;
            if !config.confirm.contains(&action) {
                config.confirm.push(action);
            }
        }
    }
    Ok(config)
}

/// The exact line a held action returns.
pub fn held_line(action: Action) -> String {
    let name = action.name();
    format!(
        "tool-computer did not run {name}: this capsule's config lists {name} in confirm, so it \
         needs a person's yes, and murmur gives a native tool no way to ask for one; ask the \
         person to do this step, or to remove {name} from confirm"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(names: &[&str]) -> Grants {
        Grants::from_names(names)
    }

    #[test]
    fn unset_is_the_default() {
        assert_eq!(parse(None, &grants(&[])).unwrap(), Config::default());
    }

    #[test]
    fn launch_and_confirm_parse() {
        let config = parse(
            Some(r#"{"launch":[["xmessage","-print","hi"]],"confirm":["left_click","type"]}"#),
            &grants(&["xmessage"]),
        )
        .unwrap();
        assert_eq!(config.launch, vec![vec!["xmessage", "-print", "hi"]]);
        assert_eq!(config.confirm, vec![Action::LeftClick, Action::Type]);
    }

    #[test]
    fn each_refusal_is_one_line() {
        let g = grants(&["xmessage"]);
        let cases = [
            (
                r#"{"size":1}"#,
                "tool-computer: config key 'size' is not a setting; the settings are launch and confirm",
            ),
            (
                r#"{"launch":[["zenity"]]}"#,
                "tool-computer: config launch[0] runs zenity, which capabilities.shell.allow does not grant",
            ),
            (
                r#"{"launch":[["/usr/bin/xmessage"]]}"#,
                "tool-computer: config launch[0] names a path (/usr/bin/xmessage); give the program's name as capabilities.shell.allow lists it",
            ),
            (
                r#"{"confirm":["explode"]}"#,
                "tool-computer: config confirm lists 'explode', which is not an action",
            ),
            (
                r#"{"launch":"xmessage"}"#,
                "tool-computer: config launch is not a list of programs; give it as [[program, argument, …], …]",
            ),
        ];
        for (raw, line) in cases {
            assert_eq!(parse(Some(raw), &g).unwrap_err(), line, "{raw}");
        }
        assert!(parse(Some("[]"), &g)
            .unwrap_err()
            .contains("is not an object"));
        assert!(parse(Some(r#"{"launch":[[]]}"#), &g)
            .unwrap_err()
            .contains("launch[0] is empty"));
        assert!(parse(Some(r#"{"launch":[["xmessage",1]]}"#), &g)
            .unwrap_err()
            .contains("launch[0][1] is not a string"));
        let many = format!(
            r#"{{"launch":[{}]}}"#,
            vec![r#"["xmessage"]"#; 17].join(",")
        );
        assert!(parse(Some(&many), &g).unwrap_err().contains("at most 16"));
    }

    #[test]
    fn the_held_line_is_exact() {
        assert_eq!(
            held_line(Action::LeftClick),
            "tool-computer did not run left_click: this capsule's config lists left_click in \
             confirm, so it needs a person's yes, and murmur gives a native tool no way to ask for \
             one; ask the person to do this step, or to remove left_click from confirm"
        );
    }
}
