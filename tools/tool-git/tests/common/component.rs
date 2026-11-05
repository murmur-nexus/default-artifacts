//! The compiled `wasm32-wasip2` component, run in Wasmtime the way murmur runs a WASM tool: a
//! fresh `Store` and instance per call, the work folder preopened at `.`, each host folder at
//! `murmur-host<path>` with its access, no environment, and `murmur:process` served by the test
//! host in [`super::process`].

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Once, OnceLock},
};

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::process::{self, ProcessHost};

mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
        with: {
            "murmur:process/process.connection": super::NoConnection,
        },
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput};
use bindings::murmur::process::process as wit;
use bindings::Tool;

/// The `connection` resource, which this host never hands out.
pub struct NoConnection;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The compiled component, built once per test process (a no-op when it is current).
/// `cargo test --workspace` (CI) runs before the separate wasm build step, so the test cannot
/// assume the artifact exists.
fn component_path() -> PathBuf {
    static BUILD: Once = Once::new();
    let root = repo_root();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let wasm = target.join("wasm32-wasip2/release/tool_git.wasm");
    BUILD.call_once(|| {
        assert!(
            root.join(".wasi-sdk/bin/clang").exists(),
            "no wasi-sdk at {}: run ./scripts/wasi-sdk.sh first; the component compiles SQLite \
             with it",
            root.join(".wasi-sdk").display()
        );
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-git",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(&root)
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(status.success(), "cargo build of the wasm component failed");
    });
    assert!(wasm.exists(), "no component at {}", wasm.display());
    wasm
}

/// One `Engine` and one compiled `Component` per test process.
fn engine_and_component() -> &'static (Engine, Component) {
    static SHARED: OnceLock<(Engine, Component)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        let component = Component::from_file(&engine, component_path()).expect("load component");
        (engine, component)
    })
}

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    process: Arc<ProcessHost>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn unavailable() -> wit::ProcessError {
    wit::ProcessError::Refused(wit::Refusal {
        rule: "unavailable".into(),
        reason: "the test host serves only run".into(),
    })
}

impl wit::Host for HostState {
    fn run(&mut self, request: wit::RunRequest) -> Result<wit::RunOutput, wit::ProcessError> {
        let request = process::Request {
            program: request.program,
            args: request.args,
            env: request.env,
            stdin: request.stdin,
            timeout_ms: request.timeout_ms,
        };
        match self.process.run(request) {
            Ok(o) => Ok(wit::RunOutput {
                exit_code: o.exit_code,
                timed_out: o.timed_out,
                stdout: o.stdout,
                stdout_truncated: o.stdout_truncated,
                stderr: o.stderr,
                stderr_truncated: o.stderr_truncated,
            }),
            Err(process::Error::Refused { rule, reason }) => {
                Err(wit::ProcessError::Refused(wit::Refusal { rule, reason }))
            }
            Err(process::Error::TimedOut) => Err(wit::ProcessError::TimedOut),
            Err(process::Error::Failed(text)) => Err(wit::ProcessError::Failed(text)),
        }
    }

    fn start(&mut self, _request: wit::StartRequest) -> Result<wit::Started, wit::ProcessError> {
        Err(unavailable())
    }

    fn find(&mut self, _name: String) -> Option<wit::Handle> {
        panic!("tool-git called murmur:process find")
    }

    fn status(&mut self, _handle: wit::Handle) -> Result<wit::State, wit::ProcessError> {
        Err(unavailable())
    }

    fn write(&mut self, _handle: wit::Handle, _bytes: Vec<u8>) -> Result<(), wit::ProcessError> {
        Err(unavailable())
    }

    fn read(
        &mut self,
        _handle: wit::Handle,
        _max_bytes: u32,
    ) -> Result<wit::Output, wit::ProcessError> {
        Err(unavailable())
    }

    fn stop(&mut self, _handle: wit::Handle) -> Result<wit::State, wit::ProcessError> {
        Err(unavailable())
    }

    fn connect(
        &mut self,
        _handle: wit::Handle,
        _port: u16,
    ) -> Result<Resource<NoConnection>, wit::ProcessError> {
        Err(unavailable())
    }
}

impl wit::HostConnection for HostState {
    fn read(
        &mut self,
        _conn: Resource<NoConnection>,
        _max_bytes: u32,
        _timeout_ms: u32,
    ) -> Result<Vec<u8>, wit::ProcessError> {
        Err(unavailable())
    }

    fn write(
        &mut self,
        _conn: Resource<NoConnection>,
        _bytes: Vec<u8>,
    ) -> Result<(), wit::ProcessError> {
        Err(unavailable())
    }

    fn drop(&mut self, conn: Resource<NoConnection>) -> wasmtime::Result<()> {
        self.table.delete(conn)?;
        Ok(())
    }
}

/// What one call gave the model.
pub struct Answer {
    pub status: &'static str,
    pub summary: Option<String>,
    pub data: Option<String>,
    pub truncated: bool,
}

/// Run one call through the component. `roots` are the host folders, `(path, writable)`, opened
/// at `murmur-host<path>` in this order.
pub fn call(
    process: &Arc<ProcessHost>,
    work: &Path,
    roots: &[(PathBuf, bool)],
    data: String,
    log_path: Option<String>,
) -> Answer {
    let (engine, component) = engine_and_component();
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    wit::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
        .expect("add murmur:process to linker");

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(work, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen work");
    for (path, writable) in roots {
        let (dirs, files) = if *writable {
            (DirPerms::all(), FilePerms::all())
        } else {
            (DirPerms::READ, FilePerms::READ)
        };
        builder
            .preopened_dir(path, format!("murmur-host{}", path.display()), dirs, files)
            .expect("preopen host root");
    }
    let mut store = Store::new(
        engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
            process: Arc::clone(process),
        },
    );
    let tool = Tool::instantiate(&mut store, component, &linker).expect("instantiate");
    let input = ToolInput {
        data: Some(data),
        log_path,
    };
    let result = tool
        .murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run");
    assert!(
        result.data_path.is_none() && result.metadata.is_empty() && result.attachments.is_empty(),
        "tool-git sets no data_path, metadata or attachments"
    );
    Answer {
        status: match result.status {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Error => "error",
        },
        summary: result.summary,
        data: result.data,
        truncated: result.truncated,
    }
}
