//! What the browser suites share: which browser they run, the fixture page and server, and an
//! `Env` over the Wasmtime test host in `libs/murmur-process-test-host`, which runs the compiled
//! component the way murmur does and starts the real browser.
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
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use murmur_process_test_host::{call, CallEnv, Kill, Session};
use serde_json::{json, Value};

const PACKAGE: &str = "tool-browser";
/// The session-process name tool-browser starts the browser under.
const PROCESS_NAME: &str = "browser";

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

// ── a session ────────────────────────────────────────────────────────────────

/// A scratch session: a runtime directory with `tools/<name>/murmur.yaml` per granted name, a
/// `bin/chromium` link to the test browser first on the children's `PATH`, a work folder, and
/// its processes.
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
        let session = Session::new(
            format!("{}:/usr/bin:/bin", bin.display()),
            home,
            work.clone(),
        );
        session
            .allow
            .lock()
            .unwrap()
            .extend(granted.iter().map(|s| s.to_string()));
        Self {
            session: Arc::new(session),
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
        let handle = self.session.latest_handle(PROCESS_NAME);
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

    /// Run one call with `data` as the request: `{status, summary, data, attachments?, took_ms}`.
    pub fn call(&self, data: Value) -> Value {
        call(
            &CallEnv {
                package: PACKAGE,
                session: &self.session,
                work: &self.work,
                runtime: self.with_runtime.then_some(self.runtime.as_path()),
                config: self.config.as_deref(),
            },
            &data,
        )
    }

    /// The pid of the running browser process.
    pub fn browser_pid(&self) -> Option<u32> {
        self.session.pid_of(PROCESS_NAME)
    }

    pub fn handle(&self) -> Option<String> {
        self.info()?["handle"].as_str().map(str::to_string)
    }

    /// How many processes the tool started in this session.
    pub fn starts(&self) -> usize {
        self.session.starts()
    }

    /// Kill the browser's process group behind the tool's back, as a crash would.
    pub fn kill_browser(&self) -> u32 {
        self.session.kill(PROCESS_NAME, Kill::Group)
    }
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
