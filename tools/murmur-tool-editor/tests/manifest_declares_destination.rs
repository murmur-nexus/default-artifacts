//! The manifest half of the `read_only` contract.
//!
//! What makes a capsule's `capabilities.filesystem.read_only` grant enforceable against this
//! tool is one line of `murmur.yaml`: `format: murmur-destination` on `dest_path`. The guest
//! code cannot show it and the unit tests cannot reach it — delete that line and every other
//! test in this crate still passes, while the runtime silently falls back to judging the tool's
//! calls by key name and `W-SEC-018` returns.
//!
//! The `operation` enum is the manifest's other copy of something the code decides: the names
//! the tool dispatches. Its test holds the enum to the names the tool lists when it rejects an
//! `operation`, which a unit test in `src/lib.rs` holds to the dispatch table.
//!
//! These read the manifest out of the checkout, the same way `mur publish` does.

use std::{fs, path::PathBuf};

fn manifest() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("murmur.yaml");
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

/// The lines nested under `name:` within `input_schema.properties`, without their indentation.
///
/// Returns `None` when the property is absent, which the callers distinguish from a property
/// present but carrying no keys.
fn property_body(manifest: &str, name: &str) -> Option<Vec<String>> {
    let mut lines = manifest
        .lines()
        .skip_while(|line| !line.starts_with("input_schema:"));
    let header = lines
        .by_ref()
        .find(|line| line.trim_end() == format!("    {name}:"))?;
    let indent = header.len() - header.trim_start().len();

    Some(
        lines
            .take_while(|line| {
                let deeper = line.len() - line.trim_start().len() > indent;
                line.trim().is_empty() || deeper
            })
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .collect(),
    )
}

#[test]
fn dest_path_is_declared_a_murmur_destination() {
    let manifest = manifest();
    let body = property_body(&manifest, "dest_path")
        .expect("input_schema must declare a `dest_path` property");

    assert!(
        body.iter().any(|line| line == "format: murmur-destination"),
        "`dest_path` must carry `format: murmur-destination` — without it the runtime guesses \
         this tool's write targets from property names. Got: {body:?}"
    );
    assert!(
        body.iter().any(|line| line == "type: string"),
        "a destination annotation only lowers on a string property. Got: {body:?}"
    );
}

#[test]
fn the_read_inputs_carry_no_destination_annotation() {
    let manifest = manifest();

    // `path` is `read_file`'s input. Annotating it would make the runtime refuse every read
    // under a `read_only` subtree — the tool made unusable by the grant it exists to respect.
    for property in ["path", "dir"] {
        let body = property_body(&manifest, property)
            .unwrap_or_else(|| panic!("input_schema must declare a `{property}` property"));
        assert!(
            !body.iter().any(|line| line.starts_with("format:")),
            "`{property}` is a read input and must carry no format annotation. Got: {body:?}"
        );
    }
}

#[test]
fn exactly_one_property_is_annotated() {
    let manifest = manifest();
    let annotations = manifest
        .lines()
        .filter(|line| line.trim().starts_with("format: murmur-"))
        .count();

    assert_eq!(
        annotations, 1,
        "this tool declares one write destination; a second annotation would widen what the \
         runtime refuses without a scenario asking for it"
    );
}

#[test]
fn the_operation_enum_lists_the_operations_the_tool_dispatches() {
    let manifest = manifest();
    let body = property_body(&manifest, "operation")
        .expect("input_schema must declare an `operation` property");
    let enum_at = body
        .iter()
        .position(|line| line == "enum:")
        .unwrap_or_else(|| panic!("`operation` must declare an `enum:`. Got: {body:?}"));
    let declared: Vec<&str> = body[enum_at + 1..]
        .iter()
        .map_while(|line| line.strip_prefix("- "))
        .collect();
    assert!(
        !declared.is_empty(),
        "`operation`'s enum lists no items. Got: {body:?}"
    );

    let out = murmur_tool_editor::logic::run(r#"{"data":{}}"#);
    let message = out["message"]
        .as_str()
        .expect("a rejection carries a message");
    let (_, listed) = message
        .split_once("expected one of ")
        .unwrap_or_else(|| panic!("no operation list in {message:?}"));
    let dispatched: Vec<&str> = listed.split(", ").collect();

    assert_eq!(
        declared, dispatched,
        "murmur.yaml's `operation` enum and OPERATIONS in src/lib.rs must change together, in \
         the same order.\n  murmur.yaml enum: {declared:?}\n  OPERATIONS:       {dispatched:?}"
    );
}
