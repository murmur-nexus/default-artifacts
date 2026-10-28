//! The manifest half of the `read_only` contract, and each manifest's copies of what the code
//! decides.
//!
//! What makes a capsule's `capabilities.filesystem.read_only` grant enforceable against the file
//! tools is a few lines of their `murmur.yaml`s: `format: murmur-destination` on `tool-write`'s
//! and `tool-edit`'s `path`, and `murmur-destinations: []` on `tool-read` and `tool-find`, which
//! declares that they write nothing. The guest code cannot show this and the unit tests cannot
//! reach it — delete any of those lines and every other test still passes, while the runtime
//! falls back to judging that tool's calls by key name.
//!
//! Each schema's property names are the manifest's other copy of something the code decides:
//! the fields the tool accepts. These tests hold them to [`Tool::fields`] and to the names the
//! tool lists when it refuses a call.
//!
//! Each manifest is parsed the way murmur parses it: `serde_norway`, then `serde_json::to_value`.
//! That conversion sorts object keys, so the order checks read the YAML mapping itself.

use std::{fs, path::PathBuf};

use murmur_file_tools::{run, Tool, EDIT_PAIR_FIELDS};
use serde_json::{json, Value};

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

/// The tool's artifact directory, `tools/<name>`.
fn tool_dir(tool: Tool) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools")
        .join(tool.name())
}

fn manifest_yaml(tool: Tool) -> serde_norway::Value {
    let path = tool_dir(tool).join("murmur.yaml");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_norway::from_str(&text).unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()))
}

/// The whole manifest as murmur converts it.
fn manifest(tool: Tool) -> Value {
    serde_json::to_value(manifest_yaml(tool)).expect("murmur.yaml converts to JSON")
}

/// `input_schema` as murmur sends it to the model.
fn schema(tool: Tool) -> Value {
    let schema = manifest(tool)["input_schema"].clone();
    assert!(
        schema.is_object(),
        "{} declares an input_schema mapping",
        tool.name()
    );
    schema
}

/// The property names of the YAML schema node at `path` (a chain of `properties` keys and
/// `items` steps), in the order the manifest writes them.
fn yaml_property_names(tool: Tool, path: &[&str]) -> Vec<String> {
    let manifest = manifest_yaml(tool);
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

// (a)
#[test]
fn exactly_tool_write_path_and_tool_edit_path_are_declared_destinations() {
    let mut annotated: Vec<String> = Vec::new();
    for tool in Tool::ALL {
        let schema = schema(tool);
        for (at, node) in nodes(&schema) {
            let Some(format) = node.get("format") else {
                continue;
            };
            assert_eq!(
                format,
                "murmur-destination",
                "{} {at}: the only format a file tool carries is murmur-destination",
                tool.name()
            );
            assert_eq!(
                node.get("type").and_then(Value::as_str),
                Some("string"),
                "a destination annotation only lowers on a string property: {} {at}",
                tool.name()
            );
            annotated.push(format!("{} {at}", tool.name()));
        }
    }
    assert_eq!(
        annotated,
        ["tool-write path", "tool-edit path"],
        "the two write destinations, and nothing else, carry `format: murmur-destination`"
    );
}

// (b)
#[test]
fn only_the_reading_tools_declare_that_they_write_nothing() {
    for tool in Tool::ALL {
        let declared = schema(tool).get("murmur-destinations").cloned();
        match tool {
            Tool::Read | Tool::Find => assert_eq!(
                declared,
                Some(json!([])),
                "{} declares `murmur-destinations: []`",
                tool.name()
            ),
            Tool::Write | Tool::Edit => assert_eq!(
                declared,
                None,
                "{} writes, so it declares no empty destination list",
                tool.name()
            ),
        }
    }
}

// (c)
#[test]
fn no_schema_node_but_the_root_and_the_edits_item_is_an_object() {
    for tool in Tool::ALL {
        let schema = schema(tool);
        for (at, node) in nodes(&schema) {
            let object_like =
                node.get("type") == Some(&json!("object")) || node.get("properties").is_some();
            let allowed = at.is_empty() || (tool == Tool::Edit && at == "edits[]");
            assert!(
                !object_like || allowed,
                "{} {at} is an object; the only nested object a file tool takes is \
                 tool-edit's edits item",
                tool.name()
            );
        }
        assert_eq!(schema["type"], "object", "{}", tool.name());
    }
    assert_eq!(schema(Tool::Edit)["properties"]["edits"]["type"], "array");
    assert_eq!(
        schema(Tool::Edit)["properties"]["edits"]["items"]["type"],
        "object"
    );
}

/// The fields an `unknown_field` refusal from `tool` says `data` accepts, probed with the key
/// `zz`.
fn accepted_fields(tool: Tool, data: Value) -> Vec<String> {
    let out = run(tool, &json!({ "data": data }).to_string());
    assert_eq!(out["error_kind"], "unknown_field", "{out}");
    let message = out["message"]
        .as_str()
        .expect("a refusal carries a message");
    let (_, accepted) = message
        .split_once(" takes ")
        .unwrap_or_else(|| panic!("no accepted fields in {message:?}"));
    accepted.split(", ").map(str::to_string).collect()
}

// (d)
#[test]
fn each_schema_declares_exactly_the_fields_its_tool_takes_in_order() {
    for tool in Tool::ALL {
        let properties = yaml_property_names(tool, &[]);
        assert_eq!(
            properties,
            tool.fields(),
            "{}'s murmur.yaml properties and Tool::fields() must change together, in the same \
             order",
            tool.name()
        );
        assert_eq!(
            properties,
            accepted_fields(tool, json!({ "zz": 1 })),
            "{}'s refusal lists its schema properties",
            tool.name()
        );
    }
    let pair = yaml_property_names(Tool::Edit, &["edits", "items"]);
    assert_eq!(pair, EDIT_PAIR_FIELDS);
    assert_eq!(
        pair,
        accepted_fields(
            Tool::Edit,
            json!({ "path": "f.txt", "edits": [{ "old": "a", "new": "b", "zz": 1 }] })
        ),
        "an `edits` element's schema properties and its accepted fields must match"
    );
}

// (e)
#[test]
fn each_schema_requires_exactly_its_required_fields() {
    for (tool, required) in [
        (Tool::Read, json!(["path"])),
        (Tool::Write, json!(["path", "content"])),
        (Tool::Edit, json!(["path", "edits"])),
        (Tool::Find, json!(["pattern"])),
    ] {
        assert_eq!(schema(tool)["required"], required, "{}", tool.name());
    }
    assert_eq!(
        schema(Tool::Edit)["properties"]["edits"]["items"]["required"],
        json!(["old", "new"])
    );
}

// (f)
#[test]
fn each_manifest_names_its_directory_and_its_component() {
    for tool in Tool::ALL {
        let manifest = manifest(tool);
        let dir = tool_dir(tool);
        let dir_name = dir
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(manifest["name"], dir_name.as_str());
        assert_eq!(manifest["name"], tool.name());
        assert_eq!(manifest["runtime"], "tool", "{}", tool.name());
        assert_eq!(manifest["implementation"], "wasm", "{}", tool.name());
        assert_eq!(
            manifest["requires_files"],
            json!([format!("{}.wasm", tool.name().replace('-', "_"))]),
            "{}",
            tool.name()
        );
    }
}

// (g)
#[test]
fn the_reading_tools_pair_no_path_key_with_a_content_key() {
    // murmur's key-name heuristic always runs beside the declared destinations. A path key beside
    // a content key in the same object would let it read tool-read or tool-find as a write.
    for tool in [Tool::Read, Tool::Find] {
        let schema = schema(tool);
        for (at, node) in nodes(&schema) {
            let Some(properties) = node.get("properties").and_then(Value::as_object) else {
                continue;
            };
            let has = |keys: &[&str]| properties.keys().any(|key| keys.contains(&key.as_str()));
            assert!(
                !(has(TOOL_PATH_KEYS) && has(TOOL_CONTENT_KEYS)),
                "{} `{}` pairs a path key with a content key: {:?}",
                tool.name(),
                if at.is_empty() { "<input>" } else { &at },
                properties.keys().collect::<Vec<_>>()
            );
        }
    }
}
