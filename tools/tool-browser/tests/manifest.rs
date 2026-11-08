//! The tool's published interface: `murmur.yaml` declares a WASM tool with its component, the
//! twelve actions, and only scalar top-level properties, read by line scan from
//! `CARGO_MANIFEST_DIR`.

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
fn declares_a_wasm_tool_with_its_component() {
    let m = manifest();
    for line in [
        "name: tool-browser",
        "version: 0.3.0",
        "runtime: tool",
        "implementation: wasm",
    ] {
        assert!(m.lines().any(|l| l == line), "missing `{line}`");
    }
    let files: Vec<&str> = block(&m, "requires_files")
        .into_iter()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(files, vec!["- tool_browser.wasm"]);
    assert!(!m.contains("supervisor") && !m.contains("on this host"));
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

/// The first ```yaml block after `heading` in the README.
fn readme_yaml(heading: &str) -> String {
    let readme =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md"))
            .unwrap();
    readme
        .lines()
        .skip_while(|l| *l != heading)
        .skip_while(|l| *l != "```yaml")
        .skip(1)
        .take_while(|l| *l != "```")
        .collect::<Vec<_>>()
        .join("\n")
}

/// The README's working manifest grants what the measured runs needed, https included: the
/// nine NSS files and the `libsqlite3.so.0` that `libsoftokn3.so` links.
#[test]
fn readme_manifest_grants_the_browser_its_files_and_an_https_host() {
    let yaml = readme_yaml("### A working manifest on `scoped` and `sealed`");
    assert!(!yaml.is_empty(), "no yaml block under the heading");
    let allow: Vec<&str> = yaml
        .lines()
        .skip_while(|l| l.trim() != "allow:")
        .skip(1)
        .take_while(|l| l.trim().starts_with("- "))
        .map(str::trim)
        .collect();
    assert_eq!(allow, vec!["- chromium"], "shell.allow: {yaml}");
    for path in ["/proc", "/etc/fonts", "/usr/share/fonts"] {
        assert!(
            yaml.lines().any(|l| l.trim() == format!("- path: {path}")),
            "path: {path}"
        );
    }
    for file in [
        "libsoftokn3.so",
        "libsoftokn3.chk",
        "libfreebl3.so",
        "libfreebl3.chk",
        "libfreeblpriv3.so",
        "libfreeblpriv3.chk",
        "libnssdbm3.so",
        "libnssdbm3.chk",
        "libnssckbi.so",
        "libsqlite3.so.0",
    ] {
        let line = format!("- {{ path: /usr/lib/x86_64-linux-gnu/{file}, list_dir: false }}");
        assert!(yaml.lines().any(|l| l.trim() == line), "{line}");
    }
    assert!(yaml.contains("    memory_bytes: 274877906944"));
    let network: Vec<&str> = yaml
        .lines()
        .skip_while(|l| *l != "  network:")
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .collect();
    assert!(
        network
            .iter()
            .any(|l| l.trim().starts_with("allow: [\"https://")),
        "network.allow lists an https host: {network:?}"
    );
}
