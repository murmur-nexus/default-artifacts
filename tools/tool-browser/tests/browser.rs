//! `tool-browser` against a real headless Chrome or Chromium, through its built binary, the way
//! murmur runs it: one process per call, JSON on stdin, one JSON object on stdout.
//!
//! Each test has its own scratch session (see `common::Env`) and so its own browser; a guard
//! closes it and kills anything `supervisor.json` recorded. A host with no browser fails these
//! tests, naming what to install.

#![cfg(target_os = "linux")]

mod common;

use std::{fs, time::Duration};

use common::{data, ref_of, wait_gone, Env, Guard};
use serde_json::json;

const NO_GRANT_LINE: &str = "tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow";

fn open_fixture(env: &Env) -> (u16, serde_json::Value) {
    let port = common::fixture_server();
    let r = env.call(json!({"action": "open", "url": format!("http://127.0.0.1:{port}/")}));
    assert_eq!(r["status"], "passed", "open: {r}");
    (port, r)
}

/// Scenario 1: open answers with the banner, the page line and a ref for `Go`.
#[test]
fn open_returns_banner_page_line_and_a_ref_for_go() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (port, r) = open_fixture(&env);
    let text = data(&r);
    let mut lines = text.lines();
    let banner = lines.next().unwrap();
    assert!(
        banner.starts_with("browser: chromium ")
            && banner.contains(", local, headless; Chrome sandbox "),
        "banner: {banner}"
    );
    assert_eq!(
        lines.next().unwrap(),
        format!("page: Fixture Page — http://127.0.0.1:{port}/")
    );
    assert!(ref_of(text, "button \"Go\"").starts_with('e'));
    assert!(env.state().join("supervisor.json").is_file());
    assert!(env.state().join("profile").is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(env.state().join("supervisor.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "supervisor.json is 0600");
    }
}

/// Scenario 2: a later snapshot keeps the ref, has no banner, and runs on the same browser.
#[test]
fn snapshot_keeps_the_ref_without_banner_on_the_same_browser() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let go = ref_of(data(&r), "button \"Go\"");
    let pid = env.info().unwrap()["browser"]["pid"].as_u64().unwrap();
    let s = env.call(json!({"action": "snapshot"}));
    assert_eq!(s["status"], "passed");
    assert!(data(&s).starts_with("page: Fixture Page"), "no banner: {s}");
    assert_eq!(ref_of(data(&s), "button \"Go\""), go);
    assert_eq!(env.info().unwrap()["browser"]["pid"].as_u64().unwrap(), pid);
    let part = env.call(json!({"action": "snapshot", "ref": go}));
    let part_lines: Vec<&str> = data(&part).lines().collect();
    assert_eq!(part_lines.len(), 2, "page line and the button: {part}");
    assert!(part_lines[1].contains("button \"Go\""));
}

/// Scenario 3: clicking Go reports the heading change and the new console error.
#[test]
fn click_reports_the_change_and_one_new_console_error() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let go = ref_of(data(&r), "button \"Go\"");
    let c = env.call(json!({"action": "click", "ref": go}));
    assert_eq!(c["status"], "passed", "{c}");
    let text = data(&c);
    assert!(text.starts_with("changed: +"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.starts_with("+ ") && l.contains("clicked")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l == "console: 1 new error(s); call console"),
        "{text}"
    );
}

/// Scenario 4: typing and selecting each show the new value.
#[test]
fn type_and_select_show_the_new_values() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let name = ref_of(data(&r), "textbox \"Name\"");
    let colour = ref_of(data(&r), "combobox");
    let t = env.call(json!({"action": "type", "ref": name, "text": "Ada"}));
    assert_eq!(t["status"], "passed", "{t}");
    assert!(
        data(&t)
            .lines()
            .any(|l| l.starts_with("+ ") && l.contains("value=\"Ada\"")),
        "{t}"
    );
    let s = env.call(json!({"action": "select", "ref": colour, "value": "Green"}));
    assert_eq!(s["status"], "passed", "{s}");
    assert!(
        data(&s)
            .lines()
            .any(|l| l.starts_with("+ ") && l.contains("value=\"Green\"")),
        "{s}"
    );
    let bad = env.call(json!({"action": "select", "ref": colour, "value": "Blue"}));
    assert_eq!(bad["status"], "error");
    assert!(data(&bad).contains("Red, Green"), "{bad}");
}

/// Scenario 5: console returns what arrived since the last look, then nothing.
#[test]
fn console_returns_new_entries_then_nothing() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    env.call(json!({"action": "click", "ref": ref_of(data(&r), "button \"Go\"")}));
    let c = env.call(json!({"action": "console"}));
    let text = data(&c);
    assert!(
        text.lines().any(|l| l.starts_with("[error] boom-load")),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("[error] boom-click")),
        "{text}"
    );
    assert_eq!(
        data(&env.call(json!({"action": "console"}))),
        "no new console messages"
    );
}

/// Scenario 6: network reports the 404 once.
#[test]
fn network_reports_the_404_once() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (port, _) = open_fixture(&env);
    let n = env.call(json!({"action": "network"}));
    assert!(
        data(&n)
            .lines()
            .any(|l| l == format!("[404] GET http://127.0.0.1:{port}/missing.js")),
        "{n}"
    );
    assert_eq!(
        data(&env.call(json!({"action": "network"}))),
        "no failed requests"
    );
}

fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    assert_eq!(&bytes[12..16], b"IHDR");
    (
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    )
}

/// Scenario 7: a screenshot is a 1280×800 PNG at the path `data` names, with an attachment;
/// a ref screenshot works too.
#[test]
fn screenshot_saves_the_viewport_png_with_an_attachment() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let s = env.call(json!({"action": "screenshot"}));
    assert_eq!(s["status"], "passed", "{s}");
    let path = s["attachments"][0]["path"].as_str().unwrap().to_string();
    assert_eq!(s["attachments"][0]["media_type"], "image/png");
    assert_eq!(path, ".tool-browser/ses_test/screenshot-1.png");
    assert!(
        data(&s).starts_with(&format!("saved {path} (1280x800, ")),
        "{s}"
    );
    assert_eq!(
        png_size(&fs::read(env.work.join(&path)).unwrap()),
        (1280, 800)
    );

    let go = ref_of(data(&r), "button \"Go\"");
    let e = env.call(json!({"action": "screenshot", "ref": go}));
    assert_eq!(e["status"], "passed", "{e}");
    let path = e["attachments"][0]["path"].as_str().unwrap();
    assert_eq!(path, ".tool-browser/ses_test/screenshot-2.png");
    let (w, h) = png_size(&fs::read(env.work.join(path)).unwrap());
    assert!(
        w > 0 && w < 400 && h > 0 && h < 200,
        "a button-sized image: {w}x{h}"
    );
}

/// Scenario 8: close stops the browser and the supervisor; the next call starts afresh.
#[test]
fn close_stops_everything_and_the_next_call_starts_a_new_browser() {
    let env = Env::new();
    let _guard = Guard(&env);
    open_fixture(&env);
    let info = env.info().unwrap();
    let (sup, browser) = (
        info["pid"].as_u64().unwrap(),
        info["browser"]["pid"].as_u64().unwrap(),
    );
    let c = env.call(json!({"action": "close"}));
    assert_eq!(c["status"], "passed");
    assert_eq!(data(&c), "browser closed");
    assert!(
        wait_gone(browser, Duration::from_secs(5)),
        "browser pid {browser} is gone"
    );
    assert!(
        wait_gone(sup, Duration::from_secs(5)),
        "supervisor pid {sup} is gone"
    );
    assert!(!env.state().join("profile").exists(), "profile/ is removed");
    assert!(!env.state().join("supervisor.json").exists());

    let s = env.call(json!({"action": "snapshot"}));
    assert_eq!(s["status"], "passed", "{s}");
    assert!(
        data(&s).starts_with("browser: chromium "),
        "the banner again: {s}"
    );
    let new_pid = env.info().unwrap()["browser"]["pid"].as_u64().unwrap();
    assert_ne!(new_pid, browser);
}

/// `back` with no history is refused; after a second page it returns to the first.
#[test]
fn back_needs_history() {
    let env = Env::new();
    let _guard = Guard(&env);
    let b = env.call(json!({"action": "back"}));
    assert_eq!(b["status"], "error");
    assert!(data(&b).ends_with("nothing to go back to"), "{b}");
    let (port, _) = open_fixture(&env);
    fs::write(
        env.work.join("second.html"),
        "<title>Second</title><p>two</p>",
    )
    .unwrap();
    env.call(json!({"action": "open", "url": "second.html"}));
    let b = env.call(json!({"action": "back"}));
    assert_eq!(b["status"], "passed", "{b}");
    assert!(
        data(&b).contains(&format!("page: Fixture Page — http://127.0.0.1:{port}/")),
        "{b}"
    );
}

/// `wait` and `press` run, and a wait that times out says what it waited for.
#[test]
fn wait_and_press() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let w = env.call(json!({"action": "wait", "text": "waiting", "ms": 2000}));
    assert_eq!(w["status"], "passed", "{w}");
    assert!(data(&w).contains("no visible change"), "{w}");
    let w = env.call(json!({"action": "wait", "text": "never-there", "ms": 300}));
    assert_eq!(w["status"], "error");
    assert!(
        data(&w).contains("\"never-there\" did not appear within 300 ms"),
        "{w}"
    );
    let name = ref_of(data(&r), "textbox \"Name\"");
    env.call(json!({"action": "type", "ref": name, "text": "Ad"}));
    let p = env.call(json!({"action": "press", "key": "a"}));
    assert_eq!(p["status"], "passed", "{p}");
    assert!(data(&p).contains("value=\"Ada\""), "{p}");
}

/// Scenario 10: with no grant, the one line, no newline, nothing created, nothing started.
#[test]
fn no_grant_gives_exactly_one_line_and_starts_nothing() {
    for unset_runtime in [false, true] {
        let mut env = Env::with_tools(&[]);
        env.with_runtime = !unset_runtime;
        let r = env.call(json!({"action": "open", "url": "about:blank"}));
        assert_eq!(r["status"], "error");
        assert_eq!(r["summary"], NO_GRANT_LINE);
        assert_eq!(r["data"], NO_GRANT_LINE);
        assert!(
            !env.work.join(".tool-browser").exists(),
            "nothing in the work folder"
        );
        assert!(
            common::pids_mentioning(env.dir.path()).is_empty(),
            "no browser process"
        );
    }
}

/// Scenario 11: granted but not installed names the program and `mur deploy run`.
#[test]
fn granted_but_missing_names_chromium_and_mur_deploy_run() {
    let env = Env::new().without_browser_on_path();
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    let line = data(&r);
    assert!(
        line.contains("grants chromium, but no chromium is on PATH"),
        "{line}"
    );
    assert!(line.contains("mur deploy run"), "{line}");
    assert!(!line.contains('\n'));
    assert!(!env.work.join(".tool-browser").exists());
}

/// Scenario 12: each refusal is an error naming the problem.
#[test]
fn refusals_name_the_problem() {
    let env = Env::new();
    let _guard = Guard(&env);
    let cases = [
        (json!({"action": "fly"}), "unknown action \"fly\""),
        (json!({"action": "click"}), "click needs ref"),
        (
            json!({"action": "open", "url": "../outside.html"}),
            "climbs out of the work folder",
        ),
        (
            json!({"action": "open", "url": "javascript:alert(1)"}),
            "open refuses javascript: URLs",
        ),
    ];
    for (request, expected) in cases {
        let r = env.call(request.clone());
        assert_eq!(r["status"], "error", "{request} → {r}");
        assert!(data(&r).contains(expected), "{request} → {r}");
    }
    assert!(
        !env.work.join(".tool-browser").exists(),
        "validation refusals start nothing"
    );

    let r = env.call(json!({"action": "click", "ref": "e999"}));
    assert_eq!(r["status"], "error");
    assert!(
        data(&r).ends_with("unknown ref e999; take a snapshot first"),
        "{r}"
    );

    let mut configured = Env::new();
    configured.config = Some(r#"{"endpoint":"http://x","port":1}"#.into());
    let r = configured.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    assert!(
        data(&r).contains("port") && data(&r).contains("config:"),
        "{r}"
    );
    configured.config = Some(r#"{"endpoint":"http://x"}"#.into());
    let r = configured.call(json!({"action": "snapshot"}));
    assert!(data(&r).contains("ws:// or wss://"), "{r}");
    assert!(!configured.work.join(".tool-browser").exists());
}

/// A remote endpoint nobody listens on names the host and `capabilities.network.allow`.
#[test]
fn unreachable_remote_endpoint_names_network_allow() {
    let mut env = Env::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    env.config = Some(format!(
        r#"{{"endpoint":"ws://127.0.0.1:{port}/devtools/browser/x"}}"#
    ));
    let _guard = Guard(&env);
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    assert!(
        data(&r).contains(&format!("could not reach the browser at 127.0.0.1:{port}")),
        "{r}"
    );
    assert!(data(&r).contains("capabilities.network.allow"), "{r}");
    fs::write(env.work.join("page.html"), "x").unwrap();
    let r = env.call(json!({"action": "open", "url": "page.html"}));
    assert!(
        data(&r).contains("a remote browser cannot see the work folder"),
        "{r}"
    );
}

/// Two calls at once on a fresh session start one browser between them.
#[test]
fn concurrent_first_calls_start_one_browser() {
    let env = Env::new();
    let _guard = Guard(&env);
    let results: Vec<_> = std::thread::scope(|s| {
        let a = s.spawn(|| env.call(json!({"action": "snapshot"})));
        let b = s.spawn(|| env.call(json!({"action": "snapshot"})));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    for r in &results {
        assert_eq!(r["status"], "passed", "{r}");
    }
    let banners = results
        .iter()
        .filter(|r| data(r).starts_with("browser: "))
        .count();
    assert_eq!(banners, 1, "one browser, one banner: {results:?}");
    let profiles = common::pids_mentioning(&env.state().join("profile"));
    let browser = env.info().unwrap()["browser"]["pid"].as_u64().unwrap();
    assert!(profiles.contains(&browser));
}

/// Kills the separately started browser of the remote test.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// With `config: {endpoint}` the tool drives a browser it did not start, found through
/// `/json/version`, and `close` leaves that browser running.
#[test]
fn remote_endpoint_drives_a_browser_it_did_not_start() {
    let mut env = Env::new();
    let profile = env.dir.path().join("remote-profile");
    fs::create_dir_all(&profile).unwrap();
    let browser = KillOnDrop(
        std::process::Command::new(common::test_browser())
            .args([
                "--headless",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--disable-gpu",
                &format!("--user-data-dir={}", profile.display()),
                "about:blank",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let port_file = profile.join("DevToolsActivePort");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let port = loop {
        if let Some(port) = fs::read_to_string(&port_file)
            .ok()
            .and_then(|t| t.lines().next().and_then(|l| l.trim().parse::<u16>().ok()))
        {
            break port;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the remote browser opened no DevTools port"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    env.config = Some(format!(r#"{{"endpoint":"ws://127.0.0.1:{port}"}}"#));
    env.with_runtime = false;
    let fixture = common::fixture_server();
    let r = env.call(json!({"action": "open", "url": format!("http://127.0.0.1:{fixture}/")}));
    assert_eq!(r["status"], "passed", "{r}");
    assert!(
        data(&r).starts_with(&format!("browser: remote 127.0.0.1:{port} ")),
        "{r}"
    );
    assert!(data(&r).contains("button \"Go\""), "{r}");
    assert!(env
        .work
        .join(".tool-browser/default/supervisor.json")
        .is_file());
    assert!(
        !env.work.join(".tool-browser/default/profile").exists(),
        "no local profile"
    );
    let c = env.call(json!({"action": "close"}));
    assert_eq!(data(&c), "browser closed");
    std::thread::sleep(Duration::from_millis(500));
    let mut browser = browser;
    assert!(
        browser.0.try_wait().unwrap().is_none(),
        "close leaves the remote browser running"
    );
}
