//! What the display suites share: a Wasmtime harness that runs the compiled component the way
//! murmur does, with a test host for `murmur:process/process@0.1.0` that starts the real Xvfb,
//! window manager and programs. Copied from tool-browser's `tests/common`, with the browser
//! lookup and fixture server left out.
//!
//! One `Env` is one murmur session: a work folder, a scratch runtime directory with a staged
//! `tools/<name>/murmur.yaml` per granted name, the children's `PATH`, and the session's
//! processes. Every call gets a fresh `Store` and instance, as murmur gives every tool call; the
//! processes outlive the calls. Dropping the `Env` kills every process group it started with
//! `SIGKILL`, as murmur does at the session's end.
//!
//! Like tool-browser's, this host checks `capabilities.shell.allow` and the 8-process cap, but
//! runs no `read_only` or `on-shell` check, no containment, and no listener check on `connect`.

// Each test binary compiles this module on its own and uses only part of it.
#![allow(dead_code)]

use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::{fs::PermissionsExt, process::CommandExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, Once, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

/// Fails, never skips, on a host missing what the display needs.
pub fn require_host() {
    for (program, package) in [
        ("Xvfb", "xvfb"),
        ("xkbcomp", "x11-xkb-utils"),
        ("xmessage", "x11-utils"),
    ] {
        assert!(
            on_path(program).is_some(),
            "this test needs {program} on PATH: install the {package} package (sudo apt install \
             {package}); a missing display server is a failure, not a skip"
        );
    }
}

/// The `PATH` the session's children get: the directories of this host's display programs.
fn children_path() -> String {
    let mut dirs: Vec<String> = Vec::new();
    for program in ["Xvfb", "xkbcomp", "xmessage", "sh"] {
        if let Some(dir) = on_path(program).and_then(|p| p.parent().map(Path::to_path_buf)) {
            let dir = dir.display().to_string();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    for dir in ["/usr/bin", "/bin"] {
        if !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_string());
        }
    }
    dirs.join(":")
}

// ── the component ────────────────────────────────────────────────────────────

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

static BUILD: Once = Once::new();

/// The compiled component, built once per test process. `cargo test --workspace` (CI) runs
/// before the separate wasm build step, so the test cannot assume the artifact exists.
fn component_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let wasm = target.join("wasm32-wasip2/release/tool_computer.wasm");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-computer",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .current_dir(manifest.join("../.."))
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

// ── the test host for murmur:process ─────────────────────────────────────────

/// murmur's `failed` text for a program missing from `PATH`: `crate::shell::spawn_contained` in
/// murmur's capsule-runtime passes the spawn's `io::Error` on as
/// `ContainedSpawnError::Exec(ShellExecError::Failed(error.to_string()))`, which
/// `session_process.rs` returns as `process-error::failed`. `std::process::Command::spawn` here
/// produces the same text, `No such file or directory (os error 2)`.
pub const NOT_FOUND_SOURCE: &str = "crates/capsule-runtime/src/shell.rs spawn_contained";

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

// ── a session ────────────────────────────────────────────────────────────────

/// A scratch session: a runtime directory with `tools/<name>/murmur.yaml` per granted name, a
/// work folder, and its processes.
pub struct Env {
    pub session: Arc<Session>,
    pub runtime: PathBuf,
    pub work: PathBuf,
    /// Preopen the runtime directory at `murmur-runtime`.
    pub with_runtime: bool,
    pub config: Option<String>,
    pub dir: tempfile::TempDir,
}

impl Env {
    /// Grants `granted` (staged manifests and the allow list).
    pub fn with_tools(granted: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        fs::create_dir_all(runtime.join("tools")).unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let session = Arc::new(Session {
            procs: Mutex::new(Vec::new()),
            next: Mutex::new(0),
            allow: Mutex::new(Vec::new()),
            path: Mutex::new(children_path()),
            home,
            work: work.clone(),
        });
        let env = Self {
            session,
            runtime,
            work,
            with_runtime: true,
            config: None,
            dir,
        };
        for name in granted {
            env.grant(name);
        }
        env
    }

    pub fn with_config(mut self, config: &str) -> Self {
        self.config = Some(config.to_string());
        self
    }

    /// Stages `name`'s manifest and adds it to the allow list.
    pub fn grant(&self, name: &str) {
        let dir = self.runtime.join("tools").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("murmur.yaml"), format!("name: {name}\n")).unwrap();
        self.session.allow.lock().unwrap().push(name.to_string());
    }

    /// The children's `PATH`, replaced by an empty folder: "granted but not installed".
    pub fn with_empty_path(self) -> Self {
        let empty = self.dir.path().join("empty-bin");
        fs::create_dir_all(&empty).unwrap();
        *self.session.path.lock().unwrap() = empty.display().to_string();
        self
    }

    /// Every `.tool-computer/<id>/` holding a `state.json`, oldest start first.
    pub fn state_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<(u64, String, PathBuf)> = fs::read_dir(self.work.join(".tool-computer"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let state: Value =
                    serde_json::from_slice(&fs::read(e.path().join("state.json")).ok()?).ok()?;
                Some((
                    state["started_at"].as_u64().unwrap_or(0),
                    state["handle"].as_str().unwrap_or("").to_string(),
                    e.path(),
                ))
            })
            .collect();
        dirs.sort();
        dirs.into_iter().map(|(_, _, p)| p).collect()
    }

    /// The folder of the display running now, or last started.
    pub fn state(&self) -> PathBuf {
        let handle = self.latest_handle("display");
        self.state_dirs()
            .into_iter()
            .find(|d| {
                fs::read(d.join("state.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .is_some_and(|s| Some(s["handle"].as_str().unwrap_or("")) == handle.as_deref())
            })
            .unwrap_or_else(|| self.work.join(".tool-computer/none"))
    }

    pub fn info(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state().join("state.json")).unwrap()).unwrap()
    }

    /// The state folder relative to the work folder, as results name it.
    pub fn state_rel(&self) -> String {
        self.state()
            .strip_prefix(&self.work)
            .unwrap()
            .display()
            .to_string()
    }

    /// Run one call with `data` as the request: `{status, summary, data, attachments?, took_ms}`.
    pub fn call(&self, data: Value) -> Value {
        let (engine, component) = engine_and_component();
        let mut linker: Linker<HostState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("add wasi to linker");
        wit::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .expect("add murmur:process to linker");

        let mut builder = WasiCtxBuilder::new();
        builder
            .preopened_dir(&self.work, ".", DirPerms::all(), FilePerms::all())
            .expect("preopen work");
        if self.with_runtime {
            builder
                .preopened_dir(
                    &self.runtime,
                    "murmur-runtime",
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .expect("preopen runtime");
        }
        if let Some(config) = &self.config {
            builder.env("MURMUR_ARTIFACT_CONFIG", config);
        }
        let mut store = Store::new(
            engine,
            HostState {
                table: ResourceTable::new(),
                wasi: builder.build(),
                session: Arc::clone(&self.session),
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

    pub fn passed(&self, data: Value) -> Value {
        let result = self.call(data.clone());
        assert_eq!(result["status"], "passed", "{data} → {result}");
        result
    }

    /// The latest handle the tool started under `name`.
    pub fn latest_handle(&self, name: &str) -> Option<String> {
        let procs = self.session.procs.lock().unwrap();
        procs
            .iter()
            .rev()
            .find(|p| p.name == name)
            .map(|p| p.handle.clone())
    }

    /// The pid of the running process under `name`.
    pub fn pid_of(&self, name: &str) -> Option<u32> {
        let mut procs = self.session.procs.lock().unwrap();
        let p = procs.iter_mut().rev().find(|p| p.name == name)?;
        p.state().is_none().then(|| p.child.id())
    }

    /// How many processes the tool started under `name` in this session.
    pub fn starts_of(&self, name: &str) -> usize {
        self.session
            .procs
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.name == name)
            .count()
    }

    /// How many processes the tool started in this session.
    pub fn starts(&self) -> usize {
        self.session.procs.lock().unwrap().len()
    }

    /// `SIGKILL` to the running process under `name`, behind the tool's back, as a crash would;
    /// waits until the host has seen it exit.
    pub fn kill_process(&self, name: &str) -> u32 {
        let mut procs = self.session.procs.lock().unwrap();
        let p = procs
            .iter_mut()
            .rev()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no process named {name}"));
        let pid = p.child.id();
        // SAFETY: kill(2) on a child this host started and has not reaped.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while p.state().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        pid
    }
}

/// `/proc/<pid>` gone or a zombie.
pub fn pid_gone(pid: u64) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
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
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u64>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(needle))
        })
        .filter(|pid| !pid_gone(*pid))
        .collect()
}

pub fn data(v: &Value) -> &str {
    v["data"].as_str().unwrap_or("")
}

pub fn lines(v: &Value) -> Vec<&str> {
    data(v).lines().collect()
}

impl Drop for Env {
    /// With `TOOL_COMPUTER_KEEP_LOGS=<dir>`, every log and `state.json` the session wrote is
    /// copied there before the scratch folders go.
    fn drop(&mut self) {
        let Some(keep) = std::env::var_os("TOOL_COMPUTER_KEEP_LOGS") else {
            return;
        };
        let keep = PathBuf::from(keep);
        let _ = fs::create_dir_all(&keep);
        for e in fs::read_dir(self.work.join(".tool-computer"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for f in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.ends_with(".log") || name == "state.json" {
                    let to = format!("{}-{name}", e.file_name().to_string_lossy());
                    let _ = fs::copy(f.path(), keep.join(to));
                }
            }
        }
    }
}

// ── PNG ──────────────────────────────────────────────────────────────────────

pub struct Png {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Png {
    pub fn read(path: &Path) -> Self {
        let bytes = fs::read(path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
        let mut at = 8;
        let (mut width, mut height, mut idat) = (0, 0, Vec::new());
        while at < bytes.len() {
            let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let kind = &bytes[at + 4..at + 8];
            let body = &bytes[at + 8..at + 8 + len];
            match kind {
                b"IHDR" => {
                    width = u32::from_be_bytes(body[0..4].try_into().unwrap());
                    height = u32::from_be_bytes(body[4..8].try_into().unwrap());
                    assert_eq!(&body[8..10], &[8, 2], "8-bit RGB");
                }
                b"IDAT" => idat.extend_from_slice(body),
                _ => {}
            }
            at += 12 + len;
        }
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(idat.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        let row = width as usize * 3;
        let mut rgb = Vec::with_capacity(row * height as usize);
        for line in raw.chunks(row + 1) {
            assert_eq!(line[0], 0, "filter type None");
            rgb.extend_from_slice(&line[1..]);
        }
        Self { width, height, rgb }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y * self.width + x) as usize * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// Whether every pixel of the box is the background colour, read at (0,0).
    pub fn box_is_background(&self, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> bool {
        let background = self.pixel(0, 0);
        (y0..y1).all(|y| (x0..x1).all(|x| self.pixel(x, y) == background))
    }
}
