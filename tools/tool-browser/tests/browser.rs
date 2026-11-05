//! `tool-browser` against a real headless Chrome or Chromium, through its compiled component in
//! Wasmtime, the way murmur runs it: one instance per call, the browser a session process that
//! the test host for `murmur:process` starts.
//!
//! Each test has its own session (see `common::Env`) and so its own browser; dropping the session
//! kills every process group it started. A host with no browser fails these tests, naming what to
//! install.

#![cfg(target_os = "linux")]

mod common;

use std::{fs, time::Duration};

use common::{data, ref_of, wait_gone, Env, Guard};
use serde_json::{json, Value};

const NO_GRANT_LINE: &str = "tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow";
const SCOPE_LINE: &str = "tool-browser cannot see which programs capabilities.shell.allow grants, because its entry declares capabilities.filesystem.scope; tool-browser runs only from an entry without one";
const BANNER_PREFIX: &str = "browser: chromium ";

fn open_fixture(env: &Env) -> (u16, Value) {
    let port = common::fixture_server();
    let r = env.call(json!({"action": "open", "url": format!("http://127.0.0.1:{port}/")}));
    assert_eq!(r["status"], "passed", "open: {r}");
    (port, r)
}

fn open_work_page(env: &Env, name: &str, html: &str) -> Value {
    fs::write(env.work.join(name), html).unwrap();
    let r = env.call(json!({"action": "open", "url": name}));
    assert_eq!(r["status"], "passed", "open: {r}");
    r
}

fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        banner.starts_with(BANNER_PREFIX) && banner.contains(", local, headless; Chrome sandbox "),
        "banner: {banner}"
    );
    assert_eq!(
        lines.next().unwrap(),
        format!("page: Fixture Page — http://127.0.0.1:{port}/")
    );
    assert!(ref_of(text, "button \"Go\"").starts_with('e'));
    assert!(env.state().join("state.json").is_file());
    assert!(env.state().join("profile").is_dir());
    assert_eq!(
        r["summary"],
        format!("open: page: Fixture Page — http://127.0.0.1:{port}/"),
        "the summary is the action's own first line, without the banner"
    );
}

/// Scenario 2: a later snapshot keeps the ref, has no banner, and runs on the same browser.
#[test]
fn snapshot_keeps_the_ref_without_banner_on_the_same_browser() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let go = ref_of(data(&r), "button \"Go\"");
    let handle = env.handle().unwrap();
    let pid = env.browser_pid().unwrap();
    let s = env.call(json!({"action": "snapshot"}));
    assert_eq!(s["status"], "passed");
    assert!(data(&s).starts_with("page: Fixture Page"), "no banner: {s}");
    assert_eq!(ref_of(data(&s), "button \"Go\""), go);
    assert_eq!(env.handle().unwrap(), handle);
    assert_eq!(env.browser_pid().unwrap(), pid);
    assert_eq!(env.starts(), 1);
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
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with("[error] boom-load"))
            .count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with("[error] boom-click"))
            .count(),
        1,
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
    assert_eq!(
        data(&n)
            .lines()
            .filter(|l| *l == format!("[404] GET http://127.0.0.1:{port}/missing.js"))
            .count(),
        1,
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
    let rel = env.state_rel();
    let s = env.call(json!({"action": "screenshot"}));
    assert_eq!(s["status"], "passed", "{s}");
    let path = s["attachments"][0]["path"].as_str().unwrap().to_string();
    assert_eq!(s["attachments"][0]["media_type"], "image/png");
    assert_eq!(path, format!("{rel}/screenshot-1.png"));
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
    assert_eq!(path, format!("{rel}/screenshot-2.png"));
    let (w, h) = png_size(&fs::read(env.work.join(path)).unwrap());
    assert!(
        w > 0 && w < 400 && h > 0 && h < 200,
        "a button-sized image: {w}x{h}"
    );
}

/// Scenario 8: close stops the browser; the next call starts afresh.
#[test]
fn close_stops_everything_and_the_next_call_starts_a_new_browser() {
    let env = Env::new();
    let _guard = Guard(&env);
    open_fixture(&env);
    let old = env.state();
    let handle = env.handle().unwrap();
    let browser = env.browser_pid().unwrap();
    let c = env.call(json!({"action": "close"}));
    assert_eq!(c["status"], "passed");
    assert_eq!(data(&c), "browser closed");
    assert!(
        wait_gone(browser.into(), Duration::from_secs(5)),
        "browser pid {browser} is gone"
    );
    assert!(!old.join("profile").exists(), "profile/ is removed");
    let saved: Value = serde_json::from_slice(&fs::read(old.join("state.json")).unwrap()).unwrap();
    assert_eq!(saved["closed"], true);

    let s = env.call(json!({"action": "snapshot"}));
    assert_eq!(s["status"], "passed", "{s}");
    assert!(data(&s).starts_with(BANNER_PREFIX), "the banner again: {s}");
    assert!(
        data(&s)
            .lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("page: ") && l.ends_with(" — about:blank")),
        "a fresh browser shows about:blank, with no note: {s}"
    );
    assert_ne!(env.handle().unwrap(), handle);
    assert_ne!(env.state(), old);
    assert_eq!(
        data(&env.call(json!({"action": "close"}))),
        "browser closed"
    );
    assert_eq!(
        data(&env.call(json!({"action": "close"}))),
        "browser closed",
        "close with nothing running"
    );
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
    let env = Env::with_tools(&[]);
    let r = env.call(json!({"action": "open", "url": "about:blank"}));
    assert_eq!(r["status"], "error");
    assert_eq!(r["summary"], NO_GRANT_LINE);
    assert_eq!(r["data"], NO_GRANT_LINE);
    assert!(
        !env.work.join(".tool-browser").exists(),
        "nothing in the work folder"
    );
    assert_eq!(env.starts(), 0, "no browser process");
}

/// Scenario 11: granted but not installed names the program and `mur deploy run`.
#[test]
fn granted_but_missing_names_chromium_and_mur_deploy_run() {
    let env = Env::new().without_browser_on_path();
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    let line = data(&r);
    assert_eq!(
        line,
        "tool-browser: capabilities.shell.allow grants chromium, but no chromium is on PATH; install Chrome or Chromium on this host first (mur deploy run does not upload host programs)"
    );
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
        (
            json!({"action": "open", "url": "data:text/html,x"}),
            "open refuses data: URLs",
        ),
        (
            json!({"action": "open", "url": "chrome://version"}),
            "open refuses chrome: URLs",
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
    let r = env.call(json!({"action": "open", "url": "/etc/hostname"}));
    assert_eq!(r["status"], "error");
    assert!(data(&r).contains("outside the work folder"), "{r}");

    let mut configured = Env::new();
    configured.config = Some(r#"{"port":1}"#.into());
    let r = configured.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    assert!(
        data(&r).contains("port") && data(&r).contains("config:"),
        "{r}"
    );
    assert!(!configured.work.join(".tool-browser").exists());
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
    assert_eq!(env.starts(), 1, "one browser process");
    assert_eq!(
        env.state_dirs().len(),
        1,
        "one state folder: {:?}",
        env.state_dirs()
    );
    let profiles = common::pids_mentioning(&format!("{}/profile", env.state_rel()));
    assert!(profiles.contains(&u64::from(env.browser_pid().unwrap())));
}

/// Any `config:` key refuses the call, `endpoint` included, and starts nothing.
#[test]
fn config_endpoint_is_refused() {
    let mut env = Env::new();
    env.config = Some(r#"{"endpoint":"ws://127.0.0.1:9222/devtools/browser/x"}"#.into());
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    assert_eq!(
        data(&r),
        "tool-browser: config: does not take endpoint; tool-browser takes no config: keys"
    );
    assert_eq!(r["summary"], r["data"]);
    assert!(!env.work.join(".tool-browser").exists());
    assert_eq!(env.starts(), 0);
    env.config = Some("{}".into());
    let _guard = Guard(&env);
    assert_eq!(env.call(json!({"action": "snapshot"}))["status"], "passed");
}

/// Refs, the last snapshot and the screenshot counter live in `state.json` between calls.
#[test]
fn state_carries_refs_across_calls() {
    let env = Env::new();
    let _guard = Guard(&env);
    let (_, r) = open_fixture(&env);
    let go = ref_of(data(&r), "button \"Go\"");
    let state = env.info().unwrap();
    for key in [
        "handle",
        "browser",
        "port",
        "browser_ws_path",
        "workdir_url",
        "target_id",
        "started_at",
        "banner_shown",
        "screenshots",
    ] {
        assert!(state.get(key).is_some(), "state.json has {key}: {state}");
    }
    assert_eq!(state["banner_shown"], true);
    assert!(state["handle"].as_str().unwrap().starts_with("prc_"));
    assert!(state["browser_ws_path"]
        .as_str()
        .unwrap()
        .starts_with("/devtools/browser/"));
    assert_eq!(
        state["workdir_url"],
        format!("file://{}", env.work.canonicalize().unwrap().display())
    );
    assert!(state["document"]["refs"][&go].is_i64(), "{state}");
    assert!(!state["document"]["last"]["lines"]
        .as_array()
        .unwrap()
        .is_empty());
    // A new call, a new DevTools session: the ref still clicks the same button.
    let c = env.call(json!({"action": "click", "ref": go}));
    assert!(data(&c).contains("clicked"), "{c}");
    env.call(json!({"action": "screenshot"}));
    assert_eq!(env.info().unwrap()["screenshots"], 1);
    assert!(
        !env.state().join("call.lock").exists(),
        "the lock is released"
    );
}

/// A browser that died is replaced on the next call, which says so.
#[test]
fn a_dead_browser_is_replaced_with_a_note() {
    let env = Env::new();
    let _guard = Guard(&env);
    open_fixture(&env);
    let old_handle = env.handle().unwrap();
    env.kill_browser();
    let s = env.call(json!({"action": "snapshot"}));
    assert_eq!(s["status"], "passed", "{s}");
    let lines: Vec<&str> = data(&s).lines().collect();
    assert!(lines[0].starts_with(BANNER_PREFIX), "{s}");
    assert_eq!(
        lines[1],
        "browser: the previous browser exited with 137; this is a new one and refs start again at e1"
    );
    assert!(lines[2].ends_with(" — about:blank"), "{s}");
    assert_ne!(env.handle().unwrap(), old_handle);
    assert_eq!(env.starts(), 2);
    let again = env.call(json!({"action": "snapshot"}));
    assert!(!data(&again).contains("previous browser"), "{again}");
}

/// When the session ends, no process of the tool's browser is left.
#[test]
fn session_end_leaves_no_browser_process() {
    let env = Env::new();
    open_fixture(&env);
    let needle = format!("{}/profile", env.state_rel());
    assert!(
        !common::pids_mentioning(&needle).is_empty(),
        "the browser runs with {needle}"
    );
    let pid = env.browser_pid().unwrap();
    drop(env);
    assert!(wait_gone(pid.into(), Duration::from_secs(5)));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !common::pids_mentioning(&needle).is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(common::pids_mentioning(&needle), Vec::<u64>::new());
}

/// `close` removes the profile and keeps the screenshots, the log and `state.json`.
#[test]
fn close_removes_the_profile_and_keeps_screenshots() {
    let env = Env::new();
    let _guard = Guard(&env);
    open_fixture(&env);
    let dir = env.state();
    env.call(json!({"action": "screenshot"}));
    assert!(dir.join("profile").is_dir());
    assert_eq!(
        data(&env.call(json!({"action": "close"}))),
        "browser closed"
    );
    assert!(!dir.join("profile").exists());
    assert!(dir.join("screenshot-1.png").is_file());
    assert!(dir.join("browser.log").is_file());
    assert!(dir.join("state.json").is_file());
    assert!(dir.join("downloads").is_dir());
}

/// An entry with `capabilities.filesystem.scope` has no runtime preopen; the tool says why it
/// cannot run there.
#[test]
fn no_runtime_preopen_names_the_scope() {
    let mut env = Env::new();
    env.with_runtime = false;
    let r = env.call(json!({"action": "open", "url": "about:blank"}));
    assert_eq!(r["status"], "error");
    assert_eq!(data(&r), SCOPE_LINE);
    assert!(!env.work.join(".tool-browser").exists());
    assert_eq!(env.starts(), 0);
}

/// A rule that refuses the start is named with its reason, and nothing is left behind.
#[test]
fn start_refused_by_a_rule_names_it() {
    let env = Env::new();
    env.session.allow.lock().unwrap().clear();
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    assert_eq!(
        data(&r),
        "tool-browser: murmur refused to start chromium: capabilities.shell.allow: binary 'chromium' is not in capabilities.shell.allow"
    );
    assert!(env.state_dirs().is_empty());
    assert_eq!(
        fs::read_dir(env.work.join(".tool-browser"))
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "the unused folder is removed"
    );
}

/// A Chrome that exits while starting is named with its code, its last stderr line and the hint
/// that fits.
#[test]
fn chrome_exiting_at_start_names_code_and_hint() {
    let env = Env::new();
    env.script_on_path(
        "chromium",
        "#!/bin/sh\necho 'starting' >&2\necho '[1:1:1105/120000.1:ERROR:chrome/browser/process_singleton_posix.cc:358] Failed to bind() /tmp/c/SingletonSocket: Permission denied (13)' >&2\nexit 21\n",
    );
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "error");
    assert_eq!(
        data(&r),
        "tool-browser: chromium exited with 21 while starting: [1:1:1105/120000.1:ERROR:chrome/browser/process_singleton_posix.cc:358] Failed to bind() /tmp/c/SingletonSocket: Permission denied (13); full Chrome and Chromium cannot run on murmur's scoped and sealed tiers, grant chrome-headless-shell under this name instead"
    );
    let log = fs::read_to_string(
        env.work
            .join(".tool-browser")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("browser.log"),
    )
    .unwrap();
    assert!(
        log.contains("SingletonSocket"),
        "browser.log keeps the output: {log}"
    );
}

/// A Chrome that cannot start its own sandbox is started once more with `--no-sandbox`, and the
/// banner says so.
#[test]
fn sandbox_failure_restarts_without_the_sandbox() {
    let env = Env::new();
    let _guard = Guard(&env);
    let marker = env.dir.path().join("sandbox-failed-once");
    env.script_on_path(
        "chromium",
        &format!(
            "#!/bin/sh\nif [ ! -e '{m}' ]; then\n  : > '{m}'\n  echo '[1:1:1105/120000.000000:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:132] No usable sandbox! If you want to live dangerously and need an immediate workaround, you can try using --no-sandbox.' >&2\n  exit 133\nfi\nexec '{b}' \"$@\"\n",
            m = marker.display(),
            b = common::test_browser().display()
        ),
    );
    let r = env.call(json!({"action": "snapshot"}));
    assert_eq!(r["status"], "passed", "{r}");
    assert!(
        data(&r).starts_with(BANNER_PREFIX)
            && data(&r).lines().next().unwrap().ends_with(
                "Chrome sandbox off (Chrome could not start its sandbox inside this capsule's containment, which holds the browser instead)"
            ),
        "{r}"
    );
    assert_eq!(env.starts(), 2);
    assert_eq!(env.info().unwrap()["browser"]["sandbox"], false);
    let cmdline = fs::read(format!("/proc/{}/cmdline", env.browser_pid().unwrap())).unwrap();
    assert!(String::from_utf8_lossy(&cmdline).contains("--no-sandbox"));
}

/// A console error raised while no call runs is reported once: counted by the next change
/// summary, listed by the next console call, and never again.
#[test]
fn console_raised_between_calls_is_reported_once() {
    let env = Env::new();
    let _guard = Guard(&env);
    open_work_page(
        &env,
        "late.html",
        "<title>Late</title><p>x</p><script>setTimeout(() => console.error('late-error'), 1000)</script>",
    );
    std::thread::sleep(Duration::from_millis(2000));
    let w = env.call(json!({"action": "wait", "ms": 10}));
    assert!(
        data(&w)
            .lines()
            .any(|l| l == "console: 1 new error(s); call console"),
        "{w}"
    );
    let c = env.call(json!({"action": "console"}));
    assert_eq!(
        data(&c)
            .lines()
            .filter(|l| l.starts_with("[error] late-error"))
            .count(),
        1,
        "{c}"
    );
    assert_eq!(
        data(&env.call(json!({"action": "console"}))),
        "no new console messages"
    );
    let w = env.call(json!({"action": "wait", "ms": 10}));
    assert!(!data(&w).contains("new error"), "{w}");
}

/// A request that fails while no call runs is reported once, without its method, which
/// Chrome's log does not carry.
#[test]
fn network_failure_between_calls_is_reported_once_without_method() {
    let env = Env::new();
    let _guard = Guard(&env);
    let refused = closed_port();
    let page: &'static str = Box::leak(
        format!(
            "<title>Late net</title><p>x</p><script>setTimeout(() => {{ fetch('/gone').catch(() => {{}}); new Image().src = 'http://127.0.0.1:{refused}/x.png'; }}, 1000)</script>"
        )
        .into_boxed_str(),
    );
    let port = common::serve_page(page);
    let r = env.call(json!({"action": "open", "url": format!("http://127.0.0.1:{port}/")}));
    assert_eq!(r["status"], "passed", "{r}");
    std::thread::sleep(Duration::from_millis(2500));
    let n = env.call(json!({"action": "network"}));
    let text = data(&n);
    assert_eq!(
        text.lines()
            .filter(|l| *l == format!("[404] http://127.0.0.1:{port}/gone"))
            .count(),
        1,
        "{n}"
    );
    assert_eq!(
        text.lines()
            .filter(|l| *l
                == format!(
                    "[failed] http://127.0.0.1:{refused}/x.png — net::ERR_CONNECTION_REFUSED"
                ))
            .count(),
        1,
        "{n}"
    );
    assert_eq!(
        data(&env.call(json!({"action": "network"}))),
        "no failed requests"
    );
}

/// A dialog that opened while no call ran is dismissed by the next call, and reported.
#[test]
fn dialog_opened_between_calls_is_dismissed() {
    let env = Env::new();
    let _guard = Guard(&env);
    open_work_page(
        &env,
        "alert.html",
        "<title>Alert</title><h1 id=h>before</h1><script>setTimeout(() => { alert('late-alert'); document.getElementById('h').textContent = 'after'; }, 1000)</script>",
    );
    std::thread::sleep(Duration::from_millis(2000));
    let s = env.call(json!({"action": "wait", "ms": 300}));
    assert_eq!(s["status"], "passed", "{s}");
    let text = data(&s);
    assert!(
        text.lines()
            .any(|l| l == "dialog: alert \"late-alert\" (dismissed)" || l == "dialog: (dismissed)"),
        "{s}"
    );
    assert!(text.contains("after"), "the page ran on: {s}");
    let again = env.call(json!({"action": "snapshot"}));
    assert_eq!(again["status"], "passed", "{again}");
}
