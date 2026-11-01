//! One tool call, in the order every call is checked: envelope, input, config, held action,
//! grants, programs on `PATH`; then the display, the action and the screenshot.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{self, held_line};
use crate::display;
use crate::exec;
use crate::grants::{self, Grants};
use crate::input;
use crate::platform::Envelope;
use crate::png;
use crate::state::{StateDir, KEEPER_LOG};
use crate::xbackend::XBackend;

pub fn run(raw: &str) -> Envelope {
    match call(raw) {
        Ok(envelope) => envelope,
        Err(line) => Envelope::error(line),
    }
}

/// The envelope's `data`, an object or a JSON-encoded object.
fn data_of(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Err("tool-computer: no input on stdin".into());
    }
    let envelope: Value =
        serde_json::from_str(raw).map_err(|e| format!("tool-computer: stdin is not JSON: {e}"))?;
    match envelope.get("data") {
        None | Some(Value::Null) => Err("tool-computer: the input has no data".into()),
        Some(Value::String(s)) => serde_json::from_str(s)
            .map_err(|e| format!("tool-computer: data is a string that is not JSON: {e}")),
        Some(other) => Ok(other.clone()),
    }
}

/// The session id: the name of `$MURMUR_RUNTIME_DIR`'s parent folder.
fn session_id(runtime_dir: &Path) -> String {
    runtime_dir
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && n != "." && n != ".." && !n.contains('/'))
        .unwrap_or_else(|| "session".to_string())
}

fn call(raw: &str) -> Result<Envelope, String> {
    let data = data_of(raw)?;
    let (action, command) = input::checked(&data)?;
    let grants = Grants::from_env();
    let config = config::parse(
        std::env::var("MURMUR_ARTIFACT_CONFIG").ok().as_deref(),
        &grants,
    )?;
    if config.confirm.contains(&action) {
        return Err(held_line(action));
    }
    if let Some(line) = grants::missing_display_grants(&grants) {
        return Err(line);
    }
    grants::resolve(&grants, &config.launch, std::env::var_os("PATH").as_ref())?;

    let runtime_dir = std::env::var_os("MURMUR_RUNTIME_DIR")
        .map(PathBuf::from)
        .expect("grants exist only with MURMUR_RUNTIME_DIR");
    let workdir = std::env::current_dir()
        .map_err(|e| format!("tool-computer: cannot read the work folder: {e}"))?;
    let state = StateDir::new(&workdir, &session_id(&runtime_dir));

    let mut ready = display::ensure(&state, &workdir)?;
    let action_line = exec::perform(&mut ready.x, &command)?;
    let frame = exec::capture(&mut ready.x, &command)?;
    let (cx, cy) = ready.x.query_pointer()?;
    let encoded = png::encode_rgb(frame.width, frame.height, &frame.rgb);
    let screen = state
        .write_screen(&encoded)
        .map_err(|e| format!("tool-computer: cannot write the screenshot: {e}"))?;

    let mut lines = ready.preface;
    lines.push(action_line.clone());
    lines.push(format!("cursor: {cx},{cy}"));
    lines.push(format!(
        "screenshot: {screen} ({}x{})",
        frame.width, frame.height
    ));
    for exit in state.take_new_exits() {
        lines.push(match exit.index {
            Some(i) => {
                let log = ready
                    .info
                    .launched
                    .iter()
                    .find(|l| l.index == i)
                    .map(|l| l.log.clone())
                    .unwrap_or_default();
                format!(
                    "launched {} (launch[{i}]) exited with status {}; output in {log}",
                    exit.argv0, exit.status
                )
            }
            None => format!(
                "window manager {} exited with status {}; output in {}",
                exit.argv0,
                exit.status,
                state.rel_path(KEEPER_LOG)
            ),
        });
    }

    Ok(Envelope {
        passed: true,
        summary: action_line,
        data: lines.join("\n"),
        attachments: vec![screen],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_may_be_an_object_or_a_json_string() {
        assert_eq!(
            data_of(r#"{"data":{"action":"screenshot"}}"#).unwrap()["action"],
            "screenshot"
        );
        assert_eq!(
            data_of(r#"{"data":"{\"action\":\"wait\"}","log_path":"x"}"#).unwrap()["action"],
            "wait"
        );
        assert!(data_of("").is_err());
        assert!(data_of("{}").unwrap_err().contains("no data"));
    }

    #[test]
    fn the_session_id_is_the_runtime_dir_parent() {
        assert_eq!(
            session_id(Path::new("/h/.murmur/sessions/ses_abc/runtime")),
            "ses_abc"
        );
        assert_eq!(session_id(Path::new("/")), "session");
    }
}
