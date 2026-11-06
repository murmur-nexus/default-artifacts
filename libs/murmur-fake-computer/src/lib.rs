//! `fake-computer`: a `wasm32-wasip2` component exporting `murmur:tool/run` (world `tool`) that
//! stands in for `tool-computer` in the drivers' `mur_run_computer_tool` suites.
//!
//! Test support only: it has no `murmur.yaml`, nothing publishes it, and the suites pack it with
//! a manifest of their own.
//!
//! Input: `tool-input.data` is the call object, or that object JSON-encoded as a string.
//!
//! It appends the call as one JSON line to `computer-calls.jsonl` in the work folder. Then, for
//! `{"action":"fail"}`, it returns status `error` with summary `fail` and data
//! `tool-computer: fail`; for anything else it returns a passed result shaped like
//! `tool-computer`'s: `<action>[ at X,Y]`, `cursor: X,Y` and `screenshot: shot.png (1440x900)`,
//! attaching `shot.png` from the work folder as `image/png`.
//!
//! Everything outside `wasm_tool` is free of `cfg(target_arch)`, so `cargo test` exercises it on
//! the host.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde_json::Value;

/// The file each call is appended to, relative to the work folder.
pub const CALL_LOG: &str = "computer-calls.jsonl";

/// The screenshot every passed result attaches, relative to the work folder.
pub const SCREENSHOT: &str = "shot.png";

/// What one call returns, before it is mapped onto the WIT `tool-result`.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub passed: bool,
    pub summary: String,
    pub data: String,
    /// `(media_type, path)` of each attachment.
    pub attachments: Vec<(String, String)>,
}

impl Outcome {
    fn error(summary: &str) -> Self {
        Self {
            passed: false,
            summary: summary.to_string(),
            data: format!("tool-computer: {summary}"),
            attachments: Vec::new(),
        }
    }
}

/// The call object `data` names: the object itself, or that object JSON-encoded as a string.
fn parse_call(data: Option<&str>) -> Option<Value> {
    match serde_json::from_str::<Value>(data?).ok()? {
        Value::String(inner) => serde_json::from_str(&inner).ok(),
        call => Some(call),
    }
}

/// A coordinate as the call wrote it: a number's own text, or nothing.
fn coordinate(call: &Value, key: &str) -> Option<String> {
    call.get(key)
        .filter(|value| value.is_number())
        .map(Value::to_string)
}

/// Run one call against the work folder `workdir`.
pub fn run(workdir: &Path, data: Option<&str>) -> Outcome {
    let Some(call) = parse_call(data) else {
        return Outcome::error("the input has no data");
    };

    let logged = OpenOptions::new()
        .create(true)
        .append(true)
        .open(workdir.join(CALL_LOG))
        .and_then(|mut file| file.write_all(format!("{call}\n").as_bytes()));
    if let Err(err) = logged {
        return Outcome::error(&format!("cannot write {CALL_LOG}: {err}"));
    }

    let action = call.get("action").and_then(Value::as_str).unwrap_or("none");
    if action == "fail" {
        return Outcome::error("fail");
    }
    let (action_line, cursor) = match (coordinate(&call, "x"), coordinate(&call, "y")) {
        (Some(x), Some(y)) => (format!("{action} at {x},{y}"), format!("{x},{y}")),
        _ => (action.to_string(), "720,450".to_string()),
    };
    Outcome {
        passed: true,
        data: format!("{action_line}\ncursor: {cursor}\nscreenshot: {SCREENSHOT} (1440x900)"),
        summary: action_line,
        attachments: vec![("image/png".to_string(), SCREENSHOT.to_string())],
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::Path;

    use exports::murmur::tool::run::{Attachment, Guest, Status, ToolInput, ToolResult};

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The work folder is the preopened `.`.
            let outcome = crate::run(Path::new("."), input.data.as_deref());
            ToolResult {
                status: if outcome.passed {
                    Status::Passed
                } else {
                    Status::Error
                },
                summary: Some(outcome.summary),
                data: Some(outcome.data),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: outcome
                    .attachments
                    .into_iter()
                    .map(|(media_type, path)| Attachment { media_type, path })
                    .collect(),
            }
        }
    }

    export!(Component);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn logged(workdir: &Path) -> Vec<Value> {
        std::fs::read_to_string(workdir.join(CALL_LOG))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("murmur_fake_computer_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_click_reports_its_coordinates_and_attaches_the_screenshot() {
        let dir = scratch("click");
        let outcome = run(&dir, Some(r#"{"action":"left_click","x":719,"y":450}"#));
        assert_eq!(
            outcome,
            Outcome {
                passed: true,
                summary: "left_click at 719,450".to_string(),
                data: "left_click at 719,450\ncursor: 719,450\nscreenshot: shot.png (1440x900)"
                    .to_string(),
                attachments: vec![("image/png".to_string(), "shot.png".to_string())],
            }
        );
        assert_eq!(
            logged(&dir),
            vec![json!({"action": "left_click", "x": 719, "y": 450})]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_action_without_coordinates_reports_the_centre() {
        let dir = scratch("type");
        let outcome = run(&dir, Some(r#""{\"action\":\"type\",\"text\":\"hi\"}""#));
        assert!(outcome.passed);
        assert_eq!(outcome.summary, "type");
        assert_eq!(
            outcome.data,
            "type\ncursor: 720,450\nscreenshot: shot.png (1440x900)"
        );
        assert_eq!(logged(&dir), vec![json!({"action": "type", "text": "hi"})]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fail_is_logged_and_returns_an_error() {
        let dir = scratch("fail");
        let outcome = run(&dir, Some(r#"{"action":"fail"}"#));
        assert_eq!(
            outcome,
            Outcome {
                passed: false,
                summary: "fail".to_string(),
                data: "tool-computer: fail".to_string(),
                attachments: Vec::new(),
            }
        );
        assert_eq!(logged(&dir), vec![json!({"action": "fail"})]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_data_is_an_error_and_logs_nothing() {
        let dir = scratch("none");
        let outcome = run(&dir, None);
        assert!(!outcome.passed);
        assert_eq!(outcome.summary, "the input has no data");
        assert!(logged(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
