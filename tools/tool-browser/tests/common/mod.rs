//! What the browser suites share: which browser they run, the fixture page and server, a
//! scratch session environment, and the guard that leaves no browser behind.

#![allow(dead_code)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

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

/// The name, of the four, under which `browser` is found on `PATH`.
pub fn name_on_path(browser: &Path) -> Option<String> {
    NAMES
        .iter()
        .find(|n| on_path(n).as_deref() == Some(browser))
        .map(|n| n.to_string())
}

/// A loopback HTTP server: `/` is the fixture page, everything else 404.
pub fn fixture_server() -> u16 {
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
                    ("200 OK", FIXTURE_PAGE)
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

/// A scratch session: a runtime directory with `tools/chromium/murmur.yaml`, a `bin/chromium`
/// link to the test browser first on `PATH`, and a work folder.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub runtime: PathBuf,
    pub work: PathBuf,
    pub path: String,
    /// Set `MURMUR_RUNTIME_DIR`.
    pub with_runtime: bool,
    pub config: Option<String>,
}

impl Env {
    pub fn new() -> Self {
        Self::with_tools(&["chromium"])
    }

    /// Grants `granted`; the `bin/chromium` link is there either way.
    pub fn with_tools(granted: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("sessions/ses_test/runtime");
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
        fs::create_dir_all(dir.path().join("home")).unwrap();
        Self {
            path: format!("{}:/usr/bin:/bin", bin.display()),
            runtime,
            work,
            dir,
            with_runtime: true,
            config: None,
        }
    }

    /// A `PATH` without the browser link, for "granted but not installed".
    pub fn without_browser_on_path(mut self) -> Self {
        let empty = self.dir.path().join("empty-bin");
        fs::create_dir_all(&empty).unwrap();
        self.path = empty.display().to_string();
        self
    }

    pub fn state(&self) -> PathBuf {
        self.work.join(".tool-browser/ses_test")
    }

    /// Run one call with `data` as the request, returning the printed envelope.
    pub fn call(&self, data: Value) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tool-browser"));
        command
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.dir.path().join("home"))
            .env("TMPDIR", self.dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.with_runtime {
            command.env("MURMUR_RUNTIME_DIR", &self.runtime);
        }
        if let Some(config) = &self.config {
            command.env("MURMUR_ARTIFACT_CONFIG", config);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"data": data, "log_path": null})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        // The call returns only once nothing holds its stdout and stderr, as murmur waits.
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim().lines().count(),
            1,
            "exactly one line on stdout: {stdout}"
        );
        let value: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert!(
            !stdout.contains("<untrusted-content"),
            "the tool writes no fence: {stdout}"
        );
        value
    }

    pub fn info(&self) -> Option<Value> {
        serde_json::from_slice(&fs::read(self.state().join("supervisor.json")).ok()?).ok()
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

/// Every pid whose command line mentions `needle`.
pub fn pids_mentioning(needle: &Path) -> Vec<u64> {
    let needle = needle.to_string_lossy().into_owned();
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u64>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&needle))
        })
        .filter(|pid| !pid_gone(*pid))
        .collect()
}

/// Closes the browser at the end of a test and kills whatever `supervisor.json` recorded.
pub struct Guard<'a>(pub &'a Env);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let info = self.0.info();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.0.call(json!({"action": "close"}));
        }));
        if let Some(info) = info {
            for pid in [info["pid"].as_u64(), info["browser"]["pid"].as_u64()]
                .into_iter()
                .flatten()
            {
                let _ = Command::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        for pid in pids_mentioning(&self.0.state()) {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .status();
        }
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
