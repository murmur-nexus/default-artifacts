//! The one JSON object every call prints: `{status, summary, data}`, plus `attachments` on a
//! screenshot. murmur fences the result itself; the tool never writes a fence marker.

use serde_json::{json, Value};

/// A passed result. `summary` is `<action>: <first line of data>`.
pub fn passed(action: &str, data: &str, attachment: Option<&str>) -> Value {
    let first = data.lines().next().unwrap_or("");
    let mut env = json!({
        "status": "passed",
        "summary": crate::util::one_line(&format!("{action}: {first}"), 200),
        "data": data,
    });
    if let Some(path) = attachment {
        env["attachments"] = json!([{"media_type": "image/png", "path": path}]);
    }
    env
}

/// An error result: `summary` and `data` are the same one line.
pub fn error(line: &str) -> Value {
    let line = crate::util::one_line(line, 4000);
    json!({"status": "error", "summary": line, "data": line})
}

/// Put the first-result banner above `data`.
pub fn prepend_banner(env: &mut Value, banner: &str) {
    let data = env["data"].as_str().unwrap_or("").to_string();
    env["data"] = json!(format!("{banner}\n{data}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let e = error("tool-browser needs a browser");
        assert_eq!(e["summary"], e["data"]);
        assert!(e.get("attachments").is_none());
        let p = passed("screenshot", "saved x.png (1x1, 1 KB)", Some("x.png"));
        assert_eq!(p["attachments"][0]["path"], "x.png");
        assert_eq!(p["summary"], "screenshot: saved x.png (1x1, 1 KB)");
    }
}
