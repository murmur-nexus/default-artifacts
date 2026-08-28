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

    /// The `version:` every generated manifest declares. A freshly scaffolded artifact starts
    /// its own version line at `0.1.0`; it is unrelated to this generator's version, which the
    /// README records separately from `CARGO_PKG_VERSION`.
    const SCAFFOLD_VERSION: &str = "0.1.0";

    /// The schema pair the tool arms declare. A hook omits both — it is dispatched with a
    /// lifecycle event, never with a tool payload.
    const TOOL_SCHEMAS: &str = concat!(
        "input_schema: |\n",
        "  {\"type\":\"object\",\"properties\":{}}\n",
        "output_schema: |\n",
        "  {\"type\":\"object\",\"properties\":{}}\n",
    );

    /// What a scaffold request's `runtime` field selects.
    ///
    /// The request's vocabulary is not the manifest's. `mur` reads an artifact's *role* from
    /// `runtime:` and its *packaging* from `implementation:`, and derives the published
    /// classification from the pair (`Manifest::registry_runtime`). Writing the request word
    /// straight into `runtime:` collapses the two: `runtime: native` publishes as wasm and is
    /// rejected outright when the artifact is named from a capsule manifest. Each arm here
    /// spells the pair back out, along with the payload entry `mur build` has to be told to
    /// pack — an undeclared `requires_files:` means an archive holding the manifest alone.
    #[derive(Clone, Copy)]
    enum Kind {
        Wasm,
        Native,
        Hook,
    }

    impl Kind {
        fn parse(runtime: &str) -> Result<Self, String> {
            match runtime {
                "native" => Ok(Self::Native),
                "wasm" => Ok(Self::Wasm),
                "hook" => Ok(Self::Hook),
                other => Err(format!(
                    "unknown runtime '{other}'; expected 'native', 'wasm', or 'hook'"
                )),
            }
        }

        /// The generated manifest's `runtime:` value — the artifact's role.
        fn role(self) -> &'static str {
            match self {
                Self::Wasm | Self::Native => "tool",
                Self::Hook => "hook",
            }
        }

        /// The generated manifest's `implementation:` value — how the payload is built.
        fn implementation(self) -> &'static str {
            match self {
                Self::Native => "native",
                Self::Wasm | Self::Hook => "wasm",
            }
        }

        /// The sole `requires_files:` entry, which is the payload `mur build` packs beside
        /// `murmur.yaml`.
        ///
        /// A native payload is `bin/<artifact-name>` because that is the only path the capsule
        /// runtime resolves a native binary at (`payload_shape::native_binary_entry`). A wasm
        /// payload is the cdylib filename cargo produces: the artifact name with `-` replaced
        /// by `_`, plus `.wasm`.
        fn requires_file(self, name: &str) -> String {
            match self {
                Self::Native => format!("bin/{name}"),
                Self::Wasm | Self::Hook => format!("{}.wasm", name.replace('-', "_")),
            }
        }
    }

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

        // Resolved before anything is created, so an unrecognised runtime leaves no
        // half-scaffolded directory behind for the author to clean up.
        let kind = Kind::parse(runtime)?;

        let tool_dir = base_dir.join(PathBuf::from("tools").join(name));

        if tool_dir.exists() {
            return Err(format!(
                "tools/{name} already exists; remove it before scaffolding"
            ));
        }

        fs::create_dir_all(&tool_dir)
            .map_err(|e| format!("failed to create {}: {e}", tool_dir.display()))?;

        write_manifest(&tool_dir, name, kind)?;

        match kind {
            Kind::Native => write_native_stub(&tool_dir, name)?,
            Kind::Wasm => write_wasm_stub(&tool_dir, name)?,
            Kind::Hook => write_hook_stub(&tool_dir, name)?,
        }

        write_readme(&tool_dir, name, kind)?;

        Ok(())
    }

    fn write_manifest(tool_dir: &Path, name: &str, kind: Kind) -> Result<(), String> {
        let role = kind.role();
        let implementation = kind.implementation();
        let requires_file = kind.requires_file(name);

        // The dispatch fields a hook manifest carries and a tool manifest does not, modelled on
        // `hooks/murmur-hook-debug/murmur.yaml`.
        let hook_keys = match kind {
            Kind::Hook => "execution_mode: async\ncommit_policy: none\n",
            Kind::Wasm | Kind::Native => "",
        };
        let schemas = match kind {
            Kind::Hook => "",
            Kind::Wasm | Kind::Native => TOOL_SCHEMAS,
        };

        let content = format!(
            "name: {name}\n\
             version: {SCAFFOLD_VERSION}\n\
             runtime: {role}\n\
             implementation: {implementation}\n\
             {hook_keys}\
             description: |\n\
             \x20 TODO: describe what {name} does\n\
             {schemas}\
             requires_files:\n\
             \x20 - {requires_file}\n"
        );
        fs::write(tool_dir.join("murmur.yaml"), content)
            .map_err(|e| format!("failed to write murmur.yaml: {e}"))
    }

    /// Writes the native payload to `bin/<name>` — the one path
    /// `payload_shape::native_binary_entry` resolves, and the path the artifact's own
    /// `package.sh` stages the compiled binary at.
    fn write_native_stub(tool_dir: &Path, name: &str) -> Result<(), String> {
        let bin_dir = tool_dir.join("bin");
        fs::create_dir_all(&bin_dir).map_err(|e| format!("failed to create bin/: {e}"))?;

        let stub = "#!/bin/sh\n\
                    # Read ToolInput JSON from stdin, write ToolResult JSON to stdout.\n\
                    # Replace this stub body with your implementation.\n\
                    INPUT=$(cat)\n\
                    echo '{\"status\":\"passed\",\"summary\":\"stub: not yet implemented\",\"data\":null,\"data_path\":null,\"truncated\":false,\"metadata\":[]}'\n";

        let run_path = bin_dir.join(name);
        fs::write(&run_path, stub).map_err(|e| format!("failed to write bin/{name}: {e}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&run_path)
                .map_err(|e| format!("failed to read bin/{name} metadata: {e}"))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&run_path, perms)
                .map_err(|e| format!("failed to chmod bin/{name}: {e}"))?;
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
        // `wit-bindgen` tracks this workspace's `[workspace.dependencies]` pin: the generated
        // crate is built against the same vendored `wit/hook` this repo's own hooks are.
        let cargo_toml = format!(
            "[package]\n\
             name = \"{cargo_name}\"\n\
             version = \"{SCAFFOLD_VERSION}\"\n\
             edition = \"2021\"\n\
             \n\
             [lib]\n\
             crate-type = [\"cdylib\", \"rlib\"]\n\
             \n\
             [dependencies]\n\
             wit-bindgen = \"0.59\"\n"
        );
        fs::write(tool_dir.join("Cargo.toml"), cargo_toml)
            .map_err(|e| format!("failed to write Cargo.toml: {e}"))?;

        // All nine `murmur:hook/lifecycle` functions. `Guest` has no defaulted methods, so a
        // stub missing any one of them does not compile.
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
        ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    };

    impl Guest for Hook {
        fn on_stage(_: StageEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_session_start(_: SessionContext) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_task_start(_: TaskStartEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_inference(_: InferenceEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_tool_call(_: ToolEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_shell(_: ShellEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_compaction(_: CompactionEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_task_end(_: TaskEndEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
        fn on_session_end(_: SessionEndEvent) -> Result<HookOutput, String> { Ok(HookOutput::None) }
    }

    export!(Hook);
}
"#;
        fs::write(tool_dir.join("src").join("lib.rs"), lib_rs)
            .map_err(|e| format!("failed to write src/lib.rs: {e}"))
    }

    fn write_readme(tool_dir: &Path, name: &str, kind: Kind) -> Result<(), String> {
        let stub_file = match kind {
            Kind::Native => format!("`bin/{name}`"),
            Kind::Hook => "`src/lib.rs`".to_string(),
            Kind::Wasm => "`component.wat`".to_string(),
        };
        let hook_note = match kind {
            Kind::Hook => "This is a hook artifact. It implements `murmur:hook/lifecycle` and receives synchronous lifecycle events from the runtime. Keep handlers fast and return `Ok(HookOutput::None)` unless the event truly could not be recorded.\n\n",
            Kind::Wasm | Kind::Native => "",
        };
        // The generator's own version, read from the crate rather than written as a literal:
        // `scripts/apply-versions.sh` rewrites manifests and `Cargo.toml`s, and cannot see
        // inside a Rust string.
        let generator_version = env!("CARGO_PKG_VERSION");
        let content = format!(
            "# {name} — Implementation Guide\n\
             \n\
             Generated by murmur-tool-create {generator_version}\n\
             \n\
             Read this before writing any implementation.\n\
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
            assert!(base.join("README.md").exists(), "README.md should exist");

            // `bin/<name>`, not `bin/run`: `payload_shape::native_binary_entry` resolves a
            // native payload at the artifact's own name and at nothing else.
            let payload = base.join("bin").join("my-tool");
            assert!(payload.exists(), "bin/my-tool should exist");
            assert!(
                !base.join("bin").join("run").exists(),
                "bin/run should no longer be written"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&payload).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o755, "bin/my-tool should be mode 0755");
            }

            let manifest = fs::read_to_string(base.join("murmur.yaml")).unwrap();
            assert!(manifest.contains("name: my-tool"));
            assert!(manifest.contains("version: 0.1.0"), "got: {manifest}");
            assert!(manifest.contains("runtime: tool"), "got: {manifest}");
            assert!(
                manifest.contains("implementation: native"),
                "got: {manifest}"
            );
            assert!(
                manifest.contains("requires_files:\n  - bin/my-tool\n"),
                "got: {manifest}"
            );
            // The defect this arm existed to carry: the request word written straight into
            // `runtime:`, which publishes the artifact as wasm.
            assert!(!manifest.contains("runtime: native"), "got: {manifest}");
        }

        #[test]
        fn scaffold_wasm_creates_expected_files() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "csv-parser", "wasm").unwrap();

            let base = tmp.path().join("tools").join("csv-parser");
            assert!(base.join("murmur.yaml").exists());
            assert!(base.join("component.wat").exists());
            assert!(base.join("README.md").exists());

            let manifest = fs::read_to_string(base.join("murmur.yaml")).unwrap();
            assert!(manifest.contains("name: csv-parser"));
            assert!(manifest.contains("version: 0.1.0"), "got: {manifest}");
            assert!(manifest.contains("runtime: tool"), "got: {manifest}");
            assert!(manifest.contains("implementation: wasm"), "got: {manifest}");
            assert!(
                manifest.contains("requires_files:\n  - csv_parser.wasm\n"),
                "got: {manifest}"
            );
            assert!(!manifest.contains("runtime: wasm"), "got: {manifest}");
            // The one version line is the scaffolded artifact's own starting version. The
            // generator used to stamp a literal of its own here, which belonged to neither.
            assert_eq!(
                manifest
                    .lines()
                    .filter(|line| line.starts_with("version:"))
                    .collect::<Vec<_>>(),
                ["version: 0.1.0"],
                "got: {manifest}"
            );
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
            assert!(manifest.contains("version: 0.1.0"), "got: {manifest}");
            assert!(manifest.contains("runtime: hook"), "got: {manifest}");
            assert!(manifest.contains("implementation: wasm"), "got: {manifest}");
            assert!(manifest.contains("execution_mode: async"), "got: {manifest}");
            assert!(manifest.contains("commit_policy: none"), "got: {manifest}");
            assert!(
                manifest.contains("requires_files:\n  - event_sink.wasm\n"),
                "got: {manifest}"
            );
            // A hook is dispatched with a lifecycle event, never a tool payload.
            assert!(!manifest.contains("input_schema"), "got: {manifest}");
            assert!(!manifest.contains("output_schema"), "got: {manifest}");

            let cargo_toml = fs::read_to_string(base.join("Cargo.toml")).unwrap();
            assert!(cargo_toml.contains("version = \"0.1.0\""), "got: {cargo_toml}");
            assert!(
                cargo_toml.contains("wit-bindgen = \"0.59\""),
                "got: {cargo_toml}"
            );

            // `Guest` defaults none of its nine methods; a stub short of any one of them is a
            // scaffold that does not compile.
            let source = fs::read_to_string(base.join("src").join("lib.rs")).unwrap();
            for func in [
                "on_stage",
                "on_session_start",
                "on_task_start",
                "on_inference",
                "on_tool_call",
                "on_shell",
                "on_compaction",
                "on_task_end",
                "on_session_end",
            ] {
                assert!(source.contains(func), "generated hook is missing {func}");
            }
        }

        #[test]
        fn readme_attributes_the_generators_own_version() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "attributed", "wasm").unwrap();

            let readme =
                fs::read_to_string(tmp.path().join("tools").join("attributed").join("README.md"))
                    .unwrap();
            let expected = format!("Generated by murmur-tool-create {}", env!("CARGO_PKG_VERSION"));
            assert!(
                readme.lines().any(|line| line == expected),
                "README should carry the attribution line {expected:?}; got:\n{readme}"
            );
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
            assert!(
                !tmp.path().join("tools").join("bad-tool").exists(),
                "a rejected runtime should leave no directory behind"
            );
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
        fn handle_request_scaffolds_from_double_encoded_envelope() {
            let tmp = TempDir::new().unwrap();
            let payload =
                r#"{"data":"{\"type\":\"tool\",\"name\":\"enveloped\",\"runtime\":\"native\"}"}"#;
            let (name, path) = handle_request(Some(payload), tmp.path()).unwrap();
            assert_eq!(name, "enveloped");
            assert_eq!(path, "tools/enveloped");
            assert!(tmp
                .path()
                .join("tools")
                .join("enveloped")
                .join("bin")
                .join("enveloped")
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
