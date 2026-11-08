//! What outlives a call: `.tool-browser/<id>/state.json` beside the browser's profile, found
//! again by the session-process handle it records, and the `call.lock` that runs one call at a
//! time against a browser.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    buffers::{LogBuffer, CONSOLE_CAP, NETWORK_CAP},
    host::Clock,
};

/// The folder in the work folder that holds one subfolder per browser started.
pub const STATE_ROOT: &str = ".tool-browser";
/// The most console fingerprints and network request ids kept for the current document.
pub const SEEN_CAP: usize = 2_000;
/// A `call.lock` older than this is left over from a call that died, and is taken over.
pub const LOCK_STALE_SECS: u64 = 120;
/// How long a call waits for another call's lock.
pub const LOCK_WAIT_MS: u64 = 90_000;
pub const BUSY_LINE: &str = "tool-browser: another tool-browser call is still running";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserInfo {
    /// The granted name Chrome was started as.
    pub name: String,
    pub version: String,
    /// Whether Chrome runs with its own sandbox.
    pub sandbox: bool,
}

/// The last full snapshot, which a change summary is compared with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSnapshot {
    pub lines: Vec<String>,
    pub url: String,
    pub title: String,
    pub document: Option<i64>,
}

/// The current document as the tool last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentState {
    /// The root's DOM backend node id the refs were given out for.
    pub root: Option<i64>,
    /// `eN` → DOM backend node id.
    pub refs: BTreeMap<String, i64>,
    pub next_ref: u32,
    pub last: Option<SavedSnapshot>,
}

/// Console and network entries not yet read, and what the tool has already seen of the current
/// document, so the console messages Chrome replays to every new DevTools session are not
/// reported twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageLog {
    pub console: LogBuffer,
    pub network: LogBuffer,
    /// Fingerprints of the console events already taken, newest last, at most [`SEEN_CAP`].
    pub fingerprints: VecDeque<String>,
    /// Network request ids whose outcome was already taken, newest last, at most [`SEEN_CAP`].
    pub network_seen: VecDeque<String>,
    /// Errors, assertions, exceptions and error-level log entries since the last result that
    /// reported them.
    pub new_errors: u64,
    /// The main frame's loader id; a new one is a new document.
    pub loader_id: Option<String>,
}

impl Default for PageLog {
    fn default() -> Self {
        Self {
            console: LogBuffer::new(CONSOLE_CAP),
            network: LogBuffer::new(NETWORK_CAP),
            fingerprints: VecDeque::new(),
            network_seen: VecDeque::new(),
            new_errors: 0,
            loader_id: None,
        }
    }
}

impl PageLog {
    /// Forget the previous document's fingerprints and request ids; unread entries stay.
    pub fn new_document(&mut self, loader_id: Option<String>) {
        self.fingerprints.clear();
        self.network_seen.clear();
        self.loader_id = loader_id;
    }

    /// Record `fingerprint`; false when it was already known.
    pub fn take_fingerprint(&mut self, fingerprint: String) -> bool {
        remember(&mut self.fingerprints, fingerprint)
    }

    /// Record a request id; false when it was already known.
    pub fn take_request(&mut self, id: &str) -> bool {
        remember(&mut self.network_seen, id.to_string())
    }

    pub fn request_seen(&self, id: &str) -> bool {
        self.network_seen.iter().any(|s| s == id)
    }
}

fn remember(seen: &mut VecDeque<String>, item: String) -> bool {
    if seen.contains(&item) {
        return false;
    }
    if seen.len() == SEEN_CAP {
        seen.pop_front();
    }
    seen.push_back(item);
    true
}

/// `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The session-process handle Chrome runs under.
    pub handle: String,
    pub browser: BrowserInfo,
    /// Chrome's DevTools port on the session's loopback.
    pub port: u16,
    /// The browser endpoint's path, `/devtools/browser/<uuid>`.
    pub browser_ws_path: String,
    /// The work folder's `file://` URL as Chrome sees it.
    pub workdir_url: String,
    /// The page target the tool drives.
    pub target_id: String,
    /// Unix seconds.
    pub started_at: u64,
    /// The first result after the start carried the banner.
    pub banner_shown: bool,
    /// Screenshots written so far; the next is `screenshot-<n + 1>.png`.
    pub screenshots: u32,
    /// `close` stopped this browser.
    #[serde(default)]
    pub closed: bool,
    /// Growing the window did not give a 1280×800 viewport, so every call sets it with
    /// `Emulation.setDeviceMetricsOverride`.
    #[serde(default)]
    pub viewport_override: bool,
    /// The note the first result carries when this browser replaced one that exited.
    #[serde(default)]
    pub replaced_note: Option<String>,
    /// Dialogs dismissed and not yet reported.
    #[serde(default)]
    pub dialogs: Vec<String>,
    #[serde(default)]
    pub document: DocumentState,
    #[serde(default)]
    pub log: PageLog,
}

/// One browser's folder, `.tool-browser/<id>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    pub id: String,
    /// The folder, as the tool opens it.
    pub root: PathBuf,
    /// The folder relative to the work folder, as results and Chrome's arguments name it.
    pub rel: String,
}

impl StateDir {
    pub fn new(work: &Path, id: &str) -> Self {
        Self {
            id: id.to_string(),
            root: work.join(STATE_ROOT).join(id),
            rel: format!("{STATE_ROOT}/{id}"),
        }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn load(&self) -> Option<State> {
        serde_json::from_slice(&fs::read(self.file("state.json")).ok()?).ok()
    }

    /// Write `state.json` through `state.json.tmp` and a rename, so a reader never sees half.
    pub fn save(&self, state: &State) -> Result<(), String> {
        let tmp = self.file("state.json.tmp");
        let text = serde_json::to_vec_pretty(state)
            .map_err(|e| format!("tool-browser: cannot serialise state.json: {e}"))?;
        fs::write(&tmp, text)
            .and_then(|_| fs::rename(&tmp, self.file("state.json")))
            .map_err(|e| format!("tool-browser: cannot write {}/state.json: {e}", self.rel))
    }
}

/// The folder whose `state.json` records `handle`, with its state.
pub fn find_by_handle(work: &Path, handle: &str) -> Option<(StateDir, State)> {
    fs::read_dir(work.join(STATE_ROOT))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            let dir = StateDir::new(work, &id);
            let state = dir.load()?;
            (state.handle == handle).then_some((dir, state))
        })
        .next()
}

/// `call.lock`, held from creation until dropped.
pub struct CallLock {
    path: PathBuf,
}

impl CallLock {
    /// Create the lock, waiting while another call holds it. A lock older than
    /// [`LOCK_STALE_SECS`] is taken over.
    pub fn acquire(dir: &StateDir, clock: &dyn Clock, wait_ms: u64) -> Result<Self, String> {
        let path = dir.file("call.lock");
        let deadline = clock.now_ms() + wait_ms;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| clock.unix_secs().saturating_sub(d.as_secs()));
                    if age.is_some_and(|a| a >= LOCK_STALE_SECS) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "tool-browser: cannot create {}/call.lock: {e}",
                        dir.rel
                    ))
                }
            }
            if clock.now_ms() >= deadline {
                return Err(BUSY_LINE.into());
            }
            clock.sleep_ms(50);
        }
    }
}

impl Drop for CallLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    /// A clock that moves only when slept on, or when something holding a clone of `now` moves
    /// it.
    #[derive(Default)]
    pub struct FakeClock {
        pub now: Rc<Cell<u64>>,
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }
        fn sleep_ms(&self, ms: u64) {
            self.now.set(self.now.get() + ms);
        }
        fn unix_secs(&self) -> u64 {
            crate::util::unix_now()
        }
    }

    pub fn sample_state() -> State {
        State {
            handle: "prc_0a1b".into(),
            browser: BrowserInfo {
                name: "chromium".into(),
                version: "145.0.7632.6".into(),
                sandbox: true,
            },
            port: 41234,
            browser_ws_path: "/devtools/browser/u".into(),
            workdir_url: "file:///w".into(),
            target_id: "T".into(),
            started_at: 1,
            banner_shown: false,
            screenshots: 0,
            closed: false,
            viewport_override: false,
            replaced_note: None,
            dialogs: Vec::new(),
            document: DocumentState::default(),
            log: PageLog::default(),
        }
    }

    #[test]
    fn state_round_trips_and_is_found_by_handle() {
        let dir = tempfile::tempdir().unwrap();
        let a = StateDir::new(dir.path(), "00aa");
        fs::create_dir_all(&a.root).unwrap();
        let mut state = sample_state();
        state.document.refs.insert("e1".into(), 42);
        state.document.last = Some(SavedSnapshot {
            lines: vec!["- button \"Go\" [ref=e1]".into()],
            url: "about:blank".into(),
            title: String::new(),
            document: Some(7),
        });
        state.log.console.push("[error] x");
        assert!(state.log.take_fingerprint("console|1|0|x".into()));
        a.save(&state).unwrap();
        assert!(!a.file("state.json.tmp").exists());
        assert_eq!(a.load().unwrap(), state);
        let (found, loaded) = find_by_handle(dir.path(), "prc_0a1b").unwrap();
        assert_eq!((found.rel.as_str(), loaded), (".tool-browser/00aa", state));
        assert!(find_by_handle(dir.path(), "prc_ffff").is_none());

        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(a.file("state.json")).unwrap()).unwrap();
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
            "document",
            "log",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
    }

    #[test]
    fn seen_sets_are_capped_newest_kept() {
        let mut log = PageLog::default();
        for i in 0..SEEN_CAP + 5 {
            assert!(log.take_fingerprint(format!("f{i}")));
            log.take_request(&format!("r{i}"));
        }
        assert!(!log.take_fingerprint(format!("f{}", SEEN_CAP + 4)));
        assert_eq!(log.fingerprints.len(), SEEN_CAP);
        assert!(log.take_fingerprint("f0".into()), "the oldest fell out");
        assert!(log.request_seen("r10") && !log.request_seen("r0"));
        log.console.push("kept");
        log.new_document(Some("L2".into()));
        assert!(log.fingerprints.is_empty() && log.network_seen.is_empty());
        assert_eq!(log.console.drain(), vec!["kept"]);
    }

    #[test]
    fn lock_waits_then_answers_busy() {
        let dir = tempfile::tempdir().unwrap();
        let d = StateDir::new(dir.path(), "00bb");
        fs::create_dir_all(&d.root).unwrap();
        let clock = FakeClock::default();
        let held = CallLock::acquire(&d, &clock, 1000).unwrap();
        assert_eq!(
            CallLock::acquire(&d, &clock, 1000).err().as_deref(),
            Some(BUSY_LINE)
        );
        assert!(clock.now_ms() >= 1000);
        drop(held);
        assert!(!d.file("call.lock").exists());
        let again = CallLock::acquire(&d, &clock, 0).unwrap();
        drop(again);
    }
}
