//! Writes the plan `launch` builds for one turn into a directory, so a recording runs `codex`
//! with exactly that plan rather than a hand-copied one.
//!
//! ```text
//! cargo run -q -p driver-codex-cli --example print_plan -- --out <dir> \
//!     [--mode new|resume] [--session <id>] [--model <name>] [--config <json>] \
//!     [--server <name> --url <url> --token <token>] \
//!     --system-prompt-file <file> --task-file <file>
//! ```
//!
//! It writes `<dir>/argv` and `<dir>/env` (NUL-separated argv entries and `NAME=VALUE` pairs),
//! `<dir>/stdin`, and each planned file under `<dir>/files/`. `{files_dir}` is left in argv for
//! the caller to replace, as the runtime does. A refused launch prints its message and exits 2.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use driver_codex_cli::{launch, Bridge, LaunchRequest, Session, SessionMode};

fn main() -> ExitCode {
    let mut flags: HashMap<String, String> = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let Some(value) = args.next() else {
            eprintln!("{flag} needs a value");
            return ExitCode::from(1);
        };
        flags.insert(flag.trim_start_matches("--").to_string(), value);
    }
    let read = |key: &str| -> String {
        let path = flags
            .get(key)
            .unwrap_or_else(|| panic!("--{key} is required"));
        fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"))
    };

    let bridge = flags.get("server").map(|server| Bridge {
        server_name: server.clone(),
        url: flags.get("url").cloned().expect("--url goes with --server"),
        bearer_token: flags
            .get("token")
            .cloned()
            .expect("--token goes with --server"),
        tool_names: Vec::new(),
    });
    let request = LaunchRequest {
        model: flags.get("model").cloned(),
        system_prompt: read("system-prompt-file"),
        config: flags.get("config").cloned(),
        bridge,
        session: Session {
            id: flags.get("session").cloned().unwrap_or_default(),
            mode: match flags.get("mode").map(String::as_str) {
                Some("resume") => SessionMode::Resume,
                _ => SessionMode::New,
            },
        },
        harness_version: None,
        task: read("task-file"),
    };

    let plan = match launch(request) {
        Ok(launched) => launched.plan,
        Err(refusal) => {
            eprintln!("{refusal}");
            return ExitCode::from(2);
        }
    };

    let out = PathBuf::from(flags.get("out").expect("--out is required"));
    fs::create_dir_all(out.join("files")).expect("out dir");
    let nul_joined = |items: Vec<String>| -> Vec<u8> {
        items
            .into_iter()
            .flat_map(|item| item.into_bytes().into_iter().chain([0]))
            .collect()
    };
    fs::write(out.join("argv"), nul_joined(plan.args)).expect("argv");
    fs::write(
        out.join("env"),
        nul_joined(
            plan.env_set
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect(),
        ),
    )
    .expect("env");
    fs::write(out.join("stdin"), plan.stdin.unwrap_or_default()).expect("stdin");
    for file in plan.files {
        fs::write(out.join("files").join(&file.name), file.contents).expect("file");
    }
    ExitCode::SUCCESS
}
