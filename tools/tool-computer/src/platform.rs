//! What every OS shares: the stdout envelope and the refusal line for an OS with no display.

use serde_json::{json, Value};

/// The one line a call gets on any OS but Linux.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn not_linux_line() -> &'static str {
    "tool-computer runs only on Linux, in a sealed capsule; on macOS or Windows, run the capsule on a Linux VM"
}

/// The JSON object a call prints on stdout.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub passed: bool,
    pub summary: String,
    pub data: String,
    /// Work-folder-relative PNG paths; present on every passed action.
    pub attachments: Vec<String>,
}

impl Envelope {
    /// A refusal: `summary` and `data` both carry `line`, and nothing is attached.
    pub fn error(line: String) -> Self {
        Self {
            passed: false,
            summary: line.clone(),
            data: line,
            attachments: Vec::new(),
        }
    }

    pub fn to_json(&self) -> String {
        let mut object = json!({
            "status": if self.passed { "passed" } else { "error" },
            "summary": self.summary,
            "data": self.data,
        });
        if self.passed {
            object["attachments"] = Value::Array(
                self.attachments
                    .iter()
                    .map(|path| json!({"media_type": "image/png", "path": path}))
                    .collect(),
            );
        }
        object.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_not_linux_line_names_linux_sealed_and_a_vm() {
        assert_eq!(
            not_linux_line(),
            "tool-computer runs only on Linux, in a sealed capsule; on macOS or Windows, run the \
             capsule on a Linux VM"
        );
    }

    #[test]
    fn an_error_carries_no_attachments_key() {
        let out: Value = serde_json::from_str(&Envelope::error("no".into()).to_json()).unwrap();
        assert_eq!(out["status"], "error");
        assert_eq!(out["summary"], "no");
        assert_eq!(out["data"], "no");
        assert!(out.get("attachments").is_none());
    }
}
