//! What the display suites share: the host checks, and an `Env` over the Wasmtime test host in
//! `libs/murmur-process-test-host`, which runs the compiled component the way murmur does and
//! starts the real Xvfb, window manager and programs.
//!
//! One `Env` is one murmur session: a work folder, a scratch runtime directory with a staged
//! `tools/<name>/murmur.yaml` per granted name, the children's `PATH`, and the session's
//! processes. Every call gets a fresh `Store` and instance, as murmur gives every tool call; the
//! processes outlive the calls. Dropping the `Env` kills every process group it started with
//! `SIGKILL`, as murmur does at the session's end.

use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use murmur_process_test_host::{call, CallEnv, Kill};
pub use murmur_process_test_host::{data, Session};
use serde_json::Value;

const PACKAGE: &str = "tool-computer";

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
        let session = Arc::new(Session::new(children_path(), home, work.clone()));
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

    pub fn passed(&self, data: Value) -> Value {
        let result = self.call(data.clone());
        assert_eq!(result["status"], "passed", "{data} → {result}");
        result
    }

    /// The latest handle the tool started under `name`.
    pub fn latest_handle(&self, name: &str) -> Option<String> {
        self.session.latest_handle(name)
    }

    /// The pid of the running process under `name`.
    pub fn pid_of(&self, name: &str) -> Option<u32> {
        self.session.pid_of(name)
    }

    /// How many processes the tool started under `name` in this session.
    pub fn starts_of(&self, name: &str) -> usize {
        self.session.starts_of(name)
    }

    /// How many processes the tool started in this session.
    pub fn starts(&self) -> usize {
        self.session.starts()
    }

    /// `SIGKILL` to the running process under `name` (its pid, not its group), behind the tool's
    /// back, as a crash would; waits until the host has seen it exit.
    pub fn kill_process(&self, name: &str) -> u32 {
        self.session.kill(name, Kill::Process)
    }
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
