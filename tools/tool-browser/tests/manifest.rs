//! Scenario 13: `murmur.yaml` declares a native tool with its binary, the twelve actions, and
//! only scalar top-level properties, read by line scan from `CARGO_MANIFEST_DIR`.

use std::path::PathBuf;

fn manifest() -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("murmur.yaml")).unwrap()
}

/// The lines of the block under a top-level `key:`.
fn block<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    let mut lines = text.lines().skip_while(|l| *l != format!("{key}:"));
    lines
        .next()
        .unwrap_or_else(|| panic!("no top-level {key}:"));
    lines
        .take_while(|l| l.is_empty() || l.starts_with(' ') || l.starts_with('#'))
        .collect()
}

#[test]
fn declares_a_native_tool_with_its_binary() {
    let m = manifest();
    for line in [
        "name: tool-browser",
        "version: 0.1.0",
        "runtime: tool",
        "implementation: native",
    ] {
        assert!(m.lines().any(|l| l == line), "missing `{line}`");
    }
    let files: Vec<&str> = block(&m, "requires_files")
        .into_iter()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(files, vec!["- bin/tool-browser"]);
}

#[test]
fn input_schema_lists_the_twelve_actions_and_scalar_properties() {
    let m = manifest();
    let schema = block(&m, "input_schema");
    let props_at = schema
        .iter()
        .position(|l| *l == "  properties:")
        .expect("properties");
    // Each property is a line at four spaces; its `type:` the first line at six.
    let mut properties = Vec::new();
    let mut i = props_at + 1;
    while i < schema.len() && (schema[i].starts_with("    ") || schema[i].is_empty()) {
        let line = schema[i];
        if line.starts_with("    ") && !line.starts_with("     ") {
            let name = line.trim().trim_end_matches(':').to_string();
            let ty = schema[i + 1]
                .trim()
                .strip_prefix("type: ")
                .unwrap_or("")
                .to_string();
            properties.push((name, ty));
        }
        i += 1;
    }
    let expected = [
        ("action", "string"),
        ("url", "string"),
        ("ref", "string"),
        ("text", "string"),
        ("key", "string"),
        ("value", "string"),
        ("submit", "boolean"),
        ("full_page", "boolean"),
        ("ms", "integer"),
    ];
    let got: Vec<(&str, &str)> = properties
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    assert_eq!(got, expected);

    let actions: Vec<&str> = schema
        .iter()
        .skip_while(|l| l.trim() != "enum:")
        .skip(1)
        .take_while(|l| l.trim().starts_with("- "))
        .map(|l| l.trim().trim_start_matches("- "))
        .collect();
    assert_eq!(
        actions,
        vec![
            "open",
            "snapshot",
            "click",
            "type",
            "press",
            "select",
            "wait",
            "back",
            "console",
            "network",
            "screenshot",
            "close"
        ]
    );
    let required: Vec<&str> = schema
        .iter()
        .skip_while(|l| l.trim() != "required:")
        .skip(1)
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(required, vec!["- action"]);
}

#[test]
fn documents_the_browser_grant() {
    let m = manifest();
    let caps = block(&m, "capabilities").join("\n");
    for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
        assert!(
            caps.contains(&format!("      - {name}\n"))
                || caps.ends_with(&format!("      - {name}")),
            "grant lists {name}"
        );
    }
    assert!(caps.contains("does not read this block"));
}
