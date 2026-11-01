//! S1–S11: the built binary against this host's real `Xvfb`.
//!
//! Each test builds a scratch `sessions/ses_test/runtime` whose `tools/<name>/murmur.yaml` files
//! are the capsule's grants, runs `tool-computer` with a scratch work folder as its current
//! directory and `DISPLAY`/`XAUTHORITY` removed, and tears down by removing the runtime folder,
//! then asserting the keeper and Xvfb are gone within 5 s. A host without `Xvfb`, `xkbcomp` or
//! `xmessage` fails naming the package to install; nothing here skips.

#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::{DefaultStream, RustConnection};

const BIN: &str = env!("CARGO_BIN_EXE_tool-computer");
const SESSION: &str = "ses_test";
const XMESSAGE_CONFIG: &str =
    r#"{"launch":[["xmessage","-print","-geometry","+20+20","-buttons","Press:0","hello"]]}"#;
/// Where xmessage draws its window with no window manager, and its `Press` button.
const BOX: (u32, u32, u32, u32) = (20, 20, 130, 75);
const PRESS: (u32, u32) = (46, 60);
const NO_WM: &str =
    "window manager none (grant one of openbox, fluxbox, icewm, twm or matchbox-window-manager)";

// ── host ─────────────────────────────────────────────────────────────────────

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Fails, never skips, on a host missing what the display needs.
fn require_host() {
    for (program, package) in [
        ("Xvfb", "xvfb"),
        ("xkbcomp", "x11-xkb-utils"),
        ("xmessage", "x11-utils"),
    ] {
        assert!(
            on_path(program),
            "this test needs {program} on PATH: install the {package} package (sudo apt install \
             {package}); a missing display server is a failure, not a skip"
        );
    }
}

fn pid_gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                == Some('Z')
        }
    }
}

fn wait_gone(pids: &[i32], within: Duration) -> Vec<i32> {
    let deadline = Instant::now() + within;
    loop {
        let alive: Vec<i32> = pids.iter().copied().filter(|p| !pid_gone(*p)).collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether any Xvfb process names `dir` on its command line (its `-auth` path does).
fn xvfb_running_under(dir: &Path) -> bool {
    let needle = dir.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
        .any(|cmd| cmd.contains("Xvfb") && cmd.contains(&needle))
}

// ── the rig ──────────────────────────────────────────────────────────────────

struct Rig {
    root: TempDir,
    config: Option<String>,
    path: Option<OsString>,
    runtime_env: bool,
    host_display: Option<(String, PathBuf)>,
    pids: RefCell<BTreeSet<i32>>,
}

impl Rig {
    fn new(grants: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let rig = Self {
            root,
            config: None,
            path: std::env::var_os("PATH"),
            runtime_env: true,
            host_display: None,
            pids: RefCell::new(BTreeSet::new()),
        };
        std::fs::create_dir_all(rig.runtime().join("tools")).unwrap();
        std::fs::create_dir_all(rig.work()).unwrap();
        for name in grants {
            rig.grant(name);
        }
        rig
    }

    fn with_config(mut self, config: &str) -> Self {
        self.config = Some(config.to_string());
        self
    }

    fn grant(&self, name: &str) {
        let dir = self.runtime().join("tools").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("murmur.yaml"), format!("name: {name}\n")).unwrap();
    }

    fn runtime(&self) -> PathBuf {
        self.root
            .path()
            .join("sessions")
            .join(SESSION)
            .join("runtime")
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn state(&self) -> PathBuf {
        self.work().join(".tool-computer").join(SESSION)
    }

    fn display(&self) -> Value {
        let text = std::fs::read_to_string(self.state().join("display.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn remember_pids(&self) {
        let Ok(text) = std::fs::read_to_string(self.state().join("display.json")) else {
            return;
        };
        let Ok(info) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let mut pids = self.pids.borrow_mut();
        for key in ["keeper_pid", "xvfb_pid"] {
            pids.insert(info[key].as_i64().unwrap() as i32);
        }
        for launched in info["launched"].as_array().unwrap() {
            pids.insert(launched["pid"].as_i64().unwrap() as i32);
        }
    }

    fn call(&self, input: Value) -> Value {
        let mut command = Command::new(BIN);
        command
            .current_dir(self.work())
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .env_remove("MURMUR_ARTIFACT_CONFIG")
            .env_remove("MURMUR_RUNTIME_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match &self.path {
            Some(path) => command.env("PATH", path),
            None => command.env_remove("PATH"),
        };
        if self.runtime_env {
            command.env("MURMUR_RUNTIME_DIR", self.runtime());
        }
        if let Some(config) = &self.config {
            command.env("MURMUR_ARTIFACT_CONFIG", config);
        }
        if let Some((display, xauth)) = &self.host_display {
            command.env("DISPLAY", display).env("XAUTHORITY", xauth);
        }
        let started = Instant::now();
        let mut child = command.spawn().expect("spawning tool-computer");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"data": input, "log_path": "log"})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        // The runtime waits for end of file on stdout and stderr, so a call that leaves anything
        // holding them would hang here.
        let output = child.wait_with_output().unwrap();
        let took = started.elapsed();
        self.remember_pids();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "one JSON object on stdout, got: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut result: Value = serde_json::from_str(stdout.trim()).unwrap();
        result["took_ms"] = json!(took.as_millis() as u64);
        result
    }

    fn passed(&self, input: Value) -> Value {
        let result = self.call(input.clone());
        assert_eq!(result["status"], "passed", "{input} → {result}");
        result
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.remember_pids();
        let _ = std::fs::remove_dir_all(self.runtime());
        let pids: Vec<i32> = self
            .pids
            .borrow()
            .iter()
            .copied()
            .filter(|p| *p > 0)
            .collect();
        let alive = wait_gone(&pids, Duration::from_secs(5));
        if !std::thread::panicking() {
            assert!(
                alive.is_empty(),
                "still running 5 s after the runtime folder went: {alive:?}"
            );
        }
    }
}

fn data(result: &Value) -> &str {
    result["data"].as_str().unwrap()
}

fn lines(result: &Value) -> Vec<&str> {
    data(result).lines().collect()
}

/// The first line after any restart line and banner.
fn action_line(result: &Value) -> &str {
    lines(result)
        .into_iter()
        .find(|l| !l.starts_with("display: ") && !l.starts_with("the previous display stopped"))
        .unwrap()
}

// ── PNG ──────────────────────────────────────────────────────────────────────

struct Png {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl Png {
    fn read(path: &Path) -> Self {
        let bytes = std::fs::read(path).unwrap();
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

    fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y * self.width + x) as usize * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// Whether every pixel of the box is the background colour, read at (0,0).
    fn box_is_background(&self, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> bool {
        let background = self.pixel(0, 0);
        (y0..y1).all(|y| (x0..x1).all(|x| self.pixel(x, y) == background))
    }
}

fn screenshot_path(rig: &Rig, result: &Value) -> PathBuf {
    rig.work()
        .join(result["attachments"][0]["path"].as_str().unwrap())
}

// ── S1 ───────────────────────────────────────────────────────────────────────

#[test]
fn s1_the_first_screenshot_starts_a_display_and_attaches_a_png() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp"]);
    let result = rig.passed(json!({"action": "screenshot"}));
    let lines = lines(&result);
    assert_eq!(lines.len(), 4, "{lines:?}");
    let banner = lines[0];
    let display = rig.display()["display"].as_u64().unwrap();
    assert_eq!(
        banner,
        format!(
            "display: Xvfb 127.0.0.1:{display}, 1440x900x24; {NO_WM}; launched nothing; shell \
             commands reach it with DISPLAY=127.0.0.1:{display} XAUTHORITY={}",
            rig.state().join("Xauthority").display()
        )
    );
    assert_eq!(lines[1], "screenshot");
    assert!(lines[2].starts_with("cursor: "), "{}", lines[2]);
    assert_eq!(
        lines[3],
        "screenshot: .tool-computer/ses_test/screen-1.png (1440x900)"
    );
    assert_eq!(result["summary"], "screenshot");
    assert_eq!(
        result["attachments"],
        json!([{"media_type": "image/png", "path": ".tool-computer/ses_test/screen-1.png"}])
    );
    let png = Png::read(&rig.state().join("screen-1.png"));
    assert_eq!((png.width, png.height), (1440, 900));
    let mode = std::fs::metadata(rig.state().join("display.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let mode = std::fs::metadata(rig.state().join("Xauthority"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    println!(
        "first action (display start) took {} ms; screenshot PNG {} bytes",
        result["took_ms"],
        std::fs::metadata(rig.state().join("screen-1.png"))
            .unwrap()
            .len()
    );
}

// ── S2 ───────────────────────────────────────────────────────────────────────

#[test]
fn s2_clicking_the_known_button_changes_the_screen() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp", "xmessage"]).with_config(XMESSAGE_CONFIG);
    let first = rig.passed(json!({"action": "screenshot"}));
    assert!(
        lines(&first)[0].contains("; launched xmessage; "),
        "{}",
        data(&first)
    );
    let first_png = Png::read(&screenshot_path(&rig, &first));
    assert!(
        !first_png.box_is_background(BOX),
        "xmessage's window is drawn in {BOX:?}"
    );

    let click = rig.passed(json!({"action": "left_click", "x": PRESS.0, "y": PRESS.1}));
    assert_eq!(click["summary"], "left_click at 46,60");
    assert!(data(&click).contains("cursor: 46,60"), "{}", data(&click));

    std::thread::sleep(Duration::from_millis(1200));
    let last = rig.passed(json!({"action": "screenshot"}));
    let exit_line = "launched xmessage (launch[0]) exited with status 0; output in \
                     .tool-computer/ses_test/launch-0-xmessage.log";
    assert!(
        data(&click).contains(exit_line) || data(&last).contains(exit_line),
        "the exit is reported:\n{}\n---\n{}",
        data(&click),
        data(&last)
    );
    let log = std::fs::read_to_string(rig.state().join("launch-0-xmessage.log")).unwrap();
    assert!(
        log.contains("Press"),
        "xmessage printed its button: {log:?}"
    );
    let last_png = Png::read(&screenshot_path(&rig, &last));
    assert!(last_png.box_is_background(BOX), "the window is gone");
    assert_ne!(
        std::fs::read(screenshot_path(&rig, &first)).unwrap(),
        std::fs::read(screenshot_path(&rig, &last)).unwrap()
    );
}

// ── S3 ───────────────────────────────────────────────────────────────────────

#[test]
fn s3_a_later_call_reuses_the_display() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp"]);
    rig.passed(json!({"action": "screenshot"}));
    let xvfb = rig.display()["xvfb_pid"].clone();
    let second = rig.passed(json!({"action": "screenshot"}));
    assert_eq!(
        lines(&second)[0],
        "screenshot",
        "no banner: {}",
        data(&second)
    );
    let moved = rig.passed(json!({"action": "mouse_move", "x": 700, "y": 450}));
    assert_eq!(lines(&moved)[0], "mouse_move to 700,450");
    assert_eq!(lines(&moved)[1], "cursor: 700,450");
    assert_eq!(rig.display()["xvfb_pid"], xvfb);
    println!("warm mouse_move took {} ms", moved["took_ms"]);
}

// ── S4 ───────────────────────────────────────────────────────────────────────

#[test]
fn s4_every_action_kind_passes_with_its_action_line() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp"]);
    let cases = [
        (
            json!({"action":"left_click","x":100,"y":100}),
            "left_click at 100,100",
        ),
        (
            json!({"action":"right_click","x":200,"y":200}),
            "right_click at 200,200",
        ),
        (
            json!({"action":"middle_click"}),
            "middle_click at the cursor",
        ),
        (
            json!({"action":"double_click","x":10,"y":10,"modifiers":"ctrl"}),
            "double_click at 10,10 with ctrl",
        ),
        (
            json!({"action":"triple_click","x":20,"y":20}),
            "triple_click at 20,20",
        ),
        (
            json!({"action":"left_click_drag","x":10,"y":10,"to_x":300,"to_y":300}),
            "left_click_drag from 10,10 to 300,300",
        ),
        (
            json!({"action":"left_mouse_down","x":400,"y":400}),
            "left_mouse_down at 400,400",
        ),
        (
            json!({"action":"mouse_move","x":500,"y":500}),
            "mouse_move to 500,500",
        ),
        (
            json!({"action":"left_mouse_up"}),
            "left_mouse_up at the cursor",
        ),
        (
            json!({"action":"scroll","scroll_direction":"down","scroll_amount":2,"modifiers":"shift"}),
            "scroll down by 2 at the cursor with shift",
        ),
        (
            json!({"action":"type","text":"ab C!é"}),
            "typed 6 characters",
        ),
        (
            json!({"action":"key","key":"ctrl+a Return"}),
            "key ctrl+a Return",
        ),
        (
            json!({"action":"hold_key","key":"shift","duration":0.2}),
            "hold_key shift for 0.2 s",
        ),
        (json!({"action":"wait","duration":0.1}), "waited 0.1 s"),
        (
            json!({"action":"zoom","x":0,"y":0,"to_x":320,"to_y":200}),
            "zoom of 0,0 to 320,200 (320x200 at 1:1)",
        ),
    ];
    for (input, line) in cases {
        let result = rig.passed(input.clone());
        assert_eq!(action_line(&result), line, "{input}");
        assert_eq!(result["summary"], line);
        assert_eq!(result["attachments"].as_array().unwrap().len(), 1);
    }
    let zoom = rig.passed(json!({"action":"zoom","x":0,"y":0,"to_x":320,"to_y":200}));
    assert!(data(&zoom).contains("(320x200)\n") || data(&zoom).ends_with("(320x200)"));
    let png = Png::read(&screenshot_path(&rig, &zoom));
    assert_eq!((png.width, png.height), (320, 200));
    let after_drag = rig.passed(json!({"action":"mouse_move","x":1439,"y":899}));
    assert_eq!(lines(&after_drag)[1], "cursor: 1439,899");
}

// ── S5 ───────────────────────────────────────────────────────────────────────

#[test]
fn s5_refused_inputs_name_what_is_wrong_and_start_nothing() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp"]);
    let cases: [(Value, &[&str]); 9] = [
        (
            json!({"action":"left_click","x":1440,"y":10}),
            &["x is 1440", "0–1439"],
        ),
        (json!({"action":"left_click","x":5}), &["x needs y"]),
        (
            json!({"action":"mouse_move"}),
            &["mouse_move needs x and y"],
        ),
        (json!({"action":"fly"}), &["'fly' is not an action"]),
        (
            json!({"action":"left_click","x":1,"y":1,"text":"shift"}),
            &["left_click does not take text", "x, y and modifiers"],
        ),
        (json!({"action":"key","key":"ctrl+bogus"}), &["'bogus'"]),
        (
            json!({"action":"zoom","x":10,"y":10,"to_x":5,"to_y":20}),
            &["to_x greater than x"],
        ),
        (
            json!({"action":"wait","duration":61}),
            &["duration is 61", "at most 60"],
        ),
        (
            json!({"action":"scroll","scroll_direction":"sideways"}),
            &["'sideways'"],
        ),
    ];
    for (input, needles) in cases {
        let result = rig.call(input.clone());
        assert_eq!(result["status"], "error", "{input}");
        assert_eq!(result["summary"], result["data"]);
        assert!(result.get("attachments").is_none());
        let line = data(&result);
        assert_eq!(line.lines().count(), 1);
        for needle in needles {
            assert!(line.contains(needle), "{input}: {line}");
        }
    }
    assert!(!rig.work().join(".tool-computer").exists());
}

// ── S6 ───────────────────────────────────────────────────────────────────────

#[test]
fn s6_without_grants_the_line_names_what_to_add() {
    require_host();
    let rig = Rig::new(&[]);
    let result = rig.call(json!({"action":"screenshot"}));
    let line =
        "tool-computer needs a display server: add Xvfb and xkbcomp to capabilities.shell.allow";
    assert_eq!(result["status"], "error");
    assert_eq!(result["summary"], line);
    assert_eq!(result["data"], line);
    assert!(!rig.work().join(".tool-computer").exists());
    assert!(!xvfb_running_under(rig.root.path()));

    rig.grant("Xvfb");
    let result = rig.call(json!({"action":"screenshot"}));
    assert_eq!(
        result["data"],
        "tool-computer needs a display server: add xkbcomp to capabilities.shell.allow"
    );

    let mut unset = Rig::new(&["Xvfb", "xkbcomp"]);
    unset.runtime_env = false;
    let result = unset.call(json!({"action":"screenshot"}));
    assert_eq!(
        result["data"], line,
        "an unset MURMUR_RUNTIME_DIR grants nothing"
    );
    assert!(!unset.work().join(".tool-computer").exists());
    assert!(!xvfb_running_under(unset.root.path()));
}

// ── S7 ───────────────────────────────────────────────────────────────────────

#[test]
fn s7_a_granted_program_missing_from_path_names_its_package() {
    require_host();
    let mut rig = Rig::new(&["Xvfb", "xkbcomp"]);
    let bin = rig.root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    rig.path = Some(bin.clone().into_os_string());
    let result = rig.call(json!({"action":"screenshot"}));
    assert_eq!(
        result["data"],
        "tool-computer: capabilities.shell.allow grants Xvfb, but no Xvfb is on PATH; install the \
         xvfb package on this host first (mur deploy run does not upload host programs)"
    );
    let xvfb = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("Xvfb"))
        .find(|p| p.is_file())
        .unwrap();
    std::os::unix::fs::symlink(xvfb, bin.join("Xvfb")).unwrap();
    let result = rig.call(json!({"action":"screenshot"}));
    assert_eq!(
        result["data"],
        "tool-computer: capabilities.shell.allow grants xkbcomp, but no xkbcomp is on PATH; install \
         the x11-xkb-utils package on this host first (mur deploy run does not upload host programs)"
    );
    assert!(!rig.work().join(".tool-computer").exists());
}

// ── S8 ───────────────────────────────────────────────────────────────────────

#[test]
fn s8_config_refusals_are_one_line_and_start_nothing() {
    require_host();
    let cases = [
        (r#"{"size":1}"#, "tool-computer: config key 'size' is not a setting; the settings are launch and confirm"),
        (r#"{"launch":[["zenity","--info"]]}"#, "tool-computer: config launch[0] runs zenity, which capabilities.shell.allow does not grant"),
        (r#"{"launch":[["/usr/bin/xmessage","hi"]]}"#, "tool-computer: config launch[0] names a path (/usr/bin/xmessage); give the program's name as capabilities.shell.allow lists it"),
        (r#"{"confirm":["explode"]}"#, "tool-computer: config confirm lists 'explode', which is not an action"),
        (r#"{"launch":"xmessage"}"#, "tool-computer: config launch is not a list of programs; give it as [[program, argument, …], …]"),
    ];
    for (config, line) in cases {
        let rig = Rig::new(&["Xvfb", "xkbcomp", "xmessage"]).with_config(config);
        let result = rig.call(json!({"action":"screenshot"}));
        assert_eq!(result["status"], "error", "{config}");
        assert_eq!(result["data"], line, "{config}");
        assert!(!rig.work().join(".tool-computer").exists(), "{config}");
    }
}

// ── S9 ───────────────────────────────────────────────────────────────────────

#[test]
fn s9_a_confirm_listed_action_is_held_and_never_runs() {
    require_host();
    let config = r#"{"launch":[["xmessage","-print","-geometry","+20+20","-buttons","Press:0","hello"]],"confirm":["left_click"]}"#;
    let rig = Rig::new(&["Xvfb", "xkbcomp", "xmessage"]).with_config(config);
    rig.passed(json!({"action":"screenshot"}));
    let held = rig.call(json!({"action":"left_click","x":PRESS.0,"y":PRESS.1}));
    let line = "tool-computer did not run left_click: this capsule's config lists left_click in \
                confirm, so it needs a person's yes, and murmur gives a native tool no way to ask \
                for one; ask the person to do this step, or to remove left_click from confirm";
    assert_eq!(held["status"], "error");
    assert_eq!(held["summary"], line);
    assert_eq!(held["data"], line);
    assert!(held.get("attachments").is_none());
    std::thread::sleep(Duration::from_millis(500));
    let after = rig.passed(json!({"action":"screenshot"}));
    assert!(
        !Png::read(&screenshot_path(&rig, &after)).box_is_background(BOX),
        "the window is still up"
    );
    let log = std::fs::read_to_string(rig.state().join("launch-0-xmessage.log")).unwrap();
    assert_eq!(log, "", "xmessage was never clicked");
}

// ── S10 ──────────────────────────────────────────────────────────────────────

/// An X server standing in for the host's screen; killed on drop.
struct HostX {
    child: Child,
    display: u16,
    cookie: Vec<u8>,
    _dir: TempDir,
    auth: PathBuf,
}

impl HostX {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("host.auth");
        let cookie: Vec<u8> = (0..16).map(|i| 0xa0 ^ i).collect();
        // FamilyWild, no address, any display, MIT-MAGIC-COOKIE-1.
        let mut bytes = vec![0xff, 0xff, 0, 0, 0, 0, 0, 18];
        bytes.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        bytes.extend_from_slice(&[0, 16]);
        bytes.extend_from_slice(&cookie);
        std::fs::write(&auth, bytes).unwrap();
        let fd_file = dir.path().join("displayfd");
        let out = std::fs::File::create(&fd_file).unwrap();
        let mut command = Command::new("Xvfb");
        command
            .args([
                "-displayfd",
                "3",
                "-screen",
                "0",
                "640x400x24",
                "-listen",
                "tcp",
                "-auth",
            ])
            .arg(&auth)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.path().join("host.log")).unwrap());
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = out.as_raw_fd();
        // SAFETY: dup2 and fcntl are async-signal-safe; they leave the displayfd file at fd 3
        // in the child, open across exec.
        unsafe {
            command.pre_exec(move || {
                let ok = if fd == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0) != -1
                } else {
                    libc::dup2(fd, 3) != -1
                };
                if !ok {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let display = loop {
            let text = std::fs::read_to_string(&fd_file).unwrap_or_default();
            if text.ends_with('\n') {
                break text.trim().parse().unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "the host Xvfb gave no display: {}",
                std::fs::read_to_string(dir.path().join("host.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        Self {
            child,
            display,
            cookie,
            _dir: dir,
            auth,
        }
    }

    fn root_is_uniformly_black(&self) -> bool {
        let conn = connect(self.display, Some(&self.cookie)).unwrap();
        let root = conn.setup().roots[0].root;
        let image = conn
            .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, 640, 400, !0)
            .unwrap()
            .reply()
            .unwrap();
        image.data.chunks(4).all(|px| px[..3] == [0, 0, 0])
    }
}

impl Drop for HostX {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn connect(display: u16, cookie: Option<&[u8]>) -> Result<RustConnection, String> {
    let tcp = TcpStream::connect(("127.0.0.1", 6000 + display)).map_err(|e| e.to_string())?;
    let (stream, _) = DefaultStream::from_tcp_stream(tcp).map_err(|e| e.to_string())?;
    let (name, data) = match cookie {
        Some(c) => (b"MIT-MAGIC-COOKIE-1".to_vec(), c.to_vec()),
        None => (Vec::new(), Vec::new()),
    };
    RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data)
        .map_err(|e| e.to_string())
}

#[test]
fn s10_the_host_screen_is_untouched_and_the_display_needs_the_cookie() {
    require_host();
    let host = HostX::start();
    assert!(host.root_is_uniformly_black());
    let mut rig = Rig::new(&["Xvfb", "xkbcomp", "xmessage"]).with_config(XMESSAGE_CONFIG);
    rig.host_display = Some((format!(":{}", host.display), host.auth.clone()));

    rig.passed(json!({"action":"screenshot"}));
    let tool_display = rig.display()["display"].as_u64().unwrap() as u16;
    assert_ne!(tool_display, host.display);
    assert!(
        host.root_is_uniformly_black(),
        "nothing drew on the host screen"
    );
    rig.passed(json!({"action":"left_click","x":PRESS.0,"y":PRESS.1}));
    assert!(
        host.root_is_uniformly_black(),
        "the click did not reach the host screen"
    );
    let log = std::fs::read_to_string(rig.state().join("launch-0-xmessage.log")).unwrap();
    assert!(
        log.contains("Press"),
        "the click reached the tool's own display"
    );

    let refused = connect(tool_display, None);
    assert!(refused.is_err(), "a client without the cookie is refused");
    let wrong = connect(tool_display, Some(&host.cookie));
    assert!(wrong.is_err(), "a client with another cookie is refused");
}

// ── S11 ──────────────────────────────────────────────────────────────────────

#[test]
fn s11_a_gone_runtime_folder_stops_the_keeper_xvfb_and_programs() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp", "xmessage"]).with_config(XMESSAGE_CONFIG);
    rig.passed(json!({"action":"screenshot"}));
    let info = rig.display();
    let pids: Vec<i32> = [
        info["keeper_pid"].as_i64().unwrap(),
        info["xvfb_pid"].as_i64().unwrap(),
        info["launched"][0]["pid"].as_i64().unwrap(),
    ]
    .map(|p| p as i32)
    .to_vec();
    assert!(
        pids.iter().all(|p| !pid_gone(*p)),
        "all three run: {pids:?}"
    );
    std::fs::remove_dir_all(rig.runtime()).unwrap();
    assert_eq!(wait_gone(&pids, Duration::from_secs(5)), Vec::<i32>::new());
    let deadline = Instant::now() + Duration::from_secs(2);
    while rig.state().join("display.json").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!rig.state().join("display.json").exists());
}

#[test]
fn s11_a_killed_xvfb_is_replaced_on_the_next_call() {
    require_host();
    let rig = Rig::new(&["Xvfb", "xkbcomp", "xmessage"]).with_config(XMESSAGE_CONFIG);
    rig.passed(json!({"action":"screenshot"}));
    let old = rig.display()["xvfb_pid"].as_i64().unwrap() as i32;
    // SAFETY: kill(2) on the Xvfb this test's tool started.
    unsafe {
        libc::kill(old, libc::SIGKILL);
    }
    let result = rig.passed(json!({"action":"screenshot"}));
    let lines = lines(&result);
    assert_eq!(
        lines[0],
        "the previous display stopped; programs on it are gone"
    );
    assert!(
        lines[1].starts_with("display: Xvfb 127.0.0.1:"),
        "{}",
        lines[1]
    );
    assert!(lines[1].contains("launched xmessage;"));
    let new = rig.display()["xvfb_pid"].as_i64().unwrap() as i32;
    assert_ne!(new, old);
    assert_eq!(lines[2], "screenshot");
}
