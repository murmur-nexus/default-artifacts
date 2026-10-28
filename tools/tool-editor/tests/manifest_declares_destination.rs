//! The manifest half of the `read_only` contract, and the manifest's copies of what the code
//! decides.
//!
//! What makes a capsule's `capabilities.filesystem.read_only` grant enforceable against this
//! tool is two lines of `murmur.yaml`: `format: murmur-destination` on `write.path` and on
//! `edit.path`. murmur lowers each to an input location, not to a property name, so `read.path`
//! stays a read. The guest code cannot show this and the unit tests cannot reach it — delete
//! either line and every other test in this crate still passes, while the runtime falls back to
//! judging that operation's calls by key name.
//!
//! The schema's property names are the manifest's other copy of something the code decides: the
//! operation keys and the fields each one accepts. These tests hold them to the names the tool
//! lists when it refuses a call, which unit tests in `src/lib.rs` hold to the dispatch table.
//!
//! The manifest is parsed the way murmur parses it: `serde_norway`, then `serde_json::to_value`.
//! That conversion sorts object keys, so the order checks read the YAML mapping itself.

use std::{fs, path::PathBuf};

use serde_json::Value;

/// Tool-input keys naming a file, copied from murmur's `TOOL_PATH_KEYS`
/// (`crates/capsule-runtime/src/protected_paths.rs`).
const TOOL_PATH_KEYS: &[&str] = &["path", "file_path", "filepath", "filename", "file"];

/// Tool-input keys carrying bytes to put somewhere, copied from murmur's `TOOL_CONTENT_KEYS`
/// (`crates/capsule-runtime/src/protected_paths.rs`). One beside a [`TOOL_PATH_KEYS`] key in the
/// same object makes murmur's key-name heuristic read the call as a write.
const TOOL_CONTENT_KEYS: &[&str] = &[
    "content",
    "contents",
    "text",
    "data",
    "body",
    "new_str",
    "new_string",
    "replacement",
    "patch",
    "diff",
];

fn manifest_yaml() -> serde_norway::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("murmur.yaml");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_norway::from_str(&text).unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()))
}

/// `input_schema` as murmur sends it to the model.
fn schema() -> Value {
    let manifest = manifest_yaml();
    let schema = manifest
        .get("input_schema")
        .expect("murmur.yaml declares an input_schema");
    serde_json::to_value(schema).expect("input_schema converts to JSON")
}

/// The property names of the YAML schema node at `path` (a chain of `properties` keys and
/// `items` steps), in the order the manifest writes them.
fn yaml_property_names(path: &[&str]) -> Vec<String> {
    let manifest = manifest_yaml();
    let mut node = manifest
        .get("input_schema")
        .expect("murmur.yaml declares an input_schema");
    for step in path {
        node = match *step {
            "items" => node.get("items"),
            key => node.get("properties").and_then(|p| p.get(key)),
        }
        .unwrap_or_else(|| panic!("no schema node at {path:?}"));
    }
    node.get("properties")
        .and_then(serde_norway::Value::as_mapping)
        .unwrap_or_else(|| panic!("no properties at {path:?}"))
        .keys()
        .map(|key| {
            key.as_str()
                .expect("property names are strings")
                .to_string()
        })
        .collect()
}

/// Every schema node, with its location rendered the way murmur renders an input location:
/// `properties` keys joined with `.`, an `items` step written `[]`.
fn walk<'a>(node: &'a Value, at: String, out: &mut Vec<(String, &'a Value)>) {
    out.push((at.clone(), node));
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            let location = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            walk(child, location, out);
        }
    }
    if let Some(items) = node.get("items") {
        walk(items, format!("{at}[]"), out);
    }
}

fn nodes(schema: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    walk(schema, String::new(), &mut out);
    out
}

#[test]
fn exactly_write_path_and_edit_path_are_declared_destinations() {
    let schema = schema();
    let annotated: Vec<(String, &Value)> = nodes(&schema)
        .into_iter()
        .filter(|(_, node)| {
            node.get("format").and_then(Value::as_str) == Some("murmur-destination")
        })
        .collect();
    let locations: Vec<&str> = annotated.iter().map(|(at, _)| at.as_str()).collect();
    assert_eq!(
        locations,
        ["edit.path", "write.path"],
        "the two write destinations, and nothing else, carry `format: murmur-destination`"
    );
    for (at, node) in annotated {
        assert_eq!(
            node.get("type").and_then(Value::as_str),
            Some("string"),
            "a destination annotation only lowers on a string property: {at}"
        );
    }
}

#[test]
fn nothing_under_read_or_find_carries_a_format() {
    // Annotating `read.path` would make the runtime refuse every read under a `read_only`
    // subtree — the tool made unusable by the grant it exists to respect. `find` reads the same
    // subtrees.
    let schema = schema();
    for (at, node) in nodes(&schema) {
        let under_a_read = ["read", "find"]
            .iter()
            .any(|op| at == *op || at.starts_with(&format!("{op}.")));
        if under_a_read {
            assert!(
                node.get("format").is_none(),
                "{at} is a read input and must carry no format: {node}"
            );
        }
    }
}

#[test]
fn only_write_pairs_a_path_key_with_a_content_key() {
    // murmur's key-name heuristic always runs beside the declared destinations. A schema object
    // outside `write` whose properties pair a path key with a content key would let it read a
    // non-write as a write.
    let schema = schema();
    for (at, node) in nodes(&schema) {
        let Some(properties) = node.get("properties").and_then(Value::as_object) else {
            continue;
        };
        let has = |keys: &[&str]| properties.keys().any(|key| keys.contains(&key.as_str()));
        if at != "write" {
            assert!(
                !(has(TOOL_PATH_KEYS) && has(TOOL_CONTENT_KEYS)),
                "`{}` pairs a path key with a content key: {:?}",
                if at.is_empty() { "<input>" } else { &at },
                properties.keys().collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn the_operations_are_the_keys_the_tool_dispatches_in_order() {
    let out = tool_editor::logic::run(r#"{"data":{}}"#);
    let message = out["message"]
        .as_str()
        .expect("a refusal carries a message");
    let listed = message
        .strip_prefix("a call holds exactly one of ")
        .and_then(|rest| rest.strip_suffix("; got none"))
        .unwrap_or_else(|| panic!("no operation list in {message:?}"));
    let dispatched: Vec<&str> = listed.split(", ").collect();

    assert_eq!(
        yaml_property_names(&[]),
        dispatched,
        "murmur.yaml's top-level properties and OPERATIONS in src/lib.rs must change together, \
         in the same order"
    );
}

/// The fields an `unknown_field` refusal says `data` accepts, probed with the key `zz`.
fn accepted_fields(data: Value) -> Vec<String> {
    let out = tool_editor::logic::run(&serde_json::json!({ "data": data }).to_string());
    assert_eq!(out["error_kind"], "unknown_field", "{out}");
    let message = out["message"]
        .as_str()
        .expect("a refusal carries a message");
    let (_, accepted) = message
        .split_once(" takes ")
        .unwrap_or_else(|| panic!("no accepted fields in {message:?}"));
    accepted.split(", ").map(str::to_string).collect()
}

#[test]
fn each_schema_object_declares_exactly_the_fields_its_refusal_lists() {
    for op in ["read", "write", "edit", "find"] {
        assert_eq!(
            yaml_property_names(&[op]),
            accepted_fields(serde_json::json!({ op: { "zz": 1 } })),
            "`{op}`'s schema properties and its accepted fields in src/lib.rs must match"
        );
    }
    assert_eq!(
        yaml_property_names(&["edit", "edits", "items"]),
        accepted_fields(serde_json::json!({
            "edit": { "path": "f.txt", "edits": [{ "old": "a", "new": "b", "zz": 1 }] }
        })),
        "an `edits` element's schema properties and its accepted fields must match"
    );
}

#[test]
fn the_schema_has_no_top_level_required() {
    // murmur enforces only the top-level `required`, and a call holds one operation key, so no
    // top-level name is required of every call. The tool refuses zero or several itself.
    let schema = schema();
    assert!(
        schema.get("required").is_none(),
        "a top-level `required` would make murmur refuse every call missing one of its names: \
         {schema}"
    );
}
