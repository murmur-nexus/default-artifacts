//! S1–S13: the compiled component in Wasmtime, against this host's real `Xvfb`, `xkbcomp` and
//! `xmessage`, through the test host for `murmur:process` in `common`.
//!
//! A host without `Xvfb`, `xkbcomp` or `xmessage` fails naming the package to install; nothing
//! here skips.

#![cfg(target_os = "linux")]

mod common;

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::{DefaultStream, RustConnection};

const XMESSAGE_CONFIG: &str =
    r#"{"launch":[["xmessage","-print","-geometry","+20+20","-buttons","Press:0","hello"]]}"#;
/// Where xmessage draws its window with no window manager, and its `Press` button.
const BOX: (u32, u32, u32, u32) = (20, 20, 130, 75);
const PRESS: (u32, u32) = (46, 60);
const NO_WM: &str =
    "window manager none (grant one of openbox, fluxbox, icewm, twm or matchbox-window-manager)";
const DISPLAY: [&str; 2] = ["Xvfb", "xkbcomp"];
const WITH_XMESSAGE: [&str; 3] = ["Xvfb", "xkbcomp", "xmessage"];

/// The first line after any restart line and banner.
fn action_line(result: &Value) -> &str {
    lines(result)
        .into_iter()
        .find(|l| !l.starts_with("display: ") && !l.starts_with("the previous display stopped"))
        .unwrap()
}

fn screenshot_path(env: &Env, result: &Value) -> PathBuf {
    env.work
        .join(result["attachments"][0]["path"].as_str().unwrap())
}

fn xmessage_log(env: &Env) -> String {
    std::fs::read_to_string(env.state().join("launch-0-xmessage.log")).unwrap_or_default()
}

// ── S1 ───────────────────────────────────────────────────────────────────────

#[test]
fn s1_the_first_screenshot_starts_a_display_and_attaches_a_png() {
    require_host();
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(XMESSAGE_CONFIG);
    let result = env.passed(json!({"action": "screenshot"}));
    let lines = lines(&result);
    assert_eq!(lines.len(), 4, "{lines:?}");
    let info = env.info();
    let display = info["display"].as_u64().unwrap();
    let rel = env.state_rel();
    assert!(
        rel.len() == ".tool-computer/".len() + 16
            && rel[".tool-computer/".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "the id is 16 lowercase hex: {rel}"
    );
    assert_eq!(
        lines[0],
        format!(
            "display: Xvfb 127.0.0.1:{display}, 1440x900x24; {NO_WM}; launched xmessage; shell \
             commands reach it with DISPLAY=127.0.0.1:{display} XAUTHORITY={rel}/Xauthority"
        )
    );
    assert_eq!(lines[1], "screenshot");
    assert!(lines[2].starts_with("cursor: "), "{}", lines[2]);
    assert_eq!(
        lines[3],
        format!("screenshot: {rel}/screen-1.png (1440x900)")
    );
    assert_eq!(result["summary"], "screenshot");
    assert_eq!(
        result["attachments"],
        json!([{"media_type": "image/png", "path": format!("{rel}/screen-1.png")}])
    );
    let png = Png::read(&env.state().join("screen-1.png"));
    assert_eq!((png.width, png.height), (1440, 900));
    assert!(
        !png.box_is_background(BOX),
        "xmessage's window is drawn in {BOX:?}"
    );
    assert_eq!(info["port"].as_u64().unwrap(), 6000 + display);
    assert_eq!(info["address"], format!("127.0.0.1:{display}"));
    assert_eq!(env.starts_of("display"), 1);
    assert_eq!(env.starts_of("launch-0"), 1);
    println!(
        "first call (display start) took {} ms; screenshot PNG {} bytes",
        result["took_ms"],
        std::fs::metadata(env.state().join("screen-1.png"))
            .unwrap()
            .len()
    );
}

// ── S2 ───────────────────────────────────────────────────────────────────────

#[test]
fn s2_clicking_the_known_button_changes_the_screen() {
    require_host();
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(XMESSAGE_CONFIG);
    let first = env.passed(json!({"action": "screenshot"}));
    let first_png = Png::read(&screenshot_path(&env, &first));
    assert!(!first_png.box_is_background(BOX));

    let click = env.passed(json!({"action": "left_click", "x": PRESS.0, "y": PRESS.1}));
    assert_eq!(click["summary"], "left_click at 46,60");
    assert_eq!(action_line(&click), "left_click at 46,60");
    assert!(data(&click).contains("cursor: 46,60"), "{}", data(&click));
    println!("warm left_click took {} ms", click["took_ms"]);

    std::thread::sleep(Duration::from_millis(1200));
    let last = env.passed(json!({"action": "screenshot"}));
    println!("warm screenshot took {} ms", last["took_ms"]);
    let exit_line = format!(
        "launched xmessage (launch[0]) exited with status 0; output in {}/launch-0-xmessage.log",
        env.state_rel()
    );
    let count = data(&click).matches(&exit_line).count() + data(&last).matches(&exit_line).count();
    assert_eq!(
        count,
        1,
        "the exit is reported exactly once:\n{}\n---\n{}",
        data(&click),
        data(&last)
    );
    let again = env.passed(json!({"action": "screenshot"}));
    assert!(!data(&again).contains("exited"), "{}", data(&again));
    let log = xmessage_log(&env);
    assert!(
        log.contains("Press"),
        "xmessage printed its button: {log:?}"
    );
    let last_png = Png::read(&screenshot_path(&env, &last));
    assert!(last_png.box_is_background(BOX), "the window is gone");
    assert_ne!(
        std::fs::read(screenshot_path(&env, &first)).unwrap(),
        std::fs::read(screenshot_path(&env, &last)).unwrap()
    );
}

// ── S3 ───────────────────────────────────────────────────────────────────────

#[test]
fn s3_a_later_call_reuses_the_display() {
    require_host();
    let env = Env::with_tools(&DISPLAY);
    env.passed(json!({"action": "screenshot"}));
    let handle = env.info()["handle"].clone();
    let second = env.passed(json!({"action": "screenshot"}));
    assert_eq!(
        lines(&second)[0],
        "screenshot",
        "no banner: {}",
        data(&second)
    );
    let moved = env.passed(json!({"action": "mouse_move", "x": 700, "y": 450}));
    assert_eq!(lines(&moved)[0], "mouse_move to 700,450");
    assert_eq!(lines(&moved)[1], "cursor: 700,450");
    assert_eq!(env.info()["handle"], handle);
    assert_eq!(env.starts_of("display"), 1, "no second display start");
    assert_eq!(env.state_dirs().len(), 1);
    println!(
        "warm screenshot took {} ms; warm mouse_move took {} ms",
        second["took_ms"], moved["took_ms"]
    );
}

// ── S4 ───────────────────────────────────────────────────────────────────────

#[test]
fn s4_every_action_kind_passes_with_its_action_line() {
    require_host();
    let env = Env::with_tools(&DISPLAY);
    let cases = [
        (json!({"action":"screenshot"}), "screenshot"),
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
    assert_eq!(cases.len(), 16, "every action");
    for (input, line) in cases {
        let result = env.passed(input.clone());
        assert_eq!(action_line(&result), line, "{input}");
        assert_eq!(result["summary"], line);
        assert_eq!(result["attachments"].as_array().unwrap().len(), 1);
    }
    let zoom = env.passed(json!({"action":"zoom","x":0,"y":0,"to_x":320,"to_y":200}));
    assert!(data(&zoom).ends_with("(320x200)"), "{}", data(&zoom));
    let png = Png::read(&screenshot_path(&env, &zoom));
    assert_eq!((png.width, png.height), (320, 200));
    let after_drag = env.passed(json!({"action":"mouse_move","x":1439,"y":899}));
    assert_eq!(lines(&after_drag)[1], "cursor: 1439,899");
    // The spare keycode `é` was typed on is cleared again.
    let typed = env.passed(json!({"action":"type","text":"é"}));
    assert_eq!(action_line(&typed), "typed 1 character");
}

// ── S5 ───────────────────────────────────────────────────────────────────────

#[test]
fn s5_refused_inputs_name_what_is_wrong_and_start_nothing() {
    require_host();
    let env = Env::with_tools(&DISPLAY);
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
        let result = env.call(input.clone());
        assert_eq!(result["status"], "error", "{input}");
        assert_eq!(result["summary"], result["data"]);
        assert!(result.get("attachments").is_none());
        let line = data(&result);
        assert_eq!(line.lines().count(), 1);
        for needle in needles {
            assert!(line.contains(needle), "{input}: {line}");
        }
    }
    assert!(!env.work.join(".tool-computer").exists());
    assert_eq!(env.starts(), 0);
}

// ── S6 ───────────────────────────────────────────────────────────────────────

#[test]
fn s6_without_grants_the_line_names_what_to_add() {
    require_host();
    let env = Env::with_tools(&[]);
    let result = env.call(json!({"action":"screenshot"}));
    let line =
        "tool-computer needs a display server: add Xvfb and xkbcomp to capabilities.shell.allow";
    assert_eq!(result["status"], "error");
    assert_eq!(result["summary"], line);
    assert_eq!(result["data"], line);
    assert!(!env.work.join(".tool-computer").exists());

    env.grant("Xvfb");
    let result = env.call(json!({"action":"screenshot"}));
    assert_eq!(
        result["data"],
        "tool-computer needs a display server: add xkbcomp to capabilities.shell.allow"
    );
    assert_eq!(env.starts(), 0);

    let mut scoped = Env::with_tools(&DISPLAY);
    scoped.with_runtime = false;
    let result = scoped.call(json!({"action":"screenshot"}));
    assert_eq!(
        result["data"],
        "tool-computer cannot see which programs capabilities.shell.allow grants, because its \
         entry declares capabilities.filesystem.scope; tool-computer runs only from an entry \
         without one"
    );
    assert!(!scoped.work.join(".tool-computer").exists());
    assert_eq!(scoped.starts(), 0);
}

// ── S7 ───────────────────────────────────────────────────────────────────────

#[test]
fn s7_xvfb_missing_from_path_names_its_package() {
    require_host();
    let env = Env::with_tools(&DISPLAY).with_empty_path();
    let result = env.call(json!({"action":"screenshot"}));
    assert_eq!(result["status"], "error");
    assert_eq!(
        result["data"],
        "tool-computer: capabilities.shell.allow grants Xvfb, but no Xvfb is on PATH; install the \
         xvfb package on this host first (mur deploy run does not upload host programs)"
    );
    assert!(
        !env.work.join(".tool-computer").exists(),
        "no state folder is left"
    );
}

// ── S8 ───────────────────────────────────────────────────────────────────────

#[test]
fn s8_config_refusals_are_one_line_and_start_nothing() {
    require_host();
    let seven = format!(r#"{{"launch":[{}]}}"#, [r#"["xmessage"]"#; 7].join(","));
    let cases = [
        (r#"{"size":1}"#, "tool-computer: config key 'size' is not a setting; the settings are launch and confirm"),
        (r#"{"launch":[["zenity","--info"]]}"#, "tool-computer: config launch[0] runs zenity, which capabilities.shell.allow does not grant"),
        (r#"{"launch":[["/usr/bin/xmessage","hi"]]}"#, "tool-computer: config launch[0] names a path (/usr/bin/xmessage); give the program's name as capabilities.shell.allow lists it"),
        (r#"{"confirm":["explode"]}"#, "tool-computer: config confirm lists 'explode', which is not an action"),
        (r#"{"launch":"xmessage"}"#, "tool-computer: config launch is not a list of programs; give it as [[program, argument, …], …]"),
        (seven.as_str(), "tool-computer: config launch holds 7 programs; at most 6 are allowed"),
    ];
    for (config, line) in cases {
        let env = Env::with_tools(&WITH_XMESSAGE).with_config(config);
        let result = env.call(json!({"action":"screenshot"}));
        assert_eq!(result["status"], "error", "{config}");
        assert_eq!(result["data"], line, "{config}");
        assert!(!env.work.join(".tool-computer").exists(), "{config}");
        assert_eq!(env.starts(), 0, "{config}");
    }
}

// ── S9 ───────────────────────────────────────────────────────────────────────

#[test]
fn s9_a_confirm_listed_action_is_held_and_never_runs() {
    require_host();
    let config = r#"{"launch":[["xmessage","-print","-geometry","+20+20","-buttons","Press:0","hello"]],"confirm":["left_click"]}"#;
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(config);
    env.passed(json!({"action":"screenshot"}));
    let held = env.call(json!({"action":"left_click","x":PRESS.0,"y":PRESS.1}));
    let line = "tool-computer did not run left_click: this capsule's config lists left_click in \
                confirm, so it needs a person's yes, and a tool cannot ask a person for one; ask \
                the person to do this step, or to remove left_click from confirm";
    assert_eq!(held["status"], "error");
    assert_eq!(held["summary"], line);
    assert_eq!(held["data"], line);
    assert!(held.get("attachments").is_none());
    std::thread::sleep(Duration::from_millis(500));
    let after = env.passed(json!({"action":"screenshot"}));
    assert!(
        !Png::read(&screenshot_path(&env, &after)).box_is_background(BOX),
        "the window is still up"
    );
    // A click makes xmessage print its button's name and exit. The log also holds xmessage's
    // stderr, where a host may warn about fonts or locale, so it is not expected to be empty.
    let log = xmessage_log(&env);
    assert!(
        !log.lines().any(|line| line == "Press"),
        "xmessage was never clicked: {log:?}"
    );
    assert!(
        !data(&after).contains("exited"),
        "no exit is reported: {}",
        data(&after)
    );
    assert!(
        env.pid_of("launch-0").is_some(),
        "xmessage is still running"
    );
}

// ── S10 ──────────────────────────────────────────────────────────────────────

/// An X server standing in for the host's screen; killed on drop.
struct HostX {
    child: Child,
    display: u16,
    cookie: Vec<u8>,
    _dir: TempDir,
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
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "640x400x24",
                "-listen",
                "tcp",
                "-auth",
            ])
            .arg(&auth)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(dir.path().join("host.log")).unwrap())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let display = loop {
            let mut byte = [0u8; 1];
            use std::io::Read as _;
            if stdout.read(&mut byte).unwrap_or(0) == 1 {
                if byte[0] == b'\n' {
                    break String::from_utf8_lossy(&got).trim().parse().unwrap();
                }
                got.push(byte[0]);
            }
            assert!(Instant::now() < deadline, "the host Xvfb gave no display");
        };
        Self {
            child,
            display,
            cookie,
            _dir: dir,
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
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(XMESSAGE_CONFIG);

    env.passed(json!({"action":"screenshot"}));
    let tool_display = env.info()["display"].as_u64().unwrap() as u16;
    assert_ne!(tool_display, host.display);
    assert!(
        host.root_is_uniformly_black(),
        "nothing drew on the host screen"
    );
    env.passed(json!({"action":"left_click","x":PRESS.0,"y":PRESS.1}));
    assert!(
        host.root_is_uniformly_black(),
        "the click did not reach the host screen"
    );
    std::thread::sleep(Duration::from_millis(500));
    env.passed(json!({"action":"screenshot"}));
    assert!(
        xmessage_log(&env).contains("Press"),
        "the click reached the tool's own display"
    );

    let refused = connect(tool_display, None);
    assert!(refused.is_err(), "a client without the cookie is refused");
    let wrong = connect(tool_display, Some(&host.cookie));
    assert!(wrong.is_err(), "a client with another cookie is refused");
    let cookie = std::fs::read(env.state().join("Xauthority")).unwrap();
    let right = connect(tool_display, Some(&cookie[cookie.len() - 16..]));
    assert!(
        right.is_ok(),
        "the Xauthority cookie is the display's: {right:?}",
        right = right.err()
    );
}

// ── S11 ──────────────────────────────────────────────────────────────────────

#[test]
fn s11_a_killed_display_is_replaced_on_the_next_call() {
    require_host();
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(XMESSAGE_CONFIG);
    env.passed(json!({"action":"screenshot"}));
    let old_dir = env.state();
    let old_xmessage = u64::from(env.pid_of("launch-0").unwrap());
    let old = env.kill_process("display");
    assert!(wait_gone(u64::from(old), Duration::from_secs(5)));
    let result = env.passed(json!({"action":"screenshot"}));
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
    assert!(lines[1].contains("launched xmessage;"), "{}", lines[1]);
    assert_eq!(lines[2], "screenshot");
    let new_dir = env.state();
    assert_ne!(new_dir, old_dir, "a new <id>");
    assert!(lines[1].contains(&env.state_rel()), "{}", lines[1]);
    assert!(pid_gone(old_xmessage), "the old xmessage is gone");
    assert_eq!(env.starts_of("display"), 2);
    assert_eq!(env.starts_of("launch-0"), 2);
    let png = Png::read(&screenshot_path(&env, &result));
    assert!(!png.box_is_background(BOX), "the new xmessage is up");
}

// ── S13 ──────────────────────────────────────────────────────────────────────

#[test]
fn s13_two_concurrent_first_calls_start_one_display() {
    require_host();
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(XMESSAGE_CONFIG);
    let results: Vec<Value> = std::thread::scope(|scope| {
        let calls: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| env.call(json!({"action":"screenshot"}))))
            .collect();
        calls.into_iter().map(|c| c.join().unwrap()).collect()
    });
    for result in &results {
        assert_eq!(result["status"], "passed", "{result}");
    }
    assert_eq!(env.starts_of("display"), 1, "one display start");
    let banners = results
        .iter()
        .filter(|r| data(r).starts_with("display: Xvfb"))
        .count();
    assert_eq!(banners, 1, "one banner: {results:#?}");
    let folders: Vec<PathBuf> = std::fs::read_dir(env.work.join(".tool-computer"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(folders.len(), 1, "one state folder: {folders:?}");
}

/// The state folder's files after a start, and nothing of the old layout.
#[test]
fn the_state_folder_holds_the_new_layout() {
    require_host();
    let env = Env::with_tools(&WITH_XMESSAGE).with_config(XMESSAGE_CONFIG);
    env.passed(json!({"action":"screenshot"}));
    let mut names: Vec<String> = std::fs::read_dir(env.state())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "Xauthority",
            "display.log",
            "launch-0-xmessage.log",
            "screen-1.png",
            "state.json"
        ]
    );
    let info = env.info();
    for key in [
        "handle",
        "display",
        "port",
        "address",
        "width",
        "height",
        "window_manager",
        "launched",
        "reported_exits",
        "started_at",
    ] {
        assert!(info.get(key).is_some(), "state.json has {key}: {info}");
    }
    assert_eq!(
        info["launched"][0]["handle"],
        json!(env.latest_handle("launch-0"))
    );
}
