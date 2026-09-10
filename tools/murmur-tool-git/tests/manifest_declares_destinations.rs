//! The manifest half of the `read_only` contract.
//!
//! What makes a capsule's `capabilities.filesystem.read_only` grant enforceable against this
//! tool is two lines of `murmur.yaml`: `format: murmur-destination` on `dest`, and the same on
//! `dest_paths`' *items*. The guest code cannot show them and the unit tests cannot reach them —
//! delete either line and every other test in this crate still passes, while the runtime silently
//! falls back to judging the tool's calls by key name.
//!
//! The fallback is not symmetric, which is why both lines are pinned here. `dest` is a member of
//! the runtime's `TOOL_DESTINATION_KEYS`, so an unlowered declaration still leaves it guessed as a
//! write. `dest_paths` folds to `destpaths` under the runtime's key matching and is in no table, so
//! its annotation is the only thing that makes `restore` judged at all.
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
fn dest_is_declared_a_murmur_destination() {
    let manifest = manifest();
    let body =
        property_body(&manifest, "dest").expect("input_schema must declare a `dest` property");

    assert!(
        body.iter().any(|line| line == "format: murmur-destination"),
        "`dest` must carry `format: murmur-destination` — it is the directory clone, \
         worktree/add, worktree/remove and create_worktree create or delete. Without it the \
         runtime falls back to guessing this tool's write targets from property names. Got: {body:?}"
    );
    assert!(
        body.iter().any(|line| line == "type: string"),
        "a destination annotation only lowers on a string property. Got: {body:?}"
    );
}

#[test]
fn dest_paths_declares_its_destination_on_the_items() {
    let manifest = manifest();
    let body = property_body(&manifest, "dest_paths")
        .expect("input_schema must declare a `dest_paths` property");

    assert_eq!(
        body.first().map(String::as_str),
        Some("type: array"),
        "`dest_paths` is restore's list of working-tree files to overwrite. Got: {body:?}"
    );

    // The annotation belongs on the element schema, not on the array. A destination annotation on
    // a container lowers to a location that resolves to nothing: the runtime collects only string
    // values, so `dest_paths` alone would name an array and yield no candidate to check. On the
    // items it lowers to `dest_paths[]` and resolves every element.
    let items = body
        .iter()
        .position(|line| line == "items:")
        .map(|at| &body[at + 1..])
        .expect("`dest_paths` must declare an `items` schema");
    let element: Vec<&String> = items
        .iter()
        .take_while(|line| !line.starts_with("description:"))
        .collect();

    assert!(
        element.iter().any(|line| *line == "type: string"),
        "`dest_paths` items must be strings. Got: {element:?}"
    );
    assert!(
        element
            .iter()
            .any(|line| *line == "format: murmur-destination"),
        "`dest_paths`' *items* must carry `format: murmur-destination`. On the array property \
         itself the annotation would resolve to nothing, because the runtime collects only string \
         values from a declared location. Got: {element:?}"
    );

    let array_keys: Vec<&String> = body.iter().take_while(|line| *line != "items:").collect();
    assert!(
        !array_keys.iter().any(|line| line.starts_with("format:")),
        "the annotation must sit on `dest_paths`' items, not on the array property. Got: {array_keys:?}"
    );
}

#[test]
fn the_read_inputs_carry_no_destination_annotation() {
    let manifest = manifest();

    // Each of these is named by an operation that writes nothing. Annotating one would make that
    // read refuse under exactly the `read_only` grant this tool exists to respect:
    //
    //   repo   every operation names it, including the pure reads log, diff, show and status.
    //          It is an operating context — what gets written under it depends on the operation,
    //          which an annotation cannot express, since it carries no condition on a sibling.
    //   path   a pathspec filter for diff and log, and status's legacy alias for repo.
    //   paths  add's staging list; the bytes written are the index under .git/, not these paths.
    for property in ["repo", "path", "paths"] {
        let body = property_body(&manifest, property)
            .unwrap_or_else(|| panic!("input_schema must declare a `{property}` property"));
        assert!(
            !body.iter().any(|line| line.starts_with("format:")),
            "`{property}` is used by a read operation and must carry no format annotation — \
             annotating it would make that read refuse under a `read_only` grant. Got: {body:?}"
        );
    }
}

#[test]
fn exactly_two_properties_are_annotated() {
    let manifest = manifest();
    let annotations = manifest
        .lines()
        .filter(|line| line.trim().starts_with("format: murmur-"))
        .count();

    assert_eq!(
        annotations, 2,
        "this tool declares two write destinations, `dest` and `dest_paths[]`; a third annotation \
         would widen what the runtime refuses without a scenario asking for it"
    );
}
