//! The decisions around starting Chrome, kept free of I/O: its arguments, reading its stderr,
//! the lines a failed start answers with, and where the work folder is as Chrome sees it.

use serde_json::Value;

use crate::util::one_line;

/// The session process name Chrome runs under.
pub const PROCESS_NAME: &str = "browser";
/// How long a whole start may take.
pub const START_LIMIT_MS: u64 = 30_000;
/// How long Chrome has to print its DevTools line.
pub const DEVTOOLS_WAIT_MS: u64 = 20_000;
/// The initial page, relative to the per-browser folder; Chrome resolves the path against its
/// working directory, the work folder, which is how the tool learns that folder's `file://` URL.
pub const BLANK_PAGE: &str = "blank.html";
pub const BLANK_HTML: &str = "<!doctype html><title></title>";

/// Chrome's arguments, every path relative to the work folder. `sandbox_off` adds
/// `--no-sandbox`, for the second attempt after Chrome could not start its own sandbox.
pub fn browser_args(state_rel: &str, sandbox_off: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--headless".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={state_rel}/profile"),
    ];
    args.extend(
        [
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--mute-audio",
            "--password-store=basic",
            "--use-mock-keychain",
            "--window-size=1280,800",
            "--disable-dev-shm-usage",
        ]
        .map(str::to_string),
    );
    if sandbox_off {
        args.push("--no-sandbox".into());
    }
    args.push(format!("{state_rel}/{BLANK_PAGE}"));
    args
}

/// Text Chrome prints when it exits because it could not start its own sandbox.
pub const SANDBOX_SIGNATURES: &[&str] = &[
    "No usable sandbox!",
    "The SUID sandbox helper binary",
    "zygote_host_impl_linux",
    "Failed to move to new namespace",
    "sandbox_host_linux",
];

/// Whether Chrome's stderr shows a sandbox failure.
pub fn sandbox_failed(stderr: &str) -> bool {
    SANDBOX_SIGNATURES.iter().any(|s| stderr.contains(s))
}

/// `DevTools listening on ws://127.0.0.1:<port><path>` → `(port, path)`.
pub fn parse_devtools_line(stderr: &str) -> Option<(u16, String)> {
    stderr.lines().find_map(|line| {
        let rest = line
            .trim()
            .strip_prefix("DevTools listening on ws://127.0.0.1:")?;
        let slash = rest.find('/')?;
        let port: u16 = rest[..slash].parse().ok()?;
        let path = rest[slash..].trim();
        (port > 0 && path.starts_with("/devtools/")).then(|| (port, path.to_string()))
    })
}

const SINGLETON_HINT: &str = "; full Chrome and Chromium cannot run on murmur's scoped and sealed tiers, grant chrome-headless-shell under this name instead";
const MEMORY_HINT: &str = "; set capabilities.resources.memory_bytes to at least 85899345920 (80 GiB), for example 274877906944";

/// The one line for a Chrome that exited before it opened DevTools. It quotes Chrome's last
/// `FATAL` line when there is one (a crash prints its stack trace after it), else its last
/// non-empty stderr line.
pub fn exit_line(name: &str, code: i32, stderr: &str) -> String {
    let lines = || {
        stderr
            .lines()
            .rev()
            .map(str::trim)
            .filter(|l| !l.is_empty())
    };
    let last = lines()
        .find(|l| l.contains(":FATAL:"))
        .or_else(|| lines().next())
        .map(|l| one_line(l, 300))
        .unwrap_or_else(|| "(nothing on stderr)".into());
    let mut line = format!("tool-browser: {name} exited with {code} while starting: {last}");
    let singleton = [
        "ProcessSingleton",
        "process_singleton",
        "SingletonSocket",
        "SingletonLock",
    ]
    .iter()
    .any(|s| stderr.contains(s));
    if singleton {
        line.push_str(SINGLETON_HINT);
    }
    let memory_text = stderr.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        // Chrome logs "Failed to adjust OOM score of renderer" on every contained start; that
        // line says nothing about memory.
        ["out of memory", "cannot allocate memory", "address space"]
            .iter()
            .any(|s| l.contains(s))
            || (l
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == "oom")
                && !l.contains("oom score"))
    });
    if memory_text || (code >= 128 && !singleton) {
        line.push_str(MEMORY_HINT);
    }
    line
}

pub fn no_port_line(name: &str, state_rel: &str) -> String {
    format!(
        "tool-browser: {name} opened no DevTools port within {} s; see {state_rel}/browser.log",
        DEVTOOLS_WAIT_MS / 1000
    )
}

pub fn refused_line(name: &str, rule: &str, reason: &str) -> String {
    format!("tool-browser: murmur refused to start {name}: {rule}: {reason}")
}

pub fn spawn_failed_line(name: &str, text: &str) -> String {
    let mut line = format!("tool-browser: could not start {name}: {text}");
    if text.contains("Permission denied") {
        line.push_str(&format!(
            " (if {name} is a script, its #! interpreter must be in capabilities.shell.allow too)"
        ));
    }
    line
}

pub fn banner(name: &str, version: &str, sandbox_on: bool) -> String {
    let sandbox = if sandbox_on {
        "Chrome sandbox on"
    } else {
        "Chrome sandbox off (Chrome could not start its sandbox inside this capsule's containment, which holds the browser instead)"
    };
    format!("browser: {name} {version}, local, headless; {sandbox}")
}

pub fn replaced_note(code: i32) -> String {
    format!(
        "browser: the previous browser exited with {code}; this is a new one and refs start again at e1"
    )
}

/// `HeadlessChrome/145.0.7632.6` → `145.0.7632.6`.
pub fn version_from_product(product: &str) -> String {
    match product.split_once('/') {
        Some((_, v)) if !v.is_empty() => v.to_string(),
        _ if product.is_empty() => "(unknown version)".into(),
        _ => product.to_string(),
    }
}

/// From `Target.getTargets`, the page Chrome opened on `<state_rel>/blank.html`: its target id
/// and the work folder's `file://` URL, which is that page's URL without the suffix.
pub fn initial_page(targets: &Value, state_rel: &str) -> Option<(String, String)> {
    let suffix = format!("/{state_rel}/{BLANK_PAGE}");
    targets
        .get("targetInfos")?
        .as_array()?
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .find_map(|t| {
            let url = t.get("url")?.as_str()?;
            let base = url.strip_suffix(&suffix)?;
            if !base.starts_with("file://") {
                return None;
            }
            Some((t.get("targetId")?.as_str()?.to_string(), base.to_string()))
        })
}

/// The work folder's host path from its `file://` URL, percent-decoded.
pub fn workdir_path(workdir_url: &str) -> Option<String> {
    let rest = workdir_url.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    path.starts_with('/')
        .then(|| crate::util::percent_decode(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_in_order_with_relative_paths() {
        let on = browser_args(".tool-browser/0123456789abcdef", false);
        let expected_head = [
            "--headless",
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--user-data-dir=.tool-browser/0123456789abcdef/profile",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--mute-audio",
            "--password-store=basic",
            "--use-mock-keychain",
            "--window-size=1280,800",
            "--disable-dev-shm-usage",
        ];
        assert_eq!(&on[..expected_head.len()], expected_head);
        assert_eq!(
            on.last().unwrap(),
            ".tool-browser/0123456789abcdef/blank.html"
        );
        assert!(!on.iter().any(|a| a == "--no-sandbox"));
        assert!(!on.iter().any(|a| a == "--single-process"));
        let off = browser_args(".tool-browser/x", true);
        assert_eq!(off.len(), on.len() + 1);
        assert_eq!(off[off.len() - 2], "--no-sandbox");
        assert!(on
            .iter()
            .chain(&off)
            .all(|a| !a.contains("--user-data-dir=/")));
    }

    #[test]
    fn devtools_line_parses() {
        let err = "\n[0105/1:ERROR:x.cc(1)] noise\nDevTools listening on ws://127.0.0.1:41234/devtools/browser/0d6c-11\n";
        assert_eq!(
            parse_devtools_line(err),
            Some((41234, "/devtools/browser/0d6c-11".into()))
        );
        assert_eq!(
            parse_devtools_line("DevTools listening on ws://127.0.0.1:/x"),
            None
        );
        assert_eq!(
            parse_devtools_line("DevTools listening on ws://10.0.0.1:9/devtools/browser/x"),
            None
        );
        assert_eq!(parse_devtools_line("nothing"), None);
    }

    #[test]
    fn sandbox_signatures_match_chrome_lines() {
        for line in [
            // chrome-headless-shell 145.0.7632.6 on murmur's scoped and sealed tiers, verbatim.
            "[1105/074242.297415:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:128] No usable sandbox! If you are running on Ubuntu 23.10+ or another Linux distro that has disabled unprivileged user namespaces with AppArmor, see https://chromium.googlesource.com/chromium/src/+/main/docs/security/apparmor-userns-restrictions.md. Otherwise see https://chromium.googlesource.com/chromium/src/+/main/docs/linux/suid_sandbox_development.md for more information on developing with the (older) SUID sandbox. If you want to live dangerously and need an immediate workaround, you can try using --no-sandbox.",
            // Chromium's text when the setuid helper is present but not usable.
            "[1:1:1105/120000.000000:FATAL:zygote_host_impl_linux.cc(128)] The SUID sandbox helper binary was found, but is not configured correctly. Rather than run without sandboxing I'm aborting now.",
        ] {
            assert!(sandbox_failed(line), "{line}");
        }
        assert!(!sandbox_failed(
            "DevTools listening on ws://127.0.0.1:1/devtools/browser/x"
        ));
        assert!(!sandbox_failed(
            "[1:1:1105/1:ERROR:process_singleton_posix.cc(358)] Failed to bind() /tmp/SingletonSocket: Permission denied (13)"
        ));
    }

    #[test]
    fn exit_lines_and_hints() {
        let singleton = "x\n[1:1:ERROR:chrome/browser/process_singleton_posix.cc:358] Failed to bind() /tmp/x/SingletonSocket: Permission denied (13)\n\n";
        let line = exit_line("chromium", 21, singleton);
        assert!(
            line.starts_with("tool-browser: chromium exited with 21 while starting: [1:1:ERROR:"),
            "{line}"
        );
        assert!(line.ends_with(SINGLETON_HINT), "{line}");
        assert!(!line.contains("memory_bytes"));
        let line = exit_line("chromium", 133, "FATAL: GPU process isn't usable. Goodbye.");
        assert!(line.ends_with(MEMORY_HINT), "{line}");
        // Full Chromium on the scoped tier: seccomp refuses its singleton socket, it aborts and
        // prints a stack trace after the FATAL line.
        let aborted = "[1:1:1105/124758.822301:FATAL:chrome/browser/process_singleton_posix.cc:297] Check failed: . socket() failed: Permission denied (13)\nReceived signal 6\n#0 0x60d6 (chrome+0x47001e9)\n[end of stack trace]\n";
        let line = exit_line("chromium", 134, aborted);
        assert!(line.contains("while starting: [1:1:1105/124758.822301:FATAL:chrome/browser/process_singleton_posix.cc:297] Check failed"), "{line}");
        assert!(
            line.ends_with(SINGLETON_HINT) && !line.contains("memory_bytes"),
            "{line}"
        );
        let line = exit_line("chrome", 1, "mmap failed: Cannot allocate memory");
        assert!(line.contains("85899345920 (80 GiB)"), "{line}");
        let line = exit_line(
            "chrome",
            5,
            "[1:1:FATAL:partition_alloc.cc] OOM: out of address space",
        );
        assert!(line.ends_with(MEMORY_HINT), "{line}");
        let oom_score = "[1:1:ERROR:content/browser/zygote_host/zygote_host_impl_linux.cc:279] Failed to adjust OOM score of renderer with pid 9: Permission denied (13)\n[1:1:FATAL:x.cc] zoom room bloom\n";
        let line = exit_line("chromium", 1, oom_score);
        assert!(!line.contains("memory_bytes"), "{line}");
        assert_eq!(
            exit_line("chrome", 1, ""),
            "tool-browser: chrome exited with 1 while starting: (nothing on stderr)"
        );
        assert_eq!(
            no_port_line("chromium", ".tool-browser/ab"),
            "tool-browser: chromium opened no DevTools port within 20 s; see .tool-browser/ab/browser.log"
        );
    }

    #[test]
    fn refusal_and_spawn_lines() {
        assert_eq!(
            refused_line("chromium", "on-shell", "denied by hook-command-guard"),
            "tool-browser: murmur refused to start chromium: on-shell: denied by hook-command-guard"
        );
        assert_eq!(
            spawn_failed_line("chromium", "Permission denied (os error 13)"),
            "tool-browser: could not start chromium: Permission denied (os error 13) (if chromium is a script, its #! interpreter must be in capabilities.shell.allow too)"
        );
        assert_eq!(
            spawn_failed_line("chrome", "boom"),
            "tool-browser: could not start chrome: boom"
        );
    }

    #[test]
    fn banners_and_note() {
        let prefix = "browser: chromium 145.0.7632.6, local, headless; Chrome sandbox ";
        assert_eq!(
            banner("chromium", "145.0.7632.6", true),
            format!("{prefix}on")
        );
        assert!(banner("chromium", "145.0.7632.6", false).starts_with(&format!("{prefix}off (")));
        assert_eq!(
            replaced_note(137),
            "browser: the previous browser exited with 137; this is a new one and refs start again at e1"
        );
        assert_eq!(
            version_from_product("HeadlessChrome/145.0.7632.6"),
            "145.0.7632.6"
        );
        assert_eq!(version_from_product(""), "(unknown version)");
    }

    #[test]
    fn workdir_url_from_the_initial_page() {
        let targets = json!({"targetInfos": [
            {"type": "browser_ui", "targetId": "U", "url": "chrome://x/"},
            {"type": "page", "targetId": "OTHER", "url": "about:blank"},
            {"type": "page", "targetId": "T1", "url": "file:///home/a%20b/w/.tool-browser/0011/blank.html"},
        ]});
        let (id, url) = initial_page(&targets, ".tool-browser/0011").unwrap();
        assert_eq!((id.as_str(), url.as_str()), ("T1", "file:///home/a%20b/w"));
        assert_eq!(workdir_path(&url).as_deref(), Some("/home/a b/w"));
        assert!(initial_page(&targets, ".tool-browser/9999").is_none());
        let http = json!({"targetInfos": [{"type": "page", "targetId": "T", "url": "http://tool-browser/0011/blank.html"}]});
        assert!(initial_page(&http, ".tool-browser/0011").is_none());
        assert_eq!(workdir_path("http://x"), None);
    }
}
