//! The crate depends on QuickJS and wit-bindgen and nothing else: the input, the config and the
//! schemas are parsed with QuickJS's own `JSON`.

#[test]
fn dependencies_are_exactly_rquickjs_and_wit_bindgen() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read Cargo.toml");
    let mut section = String::new();
    let mut names = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if section.ends_with("dependencies]") && !section.contains("dev-dependencies") {
            let name = line.split(['=', '.']).next().unwrap_or("").trim();
            names.push(format!("{section} {name}"));
        }
    }
    assert_eq!(
        names,
        ["[dependencies] rquickjs", "[dependencies] wit-bindgen"],
        "tool-repl-js's dependencies"
    );
    assert!(
        manifest.contains(r#"rquickjs = { version = "0.14", default-features = false }"#),
        "rquickjs 0.14 with default features off"
    );
}
