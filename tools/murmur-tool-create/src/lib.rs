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

        // Only the host build reaches this. `wasm32-wasip2` is not `cfg(unix)` and WASI has no
        // chmod, so the shipped component leaves the stub at the umask default and `mur build`
        // packs whatever mode it finds. The generated README carries the `chmod +x` step for
        // that reason; the capsule runtime re-applies 0755 when it extracts the payload, so a
        // non-executable archive entry costs the author a local run rather than a broken install.
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
             wit-bindgen = \"0.59\"\n\
             \n\
             # Host-only. `cargo test` builds these for your native host; none of them reach\n\
             # the wasm32-wasip2 component, whose sole dependency stays `wit-bindgen`.\n\
             # `tempfile` is unused by the generated test on purpose: the usual hook side\n\
             # effect is appending a line to a file under the workdir, and a host test for\n\
             # that needs a scratch directory. The section existing with a working entry is\n\
             # what makes writing that second test free.\n\
             [dev-dependencies]\n\
             tempfile = \"3\"\n"
        );
        fs::write(tool_dir.join("Cargo.toml"), cargo_toml)
            .map_err(|e| format!("failed to write Cargo.toml: {e}"))?;

        // The three-layer split the repo's hooks use: a `cfg`-independent `logic` module
        // holding every decision, a `wasm32`-gated adapter that converts and nothing else,
        // and one host test. A crate written entirely behind the gate compiles to nothing
        // for the host target, so `cargo test` runs none of it and reports green anyway —
        // the scaffold hands the author the shape that cannot do that.
        let lib_rs = r#"//! A `murmur:hook/lifecycle@0.8.0` hook, split so its decision logic is testable on the
//! host from the moment it is generated.
//!
//! Three layers, in the order they appear below:
//!
//! - `logic` — plain Rust mirrors of the WIT records plus [`logic::decide`], the single
//!   entry point every lifecycle dispatch routes through. No `cfg`, no `wit_bindgen`, no
//!   `exports::` types. Your hook's behaviour goes here.
//! - `wasm_hook` — the wasm32-only adapter. It converts a WIT record into a
//!   [`logic::Event`], calls [`logic::decide`], and maps the returned [`logic::Decision`]
//!   back onto a `HookOutput`. It holds no branch, threshold or string the runtime acts
//!   on.
//! - `tests` — host tests over `logic`.
//!
//! The split exists because code behind a `cfg(target_arch = "wasm32")` gate does not
//! exist for the host target. A crate whose every item sits behind that gate compiles to
//! nothing when `cargo test` builds it for the host: the run is green having executed
//! none of the crate's lines, and no CI gate distinguishes that from a covered crate.
//! Keeping the logic outside the gate is what lets the tests at the bottom of this file
//! run at all.
//!
//! Reference implementations in murmur's default-artifacts repository:
//! `hooks/murmur-hook-compact`, `hooks/murmur-hook-memory` and
//! `hooks/murmur-hook-regression-verifier`.

// ── Pure, host-testable hook logic (no WIT types, no `cfg`) ───────────────────
pub mod logic {
    // Every mirror carries the same three derives. `compaction-event.threshold` is an
    // `f64`, so `Eq` and `Hash` are not available to it, and a per-type derive list is a
    // trap for whoever later moves a float into another record.

    /// Mirror of `murmur:hook/lifecycle`'s `message`.
    ///
    /// `id` and `source-id` are absent on a message you mint; the runtime mints an `id`
    /// for any message that arrives without one.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Message {
        pub role: String,
        pub content: String,
        pub id: Option<String>,
        pub source_id: Option<String>,
    }

    /// Mirror of `tool-manifest` — a shell-tool description returned by
    /// [`Decision::WriteManifests`].
    #[derive(Clone, Debug, PartialEq)]
    pub struct ToolManifest {
        pub binary_name: String,
        pub content: String,
    }

    /// Mirror of `session-context`, the payload of `on-session-start`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct SessionContext {
        pub capsule_name: String,
        pub capsule_version: String,
        pub session_id: String,
        pub model: String,
        pub capabilities: Vec<String>,
    }

    /// Mirror of `stage-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct StageEvent {
        pub shell_allow: Vec<String>,
    }

    /// Mirror of `task-start-event`.
    ///
    /// Read `budget_tokens`, `context_window` and `prior_tokens` of `0` as "the host has
    /// not computed this" and decline, never as an unbounded budget.
    #[derive(Clone, Debug, PartialEq)]
    pub struct TaskStartEvent {
        pub task_id: String,
        pub context_id: String,
        pub source: String,
        pub input_bytes: u64,
        pub budget_tokens: u64,
        pub context_window: u64,
        pub prior_tokens: u64,
    }

    /// Mirror of `task-end-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct TaskEndEvent {
        pub task_id: String,
        pub exit_status: String,
    }

    /// Mirror of `inference-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct InferenceEvent {
        pub turn: u32,
        pub input_tokens: u64,
        pub output_tokens: u64,
        pub decision: String,
        pub tool_name: Option<String>,
        pub prompt: Option<String>,
        pub output: Option<String>,
        pub tools: Option<String>,
    }

    /// Mirror of `tool-outcome` — what a completed tool call produced.
    #[derive(Clone, Debug, PartialEq)]
    pub struct ToolOutcome {
        pub output_bytes: u64,
        pub duration_ms: u64,
        pub status: String,
    }

    /// Mirror of `tool-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct ToolEvent {
        pub turn: u32,
        pub tool_name: String,
        pub input_bytes: u64,
        /// The exact tool input JSON the tool will receive, never truncated. Decide on
        /// this field and never on a summary of it.
        pub input: String,
        /// `None` means this call has **not** run. That is the decision point, the one
        /// dispatch at which a returned [`Decision::Deny`] is honoured. `Some(..)` is the
        /// post-call observation: the call already happened and nothing can be refused.
        ///
        /// Both dispatches reach this hook for every call, so logic that ignores this
        /// field runs twice per call.
        pub outcome: Option<ToolOutcome>,
    }

    /// Mirror of `shell-outcome` — what a completed shell call produced.
    #[derive(Clone, Debug, PartialEq)]
    pub struct ShellOutcome {
        pub exit_code: i32,
        pub stdout: String,
        pub stderr: String,
        pub stdout_bytes: u64,
        pub stderr_bytes: u64,
        pub duration_ms: u64,
    }

    /// Mirror of `shell-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct ShellEvent {
        pub turn: u32,
        /// The program that was actually invoked, resolved to an absolute path when the
        /// host could resolve it. Never empty.
        pub binary: String,
        /// Display string only, truncated to 200 characters. A policy decides on `argv`
        /// and `script`, never on this.
        pub command: String,
        /// The exact argument list the runtime will pass, never truncated.
        pub argv: Vec<String>,
        /// The `-c` body when the interpreter form is used, `None` for every other form.
        pub script: Option<String>,
        /// `None` means this call has **not** run. That is the decision point, the one
        /// dispatch at which a returned [`Decision::Deny`] is honoured. `Some(..)` is the
        /// post-call observation: the call already happened and nothing can be refused.
        ///
        /// Both dispatches reach this hook for every call, so logic that ignores this
        /// field runs twice per call.
        pub outcome: Option<ShellOutcome>,
    }

    /// Mirror of `compaction-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct CompactionEvent {
        pub messages: Vec<Message>,
        pub session_tokens: u64,
        pub threshold: f64,
        pub model: Option<String>,
        pub system_prompt: Option<String>,
    }

    /// Mirror of `session-end-event`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct SessionEndEvent {
        pub total_turns: u32,
        pub total_input_tokens: u64,
        pub total_output_tokens: u64,
        pub total_tool_calls: u32,
        pub total_shell_calls: u32,
        pub duration_ms: u64,
        pub exit_status: String,
    }

    /// One variant per `murmur:hook/lifecycle` function, so all nine dispatches can route
    /// through a single [`decide`] and a test can construct any of them without a WIT
    /// binding.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Event {
        Stage(StageEvent),
        SessionStart(SessionContext),
        TaskStart(TaskStartEvent),
        Inference(InferenceEvent),
        ToolCall(ToolEvent),
        Shell(ShellEvent),
        Compaction(CompactionEvent),
        TaskEnd(TaskEndEvent),
        SessionEnd(SessionEndEvent),
    }

    /// Mirror of the `hook-output` variant. The adapter maps each case onto the WIT case
    /// of the same name.
    ///
    /// Most cases are honoured at one dispatch only; returning one elsewhere is a
    /// non-fatal dispatch fault the runtime logs and ignores. `ReopenTask` is honoured at
    /// `TaskEnd`, `SeedContext` at `TaskStart`, `ReplaceContext` at `Compaction`.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Decision {
        None,
        ReplaceContext(Vec<Message>),
        WriteManifests(Vec<ToolManifest>),
        Artifact(String),
        ReopenTask(String),
        SeedContext(Vec<Message>),
        /// Refuse the call this event is about to make. Honoured only at the
        /// decision-point dispatch of [`Event::ToolCall`] and [`Event::Shell`] — the
        /// dispatch whose `outcome` is `None`. Returned from any other event, or from the
        /// post-call observation dispatch of those two, nothing is refused.
        ///
        /// The string is the reason the agent is shown, and must be non-empty.
        Deny(String),
    }

    /// The hook's single entry point. Every lifecycle dispatch converts its WIT record
    /// into an [`Event`] and calls this.
    ///
    /// `Result<Decision, String>` mirrors the WIT `result<hook-output, string>`: an `Err`
    /// is a hook failure the runtime records, not a refusal. To refuse a call, return
    /// `Ok(Decision::Deny(reason))` at a decision-point dispatch.
    pub fn decide(event: &Event) -> Result<Decision, String> {
        match event {
            // TODO: split this arm and implement the events your hook acts on. Everything
            // the runtime observes is decided here; the adapter below only converts.
            Event::Stage(_)
            | Event::SessionStart(_)
            | Event::TaskStart(_)
            | Event::Inference(_)
            | Event::ToolCall(_)
            | Event::Shell(_)
            | Event::Compaction(_)
            | Event::TaskEnd(_)
            | Event::SessionEnd(_) => Ok(Decision::None),
        }
    }
}

// ── WASM adapter: WIT bindings ↔ pure logic (wasm32 only) ─────────────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use crate::logic::{self, Event};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    pub struct Hook;

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, Message, SessionContext,
        SessionEndEvent, ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
        ToolManifest,
    };

    /// All nine `murmur:hook/lifecycle` functions. `Guest` defaults none of them, so a
    /// stub short of any one does not compile.
    impl Guest for Hook {
        fn on_stage(event: StageEvent) -> Result<HookOutput, String> {
            dispatch(Event::Stage(logic::StageEvent {
                shell_allow: event.shell_allow,
            }))
        }

        fn on_session_start(ctx: SessionContext) -> Result<HookOutput, String> {
            dispatch(Event::SessionStart(logic::SessionContext {
                capsule_name: ctx.capsule_name,
                capsule_version: ctx.capsule_version,
                session_id: ctx.session_id,
                model: ctx.model,
                capabilities: ctx.capabilities,
            }))
        }

        fn on_task_start(event: TaskStartEvent) -> Result<HookOutput, String> {
            dispatch(Event::TaskStart(logic::TaskStartEvent {
                task_id: event.task_id,
                context_id: event.context_id,
                source: event.source,
                input_bytes: event.input_bytes,
                budget_tokens: event.budget_tokens,
                context_window: event.context_window,
                prior_tokens: event.prior_tokens,
            }))
        }

        fn on_inference(event: InferenceEvent) -> Result<HookOutput, String> {
            dispatch(Event::Inference(logic::InferenceEvent {
                turn: event.turn,
                input_tokens: event.input_tokens,
                output_tokens: event.output_tokens,
                decision: event.decision,
                tool_name: event.tool_name,
                prompt: event.prompt,
                output: event.output,
                tools: event.tools,
            }))
        }

        fn on_tool_call(event: ToolEvent) -> Result<HookOutput, String> {
            dispatch(Event::ToolCall(logic::ToolEvent {
                turn: event.turn,
                tool_name: event.tool_name,
                input_bytes: event.input_bytes,
                input: event.input,
                outcome: event.outcome.map(|o| logic::ToolOutcome {
                    output_bytes: o.output_bytes,
                    duration_ms: o.duration_ms,
                    status: o.status,
                }),
            }))
        }

        fn on_shell(event: ShellEvent) -> Result<HookOutput, String> {
            dispatch(Event::Shell(logic::ShellEvent {
                turn: event.turn,
                binary: event.binary,
                command: event.command,
                argv: event.argv,
                script: event.script,
                outcome: event.outcome.map(|o| logic::ShellOutcome {
                    exit_code: o.exit_code,
                    stdout: o.stdout,
                    stderr: o.stderr,
                    stdout_bytes: o.stdout_bytes,
                    stderr_bytes: o.stderr_bytes,
                    duration_ms: o.duration_ms,
                }),
            }))
        }

        fn on_compaction(event: CompactionEvent) -> Result<HookOutput, String> {
            dispatch(Event::Compaction(logic::CompactionEvent {
                messages: event.messages.into_iter().map(into_logic_message).collect(),
                session_tokens: event.session_tokens,
                threshold: event.threshold,
                model: event.model,
                system_prompt: event.system_prompt,
            }))
        }

        fn on_task_end(event: TaskEndEvent) -> Result<HookOutput, String> {
            dispatch(Event::TaskEnd(logic::TaskEndEvent {
                task_id: event.task_id,
                exit_status: event.exit_status,
            }))
        }

        fn on_session_end(event: SessionEndEvent) -> Result<HookOutput, String> {
            dispatch(Event::SessionEnd(logic::SessionEndEvent {
                total_turns: event.total_turns,
                total_input_tokens: event.total_input_tokens,
                total_output_tokens: event.total_output_tokens,
                total_tool_calls: event.total_tool_calls,
                total_shell_calls: event.total_shell_calls,
                duration_ms: event.duration_ms,
                exit_status: event.exit_status,
            }))
        }
    }

    /// The one path from a lifecycle dispatch to a `HookOutput`. Nothing in this module
    /// decides anything; it converts, calls `logic::decide`, and converts back.
    fn dispatch(event: Event) -> Result<HookOutput, String> {
        logic::decide(&event).map(into_hook_output)
    }

    fn into_logic_message(m: Message) -> logic::Message {
        logic::Message {
            role: m.role,
            content: m.content,
            id: m.id,
            source_id: m.source_id,
        }
    }

    fn into_wit_message(m: logic::Message) -> Message {
        Message {
            role: m.role,
            content: m.content,
            id: m.id,
            source_id: m.source_id,
        }
    }

    fn into_wit_manifest(m: logic::ToolManifest) -> ToolManifest {
        ToolManifest {
            binary_name: m.binary_name,
            content: m.content,
        }
    }

    fn into_hook_output(decision: logic::Decision) -> HookOutput {
        match decision {
            logic::Decision::None => HookOutput::None,
            logic::Decision::ReplaceContext(messages) => {
                HookOutput::ReplaceContext(messages.into_iter().map(into_wit_message).collect())
            }
            logic::Decision::WriteManifests(manifests) => {
                HookOutput::WriteManifests(manifests.into_iter().map(into_wit_manifest).collect())
            }
            logic::Decision::Artifact(json) => HookOutput::Artifact(json),
            logic::Decision::ReopenTask(reason) => HookOutput::ReopenTask(reason),
            logic::Decision::SeedContext(messages) => {
                HookOutput::SeedContext(messages.into_iter().map(into_wit_message).collect())
            }
            logic::Decision::Deny(reason) => HookOutput::Deny(reason),
        }
    }

    export!(Hook);
}

// ── Host tests over `logic` ───────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::logic::{decide, Decision, Event, ShellEvent};

    /// One test so the crate has a test before it has behaviour: `cargo test` on a native
    /// host compiles and runs this crate's code instead of an empty crate. Deleting it is
    /// then a deliberate choice visible in a diff, rather than the shape the scaffold
    /// handed you.
    ///
    /// Add your cases beside this one. `logic` is plain Rust, so a case is a struct
    /// literal and an `assert_eq!` — no Wasmtime, no WIT bindings, no capsule.
    #[test]
    fn shell_decision_point_is_not_denied_by_default() {
        // `outcome: None` is the decision point: the command has not run, and this is the
        // one dispatch at which a returned `Decision::Deny` would be honoured.
        let event = Event::Shell(ShellEvent {
            turn: 1,
            binary: "/bin/echo".to_string(),
            command: "hello".to_string(),
            argv: vec!["hello".to_string()],
            script: None,
            outcome: None,
        });

        assert_eq!(decide(&event), Ok(Decision::None));
    }
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
            Kind::Hook => "This is a hook artifact. It implements `murmur:hook/lifecycle` and receives synchronous lifecycle events from the runtime. Keep `logic::decide` fast and return `Ok(Decision::None)` unless the event truly could not be recorded.\n\n",
            Kind::Wasm | Kind::Native => "",
        };
        // `mur build` packs the payload with the mode it has on disk, and a scaffold written by
        // the wasm component arrives without the executable bit — WASI cannot set one.
        let exec_note = match kind {
            Kind::Native => format!(
                "\nThe payload must be executable before you build: `chmod +x bin/{name}`.\n"
            ),
            Kind::Wasm | Kind::Hook => String::new(),
        };
        let cargo_name = name.replace('_', "-");
        // Only the hook arm emits a Rust crate, so only the hook arm has a `cargo test` to
        // point at. The `/bin/sh` stub and the `component.wat` stub are exercised by feeding
        // them an input envelope, which is what their step 3 keeps saying.
        let step_two = match kind {
            Kind::Hook => "**Implement the decision logic** — write to the `logic` module in `src/lib.rs`. `logic::decide` is the single entry point all nine lifecycle dispatches route through, and it is plain Rust: no `wit_bindgen`, no `exports::` types, nothing behind a `cfg`. The `wasm_hook` adapter below it converts a WIT record into a `logic::Event` and a returned `logic::Decision` back into a `HookOutput`, and must keep holding no branch, threshold or string the runtime acts on.".to_string(),
            Kind::Wasm | Kind::Native => format!(
                "**Implement the entry point** — write to {stub_file}. The stub already has the correct input/output envelope — replace the body only."
            ),
        };
        let step_three = match kind {
            Kind::Hook => format!(
                "**Test the logic** — run `cargo test -p {cargo_name}`. The generated `#[cfg(test)] mod tests` block already carries one passing test, so the crate is covered before it has behaviour; add each new case beside it in that block."
            ),
            Kind::Wasm | Kind::Native => "**Test the stub** — run the stub with a sample input JSON on stdin. The stub should already exit 0 and emit a valid JSON envelope.".to_string(),
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
             - {stub_file} — stub implementation. Replace the stub body with your implementation.\n\
             {exec_note}\
             \n\
             {hook_note}\
             \n\
             ## Implementation checklist\n\
             \n\
             1. **Update `murmur.yaml`** — set `description`, define `input` (JSON schema) and `output` (JSON schema). These are what the agent sees when it calls `describe(\"{name}\")`.\n\
             2. {step_two}\n\
             3. {step_three}\n\
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

        /// The defect this arm shipped with: a generated crate whose every item sat behind
        /// `#[cfg(target_arch = "wasm32")]`. Built for the host — which is what
        /// `cargo test` does — such a crate compiles to nothing, so its test run is green
        /// having executed none of its lines.
        ///
        /// The gate attribute is counted in its full `#[cfg(...)]` form rather than as the
        /// bare `cfg(target_arch = "wasm32")` predicate, because the generated crate doc
        /// comment names the predicate in prose. One gate, and it opens after `pub mod
        /// logic` has already closed.
        #[test]
        fn scaffolded_hook_is_testable_on_the_host() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "event-sink", "hook").unwrap();
            let base = tmp.path().join("tools").join("event-sink");

            let source = fs::read_to_string(base.join("src").join("lib.rs")).unwrap();

            let logic_at = source
                .find("pub mod logic")
                .unwrap_or_else(|| panic!("generated hook has no `pub mod logic`; got:\n{source}"));
            assert!(
                source.contains("#[cfg(test)]"),
                "generated hook has no `#[cfg(test)]` block; got:\n{source}"
            );
            assert!(
                source.contains("#[test]"),
                "generated hook has no `#[test]`; got:\n{source}"
            );

            const GATE: &str = "#[cfg(target_arch = \"wasm32\")]";
            let gates: Vec<usize> = source.match_indices(GATE).map(|(at, _)| at).collect();
            assert_eq!(
                gates.len(),
                1,
                "generated hook should carry exactly one {GATE}; got {gates:?} in:\n{source}"
            );
            assert!(
                gates[0] > logic_at,
                "the wasm gate opens at {} but `pub mod logic` starts at {logic_at}; the logic \
                 must sit outside the gate or the host build sees an empty crate",
                gates[0]
            );

            // The adapter routes to the logic rather than carrying a second copy of it.
            assert!(
                source.contains("logic::decide"),
                "generated adapter should call `logic::decide`; got:\n{source}"
            );

            let cargo_toml = fs::read_to_string(base.join("Cargo.toml")).unwrap();
            assert!(
                cargo_toml.contains("[dev-dependencies]"),
                "generated Cargo.toml should declare host-only test dependencies; got:\n{cargo_toml}"
            );
            assert!(
                cargo_toml.contains("tempfile = \"3\""),
                "got:\n{cargo_toml}"
            );
        }

        /// The `logic` module is what makes the generated crate host-testable, so it must
        /// name no WIT type and no binding generator. A single `exports::` or
        /// `wit_bindgen::` reference above the gate breaks the host build outright.
        #[test]
        fn scaffolded_hook_logic_names_no_wit_types() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "pure-logic", "hook").unwrap();
            let source =
                fs::read_to_string(tmp.path().join("tools/pure-logic/src/lib.rs")).unwrap();

            // Sliced from `pub mod logic` rather than from the top of the file: the crate
            // doc comment names these in prose, telling the author to keep them out.
            let logic_at = source.find("pub mod logic").unwrap();
            let gate_at = source.find("#[cfg(target_arch = \"wasm32\")]").unwrap();
            let logic = &source[logic_at..gate_at];
            for forbidden in ["wit_bindgen", "exports::", "HookOutput", "#[cfg("] {
                assert!(
                    !logic.contains(forbidden),
                    "`{forbidden}` appears inside the generated `logic` module:\n{logic}"
                );
            }
        }

        /// Only the hook arm emits a Rust crate, so only its checklist can point at a
        /// `cargo test`. The other two arms are exercised by feeding a stub an envelope,
        /// and their wording must not drift onto a command they have no crate for.
        #[test]
        fn hook_readme_points_at_cargo_test_and_the_others_do_not() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "event_sink", "hook").unwrap();
            scaffold_tool_in(tmp.path(), "my-tool", "native").unwrap();
            scaffold_tool_in(tmp.path(), "csv-parser", "wasm").unwrap();

            let step_three = |artifact: &str| -> String {
                let readme =
                    fs::read_to_string(tmp.path().join("tools").join(artifact).join("README.md"))
                        .unwrap();
                readme
                    .lines()
                    .find(|line| line.starts_with("3. "))
                    .unwrap_or_else(|| panic!("{artifact} README has no step 3; got:\n{readme}"))
                    .to_string()
            };

            // The underscore in the artifact name is a hyphen in the crate name, so the
            // command the README prints is the one cargo actually accepts.
            let hook = step_three("event_sink");
            assert!(
                hook.contains("cargo test -p event-sink"),
                "hook step 3 should name the crate's own cargo test; got: {hook}"
            );
            assert!(
                !hook.contains("on stdin"),
                "hook step 3 should not describe a stdin stub; got: {hook}"
            );

            for tool in ["my-tool", "csv-parser"] {
                let step = step_three(tool);
                assert!(
                    step.contains("on stdin"),
                    "{tool} step 3 should keep naming the stdin stub; got: {step}"
                );
                assert!(
                    !step.contains("cargo test"),
                    "{tool} has no crate to `cargo test`; got: {step}"
                );
            }
        }

        /// The hook checklist sends the author to the `logic` module, which is where the
        /// decisions belong, rather than to `src/lib.rs` at large.
        #[test]
        fn hook_readme_points_at_the_logic_module() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "event-sink", "hook").unwrap();
            let readme = fs::read_to_string(tmp.path().join("tools/event-sink/README.md")).unwrap();
            let step_two = readme
                .lines()
                .find(|line| line.starts_with("2. "))
                .unwrap_or_else(|| panic!("hook README has no step 2; got:\n{readme}"));
            assert!(
                step_two.contains("`logic` module"),
                "hook step 2 should name the logic module; got: {step_two}"
            );
        }

        /// The scaffolder sets the executable bit only on the host build; built for
        /// `wasm32-wasip2` it cannot, and `mur build` packs the mode it finds. The README has to
        /// carry the step, or the stub the README tells the author to run is not runnable.
        #[test]
        fn native_readme_asks_for_the_executable_bit() {
            let tmp = TempDir::new().unwrap();
            scaffold_tool_in(tmp.path(), "my-tool", "native").unwrap();
            let readme =
                fs::read_to_string(tmp.path().join("tools/my-tool/README.md")).unwrap();
            assert!(
                readme.contains("`chmod +x bin/my-tool`"),
                "native README should name the chmod step; got:\n{readme}"
            );

            // A wasm payload is produced by a cargo build and is never exec'd directly.
            scaffold_tool_in(tmp.path(), "other-tool", "wasm").unwrap();
            let wasm_readme =
                fs::read_to_string(tmp.path().join("tools/other-tool/README.md")).unwrap();
            assert!(
                !wasm_readme.contains("chmod"),
                "wasm README should not mention chmod; got:\n{wasm_readme}"
            );
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
