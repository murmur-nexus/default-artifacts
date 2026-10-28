//! Host unit tests for every tool, each call made through [`run`] with a flat input.

use std::fs;

use serde_json::{json, Value};

use crate::edit::occurrences;
use crate::find::{
    cap_note, find_result, find_with_limits, glob_to_regex, normalize_dir, CapReason, Limits,
    FIND_RESULT_SIZE_LIMIT, MAX_CONTEXT_LINES,
};
use crate::read::{MAX_READ_BYTES, MAX_READ_LINES};
use crate::*;

// Fixture root for every test that drives a tool, relative to the process CWD (the crate root,
// under `cargo test`). Every tool refuses an absolute path input, so a fixture under
// `std::env::temp_dir()` is unreachable through the tool — a workdir-relative tree is the only
// shape a host test can address, and it is also the shape a real capsule uses. Gitignored; each
// test removes its own subtree.
const SCRATCH_ROOT: &str = ".test-scratch";

// An empty fixture directory at `.test-scratch/<tag>`, as a relative path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(SCRATCH_ROOT).join(tag);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create scratch dir");
    dir
}

/// `tool` run on the call `data`, through the stdin envelope.
fn call(tool: Tool, data: &Value) -> Value {
    run(tool, &json!({ "data": data }).to_string())
}

/// The fields of `tool`, as its refusal lists them.
fn takes(tool: Tool) -> String {
    format!("{} takes {}", tool.name(), tool.fields().join(", "))
}

/// The workdir's entries, recursively, sorted — the "did the call leave anything behind" check.
fn tree(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn tool_names_fields_and_effects() {
    let table: Vec<(&str, &[&str], &str)> = Tool::ALL
        .iter()
        .map(|tool| (tool.name(), tool.fields(), tool.state_effect()))
        .collect();
    assert_eq!(
        table,
        [
            ("tool-read", &["path", "start_line", "end_line"][..], "read"),
            ("tool-write", &["path", "content"][..], "mutate"),
            ("tool-edit", &["path", "edits"][..], "mutate"),
            (
                "tool-find",
                &["pattern", "dir", "glob", "recursive", "context_lines"][..],
                "read"
            ),
        ]
    );
    assert_eq!(EDIT_PAIR_FIELDS, ["old", "new"]);
}

// ── the envelope ────────────────────────────────────────────────────────────────

#[test]
fn run_returns_error_for_empty_input() {
    for tool in Tool::ALL {
        let out = run(tool, "");
        assert_eq!(out["ok"], false);
        assert_eq!(out["message"], "missing input on stdin");
    }
}

#[test]
fn the_envelope_failures_keep_their_messages() {
    for tool in Tool::ALL {
        for (raw, message) in [
            ("   ", "missing input on stdin".to_string()),
            ("{}", "missing data field".to_string()),
            (r#"{"data":null}"#, "missing data field".to_string()),
            (
                r#"{"data":3}"#,
                "data must be a JSON string or object".to_string(),
            ),
        ] {
            let out = run(tool, raw);
            assert_eq!(out["ok"], false, "{raw}");
            assert_eq!(out["message"], message, "{raw}");
            assert!(out.get("error_kind").is_none(), "{raw}: {out:?}");
        }
        let out = run(tool, "{");
        assert!(
            out["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid stdin JSON: "),
            "{out:?}"
        );
        let out = run(tool, r#"{"data":"{"}"#);
        assert!(
            out["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid data JSON string: "),
            "{out:?}"
        );
    }
}

#[test]
fn data_that_decodes_to_a_non_object_is_refused() {
    for tool in Tool::ALL {
        for data in [json!("[1,2]"), json!("\"read\""), json!(3), json!([1])] {
            let out = run(tool, &json!({ "data": data }).to_string());
            assert_eq!(out["ok"], false, "{data}");
            assert_eq!(
                out["message"], "data must be a JSON string or object",
                "{data}"
            );
        }
    }
}

#[test]
fn run_handles_double_encoded_data() {
    let inner = r#"{"bogus_double_enc":{}}"#;
    let envelope = format!(
        r#"{{"data":"{}","log_path":null}}"#,
        inner.replace('"', "\\\"")
    );
    let out = run(Tool::Read, &envelope);
    assert_eq!(out["ok"], false);
    assert_eq!(
        out["message"],
        format!("unknown field \"bogus_double_enc\": {}", takes(Tool::Read))
    );

    let dir = scratch("double_encoded_read");
    let path = write_numbered_file(&dir, "f.txt", 2);
    let inner = json!({ "path": &path }).to_string();
    let out = run(Tool::Read, &json!({ "data": inner }).to_string());
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "line 1\nline 2\n");
    let _ = fs::remove_dir_all(&dir);
}

// ── fields a tool does not take ─────────────────────────────────────────────────

#[test]
fn run_refuses_unknown_top_level_keys_sorted_and_quoted() {
    let out = call(
        Tool::Read,
        &json!({ "operation": "read_file", "dest_path": "a.txt" }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::UNKNOWN_FIELD);
    assert_eq!(
        out["message"],
        "unknown field \"dest_path\", \"operation\": tool-read takes path, start_line, end_line"
    );
    assert_eq!(out["summary"], out["message"]);
    assert!(out["metadata"].is_null(), "{out:?}");

    // Refused whatever its value, null included, and ahead of every field the tool takes.
    let out = call(Tool::Read, &json!({ "path": "a.txt", "no_cache": null }));
    assert_eq!(out["error_kind"], err::UNKNOWN_FIELD, "{out:?}");
    assert_eq!(
        out["message"],
        "unknown field \"no_cache\": tool-read takes path, start_line, end_line"
    );
}

#[test]
fn an_unknown_key_is_quoted_with_escapes_on_one_line() {
    let out = call(Tool::Write, &json!({ "write\"file": 1, "write\nfile": 2 }));
    let message = out["message"].as_str().unwrap();
    assert!(!message.contains('\n'), "{message:?}");
    assert_eq!(
        message,
        r#"unknown field "write\nfile", "write\"file": tool-write takes path, content"#
    );
}

#[test]
fn each_tool_refuses_a_field_it_does_not_take() {
    // S4. Each shape reaches the tool — murmur checks only that the required fields are present —
    // and is refused naming the sorted, quoted keys and the fields that tool takes.
    let nested_0_12_0 = [
        json!({ "read": { "path": "a.txt" } }),
        json!({ "write": { "path": "a.txt", "content": "x" } }),
        json!({ "edit": { "path": "a.txt", "edits": [{ "old": "a", "new": "b" }] } }),
        json!({ "find": { "pattern": "a" } }),
    ];
    let mut cases: Vec<(Tool, Value, &str)> = vec![
        (
            Tool::Read,
            json!({ "read": "path</arg_key><arg_value>xjq.py" }),
            "\"read\"",
        ),
        (
            Tool::Write,
            json!({ "path": "a.txt", "write": { "path": "a.txt" }, "content": "x" }),
            "\"write\"",
        ),
        (
            Tool::Edit,
            json!({ "path": "a", "old_string": "x", "new_string": "y" }),
            "\"new_string\", \"old_string\"",
        ),
    ];
    for tool in Tool::ALL {
        for (shape, key) in
            nested_0_12_0
                .iter()
                .zip(["\"read\"", "\"write\"", "\"edit\"", "\"find\""])
        {
            cases.push((tool, shape.clone(), key));
        }
        cases.push((tool, json!({ "zz": 1, "aa": null }), "\"aa\", \"zz\""));
    }
    for (tool, keys) in [
        (Tool::Read, "\"content\", \"dest_path\", \"operation\""),
        (Tool::Write, "\"dest_path\", \"operation\""),
        (Tool::Edit, "\"content\", \"dest_path\", \"operation\""),
        (Tool::Find, "\"content\", \"dest_path\", \"operation\""),
    ] {
        cases.push((
            tool,
            json!({ "operation": "write_file", "dest_path": "a.txt", "content": "x" }),
            keys,
        ));
    }

    for (tool, data, keys) in cases {
        let out = call(tool, &data);
        assert_eq!(out["ok"], false, "{} {data}", tool.name());
        assert_eq!(out["status"], "error", "{} {data}", tool.name());
        assert_eq!(
            out["error_kind"],
            err::UNKNOWN_FIELD,
            "{} {data}",
            tool.name()
        );
        assert_eq!(
            out["message"],
            format!("unknown field {keys}: {}", takes(tool)),
            "{} {data}",
            tool.name()
        );
        assert!(out["metadata"].is_null(), "{} {data}", tool.name());
    }
}

#[test]
fn glm_shapes_are_refused_and_nothing_is_touched() {
    // The two shapes GLM 5.3 sent tool-editor 0.12.0, plus the earlier shapes, with every path
    // aimed into a fixture: each is refused before the filesystem is reached.
    let dir = scratch("glm_shapes");
    let file = dir.join("a.txt");
    fs::write(&file, "hello\n").unwrap();
    let fresh = dir.join("fresh.txt");
    let file_s = file.to_str().unwrap();
    let fresh_s = fresh.to_str().unwrap();
    let before = tree(&dir);

    for (tool, data) in [
        (
            Tool::Read,
            json!({ "read": format!("{file_s}</arg_key><arg_value>xjq.py") }),
        ),
        (
            Tool::Write,
            json!({ "path": fresh_s, "write": { "path": fresh_s }, "content": "x" }),
        ),
        (
            Tool::Write,
            json!({ "write": { "path": fresh_s, "content": "x" } }),
        ),
        (
            Tool::Edit,
            json!({ "edit": { "path": file_s, "edits": [{ "old": "hello", "new": "bye" }] } }),
        ),
        (
            Tool::Edit,
            json!({ "path": file_s, "old_string": "hello", "new_string": "bye" }),
        ),
        (
            Tool::Write,
            json!({ "operation": "write_file", "dest_path": fresh_s, "content": "x" }),
        ),
        (
            Tool::Edit,
            json!({ "operation": "replace_in_file", "dest_path": file_s, "old_string": "hello", "new_string": "bye" }),
        ),
        (
            Tool::Find,
            json!({ "find": { "pattern": "hello", "dir": dir.to_str().unwrap() } }),
        ),
    ] {
        let out = call(tool, &data);
        assert_eq!(out["ok"], false, "{data}");
        assert_eq!(out["error_kind"], err::UNKNOWN_FIELD, "{data}");
        assert!(
            out["content"].is_null() && out["matches"].is_null(),
            "{out:?}"
        );
        assert!(out["metadata"].is_null(), "{data}");
    }
    assert_eq!(fs::read(&file).unwrap(), b"hello\n");
    assert!(
        !fresh.exists(),
        "a refused call must not reach the filesystem"
    );
    assert_eq!(tree(&dir), before);
    let _ = fs::remove_dir_all(&dir);
}

// One half of the drift check: each refusal lists `Tool::fields()`, in order. The other half, in
// tests/manifests.rs, holds each manifest's properties to it.
#[test]
fn tool_fields_are_listed_in_the_refusal() {
    for tool in Tool::ALL {
        let out = call(tool, &json!({ "zz": 1 }));
        let message = out["message"].as_str().unwrap();
        let listed = message
            .strip_prefix(&format!("unknown field \"zz\": {} takes ", tool.name()))
            .unwrap_or_else(|| panic!("no list in {message}"));
        assert_eq!(listed.split(", ").collect::<Vec<_>>(), tool.fields());
    }
    let out = call(
        Tool::Edit,
        &json!({ "path": "a.txt", "edits": [{ "old": "a", "new": "b", "zz": 1 }] }),
    );
    let message = out["message"].as_str().unwrap();
    let listed = message
        .strip_prefix("unknown field \"zz\" in edit 1 of 1: an edit takes ")
        .unwrap_or_else(|| panic!("no list in {message}"));
    assert_eq!(listed.split(", ").collect::<Vec<_>>(), EDIT_PAIR_FIELDS);
}

// ── required fields ─────────────────────────────────────────────────────────────

#[test]
fn every_tool_reaches_its_handler() {
    // With an empty call, each handler refuses on its own first required field — proof the call
    // passed the field check and reached the handler its tool is paired with.
    for (tool, field) in [
        (Tool::Read, "path"),
        (Tool::Write, "path"),
        (Tool::Edit, "path"),
        (Tool::Find, "pattern"),
    ] {
        let out = call(tool, &json!({}));
        assert_eq!(out["ok"], false, "{}", tool.name());
        assert_eq!(
            out["message"],
            format!("missing required field: {field}"),
            "{} did not reach its own handler",
            tool.name()
        );
        assert!(out.get("error_kind").is_none(), "{out:?}");
        assert!(out["metadata"].is_null(), "{out:?}");
    }
}

#[test]
fn a_missing_or_null_required_field_is_refused_before_the_file_is_read() {
    // S5. murmur lets a `null` required field through (presence is its rule); the tool counts it
    // as absent. The file does not exist, so a refusal other than not_found proves the inputs
    // were checked before any read, and nothing is created.
    let dir = scratch("required_fields");
    let absent = dir.join("absent.txt");
    let absent = absent.to_str().unwrap();
    for (tool, data, message) in [
        (
            Tool::Read,
            json!({ "path": null }),
            "missing required field: path",
        ),
        (
            Tool::Read,
            json!({ "path": "" }),
            "missing required field: path",
        ),
        (
            Tool::Write,
            json!({ "path": null, "content": "x" }),
            "missing required field: path",
        ),
        (
            Tool::Write,
            json!({ "path": absent }),
            "missing required field: content",
        ),
        (
            Tool::Write,
            json!({ "path": absent, "content": null }),
            "missing required field: content",
        ),
        (
            Tool::Edit,
            json!({ "path": null, "edits": [{ "old": "a", "new": "b" }] }),
            "missing required field: path",
        ),
        (
            Tool::Edit,
            json!({ "path": absent }),
            "missing required field: edits",
        ),
        (
            Tool::Edit,
            json!({ "path": absent, "edits": null }),
            "missing required field: edits",
        ),
        (
            Tool::Find,
            json!({ "pattern": null }),
            "missing required field: pattern",
        ),
        (
            Tool::Find,
            json!({ "pattern": "" }),
            "missing required field: pattern",
        ),
    ] {
        let out = call(tool, &data);
        assert_eq!(out["ok"], false, "{data}");
        assert_eq!(out["message"], message, "{} {data}", tool.name());
        assert!(out.get("error_kind").is_none(), "{data}: {out:?}");
    }
    assert!(fs::read_dir(&dir).unwrap().next().is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_null_optional_field_counts_as_absent() {
    let dir = scratch("null_optional");
    fs::write(dir.join("a.txt"), "needle\n").unwrap();
    let out = call(
        Tool::Find,
        &json!({
            "pattern": "needle", "dir": dir.to_str().unwrap(),
            "glob": null, "recursive": null, "context_lines": null,
        }),
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(
        out["data"],
        json!({ "matches": [{ "path": "a.txt", "line": 1, "text": "needle" }] })
    );
    assert_eq!(out["metadata"]["state_effect"], "read");
    let _ = fs::remove_dir_all(&dir);
}

// ── state_effect ────────────────────────────────────────────────────────────────

#[test]
fn with_state_effect_declares_on_success_only() {
    let ok = with_state_effect(ok_summary_only("done", "done"), "mutate");
    assert_eq!(ok["metadata"]["state_effect"], "mutate");

    let failed = with_state_effect(fail_msg("nope"), "mutate");
    assert!(
        failed["metadata"].is_null(),
        "a failed call must not declare a state effect"
    );
}

#[test]
fn read_declares_read_effect() {
    let dir = scratch("state_effect_read");
    let path = dir.join("hello.txt");
    fs::write(&path, "hi\n").unwrap();
    let envelope = json!({
        "data": { "path": path.to_str().unwrap() },
        "log_path": null,
    });
    let out = run(Tool::Read, &envelope.to_string());
    assert_eq!(out["ok"], true, "read should succeed: {out:?}");
    assert_eq!(out["metadata"]["state_effect"], "read");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn write_declares_mutate_effect() {
    let dir = scratch("state_effect_write");
    let path = dir.join("out.txt");
    let envelope = json!({
        "data": { "path": path.to_str().unwrap(), "content": "x" },
        "log_path": null,
    });
    let out = run(Tool::Write, &envelope.to_string());
    assert_eq!(out["ok"], true, "write should succeed: {out:?}");
    assert_eq!(out["metadata"]["state_effect"], "mutate");
    assert_eq!(out["message"], format!("wrote {}", path.display()));
    assert_eq!(out["summary"], "1 bytes written");
    assert!(out["data"].is_null(), "{out:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn write_creates_missing_directories() {
    let dir = scratch("write_dirs");
    let path = dir.join("a/b/out.txt");
    let out = call(
        Tool::Write,
        &json!({ "path": path.to_str().unwrap(), "content": "deep" }),
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "deep");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn write_replaces_an_existing_file() {
    let dir = scratch("write_replace");
    let path = dir.join("f.txt");
    fs::write(&path, "a much longer original\n").unwrap();
    let out = call(
        Tool::Write,
        &json!({ "path": path.to_str().unwrap(), "content": "" }),
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["summary"], "0 bytes written");
    assert_eq!(fs::read(&path).unwrap(), b"");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_null_field_counts_as_absent() {
    let dir = scratch("null_fields");
    let path = write_numbered_file(&dir, "f.txt", 3);
    let out = call(
        Tool::Read,
        &json!({ "path": &path, "start_line": null, "end_line": null }),
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_data_keys(&out, &["content", "total_lines"]);

    let out = call(
        Tool::Write,
        &json!({ "path": dir.join("w.txt").to_str().unwrap(), "content": null }),
    );
    assert_eq!(out["message"], "missing required field: content");
    assert!(!dir.join("w.txt").exists());

    let out = call(Tool::Edit, &json!({ "path": &path, "edits": null }));
    assert_eq!(out["message"], "missing required field: edits");
    let _ = fs::remove_dir_all(&dir);
}

// ── dispatch: what a component hands the WIT `ToolResult` ──────────────────────────

#[test]
fn dispatch_maps_status_data_truncated_and_state_effect() {
    let dir = scratch("dispatch");
    let small = dir.join("small.txt");
    fs::write(&small, "hi there").unwrap();
    let small = small.to_str().unwrap();
    let big = write_numbered_file_keep(&dir, "big.txt", 5000);
    let many = dir.join("many");
    fs::create_dir_all(&many).unwrap();
    fs::write(
        many.join("m.txt"),
        (0..250).map(|i| format!("hit {i}\n")).collect::<String>(),
    )
    .unwrap();

    // A whole read: data is the result's `data` field, re-serialized.
    let out = dispatch(Tool::Read, Some(&json!({ "path": small }).to_string()));
    assert_eq!(out.status, OutcomeStatus::Passed);
    assert_eq!(out.summary.as_deref(), Some("8 bytes"));
    assert_eq!(
        serde_json::from_str::<Value>(out.data.as_deref().unwrap()).unwrap(),
        json!({ "content": "hi there", "total_lines": 1 })
    );
    assert!(!out.truncated);
    assert_eq!(out.state_effect, Some("read"));

    // A cut read sets `truncated`.
    let out = dispatch(Tool::Read, Some(&json!({ "path": &big }).to_string()));
    assert_eq!(out.status, OutcomeStatus::Passed);
    assert!(out.truncated);
    let data: Value = serde_json::from_str(out.data.as_deref().unwrap()).unwrap();
    assert_eq!(data["end_line"], 2000);
    assert!(data["truncated"].is_string());

    // A capped find sets `truncated`.
    let out = dispatch(
        Tool::Find,
        Some(&json!({ "pattern": "hit", "dir": many.to_str().unwrap() }).to_string()),
    );
    assert_eq!(out.status, OutcomeStatus::Passed);
    assert!(out.truncated);
    assert_eq!(
        out.summary.as_deref(),
        Some("200 matches in 1 files (capped)")
    );
    assert_eq!(out.state_effect, Some("read"));

    // A write and an edit carry no data.
    let written = dir.join("w.txt");
    let written = written.to_str().unwrap();
    let out = dispatch(
        Tool::Write,
        Some(&json!({ "path": written, "content": "one" }).to_string()),
    );
    assert_eq!(
        out,
        ToolOutcome {
            status: OutcomeStatus::Passed,
            summary: Some("3 bytes written".to_string()),
            data: None,
            truncated: false,
            state_effect: Some("mutate"),
        }
    );
    let out = dispatch(
        Tool::Edit,
        Some(&json!({ "path": written, "edits": [{ "old": "one", "new": "two" }] }).to_string()),
    );
    assert_eq!(
        out,
        ToolOutcome {
            status: OutcomeStatus::Passed,
            summary: Some("1 edits".to_string()),
            data: None,
            truncated: false,
            state_effect: Some("mutate"),
        }
    );
    assert_eq!(fs::read_to_string(written).unwrap(), "two");

    // An error carries its message in `summary`, no data and no state_effect.
    let out = dispatch(
        Tool::Edit,
        Some(&json!({ "path": written, "edits": [] }).to_string()),
    );
    assert_eq!(
        out,
        ToolOutcome {
            status: OutcomeStatus::Error,
            summary: Some("edits is empty: it takes at least one {old, new} object".to_string()),
            data: None,
            truncated: false,
            state_effect: None,
        }
    );

    // A double-encoded string re-parses inside `run`.
    let double = json!(json!({ "path": small }).to_string()).to_string();
    let out = dispatch(Tool::Read, Some(&double));
    assert_eq!(out.status, OutcomeStatus::Passed, "{out:?}");
    assert_eq!(out.state_effect, Some("read"));

    // Data that is not JSON travels on as a string, which `run` cannot decode.
    let out = dispatch(Tool::Read, Some("path=a.txt"));
    assert_eq!(out.status, OutcomeStatus::Error);
    assert!(
        out.summary
            .as_deref()
            .unwrap()
            .starts_with("invalid data JSON string: "),
        "{out:?}"
    );

    // Absent data is the envelope with no data key.
    for tool in Tool::ALL {
        assert_eq!(
            dispatch(tool, None),
            ToolOutcome {
                status: OutcomeStatus::Error,
                summary: Some("missing data field".to_string()),
                data: None,
                truncated: false,
                state_effect: None,
            }
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// `write_numbered_file` without clearing `dir` first.
fn write_numbered_file_keep(dir: &std::path::Path, name: &str, n: usize) -> String {
    let content: String = (1..=n).map(|i| format!("line {i}\n")).collect();
    let path = dir.join(name);
    fs::write(&path, content).expect("failed to write test file");
    path.to_string_lossy().to_string()
}

#[test]
fn fail_msg_returns_ok_false() {
    let out = fail_msg("something went wrong");
    assert_eq!(out["ok"], false);
    assert_eq!(out["message"], "something went wrong");
}

#[test]
fn err_result_includes_error_kind() {
    let out = err_result(err::NOT_FOUND, "file.txt: not found");
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::NOT_FOUND);
    assert_eq!(out["message"], "file.txt: not found");
}

#[test]
fn err_constants_are_distinct() {
    let kinds = [
        err::NOT_FOUND,
        err::PERMISSION_DENIED,
        err::IO_ERROR,
        err::STRING_NOT_FOUND,
        err::MULTIPLE_MATCHES,
        err::UNKNOWN_FIELD,
        err::INVALID_PATTERN,
        err::RESULT_SIZE_EXCEEDED,
        err::INVALID_RANGE,
        err::RANGE_TOO_LARGE,
        err::CONTEXT_TOO_LARGE,
        err::ABSOLUTE_PATH,
        err::OUTSIDE_WORKDIR,
    ];
    for (i, a) in kinds.iter().enumerate() {
        for (j, b) in kinds.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "error kind constants must be unique");
            }
        }
    }
}

#[test]
fn ok_with_flattens_data_fields() {
    let out = ok_with("done", json!({ "count": 3 }), "3 replacements");
    assert_eq!(out["ok"], true);
    assert_eq!(out["message"], "done");
    assert_eq!(out["summary"], "3 replacements");
    assert_eq!(out["data"]["count"], 3);
    assert_eq!(out["count"], 3); // flattened
}

// ── tool-edit ───────────────────────────────────────────────────────────────────

// ── edit ────────────────────────────────────────────────────────────────

// `{"path": path, "edits": [{old, new}, …]}` through `run`.
fn edit_call(path: &str, pairs: &[(&str, &str)]) -> Value {
    let edits: Vec<Value> = pairs
        .iter()
        .map(|(old, new)| json!({ "old": old, "new": new }))
        .collect();
    call(Tool::Edit, &json!({ "path": path, "edits": edits }))
}

#[test]
fn edit_applies_its_pairs_in_order_each_to_the_result_of_the_one_before() {
    // S1. The second pair only matches the text the first produced.
    let dir = scratch("edit_in_order");
    let file = dir.join("f.txt");
    fs::write(&file, "one two three\n").unwrap();
    let path = file.to_str().unwrap();

    let out = edit_call(path, &[("one", "1x"), ("1x two", "12"), ("three", "3")]);
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "12 3\n");
    assert_eq!(out["metadata"]["state_effect"], "mutate");
    assert_eq!(out["message"], format!("applied 3 edit(s) to {path}"));
    assert_eq!(out["summary"], "3 edits");
    assert!(out["data"].is_null(), "{out:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn edit_with_a_missing_old_changes_nothing_and_names_the_pair() {
    // S2. The first pair matched in memory; the file still holds the original bytes.
    let dir = scratch("edit_not_found");
    let file = dir.join("f.txt");
    fs::write(&file, "one two three\n").unwrap();
    let path = file.to_str().unwrap();

    let out = edit_call(path, &[("one", "1"), ("absent", "x"), ("three", "3")]);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::STRING_NOT_FOUND);
    assert_eq!(
        out["message"],
        format!("edit 2 of 3: old occurs 0 times in {path}; the file is unchanged")
    );
    assert!(out["metadata"].is_null(), "{out:?}");
    assert_eq!(fs::read(&file).unwrap(), b"one two three\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn edit_with_an_ambiguous_old_changes_nothing_and_counts_overlaps() {
    // S3. `aa` occurs twice in `aaa` (at 0 and 1); `dup` twice in `dup dup`.
    let dir = scratch("edit_multiple");
    for (content, pairs) in [
        ("x aaa\n", [("x", "y"), ("aa", "b")]),
        ("x dup dup\n", [("x", "y"), ("dup", "one")]),
    ] {
        let file = dir.join("f.txt");
        fs::write(&file, content).unwrap();
        let path = file.to_str().unwrap();

        let out = edit_call(path, &pairs);
        assert_eq!(out["ok"], false, "{content}");
        assert_eq!(out["error_kind"], err::MULTIPLE_MATCHES, "{content}");
        assert_eq!(
            out["message"],
            format!("edit 2 of 2: old occurs 2 times in {path}; the file is unchanged")
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), content);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn edit_replaces_only_the_one_occurrence() {
    let dir = scratch("edit_one_occurrence");
    let file = dir.join("f.txt");
    fs::write(&file, "keep hello keep\n").unwrap();

    let out = edit_call(
        file.to_str().unwrap(),
        &[("hello", ""), ("keep\n", "end\n")],
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "keep  end\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn occurrences_counts_overlaps_at_char_boundaries() {
    assert_eq!(occurrences("aaa", "aa"), 2);
    assert_eq!(occurrences("aaaa", "aa"), 3);
    assert_eq!(occurrences("dup dup", "dup"), 2);
    assert_eq!(occurrences("abc", "d"), 0);
    assert_eq!(occurrences("ééé", "éé"), 2);
    assert_eq!(occurrences("aéaéa", "aéa"), 2);
    assert_eq!(occurrences("x", "x"), 1);
}

#[test]
fn edit_refuses_bad_edits_before_reading_the_file() {
    // The file does not exist: a refusal other than not_found proves the inputs were
    // checked before the read.
    let dir = scratch("edit_validation");
    let path = dir.join("absent.txt");
    let path = path.to_str().unwrap();
    for (edits, message) in [
        (
            json!([]),
            "edits is empty: it takes at least one {old, new} object",
        ),
        (
            json!("old→new"),
            "edits must be an array of {old, new} objects",
        ),
        (
            json!([{ "old": "a", "new": "b" }, { "old": "", "new": "x" }]),
            "edit 2 of 2: old is empty; it must name the text to replace",
        ),
        (
            json!([{ "new": "x" }]),
            "edit 1 of 1: missing required field: old",
        ),
        (
            json!([{ "old": "x" }]),
            "edit 1 of 1: missing required field: new",
        ),
        (
            json!([{ "old": "x", "new": 3 }]),
            "edit 1 of 1: missing required field: new",
        ),
        (
            json!(["x"]),
            "edit 1 of 1 must be an object with old and new",
        ),
    ] {
        let out = call(Tool::Edit, &json!({ "path": path, "edits": edits }));
        assert_eq!(out["ok"], false, "{edits}");
        assert_eq!(out["message"], message, "{edits}");
        assert!(out.get("error_kind").is_none(), "{edits}: {out:?}");
    }

    let out = call(
        Tool::Edit,
        &json!({ "path": path, "edits": [
        { "old": "a", "new": "b" }, { "old": "c", "new": "d", "path": "x" }
    ] }),
    );
    assert_eq!(out["error_kind"], err::UNKNOWN_FIELD);
    assert_eq!(
        out["message"],
        "unknown field \"path\" in edit 2 of 2: an edit takes old, new"
    );
    assert!(!dir.join("absent.txt").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn edit_of_a_missing_file_is_not_found_and_declares_no_effect() {
    let dir = scratch("edit_missing");
    let path = dir.join("absent.txt");
    let out = edit_call(path.to_str().unwrap(), &[("a", "b")]);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::NOT_FOUND);
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}: ", path.display())),
        "{out:?}"
    );
    assert!(out["metadata"].is_null());
    assert!(!path.exists(), "a failed edit creates nothing");
    let _ = fs::remove_dir_all(&dir);
}

// ── Absolute path refusal ───────────────────────────────────────────────
//
// Every path input is relative to the capsule workdir. Under the single WASI preopen a
// tool is dispatched with, an absolute value resolves *inside* the preopen rather than
// failing, and every operation resolves it the same way — so the only place the
// divergence from the contract shows up is outside the capsule. Each operation
// therefore refuses an absolute value before it reaches the filesystem.

#[test]
fn reject_absolute_renders_one_message_shape_for_every_location() {
    assert!(
        reject_absolute("path", "src/main.rs").is_none(),
        "a relative value is not refused"
    );
    assert!(
        reject_absolute("dir", "").is_none(),
        "an empty value is not absolute — it names the workdir root"
    );

    for (location, value, expected) in [
        (
            "path",
            "/etc/hosts",
            "'path' must be relative to the capsule workdir; got '/etc/hosts' (did you mean 'hosts'?)",
        ),
        (
            "path",
            "/app/results.txt",
            "'path' must be relative to the capsule workdir; got '/app/results.txt' (did you mean 'results.txt'?)",
        ),
        (
            "dir",
            "/app",
            "'dir' must be relative to the capsule workdir; got '/app' (did you mean 'app'?)",
        ),
        // No final component to suggest: the parenthetical is omitted entirely rather
        // than rendered empty.
        ("dir", "/", "'dir' must be relative to the capsule workdir; got '/'"),
        (
            "path",
            "/app/..",
            "'path' must be relative to the capsule workdir; got '/app/..'",
        ),
    ] {
        let out = reject_absolute(location, value).expect("an absolute value is refused");
        assert_eq!(out["ok"], false);
        assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
        assert_eq!(out["message"], expected);
        assert_eq!(out["summary"], expected);
    }
}

#[test]
fn every_path_location_refuses_an_absolute_value_through_dispatch() {
    // S8. Each fixture exists at the absolute value's real location, so a not-found
    // cannot stand in for the refusal, and nothing is created, changed or declared.
    let dir = scratch("absolute_every_location");
    let file = dir.join("results.txt");
    fs::write(&file, "original\n").unwrap();
    let absolute = fs::canonicalize(&file)
        .unwrap()
        .to_string_lossy()
        .to_string();
    let absolute_dir = fs::canonicalize(&dir)
        .unwrap()
        .to_string_lossy()
        .to_string();
    let fresh = format!("{absolute_dir}/app/new.txt");

    for (tool, data, location, value) in [
        (
            Tool::Read,
            json!({ "path": &absolute }),
            "path",
            absolute.as_str(),
        ),
        (
            Tool::Write,
            json!({ "path": &fresh, "content": "x" }),
            "path",
            fresh.as_str(),
        ),
        (
            Tool::Edit,
            json!({ "path": &absolute, "edits": [{ "old": "original", "new": "x" }] }),
            "path",
            absolute.as_str(),
        ),
        (
            Tool::Find,
            json!({ "pattern": "original", "dir": &absolute_dir }),
            "dir",
            absolute_dir.as_str(),
        ),
        (
            Tool::Find,
            json!({ "pattern": "original", "glob": "/src/*.rs" }),
            "glob",
            "/src/*.rs",
        ),
    ] {
        let out = call(tool, &data);
        assert_eq!(out["ok"], false, "{data}");
        assert_eq!(out["error_kind"], err::ABSOLUTE_PATH, "{data}");
        let message = out["message"].as_str().unwrap();
        assert!(
            message.starts_with(&format!(
                "'{location}' must be relative to the capsule workdir; got '{value}'"
            )),
            "{message}"
        );
        assert!(out["metadata"].is_null(), "{data}");
        assert!(
            out["content"].is_null() && out["matches"].is_null(),
            "{out:?}"
        );
    }
    assert_eq!(fs::read_to_string(&file).unwrap(), "original\n");
    assert!(
        !dir.join("app").exists(),
        "a refused write creates no directory"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_refuses_an_absolute_path_even_when_the_file_exists() {
    // The behaviour this refusal removes: an absolute value that a previous write
    // already created reads back "correctly" from the wrong place. The fixture exists,
    // so a not-found cannot stand in for the contract error.
    let dir = scratch("absolute_read");
    let file = dir.join("results.txt");
    fs::write(&file, "content from the wrong place\n").unwrap();
    let absolute = fs::canonicalize(&file)
        .unwrap()
        .to_string_lossy()
        .to_string();

    let out = call(Tool::Read, &json!({ "path": &absolute }));
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    assert!(
        out["content"].is_null(),
        "a refused read returns no content: {out:?}"
    );
    let msg = out["message"].as_str().unwrap();
    assert!(
        msg.starts_with("'path' must be relative to the capsule workdir"),
        "the refusal names the location the caller wrote: {msg}"
    );
    assert!(msg.contains("did you mean 'results.txt'"), "{msg}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn write_refuses_an_absolute_path_before_creating_directories() {
    // The `create_dir_all` in op_write is what manufactured `<workdir>/app/` for
    // `/app/results.txt`. The refusal lands ahead of it, so a rejected write leaves no
    // stray directory behind.
    let dir = scratch("absolute_write");
    let absolute = fs::canonicalize(&dir)
        .unwrap()
        .join("app")
        .join("results.txt")
        .to_string_lossy()
        .to_string();

    let out = call(
        Tool::Write,
        &json!({
            "path": &absolute,
            "content": "written to the wrong place",
        }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    assert!(
        out["message"].as_str().unwrap().starts_with("'path'"),
        "the refusal names path: {out:?}"
    );
    assert!(
        !dir.join("app").exists(),
        "a refused write must not create the directories it named"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn absolute_write_through_dispatch_declares_no_mutation() {
    // Through `run`, so the state_effect wrapper is in play: a refused write is not
    // recorded as a mutation.
    let envelope = json!({
        "data": {
            "path": "/app/results.txt", "content": "written to the wrong place",
        },
        "log_path": null,
    });
    let out = run(Tool::Write, &envelope.to_string());
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    assert!(
        out["metadata"].is_null(),
        "a refused write declares no effect: {out:?}"
    );
}

#[test]
fn edit_refuses_an_absolute_path_and_edits_nothing() {
    let dir = scratch("absolute_edit");
    let file = dir.join("patch.txt");
    fs::write(&file, "hello world\n").unwrap();
    let absolute = fs::canonicalize(&file)
        .unwrap()
        .to_string_lossy()
        .to_string();

    let out = call(
        Tool::Edit,
        &json!({
            "path": &absolute,
            "edits": [{ "old": "hello", "new": "goodbye" }],
        }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    assert!(
        out["message"].as_str().unwrap().starts_with("'path'"),
        "{out:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "hello world\n",
        "a refused edit leaves the file byte-identical"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_refuses_an_absolute_dir() {
    let out = call(
        Tool::Find,
        &json!({
            "pattern": "needle",
            "dir": "/app",
            "recursive": true,
        }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    let msg = out["message"].as_str().unwrap();
    assert!(msg.contains("'dir'"), "{msg}");
    assert!(msg.contains("did you mean 'app'"), "{msg}");
    assert!(
        out["matches"].is_null(),
        "a refused search performs no walk: {out:?}"
    );
}

#[test]
fn find_refuses_the_bare_root_slash_as_absolute() {
    // Under the preopen "/" resolves to the workdir root, which a relative "" or "."
    // already names; spelled absolutely it is refused like any other absolute dir. With
    // no final component there is nothing to suggest.
    let out = call(
        Tool::Find,
        &json!({
            "pattern": "needle",
            "dir": "/",
            "recursive": true,
        }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    let msg = out["message"].as_str().unwrap();
    assert!(msg.contains("'dir'"), "{msg}");
    assert!(msg.contains("got '/'"), "{msg}");
    assert!(
        !msg.contains("did you mean"),
        "no file name to suggest: {msg}"
    );
}

#[test]
fn an_absent_path_input_still_reports_the_missing_field() {
    // The absolute check only runs on a value that was successfully extracted, so an
    // absent input keeps its missing-field message and carries no error_kind.
    for (out, location) in [
        (call(Tool::Read, &json!({})), "path"),
        (call(Tool::Write, &json!({ "content": "x" })), "path"),
        (call(Tool::Edit, &json!({ "edits": [] })), "path"),
        (call(Tool::Find, &json!({ "dir": "src" })), "pattern"),
    ] {
        assert_eq!(out["ok"], false);
        assert!(
            out["error_kind"].is_null(),
            "a missing field is not an absolute-path refusal: {out:?}"
        );
        assert_eq!(
            out["message"],
            format!("missing required field: {location}"),
            "{out:?}"
        );
    }
}

#[test]
fn an_absolute_path_is_reported_ahead_of_a_missing_sibling_field() {
    // tool-write's and tool-edit's path are validated at their extraction site, before
    // `content` or `edits` is read, so the caller hears about the path they cannot use
    // rather than a second field.
    for out in [
        call(Tool::Write, &json!({ "path": "/app/results.txt" })),
        call(Tool::Edit, &json!({ "path": "/app/results.txt" })),
    ] {
        assert_eq!(out["ok"], false);
        assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
        let message = out["message"].as_str().unwrap();
        assert!(
            !message.contains("content") && !message.contains("edits"),
            "the absolute path is the first thing reported: {out:?}"
        );
    }
}

#[test]
fn find_caps_at_the_result_ceiling() {
    // 200 matching lines of ~2,000 bytes each: under the match limit, but the result
    // (every match emitted twice by `ok_with`) would be ~800 KB. The search returns the
    // matches that fit and says why it stopped.
    let dir = scratch("find_result_ceiling");
    let padding = "x".repeat(2000);
    let mut content = String::new();
    for i in 0..200 {
        content.push_str(&format!("line {i} has marker {padding}\n"));
    }
    fs::write(dir.join("large_match_file.txt"), content).unwrap();

    let out = call(
        Tool::Find,
        &json!({
            "pattern": "marker",
            "dir": dir.to_str().unwrap(),
            "recursive": false,
        }),
    );
    assert_eq!(out["ok"], true, "{}", out["message"]);
    assert_eq!(out["status"], "passed");
    let n = out["matches"].as_array().unwrap().len();
    assert!(n > 0 && n < 200, "got {n} matches");
    assert_eq!(
        out["data"]["capped"],
        "Search stopped at the 500 KB result size limit after reading 1 file."
    );
    assert_eq!(out["summary"], format!("{n} matches in 1 files (capped)"));
    assert!(
        serde_json::to_string(&out).unwrap().len() <= FIND_RESULT_SIZE_LIMIT,
        "a capped result fits under the ceiling"
    );
    // The matches kept are the first ones, in file order.
    for (i, m) in out["matches"].as_array().unwrap().iter().enumerate() {
        assert_eq!(m["line"], i + 1);
    }
    // With the metadata `run` attaches, the result still fits.
    let via_run = call(
        Tool::Find,
        &json!({
            "pattern": "marker",
            "dir": dir.to_str().unwrap(),
        }),
    );
    assert_eq!(via_run["metadata"]["state_effect"], "read");
    assert!(serde_json::to_string(&via_run).unwrap().len() <= FIND_RESULT_SIZE_LIMIT);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_refuses_a_single_match_over_the_ceiling() {
    // No partial answer exists: the first match alone does not fit.
    let dir = scratch("find_single_oversized");
    let line = format!("marker {}\n", "x".repeat(FIND_RESULT_SIZE_LIMIT));
    fs::write(dir.join("huge.txt"), line).unwrap();

    let out = call(
        Tool::Find,
        &json!({
            "pattern": "marker",
            "dir": dir.to_str().unwrap(),
        }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::RESULT_SIZE_EXCEEDED);
    assert_eq!(
        out["message"],
        "a single match exceeds the find result size limit of 512000 bytes \
         (500 KB)."
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_respects_specific_dir() {
    // This test verifies that find works with a valid specific directory
    let temp_dir = scratch("find_valid");

    let test_file = temp_dir.join("test.txt");
    fs::write(&test_file, "hello world\nfoo bar\n").expect("failed to write test file");

    let dir_name = temp_dir.to_string_lossy().to_string();
    let op = json!({
        "pattern": "world",
        "dir": &dir_name,
        "recursive": false,
    });

    let out = call(Tool::Find, &op);
    assert_eq!(out["ok"], true);
    assert!(out["matches"].is_array());
    assert_eq!(out["matches"].as_array().unwrap().len(), 1);

    // Cleanup
    let _ = fs::remove_dir_all(&temp_dir);
}

// Asserts a successful result's `data` carries exactly `keys` — no more, no fewer — so
// a key the contract does not name cannot ride along unnoticed.
fn assert_data_keys(out: &Value, keys: &[&str]) {
    let mut got: Vec<&str> = out["data"]
        .as_object()
        .unwrap_or_else(|| panic!("data is not an object: {out:?}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = keys.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "{out:?}");
}

#[test]
fn read_returns_content_and_total_lines_only() {
    let temp_dir = scratch("read");

    let test_file = temp_dir.join("test.txt");
    fs::write(&test_file, "test content").expect("failed to write test file");

    let path = test_file.to_string_lossy().to_string();
    let op = json!({
        "path": &path,
    });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], true);
    assert_eq!(out["content"], "test content");
    assert_data_keys(&out, &["content", "total_lines"]);

    let _ = fs::remove_dir_all(&temp_dir);
}

// ── Bounded read (start_line / end_line) ────────────────────────────
//
// A shared helper writes a file of `n` numbered lines ("line 1".."line n"), the fixture
// every range test slices.

fn write_numbered_file(dir: &std::path::Path, name: &str, n: usize) -> String {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).expect("failed to create temp dir");
    let mut content = String::new();
    for i in 1..=n {
        content.push_str(&format!("line {i}\n"));
    }
    let path = dir.join(name);
    fs::write(&path, content).expect("failed to write test file");
    path.to_string_lossy().to_string()
}

#[test]
fn read_whole_file_reports_total_lines() {
    // A whole-file read (no start/end) returns its content and total_lines — but NO
    // start_line/end_line keys.
    let dir = scratch("read_wholefile_total");
    let path = write_numbered_file(&dir, "f.txt", 5);
    let op = json!({ "path": &path });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "line 1\nline 2\nline 3\nline 4\nline 5\n");
    assert_eq!(out["total_lines"], 5);
    assert_data_keys(&out, &["content", "total_lines"]);
    assert!(
        out["start_line"].is_null(),
        "whole-file read must not add start_line"
    );
    assert!(
        out["end_line"].is_null(),
        "whole-file read must not add end_line"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_happy_path() {
    let dir = scratch("read_ranged");
    let path = write_numbered_file(&dir, "f.txt", 10);
    let op = json!({ "path": &path, "start_line": 2, "end_line": 4 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    // Sliced span, joined with "\n" (no trailing newline — lines() strips terminators).
    assert_eq!(out["content"], "line 2\nline 3\nline 4");
    assert_eq!(out["total_lines"], 10);
    assert_eq!(out["start_line"], 2);
    assert_eq!(out["end_line"], 4);
    assert_data_keys(&out, &["content", "total_lines", "start_line", "end_line"]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_start_only_reads_to_eof() {
    let dir = scratch("read_start_only");
    let path = write_numbered_file(&dir, "f.txt", 6);
    let op = json!({ "path": &path, "start_line": 5 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "line 5\nline 6");
    assert_eq!(out["start_line"], 5);
    assert_eq!(out["end_line"], 6); // resolved to total_lines

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_end_only_reads_from_top() {
    let dir = scratch("read_end_only");
    let path = write_numbered_file(&dir, "f.txt", 6);
    let op = json!({ "path": &path, "end_line": 3 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "line 1\nline 2\nline 3");
    assert_eq!(out["start_line"], 1); // resolved default
    assert_eq!(out["end_line"], 3);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_inverted_span_errors() {
    let dir = scratch("read_inverted");
    let path = write_numbered_file(&dir, "f.txt", 10);
    let op = json!({ "path": &path, "start_line": 5, "end_line": 2 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::INVALID_RANGE);
    assert!(out["message"].as_str().unwrap().contains("inverted"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_start_below_one_errors() {
    let dir = scratch("read_start_zero");
    let path = write_numbered_file(&dir, "f.txt", 10);
    let op = json!({ "path": &path, "start_line": 0 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::INVALID_RANGE);
    assert!(out["message"].as_str().unwrap().contains(">= 1"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_start_beyond_eof_errors() {
    // start past EOF errors and states the real line count — never silently falls back
    // to the whole file.
    let dir = scratch("read_start_eof");
    let path = write_numbered_file(&dir, "f.txt", 4);
    let op = json!({ "path": &path, "start_line": 99 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::INVALID_RANGE);
    let msg = out["message"].as_str().unwrap();
    assert!(msg.contains("beyond end of file"), "{msg}");
    assert!(
        msg.contains("4 lines"),
        "message must state real line count: {msg}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_end_beyond_eof_clamps() {
    // The one asymmetric case: end past EOF clamps to total_lines rather than erroring.
    let dir = scratch("read_end_eof");
    let path = write_numbered_file(&dir, "f.txt", 4);
    let op = json!({ "path": &path, "start_line": 3, "end_line": 999 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "line 3\nline 4");
    assert_eq!(out["end_line"], 4); // clamped, reported as resolved
    assert_eq!(out["total_lines"], 4);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_ranged_oversized_span_errors() {
    let dir = scratch("read_oversized");
    // A file larger than the cap, requested whole via an explicit span.
    let path = write_numbered_file(&dir, "big.txt", MAX_READ_LINES + 500);
    let op = json!({
        "path": &path,
        "start_line": 1,
        "end_line": MAX_READ_LINES + 500,
    });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::RANGE_TOO_LARGE);
    assert!(out["message"]
        .as_str()
        .unwrap()
        .contains("exceeds the maximum"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_end_only_span_over_the_line_limit_errors() {
    let dir = scratch("read_end_only_oversized");
    let path = write_numbered_file(&dir, "big.txt", 2500);
    let op = json!({ "path": &path, "end_line": 2500 });

    let out = call(Tool::Read, &op);
    assert_eq!(out["ok"], false, "{out:?}");
    assert_eq!(out["error_kind"], err::RANGE_TOO_LARGE);
    assert_eq!(
        out["message"],
        "requested range of 2500 lines exceeds the maximum of 2000; narrow start_line/end_line"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ── read limits: lines, bytes and line length ───────────────────────

// `first..=last` of a `write_numbered_file` fixture, joined as a read returns them.
fn numbered_lines(first: usize, last: usize) -> String {
    (first..=last)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn read_with_no_range_stops_at_the_line_limit() {
    let dir = scratch("read_line_limit");
    let path = write_numbered_file(&dir, "big.txt", 5000);

    let out = call(Tool::Read, &json!({ "path": &path }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], numbered_lines(1, 2000));
    assert!(out["content"].as_str().unwrap().ends_with("line 2000"));
    assert_data_keys(
        &out,
        &[
            "content",
            "total_lines",
            "start_line",
            "end_line",
            "truncated",
        ],
    );
    assert_eq!(out["total_lines"], 5000);
    assert_eq!(out["start_line"], 1);
    assert_eq!(out["end_line"], 2000);
    assert_eq!(
        out["truncated"],
        "Read stopped at the limit of 2000 lines. Lines 1-2000 of 5000 are returned; start_line 2001 continues the read."
    );
    assert_eq!(out["message"], format!("read {path} lines 1-2000"));
    assert!(
        out["summary"]
            .as_str()
            .unwrap()
            .ends_with("(lines 1-2000 of 5000, truncated)"),
        "{out:?}"
    );
    assert_eq!(out["metadata"]["state_effect"], "read");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_start_line_named_in_the_note_continues_the_read() {
    let dir = scratch("read_continue");
    let path = write_numbered_file(&dir, "big.txt", 5000);

    let second = call(Tool::Read, &json!({ "path": &path, "start_line": 2001 }));
    assert_eq!(second["ok"], true, "{second:?}");
    assert_eq!(second["content"], numbered_lines(2001, 4000));
    assert_eq!(second["start_line"], 2001);
    assert_eq!(second["end_line"], 4000);
    assert!(
        second["truncated"]
            .as_str()
            .unwrap()
            .ends_with("start_line 4001 continues the read."),
        "{second:?}"
    );

    let last = call(Tool::Read, &json!({ "path": &path, "start_line": 4001 }));
    assert_eq!(last["ok"], true, "{last:?}");
    assert_eq!(last["content"], numbered_lines(4001, 5000));
    assert_eq!(last["start_line"], 4001);
    assert_eq!(last["end_line"], 5000);
    assert_data_keys(&last, &["content", "total_lines", "start_line", "end_line"]);
    assert_eq!(
        last["summary"],
        format!(
            "{} bytes (lines 4001-5000 of 5000)",
            numbered_lines(4001, 5000).len()
        )
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_with_no_range_within_every_limit_is_verbatim() {
    let dir = scratch("read_fits");
    let path = write_numbered_file(&dir, "f.txt", MAX_READ_LINES);
    let expected = fs::read_to_string(&path).unwrap();
    assert!(expected.len() <= MAX_READ_BYTES);

    let out = call(Tool::Read, &json!({ "path": &path }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], expected.as_str());
    assert!(out["content"].as_str().unwrap().ends_with("line 2000\n"));
    assert_eq!(out["total_lines"], MAX_READ_LINES);
    assert_data_keys(&out, &["content", "total_lines"]);
    assert_eq!(out["message"], format!("read {path}"));
    assert_eq!(out["summary"], format!("{} bytes", expected.len()));

    let empty = dir.join("empty.txt");
    fs::write(&empty, "").unwrap();
    let out = call(Tool::Read, &json!({ "path": empty.to_string_lossy() }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "");
    assert_eq!(out["total_lines"], 0);
    assert_data_keys(&out, &["content", "total_lines"]);

    let _ = fs::remove_dir_all(&dir);
}

// 60 lines of 1,900 `x`, 114,060 bytes: over the byte limit and under the line limit.
fn write_wide_file(dir: &std::path::Path) -> String {
    let path = dir.join("wide.txt");
    fs::write(&path, format!("{}\n", "x".repeat(1900)).repeat(60)).unwrap();
    path.to_string_lossy().to_string()
}

// 53×1900+52 = 100,752 ≤ 102,400 < 54×1900+53 = 102,653: 53 lines fit.
const WIDE_NOTE: &str = "Read stopped at the limit of 100 KB. Lines 1-53 of 60 are returned; start_line 54 continues the read.";

#[test]
fn read_with_no_range_stops_at_the_byte_limit() {
    let dir = scratch("read_byte_limit");
    let path = write_wide_file(&dir);
    assert_eq!(fs::metadata(&path).unwrap().len(), 114_060);

    let out = call(Tool::Read, &json!({ "path": &path }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["start_line"], 1);
    assert_eq!(out["end_line"], 53);
    assert_eq!(out["total_lines"], 60);
    assert_eq!(out["content"].as_str().unwrap().len(), 100_752);
    assert!(out["content"].as_str().unwrap().len() <= MAX_READ_BYTES);
    assert_eq!(out["truncated"], WIDE_NOTE);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_explicit_span_stops_at_the_byte_limit() {
    let dir = scratch("read_byte_limit_span");
    let path = write_wide_file(&dir);

    let out = call(
        Tool::Read,
        &json!({ "path": &path, "start_line": 1, "end_line": 60 }),
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["start_line"], 1);
    assert_eq!(out["end_line"], 53);
    assert_eq!(out["content"].as_str().unwrap().len(), 100_752);
    assert_eq!(out["truncated"], WIDE_NOTE);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_cuts_a_long_line_to_its_first_2000_characters() {
    let dir = scratch("read_long_line");
    let one = dir.join("one.txt");
    fs::write(&one, "a".repeat(50_000)).unwrap();

    let out = call(Tool::Read, &json!({ "path": one.to_string_lossy() }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["content"], "a".repeat(2000));
    assert_eq!(out["start_line"], 1);
    assert_eq!(out["end_line"], 1);
    assert_eq!(out["total_lines"], 1);
    assert_eq!(
        out["truncated"],
        "Lines longer than 2000 characters are cut to their first 2000 characters: 1 in this read, the first at line 1."
    );

    // Multi-byte characters are cut on a char boundary: 2000 `é` are 4000 bytes.
    let accents = dir.join("accents.txt");
    fs::write(&accents, format!("first\n{}\nthird\n", "é".repeat(3000))).unwrap();
    let out = call(Tool::Read, &json!({ "path": accents.to_string_lossy() }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(
        out["content"],
        format!("first\n{}\nthird", "é".repeat(2000))
    );
    assert_eq!("é".repeat(2000).len(), 4000);
    assert_eq!(out["end_line"], 3);
    assert!(
        out["truncated"]
            .as_str()
            .unwrap()
            .ends_with("1 in this read, the first at line 2."),
        "{out:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_note_names_the_line_limit_then_the_cut_lines() {
    let dir = scratch("read_both_notes");
    let path = write_numbered_file(&dir, "f.txt", 2500);
    let mut lines: Vec<String> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines[9] = "b".repeat(2500);
    fs::write(&path, lines.join("\n") + "\n").unwrap();

    let out = call(Tool::Read, &json!({ "path": &path }));
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(out["end_line"], 2000);
    assert_eq!(
        out["truncated"],
        "Read stopped at the limit of 2000 lines. Lines 1-2000 of 2500 are returned; start_line 2001 continues the read. Lines longer than 2000 characters are cut to their first 2000 characters: 1 in this read, the first at line 10."
    );

    let _ = fs::remove_dir_all(&dir);
}

// ── Repeat reads: every successful read returns the bytes ────────────────
//
// Nothing an earlier read did can change what a later read returns: the tool keeps no
// state between calls and writes nothing, so each read reflects the file as it is now.

#[test]
fn read_repeat_whole_file_read_returns_content_again() {
    let dir = scratch("read_repeat_whole");
    let path = write_numbered_file(&dir, "f.txt", 5);
    let op = json!({ "path": &path });

    let first = call(Tool::Read, &op);
    let second = call(Tool::Read, &op);
    for out in [&first, &second] {
        assert_eq!(out["ok"], true, "{out:?}");
        assert_eq!(out["content"], "line 1\nline 2\nline 3\nline 4\nline 5\n");
        assert_eq!(out["total_lines"], 5);
        assert_eq!(out["message"], format!("read {path}"));
        assert_data_keys(out, &["content", "total_lines"]);
    }
    assert_eq!(first, second);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_repeat_ranged_read_returns_content_again() {
    let dir = scratch("read_repeat_ranged");
    let path = write_numbered_file(&dir, "f.txt", 10);
    let op = json!({ "path": &path, "start_line": 3, "end_line": 5 });

    for _ in 0..2 {
        let out = call(Tool::Read, &op);
        assert_eq!(out["ok"], true, "{out:?}");
        assert_eq!(out["content"], "line 3\nline 4\nline 5");
        assert_eq!(out["total_lines"], 10);
        assert_eq!(out["start_line"], 3);
        assert_eq!(out["end_line"], 5);
        assert_eq!(out["message"], format!("read {path} lines 3-5"));
        assert_data_keys(&out, &["content", "total_lines", "start_line", "end_line"]);
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_sees_a_same_second_same_length_edit() {
    // No pause: the rewrite lands within the same second as the first read and keeps
    // the byte length, so neither a whole-second mtime nor the size tells them apart.
    let dir = scratch("read_same_second_edit");
    let file = dir.join("f.txt");
    fs::write(&file, "aaaa").unwrap();
    let path = file.to_string_lossy().to_string();
    let op = json!({ "path": &path });

    let before = call(Tool::Read, &op);
    assert_eq!(before["content"], "aaaa", "{before:?}");

    fs::write(&file, "bbbb").unwrap();
    let after = call(Tool::Read, &op);
    assert_eq!(after["ok"], true, "{after:?}");
    assert_eq!(after["content"], "bbbb");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_of_a_missing_file_is_not_found_and_declares_no_effect() {
    let dir = scratch("read_missing");
    let path = dir.join("absent.txt").to_string_lossy().to_string();

    for extra in [json!({}), json!({ "start_line": 1, "end_line": 2 })] {
        let mut data = json!({ "path": &path });
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let out = call(Tool::Read, &data);
        assert_eq!(out["ok"], false, "{out:?}");
        assert_eq!(out["error_kind"], err::NOT_FOUND, "{out:?}");
        let message = out["message"].as_str().unwrap();
        assert!(message.starts_with(&format!("{path}: ")), "{out:?}");
        assert!(
            out["metadata"].is_null(),
            "a failed read declares no effect: {out:?}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_refuses_an_unknown_input() {
    let dir = scratch("read_unknown_inputs");
    let path = write_numbered_file(&dir, "f.txt", 3);

    let out = call(Tool::Read, &json!({ "path": &path, "no_cache": true }));
    assert_eq!(out["ok"], false, "{out:?}");
    assert_eq!(out["error_kind"], err::UNKNOWN_FIELD);
    assert_eq!(
        out["message"],
        "unknown field \"no_cache\": tool-read takes path, start_line, end_line"
    );
    assert!(
        out["content"].is_null() && out["metadata"].is_null(),
        "{out:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ── find context_lines ─────────────────────────────────────────

fn write_find_fixture(dir: &std::path::Path, name: &str, content: &str) -> String {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).expect("failed to create temp dir");
    fs::write(dir.join(name), content).expect("failed to write fixture");
    dir.to_string_lossy().to_string()
}

#[test]
fn find_default_has_no_context_keys() {
    // context_lines omitted → the match shape is byte-for-byte the historical one:
    // {path, line, text} with NO context_before/context_after keys at all.
    let dir = scratch("find_no_ctx");
    let dir_name = write_find_fixture(&dir, "a.txt", "one\ntwo needle\nthree\n");
    let op = json!({ "pattern": "needle", "dir": &dir_name, "recursive": false });

    let out = call(Tool::Find, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    let m = &out["matches"][0];
    assert_eq!(m["line"], 2);
    assert_eq!(m["text"], "two needle");
    assert!(
        m.get("context_before").is_none(),
        "no context key when omitted"
    );
    assert!(
        m.get("context_after").is_none(),
        "no context key when omitted"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_context_lines_happy_path() {
    let dir = scratch("find_ctx");
    let dir_name = write_find_fixture(&dir, "a.txt", "L1\nL2\nL3 needle\nL4\nL5\n");
    let op = json!({
        "pattern": "needle",
        "dir": &dir_name, "recursive": false, "context_lines": 1
    });

    let out = call(Tool::Find, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    let m = &out["matches"][0];
    assert_eq!(m["line"], 3);
    assert_eq!(m["context_before"], json!(["L2"]));
    assert_eq!(m["context_after"], json!(["L4"]));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_context_clamped_at_boundaries() {
    // A match on the first line gets an empty context_before (clamped, never padded)
    // and a context_after shortened to the lines that exist.
    let dir = scratch("find_ctx_boundary");
    let dir_name = write_find_fixture(&dir, "a.txt", "head needle\nb\nc\n");
    let op = json!({
        "pattern": "needle",
        "dir": &dir_name, "recursive": false, "context_lines": 5
    });

    let out = call(Tool::Find, &op);
    assert_eq!(out["ok"], true, "{out:?}");
    let m = &out["matches"][0];
    assert_eq!(m["line"], 1);
    assert_eq!(
        m["context_before"],
        json!([]),
        "top-of-file match: empty before"
    );
    assert_eq!(
        m["context_after"],
        json!(["b", "c"]),
        "clamped to existing lines"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_rejects_oversized_context() {
    let dir = scratch("find_ctx_toobig");
    let dir_name = write_find_fixture(&dir, "a.txt", "needle\n");
    for bad in [MAX_CONTEXT_LINES + 1, -1] {
        let op = json!({
            "pattern": "needle",
            "dir": &dir_name, "recursive": false, "context_lines": bad
        });
        let out = call(Tool::Find, &op);
        assert_eq!(out["ok"], false, "context_lines {bad} must be rejected");
        assert_eq!(out["error_kind"], err::CONTEXT_TOO_LARGE);
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_size_ceiling_accounts_for_context() {
    // The same search with a context window carries the neighbouring lines in every
    // match, so it reaches the result ceiling after fewer matches than without one —
    // proving the size accounting measures the context text, not just the match lines.
    let dir = scratch("find_ctx_ceiling");

    let padding = "x".repeat(200);
    let mut content = String::new();
    // 1200 lines; every 4th is a match. The 900 non-matching lines are heavy padding
    // that only enters the payload once context is requested.
    for i in 0..1200 {
        if i % 4 == 0 {
            content.push_str(&format!("marker {padding}\n"));
        } else {
            content.push_str(&format!("{padding}\n"));
        }
    }
    fs::write(dir.join("big.txt"), content).expect("failed to write fixture");
    let dir_name = dir.to_string_lossy().to_string();

    let no_ctx = call(
        Tool::Find,
        &json!({
            "pattern": "marker",
            "dir": &dir_name, "recursive": false, "context_lines": 0
        }),
    );
    let with_ctx = call(
        Tool::Find,
        &json!({
            "pattern": "marker",
            "dir": &dir_name, "recursive": false, "context_lines": 3
        }),
    );
    for out in [&no_ctx, &with_ctx] {
        assert_eq!(out["ok"], true, "{}", out["message"]);
        assert!(serde_json::to_string(out).unwrap().len() <= FIND_RESULT_SIZE_LIMIT);
    }
    let without = no_ctx["matches"].as_array().unwrap().len();
    let with = with_ctx["matches"].as_array().unwrap().len();
    assert!(with < without, "with context {with}, without {without}");
    assert_eq!(
        with_ctx["data"]["capped"],
        "Search stopped at the 500 KB result size limit after reading 1 file."
    );

    let _ = fs::remove_dir_all(&dir);
}

// ── find: dir normalization ────────────────────────────────────

#[test]
fn normalize_dir_resolves_dot_segments_lexically() {
    for root in ["", ".", "./", "./.", "src/..", "a/b/../.."] {
        assert_eq!(
            normalize_dir(root).as_deref(),
            Some(""),
            "{root:?} is the root"
        );
    }
    assert_eq!(normalize_dir("src/./lib/").as_deref(), Some("src/lib"));
    assert_eq!(normalize_dir("src//lib").as_deref(), Some("src/lib"));
    for climb in ["..", "../x", "src/../.."] {
        assert_eq!(
            normalize_dir(climb),
            None,
            "{climb:?} climbs above the root"
        );
    }
}

#[test]
fn find_refuses_a_dir_above_the_workdir() {
    // `pattern` is an invalid regex: the refusal lands ahead of compiling it, and so
    // ahead of any walk.
    for climb in ["..", "../x", "src/../.."] {
        let out = call(
            Tool::Find,
            &json!({
                "pattern": "(", "dir": climb,
            }),
        );
        assert_eq!(out["ok"], false, "{climb}");
        assert_eq!(out["error_kind"], err::OUTSIDE_WORKDIR, "{climb}: {out:?}");
        assert_eq!(
            out["message"],
            format!("'dir' resolves outside the capsule workdir: '{climb}'")
        );
        assert!(out["matches"].is_null());
    }
}

#[test]
fn find_resolves_dot_segments_inside_dir() {
    let dir = scratch("find_dot_segments");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("top.txt"), "needle\n").unwrap();
    fs::write(dir.join("sub/inner.txt"), "needle\n").unwrap();
    let base = dir.to_str().unwrap();

    for spelling in [
        format!("{base}/sub/.."),
        format!("./{base}/."),
        format!("{base}/"),
    ] {
        let out = call(
            Tool::Find,
            &json!({
                "pattern": "needle", "dir": spelling,
            }),
        );
        assert_eq!(out["ok"], true, "{spelling}: {out:?}");
        assert_eq!(
            out["data"],
            json!({ "matches": [
                { "path": "sub/inner.txt", "line": 1, "text": "needle" },
                { "path": "top.txt", "line": 1, "text": "needle" },
            ]}),
            "{spelling}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_rejects_a_non_string_dir_or_glob() {
    let out = call(
        Tool::Find,
        &json!({
            "pattern": "x", "dir": 3,
        }),
    );
    assert_eq!(out["message"], "dir must be a string");
    assert!(out["error_kind"].is_null());
    let out = call(
        Tool::Find,
        &json!({
            "pattern": "x", "glob": ["*.py"],
        }),
    );
    assert_eq!(out["message"], "glob must be a string");
    assert!(out["error_kind"].is_null());
}

// ── find: glob ─────────────────────────────────────────────────

#[test]
fn glob_to_regex_translates_each_wildcard() {
    let matches = |glob: &str, subject: &str| glob_to_regex(glob).unwrap().is_match(subject);

    assert!(matches("*.py", "app.py"));
    assert!(!matches("*.py", "app.pyc"));
    assert!(
        !matches("*.py", "pkg/app.py"),
        "`*` stays within one segment"
    );

    assert!(
        matches("src/**/*.rs", "src/a.rs"),
        "`**/` matches zero segments"
    );
    assert!(matches("src/**/*.rs", "src/x/y/a.rs"));
    assert!(!matches("src/**/*.rs", "lib/a.rs"));
    assert!(!matches("src/**/*.rs", "src/a.rsx"), "anchored at the end");
    assert!(
        !matches("src/**/*.rs", "x/src/a.rs"),
        "anchored at the start"
    );

    assert!(matches("lib/**", "lib/a"));
    assert!(matches("lib/**", "lib/x/y/z.txt"));
    assert!(!matches("lib/**", "libx/a"));
    assert!(matches("**", "anything/at/all"));
    assert!(matches("**/*.md", "notes.md"));
    assert!(matches("**/*.md", "docs/a/notes.md"));

    assert!(matches("f?.txt", "f1.txt"));
    assert!(!matches("f?.txt", "f12.txt"));
    assert!(!matches("a?b", "a/b"), "`?` never matches `/`");

    assert!(matches("a+b.txt", "a+b.txt"), "metacharacters are literal");
    assert!(!matches("a+b.txt", "aab.txt"));
    assert!(!matches("a+b.txt", "a+bxtxt"), "`.` is literal");
    assert!(matches("(x)[y]{z}^$|.txt", "(x)[y]{z}^$|.txt"));
}

#[test]
fn find_glob_filters_by_name_or_relative_path() {
    let dir = scratch("find_glob");
    fs::create_dir_all(dir.join("lib")).unwrap();
    fs::create_dir_all(dir.join("src/a")).unwrap();
    for file in ["main.py", "lib/helper.py", "lib/notes.md", "src/a/b.rs"] {
        fs::write(dir.join(file), "needle\n").unwrap();
    }
    let paths = |glob: &str| -> Vec<String> {
        let out = call(
            Tool::Find,
            &json!({
                "pattern": "needle",
                "dir": dir.to_str().unwrap(), "glob": glob,
            }),
        );
        assert_eq!(out["ok"], true, "{glob}: {out:?}");
        out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(paths("*.py"), ["lib/helper.py", "main.py"]);
    assert_eq!(paths("src/**/*.rs"), ["src/a/b.rs"]);
    assert_eq!(paths("lib/*"), ["lib/helper.py", "lib/notes.md"]);
    assert_eq!(
        paths(""),
        ["lib/helper.py", "lib/notes.md", "main.py", "src/a/b.rs"]
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_refuses_an_absolute_glob() {
    let out = call(
        Tool::Find,
        &json!({
            "pattern": "needle", "glob": "/src/*.rs",
        }),
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error_kind"], err::ABSOLUTE_PATH);
    assert_eq!(
        out["message"],
        "'glob' must be relative to the capsule workdir; got '/src/*.rs' (did you mean '*.rs'?)"
    );
    assert!(
        out["matches"].is_null(),
        "a refused search performs no walk: {out:?}"
    );
}

// ── find: walk rules and budgets ───────────────────────────────

#[test]
fn find_skips_hidden_build_and_binary_entries_below_the_root() {
    let dir = scratch("find_skip_rules");
    for sub in [".git", ".hidden", "target", "node_modules/m"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for file in [
        "a.txt",
        ".git/config",
        ".hidden/x.txt",
        "target/out.txt",
        "node_modules/m/i.js",
        ".env",
    ] {
        fs::write(dir.join(file), "needle\n").unwrap();
    }
    fs::write(dir.join("bin.dat"), b"needle\n\0\x01").unwrap();
    fs::write(dir.join("latin1.txt"), b"needle \xe9\n").unwrap();
    let base = dir.to_str().unwrap();

    let paths = |dir: &str| -> Vec<String> {
        let out = call(
            Tool::Find,
            &json!({
                "pattern": "needle", "dir": dir,
            }),
        );
        assert_eq!(out["ok"], true, "{out:?}");
        out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(paths(base), [".env", "a.txt"]);
    assert_eq!(paths(&format!("{base}/.git")), ["config"]);
    assert_eq!(paths(&format!("{base}/target")), ["out.txt"]);

    let _ = fs::remove_dir_all(&dir);
}

// Five files `f0`..`f4`, each 7 bytes ("needle\n"), under a fresh scratch dir.
fn write_five_needles(tag: &str) -> std::path::PathBuf {
    let dir = scratch(tag);
    for i in 0..5 {
        fs::write(dir.join(format!("f{i}")), "needle\n").unwrap();
    }
    dir
}

fn matched_paths(out: &Value) -> Vec<&str> {
    out["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect()
}

#[test]
fn find_caps_at_the_file_limit() {
    let dir = write_five_needles("find_file_limit");
    let limits = Limits {
        files: 3,
        ..Limits::PRODUCTION
    };
    let op = json!({ "pattern": "needle", "dir": dir.to_str().unwrap() });

    let out = find_with_limits(&op, limits);
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(matched_paths(&out), ["f0", "f1", "f2"]);
    assert_eq!(
        out["data"]["capped"],
        "Search stopped at the limit of 3 files walked after reading 3 files."
    );
    assert_eq!(out["summary"], "3 matches in 3 files (capped)");
    assert_eq!(out["message"], out["summary"]);

    // Files the glob excludes count toward the limit: the walk passes f0..f2 unread and
    // stops before it reaches f4.
    let mut globbed = op.clone();
    globbed["glob"] = json!("f4");
    let out = find_with_limits(&globbed, limits);
    assert_eq!(out["ok"], true, "{out:?}");
    assert!(matched_paths(&out).is_empty(), "{out:?}");
    assert_eq!(out["summary"], "0 matches in 0 files (capped)");
    assert_eq!(
        out["data"]["capped"],
        "Search stopped at the limit of 3 files walked after reading 0 files."
    );

    // A walk that ends on exactly the limit was not stopped by it.
    let out = find_with_limits(
        &op,
        Limits {
            files: 5,
            ..Limits::PRODUCTION
        },
    );
    assert_eq!(
        out["data"],
        json!({ "matches": out["matches"] }),
        "no capped key"
    );
    assert_eq!(out["summary"], "5 matches in 5 files");

    // The same holds when the glob admits only the last file walked.
    let out = find_with_limits(
        &globbed,
        Limits {
            files: 5,
            ..Limits::PRODUCTION
        },
    );
    assert_eq!(matched_paths(&out), ["f4"]);
    assert!(out["data"].get("capped").is_none(), "{out:?}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_non_recursive_walk_counts_files_the_glob_excludes() {
    let dir = write_five_needles("find_file_limit_flat");
    let out = find_with_limits(
        &json!({
            "pattern": "needle",
            "dir": dir.to_str().unwrap(),
            "recursive": false,
            "glob": "f4",
        }),
        Limits {
            files: 3,
            ..Limits::PRODUCTION
        },
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert!(matched_paths(&out).is_empty(), "{out:?}");
    assert_eq!(out["summary"], "0 matches in 0 files (capped)");
    assert_eq!(
        out["data"]["capped"],
        "Search stopped at the limit of 3 files walked after reading 0 files."
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_caps_at_the_byte_limit() {
    let dir = write_five_needles("find_byte_limit");
    // Two 7-byte files fit in 20 bytes; the third would take the total to 21.
    let limits = Limits {
        bytes: 20,
        ..Limits::PRODUCTION
    };
    let out = find_with_limits(
        &json!({ "pattern": "needle", "dir": dir.to_str().unwrap() }),
        limits,
    );
    assert_eq!(out["ok"], true, "{out:?}");
    assert_eq!(matched_paths(&out), ["f0", "f1"]);
    assert_eq!(
        out["data"]["capped"],
        "Search stopped at the limit of 20 bytes read after reading 2 files."
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_caps_at_the_match_limit_and_reads_no_further_file() {
    let dir = write_five_needles("find_match_limit");
    fs::write(dir.join("f1"), "needle\nneedle\nneedle\n").unwrap();
    let op = json!({ "pattern": "needle", "dir": dir.to_str().unwrap() });

    let out = find_with_limits(
        &op,
        Limits {
            matches: 3,
            ..Limits::PRODUCTION
        },
    );
    assert_eq!(matched_paths(&out), ["f0", "f1", "f1"]);
    assert_eq!(out["matches"][2]["line"], 2);
    assert_eq!(
        out["data"]["capped"],
        "Search stopped at the limit of 3 matches after reading 2 files."
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cap_notes_name_each_production_limit() {
    let limits = Limits::PRODUCTION;
    for (reason, named) in [
        (CapReason::Matches, "the limit of 200 matches"),
        (CapReason::Files, "the limit of 5000 files walked"),
        (CapReason::Bytes, "the limit of 32 MiB read"),
        (CapReason::ResultSize, "the 500 KB result size limit"),
    ] {
        let note = cap_note(reason, limits, 1);
        assert_eq!(
            note,
            format!("Search stopped at {named} after reading 1 file.")
        );
    }
}

#[test]
fn a_capped_note_sorts_before_the_matches() {
    let data = find_result(
        &[json!({"path": "a", "line": 1, "text": "x"})],
        1,
        Some(CapReason::Matches),
        Limits::PRODUCTION,
    )["data"]
        .to_string();
    let capped = data.find("\"capped\"").unwrap();
    let matches = data.find("\"matches\"").unwrap();
    assert!(capped < matches, "{data}");
}
