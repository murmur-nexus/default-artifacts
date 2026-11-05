//! What the browser suites share: which browser they run, the fixture page and server, and a
//! Wasmtime harness that runs the compiled component the way murmur does, with a test host for
//! `murmur:process/process@0.1.0` that starts the real browser.
//!
//! One `Env` is one murmur session: a work folder, a scratch runtime directory with a staged
//! `tools/<name>/murmur.yaml` per granted name, a `bin/` holding a `chromium` link to the test
//! browser, and the session's processes. Every call gets a fresh `Store` and instance, as murmur
//! gives every tool call; the processes outlive the calls. Dropping the `Env` kills every
//! process group it started with `SIGKILL`, as murmur does at the session's end.

// Each test binary compiles this module on its own and uses only part of it: `mur_run_browser`
// takes the browser lookup and the fixture page, `browser` the rest. Every item is used by one.
#![allow(dead_code)]

use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
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

pub const NAMES: [&str; 4] = ["chromium", "chromium-browser", "google-chrome", "chrome"];

pub const FIXTURE_PAGE: &str = r#"<!doctype html>
<html><head><title>Fixture Page</title><link rel="icon" href="data:,">
<script src="missing.js"></script></head>
<body onload="console.error('boom-load')">
<h1 id="h">waiting</h1>
<button id="go" onclick="document.getElementById('h').textContent='clicked'; console.error('boom-click')">Go</button>
<p><label>Name <input id="name" type="text"></label></p>
<p><label>Colour <select id="colour"><option value="r">Red</option><option value="g">Green</option></select></label></p>
</body></html>
"#;

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

/// The browser the suites run: `TOOL_BROWSER_TEST_CHROME`, else the first of the four names on
/// `PATH`, else the newest Playwright Chromium. Panics, naming what to install, when none.
pub fn test_browser() -> PathBuf {
    if let Some(explicit) = std::env::var_os("TOOL_BROWSER_TEST_CHROME") {
        let p = PathBuf::from(explicit);
        assert!(
            is_executable(&p),
            "TOOL_BROWSER_TEST_CHROME names {}, which is not executable",
            p.display()
        );
        return p;
    }
    if let Some(p) = NAMES.iter().find_map(|n| on_path(n)) {
        return p;
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut found: Vec<(u32, PathBuf)> = fs::read_dir(home.join(".cache/ms-playwright"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rev: u32 = name.strip_prefix("chromium-")?.parse().ok()?;
            let chrome = fs::read_dir(e.path())
                .ok()?
                .flatten()
                .filter(|d| d.file_name().to_string_lossy().starts_with("chrome-linux"))
                .map(|d| d.path().join("chrome"))
                .find(|p| is_executable(p))?;
            Some((rev, chrome))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, p)| p).unwrap_or_else(|| {
        panic!(
            "no browser for the tool-browser tests: install Chromium or Google Chrome (one of \
             chromium, chromium-browser, google-chrome or chrome on PATH), or run \
             `npx playwright install chromium`, or set TOOL_BROWSER_TEST_CHROME to a chrome binary"
        )
    })
}

/// A loopback HTTP server: `/` is `page` (the fixture page by default), everything else 404.
pub fn fixture_server() -> u16 {
    serve_page(FIXTURE_PAGE)
}

pub fn serve_page(page: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = if path == "/" || path == "/index.html" {
                    ("200 OK", page)
                } else {
                    ("404 Not Found", "not found")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
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
    let wasm = target.join("wasm32-wasip2/release/tool_browser.wasm");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "tool-browser",
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
/// `bin/chromium` link to the test browser first on `PATH`, a work folder, and its processes.
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
    pub fn new() -> Self {
        Self::with_tools(&["chromium"])
    }

    /// Grants `granted` (staged manifests and the allow list); the `bin/chromium` link is there
    /// either way.
    pub fn with_tools(granted: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        fs::create_dir_all(runtime.join("tools")).unwrap();
        for name in granted {
            fs::create_dir_all(runtime.join("tools").join(name)).unwrap();
            fs::write(
                runtime.join("tools").join(name).join("murmur.yaml"),
                "name: x\n",
            )
            .unwrap();
        }
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(test_browser(), bin.join("chromium")).unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let session = Arc::new(Session {
            procs: Mutex::new(Vec::new()),
            next: Mutex::new(0),
            allow: Mutex::new(granted.iter().map(|s| s.to_string()).collect()),
            path: Mutex::new(format!("{}:/usr/bin:/bin", bin.display())),
            home,
            work: work.clone(),
        });
        Self {
            session,
            runtime,
            work,
            with_runtime: true,
            config: None,
            dir,
        }
    }

    /// A `PATH` without the browser link, for "granted but not installed".
    pub fn without_browser_on_path(self) -> Self {
        let empty = self.dir.path().join("empty-bin");
        fs::create_dir_all(&empty).unwrap();
        *self.session.path.lock().unwrap() = empty.display().to_string();
        self
    }

    /// Put an executable script named `name` first on `PATH`.
    pub fn script_on_path(&self, name: &str, body: &str) -> PathBuf {
        let bin = self.dir.path().join("scripts");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let mut p = self.session.path.lock().unwrap();
        if !p.starts_with(&bin.display().to_string()) {
            *p = format!("{}:{}", bin.display(), p);
        }
        path
    }

    /// Every `.tool-browser/<id>/` holding a `state.json`, oldest start first.
    pub fn state_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<(u64, String, PathBuf)> = fs::read_dir(self.work.join(".tool-browser"))
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

    /// The folder of the browser the last start made.
    pub fn state(&self) -> PathBuf {
        let handle = self
            .session
            .procs
            .lock()
            .unwrap()
            .last()
            .map(|p| p.handle.clone());
        self.state_dirs()
            .into_iter()
            .find(|d| {
                fs::read(d.join("state.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .is_some_and(|s| Some(s["handle"].as_str().unwrap_or("")) == handle.as_deref())
            })
            .unwrap_or_else(|| self.work.join(".tool-browser/none"))
    }

    pub fn info(&self) -> Option<Value> {
        serde_json::from_slice(&fs::read(self.state().join("state.json")).ok()?).ok()
    }

    /// The state folder relative to the work folder, as results name it.
    pub fn state_rel(&self) -> String {
        self.state()
            .strip_prefix(&self.work)
            .unwrap()
            .display()
            .to_string()
    }

    /// Run one call with `data` as the request: `{status, summary, data, attachments?}`.
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
        let tool = Tool::instantiate(&mut store, component, &linker).expect("instantiate");
        let input = ToolInput {
            data: Some(data.to_string()),
            log_path: None,
        };
        let result = tool
            .murmur_tool_run()
            .call_run(&mut store, &input)
            .expect("call run");
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
        value
    }

    /// The pid of the running browser process, the session process the tool started last.
    pub fn browser_pid(&self) -> Option<u32> {
        let mut procs = self.session.procs.lock().unwrap();
        let p = procs.last_mut()?;
        p.state().is_none().then(|| p.child.id())
    }

    pub fn handle(&self) -> Option<String> {
        self.info()?["handle"].as_str().map(str::to_string)
    }

    /// How many processes the tool started in this session.
    pub fn starts(&self) -> usize {
        self.session.procs.lock().unwrap().len()
    }

    /// Kill the browser behind the tool's back, as a crash would.
    pub fn kill_browser(&self) -> u32 {
        let mut procs = self.session.procs.lock().unwrap();
        let p = procs.last_mut().unwrap();
        kill_group(p.pgid, "KILL");
        let deadline = Instant::now() + Duration::from_secs(5);
        while p.state().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        p.child.id()
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

/// Closes the browser at the end of a test; the session's drop kills what is left.
pub struct Guard<'a>(pub &'a Env);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.0.call(json!({"action": "close"}));
        }));
    }
}

pub fn data(v: &Value) -> &str {
    v["data"].as_str().unwrap_or("")
}

/// The ref on the first line matching `role "name"`.
pub fn ref_of(snapshot: &str, role_and_name: &str) -> String {
    let line = snapshot
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("- {role_and_name}")))
        .unwrap_or_else(|| panic!("no `{role_and_name}` line in:\n{snapshot}"));
    let start = line
        .find("[ref=")
        .unwrap_or_else(|| panic!("no ref on: {line}"))
        + 5;
    line[start..].trim_end_matches(']').to_string()
}

impl Drop for Env {
    /// With `TOOL_BROWSER_KEEP_LOGS=<dir>`, every `browser.log` and `state.json` the
    /// session wrote is copied there before the scratch folders go.
    fn drop(&mut self) {
        let Some(keep) = std::env::var_os("TOOL_BROWSER_KEEP_LOGS") else {
            return;
        };
        let keep = PathBuf::from(keep);
        let _ = fs::create_dir_all(&keep);
        for e in fs::read_dir(self.work.join(".tool-browser"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for (file, ext) in [("browser.log", "log"), ("state.json", "json")] {
                let from = e.path().join(file);
                if from.is_file() {
                    let name = format!("{}.{ext}", e.file_name().to_string_lossy());
                    let _ = fs::copy(&from, keep.join(name));
                }
            }
        }
    }
}
