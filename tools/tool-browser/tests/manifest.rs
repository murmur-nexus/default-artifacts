//! The tool's published interface: `murmur.yaml` declares a WASM tool with its component, the
//! twelve actions, only scalar top-level properties, and what Chrome needs from the host under
//! `programs:`; and the README's working manifest grants only the name, the hosts and the
//! resources. Read by line scan from `CARGO_MANIFEST_DIR`, and `programs:` parsed with
//! `serde_norway` as `mur` parses it, so the anchor and its aliases are resolved.

use std::path::PathBuf;

use serde_norway::Value;
use tool_browser::discovery::BROWSER_NAMES;

mod common;

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
        "version: 0.4.0",
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
fn description_says_calls_run_one_at_a_time() {
    let m = manifest();
    let description: String = m
        .lines()
        .skip_while(|l| *l != "description: |")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(description.contains("one at a time"), "{description}");
    assert!(description.contains("no set order"), "{description}");
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

/// The README's working manifest is the artifact, the `chromium` grant, the hosts the task opens
/// and Chrome's resources: what Chrome needs from the host comes from `programs:`, so the block
/// names no `interpreter_runtime` and no host path.
#[test]
fn readme_manifest_grants_only_the_name_an_https_host_and_resources() {
    let yaml = common::readme_yaml("### A working manifest on `scoped` and `sealed`");
    assert!(!yaml.is_empty(), "no yaml block under the heading");
    for line in [
        "  - name: murmur/tool-browser",
        "    runtime: tool",
        "    memory_bytes: 274877906944",
        "    cgroup_pids_max: 1024",
        "    cgroup_memory_bytes: 4294967296",
    ] {
        assert!(yaml.lines().any(|l| l == line), "`{line}` in {yaml}");
    }
    let allow: Vec<&str> = yaml
        .lines()
        .skip_while(|l| *l != "  shell:")
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .map(str::trim)
        .collect();
    assert_eq!(allow, vec!["allow: [chromium]"], "shell: {yaml}");
    let network: Vec<&str> = yaml
        .lines()
        .skip_while(|l| *l != "  network:")
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .collect();
    assert!(
        network
            .iter()
            .any(|l| l.trim().starts_with("allow: [https://")),
        "network.allow lists an https host: {network:?}"
    );
    for absent in ["interpreter_runtime", "/proc", "/home", "/usr/lib"] {
        assert!(!yaml.contains(absent), "`{absent}` in {yaml}");
    }
    for line in yaml.lines() {
        let value = line.trim().trim_start_matches("- ");
        assert!(!value.starts_with('/'), "a host path: {line}");
    }
}

/// One need: its form and value, the `beside` of a `file`, and its `packages.ubuntu`.
type Need = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
);

/// What Chrome needs from the host, in the order `murmur.yaml` declares it for every name.
const CHROME_NEEDS: [Need; 24] = [
    ("program_folder", ".", None, None),
    ("library", "libsoftokn3.so", None, Some("libnss3")),
    ("file", "libsoftokn3.chk", Some("libsoftokn3.so"), None),
    ("library", "libfreebl3.so", None, Some("libnss3")),
    ("file", "libfreebl3.chk", Some("libfreebl3.so"), None),
    ("library", "libfreeblpriv3.so", None, Some("libnss3")),
    (
        "file",
        "libfreeblpriv3.chk",
        Some("libfreeblpriv3.so"),
        None,
    ),
    ("library", "libnssdbm3.so", None, Some("libnss3")),
    ("file", "libnssdbm3.chk", Some("libnssdbm3.so"), None),
    ("library", "libnssckbi.so", None, Some("libnss3")),
    ("library", "libnss3.so", None, Some("libnss3")),
    ("library", "libnssutil3.so", None, Some("libnss3")),
    ("library", "libnspr4.so", None, Some("libnspr4")),
    ("library", "libatk-1.0.so.0", None, Some("libatk1.0-0t64")),
    (
        "library",
        "libatk-bridge-2.0.so.0",
        None,
        Some("libatk-bridge2.0-0t64"),
    ),
    ("library", "libatspi.so.0", None, Some("libatspi2.0-0t64")),
    (
        "library",
        "libXcomposite.so.1",
        None,
        Some("libxcomposite1"),
    ),
    ("library", "libXdamage.so.1", None, Some("libxdamage1")),
    ("library", "libXfixes.so.3", None, Some("libxfixes3")),
    ("library", "libXrandr.so.2", None, Some("libxrandr2")),
    ("library", "libgbm.so.1", None, Some("libgbm1")),
    ("library", "libasound.so.2", None, Some("libasound2t64")),
    ("system", "fonts", None, Some("fonts-liberation")),
    ("etc", "fonts/fonts.conf", None, Some("fontconfig")),
];

fn str_of(value: &Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected a string, got {value:?}"))
}

/// Every string under `value`, map keys included.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Sequence(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Mapping(map) => map.iter().for_each(|(k, v)| {
            strings(k, out);
            strings(v, out);
        }),
        Value::Tagged(tagged) => strings(&tagged.value, out),
        _ => {}
    }
}

#[test]
fn declares_what_chrome_needs_for_every_browser_name() {
    let doc: Value = serde_norway::from_str(&manifest()).expect("murmur.yaml parses");
    let programs = doc
        .get("programs")
        .and_then(Value::as_mapping)
        .expect("a top-level programs: map");
    let names: Vec<&str> = programs.keys().map(str_of).collect();
    assert_eq!(
        names, BROWSER_NAMES,
        "one program per browser name, in order"
    );

    for (name, program) in programs {
        let name = str_of(name);
        let program = program.as_mapping().expect("a program is a map");
        let keys: Vec<&str> = program.keys().map(str_of).collect();
        assert_eq!(keys, ["needs"], "{name} holds only needs: no download:");
        let needs = program["needs"].as_sequence().expect("needs is a list");
        let got: Vec<(String, String, Option<String>, Option<String>)> = needs
            .iter()
            .map(|need| {
                let need = need.as_mapping().expect("a need is a map");
                let form = need
                    .iter()
                    .map(|(k, v)| (str_of(k), v))
                    .find(|(k, _)| *k != "beside" && *k != "packages")
                    .unwrap_or_else(|| panic!("{name}: a need without a form: {need:?}"));
                for key in need.keys().map(str_of) {
                    assert!(
                        key == form.0 || key == "beside" || key == "packages",
                        "{name}: a need holds one form, beside and packages: {need:?}"
                    );
                }
                let packages = need.get("packages").map(|p| {
                    let p = p.as_mapping().expect("packages is a map");
                    let keys: Vec<&str> = p.keys().map(str_of).collect();
                    assert_eq!(keys, ["ubuntu"], "{name}: packages names Ubuntu's only");
                    str_of(&p["ubuntu"]).to_string()
                });
                (
                    form.0.to_string(),
                    str_of(form.1).to_string(),
                    need.get("beside").map(|b| str_of(b).to_string()),
                    packages,
                )
            })
            .collect();
        let want: Vec<(String, String, Option<String>, Option<String>)> = CHROME_NEEDS
            .iter()
            .map(|(form, value, beside, package)| {
                (
                    form.to_string(),
                    value.to_string(),
                    beside.map(str::to_string),
                    package.map(str::to_string),
                )
            })
            .collect();
        assert_eq!(got, want, "{name}'s needs");
    }

    let mut all = Vec::new();
    strings(&doc["programs"], &mut all);
    for s in &all {
        assert!(
            !s.starts_with('/') && !s.starts_with('~') && !s.contains('$'),
            "no host path, home folder or expansion under programs: {s}"
        );
        assert!(
            !s.contains("/proc") && s != "proc",
            "nothing under /proc: {s}"
        );
    }
}
