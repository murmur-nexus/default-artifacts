//! Tool-scaffolding tool, packaged as a `wasm32-wasip2` component exporting
//! `murmur:tool/run` (world `tool`).
//!
//! The scaffold logic (request parsing plus the pure file-writing functions) lives in a
//! `cfg`-independent [`logic`] module so it stays host-testable with `cargo test` — the
//! same split every hook crate uses (see `hooks/murmur-hook-compact/src/lib.rs`). The
//! `wasm_tool` module (compiled only for `wasm32`) is a thin adapter mapping the
//! `murmur:tool/run` `ToolInput`/`ToolResult` to [`logic::handle_request`].
//!
//! There is no direct-argv CLI mode: a `wasm32-wasip2` `cdylib` exporting
//! `murmur:tool/run` has no standalone `main(args)` a developer can invoke outside a
//! capsule host, so all input arrives through the stdin envelope handled by
//! [`logic::handle_request`].

// ── Pure, host-testable scaffold logic (no WASM bindings, no `cfg`) ────────────
pub mod logic {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::Value;

    /// Handle a scaffold request payload — the `data` field the native binary read from its
    /// stdin envelope. Returns the created tool's `(name, relative_path)` on success.
    /// `base_dir` is the workdir root under which `tools/<name>/` is created (the preopened
    /// `.` at dispatch time).
    ///
    /// Accepts either a raw request object `{"type":"tool","name":...,"runtime":...}` or a
    /// double-encoded envelope `{"data":"<json-string>",...}`.
    pub fn handle_request(data: Option<&str>, base_dir: &Path) -> Result<(String, String), String> {
        let input = match data {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => return Err("no input provided on stdin".to_string()),
        };

        let parsed: Value =
            serde_json::from_str(input).map_err(|e| format!("invalid stdin JSON: {e}"))?;

        let request = if let Some(data_str) = parsed.get("data").and_then(|d| d.as_str()) {
            serde_json::from_str::<Value>(data_str)
                .map_err(|e| format!("invalid 'data' JSON: {e}"))?
        } else {
            parsed
        };

        let scaffold_type = request
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("tool");

        match scaffold_type {
            "tool" => {
                let name = request
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| "missing 'name' field in request".to_string())?;
                let runtime = request
                    .get("runtime")
                    .and_then(|r| r.as_str())
                    .unwrap_or("native");

                scaffold_tool_in(base_dir, name, runtime)?;
                Ok((name.to_string(), format!("tools/{name}")))
            }
            other => Err(format!(
                "unknown scaffold type '{other}'; supported types: tool"
            )),
        }
    }

    pub fn scaffold_tool_in(base_dir: &Path, name: &str, runtime: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("tool name must not be empty".to_string());
        }

        let tool_dir = base_dir.join(PathBuf::from("tools").join(name));

        if tool_dir.exists() {
            return Err(format!(
                "tools/{name} already exists; remove it before scaffolding"
            ));
        }

        fs::create_dir_all(&tool_dir)
            .map_err(|e| format!("failed to create {}: {e}", tool_dir.display()))?;

        write_manifest(&tool_dir, name, runtime)?;

        match runtime {
            "native" => write_native_stub(&tool_dir, name)?,
            "wasm" => write_wasm_stub(&tool_dir, name)?,
            "hook" => write_hook_stub(&tool_dir, name)?,
            other => {
                return Err(format!(
                    "unknown runtime '{other}'; expected 'native', 'wasm', or 'hook'"
                ))
            }
        }

        write_readme(&tool_dir, name, runtime)?;

        Ok(())
    }

    fn write_manifest(tool_dir: &Path, name: &str, runtime: &str) -> Result<(), String> {
        let content = format!(
            "name: {name}\n\
             version: 0.3.2\n\
             runtime: {runtime}\n\
             description: |\n\
             \x20 TODO: describe what {name} does\n\
             input_schema: |\n\
             \x20 {{\"type\":\"object\",\"properties\":{{}}}}\n\
             output_schema: |\n\
             \x20 {{\"type\":\"object\",\"properties\":{{}}}}\n"
        );
        fs::write(tool_dir.join("murmur.yaml"), content)
            .map_err(|e| format!("failed to write murmur.yaml: {e}"))
    }

    fn write_native_stub(tool_dir: &Path, _name: &str) -> Result<(), String> {
        let bin_dir = tool_dir.join("bin");
        fs::create_dir_all(&bin_dir).map_err(|e| format!("failed to create bin/: {e}"))?;

        let stub = "#!/bin/sh\n\
                    # Read ToolInput JSON from stdin, write ToolResult JSON to stdout.\n\
                    # Replace this stub body with your implementation.\n\
                    INPUT=$(cat)\n\
                    echo '{\"status\":\"passed\",\"summary\":\"stub: not yet implemented\",\"data\":null,\"data_path\":null,\"truncated\":false,\"metadata\":[]}'\n";

        let run_path = bin_dir.join("run");
        fs::write(&run_path, stub).map_err(|e| format!("failed to write bin/run: {e}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&run_path)
                .map_err(|e| format!("failed to read bin/run metadata: {e}"))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&run_path, perms)
                .map_err(|e| format!("failed to chmod bin/run: {e}"))?;
        }

        Ok(())
    }

    fn write_wasm_stub(tool_dir: &Path, _name: &str) -> Result<(), String> {
        let stub = "(module\n  ;; TODO: implement your tool here\n  ;; See README.md for the implementation guide\n)\n";
        fs::write(tool_dir.join("component.wat"), stub)
            .map_err(|e| format!("failed to write component.wat: {e}"))
    }

    fn write_hook_stub(tool_dir: &Path, name: &str) -> Result<(), String> {
        fs::create_dir_all(tool_dir.join("src"))
            .map_err(|e| format!("failed to create src/: {e}"))?;
        let cargo_name = name.replace('_', "-");
        let cargo_toml = format!(
            "[package]\n\
             name = \"{cargo_name}\"\n\
             version = \"0.3.2\"\n\
             edition = \"2021\"\n\
             \n\
             [lib]\n\
             crate-type = [\"cdylib\", \"rlib\"]\n\
             \n\
             [dependencies]\n\
             wit-bindgen = \"0.46\"\n"
        );
        fs::write(tool_dir.join("Cargo.toml"), cargo_toml)
            .map_err(|e| format!("failed to write Cargo.toml: {e}"))?;

        let lib_rs = r#"#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    pub struct Hook;

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
        ShellEvent, StageEvent, ToolEvent,
    };

    impl Guest for Hook {
        fn on_stage(_: StageEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_session_start(_: SessionContext) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_inference(_: InferenceEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_tool_call(_: ToolEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_shell(_: ShellEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_compaction(_: CompactionEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_session_end(_: SessionEndEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
    }

    export!(Hook);
}
"#;
        fs::write(tool_dir.join("src").join("lib.rs"), lib_rs)
            .map_err(|e| format!("failed to write src/lib.rs: {e}"))
    }

    fn write_readme(tool_dir: &Path, name: &str, runtime: &str) -> Result<(), String> {
        let stub_file = match runtime {
            "native" => "`bin/run`",
            "hook" => "`src/lib.rs`",
            _ => "`component.wat`",
        };
        let hook_note = if runtime == "hook" {
            "This is a hook artifact. It implements `murmur:hook/lifecycle` and receives synchronous lifecycle events from the runtime. Keep handlers fast and return `Ok(HookOutput::None)` unless the event truly could not be recorded.\n\n"
        } else {
            ""
        };
        let content = format!(
            "# {name} — Implementation Guide\n\
             \n\
             Generated by murmur-tool-create 0.3.2. Read this before writing any implementation.\n\
             \n\
             ## What was created\n\
             \n\
             - `murmur.yaml` — pre-filled manifest. Review the `input` and `output` fields and update the schema to match your tool's contract.\n\
             - {stub_file} — executable stub. Replace the stub body with your implementation.\n\
             \n\
             {hook_note}\
             \n\
             ## Implementation checklist\n\
             \n\
             1. **Update `murmur.yaml`** — set `description`, define `input` (JSON schema) and `output` (JSON schema). These are what the agent sees when it calls `describe(\"{name}\")`.\n\
             2. **Implement the entry point** — write to {stub_file}. The stub already has the correct input/output envelope — replace the body only.\n\
             3. **Test the stub** — run the stub with a sample input JSON on stdin. The stub should already exit 0 and emit a valid JSON envelope.\n\
             4. **Invoke the tool** — call it via `murmur:tool-registry/invoke` with the name `{name}`.\n\
             \n\
             ## Input/output contract\n\
             \n\
             **Input (on stdin as JSON):**\n\
             ```json\n\
             {{ \"data\": \"<your JSON payload here>\", \"log_path\": \"<path or null>\" }}\n\
             ```\n\
             \n\
             **Output (to stdout as JSON):**\n\
             ```json\n\
             {{ \"status\": \"passed\", \"summary\": \"<what happened>\", \"data\": \"<result or null>\", \"data_path\": null, \"metadata\": null }}\n\
             ```\n"
        );
        fs::write(tool_dir.join("README.md"), content)
            .map_err(|e| format!("failed to write README.md: {e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tempfile::TempDir;

        #[test]
        fn scaffold_native_creates_expected_files() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "my-tool", "native").unwrap();

            let base = tmp.path().join("tools").join("my-tool");
            assert!(
                base.join("murmur.yaml").exists(),
                "murmur.yaml should exist"
            );
            assert!(
                base.join("bin").join("run").exists(),
                "bin/run should exist"
            );
            assert!(base.join("README.md").exists(), "README.md should exist");

            let manifest = fs::read_to_string(base.join("murmur.yaml")).unwrap();
            assert!(manifest.contains("name: my-tool"));
            assert!(manifest.contains("runtime: native"));
        }

        #[test]
        fn scaffold_wasm_creates_expected_files() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "csv-parser", "wasm").unwrap();

            let base = tmp.path().join("tools").join("csv-parser");
            assert!(base.join("murmur.yaml").exists());
            assert!(base.join("component.wat").exists());
            assert!(base.join("README.md").exists());
        }

        #[test]
        fn scaffold_hook_creates_expected_files() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "event-sink", "hook").unwrap();

            let base = tmp.path().join("tools").join("event-sink");
            assert!(base.join("murmur.yaml").exists());
            assert!(base.join("Cargo.toml").exists());
            assert!(base.join("src").join("lib.rs").exists());
            assert!(base.join("README.md").exists());

            let manifest = fs::read_to_string(base.join("murmur.yaml")).unwrap();
            assert!(manifest.contains("runtime: hook"));
            let source = fs::read_to_string(base.join("src").join("lib.rs")).unwrap();
            assert!(source.contains("on_session_start"));
            assert!(source.contains("on_session_end"));
        }

        #[test]
        fn scaffold_fails_if_directory_exists() {
            let tmp = TempDir::new().unwrap();

            scaffold_tool_in(tmp.path(), "existing", "native").unwrap();
            let err = scaffold_tool_in(tmp.path(), "existing", "native").unwrap_err();
            assert!(err.contains("already exists"), "got: {err}");
        }

        #[test]
        fn scaffold_fails_on_unknown_runtime() {
            let tmp = TempDir::new().unwrap();

            let err = scaffold_tool_in(tmp.path(), "bad-tool", "quantum").unwrap_err();
            assert!(err.contains("unknown runtime"), "got: {err}");
        }

        #[test]
        fn handle_request_scaffolds_from_raw_request() {
            let tmp = TempDir::new().unwrap();
            let payload = r#"{"type":"tool","name":"from-req","runtime":"wasm"}"#;
            let (name, path) = handle_request(Some(payload), tmp.path()).unwrap();
            assert_eq!(name, "from-req");
            assert_eq!(path, "tools/from-req");
            assert!(tmp
                .path()
                .join("tools")
                .join("from-req")
                .join("component.wat")
                .exists());
        }

        #[test]
        fn handle_request_missing_name_errors() {
            let tmp = TempDir::new().unwrap();
            let err = handle_request(Some(r#"{"type":"tool"}"#), tmp.path()).unwrap_err();
            assert!(err.contains("missing 'name'"), "got: {err}");
        }

        #[test]
        fn handle_request_none_data_errors() {
            let tmp = TempDir::new().unwrap();
            let err = handle_request(None, tmp.path()).unwrap_err();
            assert!(err.contains("no input provided"), "got: {err}");
        }
    }
}

// ── WASM adapter: WIT bindings + request/result mapping (wasm32 only) ──────────
#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::Path;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // `input.data` carries the scaffold request the agent passed. Scaffold under the
            // preopened workdir root (".") — the same CWD the native binary used.
            match crate::logic::handle_request(input.data.as_deref(), Path::new(".")) {
                Ok((name, path)) => ToolResult {
                    status: Status::Passed,
                    summary: Some(format!("Created tools/{name}/")),
                    data: Some(format!("{{\"path\":\"{path}\"}}")),
                    data_path: None,
                    truncated: false,
                    metadata: Vec::new(),
                },
                Err(e) => ToolResult {
                    status: Status::Error,
                    summary: Some(format!("scaffold failed: {e}")),
                    data: None,
                    data_path: None,
                    truncated: false,
                    metadata: Vec::new(),
                },
            }
        }
    }

    export!(Component);
}
