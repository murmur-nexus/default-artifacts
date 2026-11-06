//! The test host the WASM tools on `murmur:process` share: a Wasmtime harness that runs a
//! tool's compiled `wasm32-wasip2` component the way murmur does, with a host for
//! `murmur:process/process@0.1.0` that starts real programs outside any capsule.
//!
//! A [`Session`] is one murmur session's processes, shared by every call; [`call`] runs one tool
//! call in a fresh `Store` and instance, as murmur gives every tool call. Dropping the `Session`
//! kills every process group it started with `SIGKILL`, as murmur does at the session's end.
//!
//! This host checks `capabilities.shell.allow` and murmur's 8-process cap, and a program missing
//! from `PATH` fails with the text murmur gives (`No such file or directory (os error 2)`, from
//! `spawn_contained` in murmur's `crates/capsule-runtime/src/shell.rs`). It runs no `read_only`
//! or `on-shell` check, no containment and no listener check on `connect`, and it does not serve
//! `run`.
//!
//! A dev-dependency only, and built for host targets only: it is empty on `wasm32`, so the
//! workspace wasm build compiles it without Wasmtime.

#![cfg(not(target_arch = "wasm32"))]

use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::TcpStream,
    os::unix::{process::CommandExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit/guest",
        with: {
            "murmur:process/process.connection": super::TestConn,
        },
    });
}
use bindings::exports::murmur::tool::run::{Status, ToolInput};
use bindings::murmur::process::process as wit;
use bindings::Tool;

/// The compiled component of `package` and its `Engine`, built once per test process with
/// `cargo build -p <package> --target wasm32-wasip2 --release`: `cargo test --workspace` (CI)
/// runs before the separate wasm build step, so a test cannot assume the artifact exists. One
/// test process calls one package.
pub fn component(package: &str) -> (&'static Engine, &'static Component) {
    static SHARED: OnceLock<(String, Engine, Component)> = OnceLock::new();
    let (built, engine, component) = SHARED.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        let wasm = target
            .join("wasm32-wasip2/release")
            .join(format!("{}.wasm", package.replace('-', "_")));
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                package,
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(&root)
            .status()
            .expect("failed to spawn `cargo build` for the wasm component");
        assert!(status.success(), "cargo build of the wasm component failed");
        assert!(wasm.exists(), "no component at {}", wasm.display());
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        let component = Component::from_file(&engine, &wasm).expect("load component");
        (package.to_string(), engine, component)
    });
    assert_eq!(built, package, "one test process calls one package");
    (engine, component)
}

// ── the host for murmur:process ──────────────────────────────────────────────

/// Each stream keeps its newest 1 MiB, as murmur's does.
const STREAM_KEEP: usize = 1024 * 1024;

#[derive(Default)]
struct StreamBuf {
    bytes: VecDeque<u8>,
    dropped: u64,
}

fn pump(
    mut from: impl Read + Send + 'static,
    into: Arc<Mutex<StreamBuf>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match from.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut b = into.lock().unwrap();
                    b.bytes.extend(&chunk[..n]);
                    while b.bytes.len() > STREAM_KEEP {
                        b.bytes.pop_front();
                        b.dropped += 1;
                    }
                }
            }
        }
    })
}

struct Proc {
    handle: String,
    name: String,
    child: Child,
    pgid: i32,
    stdout: Arc<Mutex<StreamBuf>>,
    stderr: Arc<Mutex<StreamBuf>>,
    /// The threads reading stdout and stderr, joined once the process exits so its last output
    /// is buffered before its exit is reported.
    readers: Vec<thread::JoinHandle<()>>,
    exit: Option<i32>,
}

impl Proc {
    fn state(&mut self) -> Option<i32> {
        if self.exit.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                let code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
                self.exit = Some(code);
                // What the process left in its group goes with it, as in murmur.
                kill_group(self.pgid, "KILL");
                for reader in self.readers.drain(..) {
                    let _ = reader.join();
                }
            }
        }
        self.exit
    }
}

fn kill_group(pgid: i32, signal: &str) {
    let _ = Command::new("kill")
        .args([format!("-{signal}"), "--".into(), format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
}

/// What [`Session::kill`] sends `SIGKILL` to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kill {
    /// The process alone; what it started lives on until the host sees it exit.
    Process,
    /// Its whole process group.
    Group,
}

/// The session's processes, shared by every call's store.
pub struct Session {
    procs: Mutex<Vec<Proc>>,
    next: Mutex<u64>,
    /// `capabilities.shell.allow`, as the test host checks it.
    pub allow: Mutex<Vec<String>>,
    pub path: Mutex<String>,
    home: PathBuf,
    work: PathBuf,
}

impl Session {
    /// A session whose programs run in `work` with `PATH=path` and `HOME=home`, and that grants
    /// nothing yet: push names onto [`Session::allow`].
    pub fn new(path: String, home: PathBuf, work: PathBuf) -> Self {
        Self {
            procs: Mutex::new(Vec::new()),
            next: Mutex::new(0),
            allow: Mutex::new(Vec::new()),
            path: Mutex::new(path),
            home,
            work,
        }
    }

    fn lookup<'a>(procs: &'a mut [Proc], handle: &str) -> Result<&'a mut Proc, wit::ProcessError> {
        procs
            .iter_mut()
            .find(|p| p.handle == handle)
            .ok_or(wit::ProcessError::UnknownHandle)
    }

    /// Every pgid the session started.
    pub fn pgids(&self) -> Vec<i32> {
        self.procs.lock().unwrap().iter().map(|p| p.pgid).collect()
    }

    /// The latest handle the tool started under `name`.
    pub fn latest_handle(&self, name: &str) -> Option<String> {
        let procs = self.procs.lock().unwrap();
        procs
            .iter()
            .rev()
            .find(|p| p.name == name)
            .map(|p| p.handle.clone())
    }

    /// The pid of the latest process under `name`, while it runs.
    pub fn pid_of(&self, name: &str) -> Option<u32> {
        let mut procs = self.procs.lock().unwrap();
        let p = procs.iter_mut().rev().find(|p| p.name == name)?;
        p.state().is_none().then(|| p.child.id())
    }

    /// How many processes the tool started under `name` in this session.
    pub fn starts_of(&self, name: &str) -> usize {
        self.procs
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.name == name)
            .count()
    }

    /// How many processes the tool started in this session.
    pub fn starts(&self) -> usize {
        self.procs.lock().unwrap().len()
    }

    /// `SIGKILL` to the latest process under `name`, or to its process group, behind the tool's
    /// back, as a crash would; waits until the host has seen it exit. Its pid.
    pub fn kill(&self, name: &str, what: Kill) -> u32 {
        let mut procs = self.procs.lock().unwrap();
        let p = procs
            .iter_mut()
            .rev()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no process named {name}"));
        match what {
            Kill::Group => kill_group(p.pgid, "KILL"),
            Kill::Process => {
                let _ = p.child.kill();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while p.state().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        p.child.id()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for p in self.procs.get_mut().unwrap().iter_mut() {
            kill_group(p.pgid, "KILL");
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }
}

/// A `connection` resource: one TCP stream, closed with the call's store.
pub struct TestConn {
    stream: TcpStream,
}

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    session: Arc<Session>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn state_of(code: Option<i32>) -> wit::State {
    match code {
        None => wit::State::Running,
        Some(c) => wit::State::Exited(c),
    }
}

impl wit::Host for HostState {
    fn run(&mut self, _request: wit::RunRequest) -> Result<wit::RunOutput, wit::ProcessError> {
        Err(wit::ProcessError::Failed(
            "the test host does not serve run".into(),
        ))
    }

    fn start(&mut self, request: wit::StartRequest) -> Result<wit::Started, wit::ProcessError> {
        let s = &self.session;
        if !s.allow.lock().unwrap().contains(&request.program) {
            return Err(wit::ProcessError::Refused(wit::Refusal {
                rule: "capabilities.shell.allow".into(),
                reason: format!(
                    "binary '{}' is not in capabilities.shell.allow",
                    request.program
                ),
            }));
        }
        // The whole start holds the registry, so two calls starting one name start one process.
        let mut procs = s.procs.lock().unwrap();
        if let Some(p) = procs.iter_mut().rev().find(|p| p.name == request.name) {
            if p.state().is_none() {
                return Ok(wit::Started {
                    handle: p.handle.clone(),
                    already_running: true,
                });
            }
        }
        if procs
            .iter_mut()
            .map(|p| p.state())
            .filter(Option::is_none)
            .count()
            >= 8
        {
            return Err(wit::ProcessError::Refused(wit::Refusal {
                rule: "session-process-cap".into(),
                reason: "this session already holds 8 running processes".into(),
            }));
        }
        let mut child = Command::new(&request.program)
            .args(&request.args)
            .current_dir(&s.work)
            .env_clear()
            .env("PATH", s.path.lock().unwrap().as_str())
            .env("HOME", &s.home)
            .envs(request.env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))?;
        let stdout = Arc::new(Mutex::new(StreamBuf::default()));
        let stderr = Arc::new(Mutex::new(StreamBuf::default()));
        let readers = vec![
            pump(child.stdout.take().unwrap(), Arc::clone(&stdout)),
            pump(child.stderr.take().unwrap(), Arc::clone(&stderr)),
        ];
        let mut next = s.next.lock().unwrap();
        *next += 1;
        let handle = format!("prc_{:08x}", *next);
        procs.push(Proc {
            handle: handle.clone(),
            name: request.name,
            pgid: child.id() as i32,
            child,
            stdout,
            stderr,
            readers,
            exit: None,
        });
        Ok(wit::Started {
            handle,
            already_running: false,
        })
    }

    fn find(&mut self, name: String) -> Option<wit::Handle> {
        let procs = self.session.procs.lock().unwrap();
        procs
            .iter()
            .rev()
            .find(|p| p.name == name)
            .map(|p| p.handle.clone())
    }

    fn status(&mut self, handle: wit::Handle) -> Result<wit::State, wit::ProcessError> {
        let mut procs = self.session.procs.lock().unwrap();
        Ok(state_of(Session::lookup(&mut procs, &handle)?.state()))
    }

    fn write(&mut self, _handle: wit::Handle, _bytes: Vec<u8>) -> Result<(), wit::ProcessError> {
        Err(wit::ProcessError::NotRunning)
    }

    fn read(
        &mut self,
        handle: wit::Handle,
        max_bytes: u32,
    ) -> Result<wit::Output, wit::ProcessError> {
        let mut procs = self.session.procs.lock().unwrap();
        let p = Session::lookup(&mut procs, &handle)?;
        let take = |b: &Arc<Mutex<StreamBuf>>| {
            let mut b = b.lock().unwrap();
            let n = b.bytes.len().min(max_bytes as usize);
            (
                b.bytes.drain(..n).collect::<Vec<u8>>(),
                std::mem::take(&mut b.dropped),
            )
        };
        let (stdout, stdout_dropped) = take(&p.stdout);
        let (stderr, stderr_dropped) = take(&p.stderr);
        Ok(wit::Output {
            stdout,
            stderr,
            stdout_dropped,
            stderr_dropped,
        })
    }

    fn stop(&mut self, handle: wit::Handle) -> Result<wit::State, wit::ProcessError> {
        let mut procs = self.session.procs.lock().unwrap();
        let p = Session::lookup(&mut procs, &handle)?;
        if p.state().is_none() {
            kill_group(p.pgid, "TERM");
            let deadline = Instant::now() + Duration::from_secs(2);
            while p.state().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if p.state().is_none() {
                kill_group(p.pgid, "KILL");
                let _ = p.child.wait();
                p.exit = Some(128 + 9);
            }
        }
        Ok(state_of(p.state()))
    }

    fn connect(
        &mut self,
        handle: wit::Handle,
        port: u16,
    ) -> Result<Resource<TestConn>, wit::ProcessError> {
        {
            let mut procs = self.session.procs.lock().unwrap();
            if Session::lookup(&mut procs, &handle)?.state().is_some() {
                return Err(wit::ProcessError::NotRunning);
            }
        }
        let stream = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))?;
        stream.set_nodelay(true).ok();
        self.table
            .push(TestConn { stream })
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))
    }
}

impl wit::HostConnection for HostState {
    fn read(
        &mut self,
        conn: Resource<TestConn>,
        max_bytes: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, wit::ProcessError> {
        let c = self
            .table
            .get(&conn)
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))?;
        c.stream
            .set_read_timeout(Some(Duration::from_millis(u64::from(timeout_ms.max(1)))))
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))?;
        let mut buf = vec![0u8; (max_bytes as usize).min(STREAM_KEEP)];
        match (&c.stream).read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(wit::ProcessError::TimedOut)
            }
            Err(e) => Err(wit::ProcessError::Failed(e.to_string())),
        }
    }

    fn write(&mut self, conn: Resource<TestConn>, bytes: Vec<u8>) -> Result<(), wit::ProcessError> {
        let c = self
            .table
            .get(&conn)
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))?;
        (&c.stream)
            .write_all(&bytes)
            .map_err(|e| wit::ProcessError::Failed(e.to_string()))
    }

    fn drop(&mut self, conn: Resource<TestConn>) -> wasmtime::Result<()> {
        self.table.delete(conn)?;
        Ok(())
    }
}

// ── one call ─────────────────────────────────────────────────────────────────

/// What one call runs with besides its input.
pub struct CallEnv<'a> {
    /// The cargo package whose `wasm32-wasip2` component is called.
    pub package: &'a str,
    pub session: &'a Arc<Session>,
    /// Preopened at `.`, read-write.
    pub work: &'a Path,
    /// Preopened at `murmur-runtime`, read-only, when given.
    pub runtime: Option<&'a Path>,
    /// `MURMUR_ARTIFACT_CONFIG`, when given.
    pub config: Option<&'a str>,
}

/// Runs one call with `data` as the request in a fresh store and instance, as murmur runs every
/// tool call: `{status, summary, data, attachments?, took_ms}`. Panics when the result carries a
/// `data_path`, `truncated` or metadata, or holds an `<untrusted-content>` fence, which only
/// murmur writes.
pub fn call(env: &CallEnv, data: &Value) -> Value {
    let (engine, component) = component(env.package);
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
    wit::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(&mut linker, |s| s)
        .expect("add murmur:process to linker");

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(env.work, ".", DirPerms::all(), FilePerms::all())
        .expect("preopen work");
    if let Some(runtime) = env.runtime {
        builder
            .preopened_dir(runtime, "murmur-runtime", DirPerms::READ, FilePerms::READ)
            .expect("preopen runtime");
    }
    if let Some(config) = env.config {
        builder.env("MURMUR_ARTIFACT_CONFIG", config);
    }
    let mut store = Store::new(
        engine,
        HostState {
            table: ResourceTable::new(),
            wasi: builder.build(),
            session: Arc::clone(env.session),
        },
    );
    let started = Instant::now();
    let tool = Tool::instantiate(&mut store, component, &linker).expect("instantiate");
    let input = ToolInput {
        data: Some(data.to_string()),
        log_path: None,
    };
    let result = tool
        .murmur_tool_run()
        .call_run(&mut store, &input)
        .expect("call run");
    let took = started.elapsed();
    let mut value = json!({
        "status": match result.status {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Error => "error",
        },
        "summary": result.summary,
        "data": result.data,
    });
    assert!(result.data_path.is_none() && !result.truncated && result.metadata.is_empty());
    if !result.attachments.is_empty() {
        value["attachments"] = result
            .attachments
            .iter()
            .map(|a| json!({"media_type": a.media_type, "path": a.path}))
            .collect();
    }
    let text = value.to_string();
    assert!(
        !text.contains("<untrusted-content") && !text.contains("</untrusted-content"),
        "the tool writes no fence: {text}"
    );
    value["took_ms"] = json!(took.as_millis() as u64);
    value
}

/// `/proc/<pid>` gone or a zombie.
pub fn pid_gone(pid: u64) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
    }
}

pub fn wait_gone(pid: u64, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if pid_gone(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    pid_gone(pid)
}

/// Every live pid whose command line mentions `needle`.
pub fn pids_mentioning(needle: &str) -> Vec<u64> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u64>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(needle))
        })
        .filter(|pid| !pid_gone(*pid))
        .collect()
}

/// A call's `data` text.
pub fn data(v: &Value) -> &str {
    v["data"].as_str().unwrap_or("")
}
