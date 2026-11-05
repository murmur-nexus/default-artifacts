//! The one result every call returns: a status, a summary, the text, and on a screenshot the
//! PNG it attaches. murmur fences the result itself; the tool never writes a fence marker.

/// What a call answers, before the WASM boundary turns it into a `ToolResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub passed: bool,
    pub summary: String,
    pub data: String,
    /// The PNG a screenshot wrote, relative to the work folder.
    pub attachment: Option<String>,
}

/// A passed result. `summary` is `<action>: <first line of data>`.
pub fn passed(action: &str, data: &str, attachment: Option<&str>) -> Reply {
    let first = data.lines().next().unwrap_or("");
    Reply {
        passed: true,
        summary: crate::util::one_line(&format!("{action}: {first}"), 200),
        data: data.to_string(),
        attachment: attachment.map(str::to_string),
    }
}

/// An error result: `summary` and `data` are the same one line.
pub fn error(line: &str) -> Reply {
    let line = crate::util::one_line(line, 4000);
    Reply {
        passed: false,
        summary: line.clone(),
        data: line,
        attachment: None,
    }
}

/// Put the first-result banner above `data`.
pub fn prepend_banner(reply: &mut Reply, banner: &str) {
    reply.data = format!("{banner}\n{}", reply.data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let e = error("tool-browser needs a browser");
        assert_eq!(e.summary, e.data);
        assert!(e.attachment.is_none() && !e.passed);
        let p = passed("screenshot", "saved x.png (1x1, 1 KB)", Some("x.png"));
        assert_eq!(p.attachment.as_deref(), Some("x.png"));
        assert_eq!(p.summary, "screenshot: saved x.png (1x1, 1 KB)");
        let mut b = passed("open", "page: x", None);
        prepend_banner(
            &mut b,
            "browser: chromium 1, local, headless; Chrome sandbox on",
        );
        assert!(b.data.starts_with("browser: chromium 1") && b.data.ends_with("\npage: x"));
        assert_eq!(b.summary, "open: page: x");
    }
}
