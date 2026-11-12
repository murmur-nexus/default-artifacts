//! Writes what `launch` plans for one request to disk, the way the runtime would, so a recording
//! runs exactly the driver's argv, environment switches and files.
//!
//! ```text
//! cargo run -p driver-grok-build --example write_plan -- \
//!     <files-dir> <plan.json> <server> <url> <token> <tools,comma,separated> \
//!     <new|resume> <session-id> <config-json|-> <model|-> <system-prompt> <task>
//! ```
//!
//! The files go into `<files-dir>`, and `<plan.json>` gets `{"args": [...], "env_set": [[k, v],
//! ...]}` with `{files_dir}` already replaced by `<files-dir>`, as the runtime replaces it.

use driver_grok_build::{launch, Bridge, LaunchRequest, Session, SessionMode};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [files_dir, plan_path, server, url, token, tools, mode, session_id, config, model, system_prompt, task] =
        <[String; 12]>::try_from(args).unwrap_or_else(|args| {
            panic!(
                "expected 12 arguments (see the doc comment), got {}",
                args.len()
            )
        });
    let optional = |value: String| (value != "-").then_some(value);

    let launched = launch(LaunchRequest {
        model: optional(model),
        system_prompt,
        config: optional(config),
        bridge: Some(Bridge {
            server_name: server,
            url,
            bearer_token: token,
            tool_names: tools.split(',').map(str::to_string).collect(),
        }),
        session: Session {
            id: session_id,
            mode: match mode.as_str() {
                "new" => SessionMode::New,
                "resume" => SessionMode::Resume,
                other => panic!("mode must be new or resume, got {other}"),
            },
        },
        harness_version: None,
        task,
    })
    .unwrap_or_else(|refusal| panic!("launch refused the request: {refusal}"));

    let plan = launched.plan;
    std::fs::create_dir_all(&files_dir).expect("the files dir can be created");
    for file in &plan.files {
        std::fs::write(format!("{files_dir}/{}", file.name), &file.contents)
            .expect("a plan file can be written");
    }
    let substitute = |text: &String| text.replace("{files_dir}", &files_dir);
    let written = json!({
        "args": plan.args.iter().map(substitute).collect::<Vec<_>>(),
        "env_set": plan.env_set.iter().map(|(name, value)| [name.clone(), substitute(value)]).collect::<Vec<_>>(),
        "stdin": plan.stdin.is_some(),
        "keep_stdin_open": plan.keep_stdin_open,
        "interrupt_stdin": plan.interrupt_stdin.is_some(),
    });
    std::fs::write(&plan_path, format!("{written:#}\n")).expect("the plan can be written");
}
