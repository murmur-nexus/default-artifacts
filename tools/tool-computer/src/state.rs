//! What outlives a call: `.tool-computer/<id>/` beside the display it started, holding
//! `state.json` (found again by the `display` handle it records), the cookie, the logs and the
//! screenshots.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The folder in the work folder that holds one subfolder per display started.
pub const STATE_ROOT: &str = ".tool-computer";
/// Logs are cut at this size; later output is discarded.
pub const LOG_CAP: u64 = 1024 * 1024;
/// How many `screen-<n>.png` files stay; older ones are deleted after each write.
pub const SCREENS_KEPT: usize = 20;

pub const STATE_JSON: &str = "state.json";
pub const XAUTHORITY: &str = "Xauthority";
/// Xvfb's and the window manager's output.
pub const DISPLAY_LOG: &str = "display.log";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowManager {
    pub name: String,
    /// The session-process handle of `wm`.
    pub handle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launched {
    /// The `config.launch` entry.
    pub index: usize,
    pub argv: Vec<String>,
    /// The session-process handle of `launch-<index>`; `None` when it was not started.
    pub handle: Option<String>,
    /// Work-folder-relative path of the program's log.
    pub log: String,
    /// `<rule>: <reason>` when murmur refused to start it.
    pub refused: Option<String>,
}

/// `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The session-process handle of `display`, Xvfb.
    pub handle: String,
    pub display: u16,
    /// `6000 + display`.
    pub port: u16,
    /// `127.0.0.1:<display>`.
    pub address: String,
    pub width: u32,
    pub height: u32,
    pub window_manager: Option<WindowManager>,
    pub launched: Vec<Launched>,
    /// Handles whose exit a result has reported.
    pub reported_exits: Vec<String>,
    /// Unix seconds.
    pub started_at: u64,
}

/// One display's folder, `.tool-computer/<id>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    pub id: String,
    /// The folder, as the tool opens it.
    pub root: PathBuf,
    /// The folder relative to the work folder, as results and Xvfb's arguments name it.
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

    pub fn rel_file(&self, name: &str) -> String {
        format!("{}/{name}", self.rel)
    }

    pub fn load(&self) -> Option<State> {
        serde_json::from_slice(&fs::read(self.file(STATE_JSON)).ok()?).ok()
    }

    /// Writes `state.json` through `state.json.tmp` and a rename, so a reader never sees half.
    pub fn save(&self, state: &State) -> Result<(), String> {
        let tmp = self.file("state.json.tmp");
        let text = serde_json::to_vec_pretty(state)
            .map_err(|e| format!("tool-computer: cannot serialise state.json: {e}"))?;
        fs::write(&tmp, text)
            .and_then(|_| fs::rename(&tmp, self.file(STATE_JSON)))
            .map_err(|e| format!("tool-computer: cannot write {}/state.json: {e}", self.rel))
    }

    /// Writes the next `screen-<n>.png` and keeps only the newest [`SCREENS_KEPT`]; returns its
    /// work-folder-relative path.
    pub fn write_screen(&self, png: &[u8]) -> io::Result<String> {
        let mut numbers = self.screen_numbers();
        let mut n = numbers.last().copied().unwrap_or(0) + 1;
        // A call running beside this one may take the same number first.
        let (name, mut file) = loop {
            let name = format!("screen-{n}.png");
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.file(&name))
            {
                Ok(file) => break (name, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e),
            }
        };
        file.write_all(png)?;
        numbers.push(n);
        if numbers.len() > SCREENS_KEPT {
            for old in &numbers[..numbers.len() - SCREENS_KEPT] {
                let _ = fs::remove_file(self.file(&format!("screen-{old}.png")));
            }
        }
        Ok(self.rel_file(&name))
    }

    fn screen_numbers(&self) -> Vec<u64> {
        let mut numbers: Vec<u64> = fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                name.strip_prefix("screen-")?
                    .strip_suffix(".png")?
                    .parse()
                    .ok()
            })
            .collect();
        numbers.sort_unstable();
        numbers
    }

    /// Appends `bytes` to the log `name`, which stops growing at [`LOG_CAP`].
    pub fn append_log(&self, name: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let path = self.file(name);
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len >= LOG_CAP {
            return;
        }
        let room = (LOG_CAP - len) as usize;
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(&bytes[..bytes.len().min(room)]);
        }
    }

    /// The last `n` non-empty lines of the log `name`.
    pub fn log_tail(&self, name: &str, n: usize) -> Vec<String> {
        let text = fs::read(self.file(name))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(n)..]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    /// Removes a folder this call made and does not keep.
    pub fn discard(&self) {
        let _ = fs::remove_dir_all(&self.root);
        if let Some(parent) = self.root.parent() {
            // Only when empty: another display's folder stays.
            let _ = fs::remove_dir(parent);
        }
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

const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
const FAMILY_WILD: u16 = 0xffff;

/// An Xauthority file holding one FamilyWild MIT-MAGIC-COOKIE-1 entry for `display` (any
/// display when `None`).
pub fn xauthority_bytes(display: Option<u16>, cookie: &[u8]) -> Vec<u8> {
    let number = display.map(|d| d.to_string()).unwrap_or_default();
    // The family is a bare u16; the address, number, name and data are length-prefixed.
    let mut out = FAMILY_WILD.to_be_bytes().to_vec();
    for field in [b"".as_slice(), number.as_bytes(), COOKIE_NAME, cookie] {
        out.extend_from_slice(&(field.len() as u16).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// The cookie of the first MIT-MAGIC-COOKIE-1 entry in an Xauthority file.
pub fn read_cookie(path: &Path) -> Option<Vec<u8>> {
    let bytes = fs::read(path).ok()?;
    let mut rest = bytes.as_slice();
    let field = |rest: &mut &[u8]| -> Option<Vec<u8>> {
        let len = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?) as usize;
        let value = rest.get(2..2 + len)?.to_vec();
        *rest = &rest[2 + len..];
        Some(value)
    };
    while rest.len() >= 2 {
        rest = &rest[2..];
        let _address = field(&mut rest)?;
        let _number = field(&mut rest)?;
        let name = field(&mut rest)?;
        let data = field(&mut rest)?;
        if name == COOKIE_NAME {
            return Some(data);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(handle: &str) -> State {
        State {
            handle: handle.into(),
            display: 3,
            port: 6003,
            address: "127.0.0.1:3".into(),
            width: 1440,
            height: 900,
            window_manager: None,
            launched: vec![Launched {
                index: 0,
                argv: vec!["xmessage".into(), "hi".into()],
                handle: None,
                log: ".tool-computer/a/launch-0-xmessage.log".into(),
                refused: Some("session-process-cap: full".into()),
            }],
            reported_exits: vec!["prc_1".into()],
            started_at: 7,
        }
    }

    #[test]
    fn xauthority_round_trips_its_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Xauthority");
        let cookie: Vec<u8> = (0..16).collect();
        fs::write(&path, xauthority_bytes(Some(7), &cookie)).unwrap();
        assert_eq!(read_cookie(&path).unwrap(), cookie);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xff], "FamilyWild");
        assert_eq!(&bytes[2..4], &[0, 0], "no address");
        assert_eq!(&bytes[4..7], &[0, 1, b'7'], "display number 7");
        assert_eq!(&bytes[7..9], &[0, 18]);
        assert_eq!(&bytes[9..27], COOKIE_NAME);
        assert_eq!(&bytes[27..29], &[0, 16]);
        let any = xauthority_bytes(None, &cookie);
        assert_eq!(&any[4..6], &[0, 0], "no display number");
    }

    #[test]
    fn state_json_round_trips_and_is_found_by_handle() {
        let dir = tempfile::tempdir().unwrap();
        let a = StateDir::new(dir.path(), "aaaa");
        let b = StateDir::new(dir.path(), "bbbb");
        fs::create_dir_all(&a.root).unwrap();
        fs::create_dir_all(&b.root).unwrap();
        a.save(&state("prc_1")).unwrap();
        b.save(&state("prc_2")).unwrap();
        assert!(!a.file("state.json.tmp").exists());
        let (found, loaded) = find_by_handle(dir.path(), "prc_2").unwrap();
        assert_eq!(found, b);
        assert_eq!(loaded, state("prc_2"));
        assert!(find_by_handle(dir.path(), "prc_3").is_none());
        let text = fs::read_to_string(a.file(STATE_JSON)).unwrap();
        for key in [
            "\"handle\"",
            "\"display\"",
            "\"port\"",
            "\"address\"",
            "\"width\"",
            "\"height\"",
            "\"window_manager\": null",
            "\"launched\"",
            "\"refused\"",
            "\"reported_exits\"",
            "\"started_at\"",
        ] {
            assert!(text.contains(key), "{key} in {text}");
        }
        assert!(text.contains("\n  "), "pretty JSON");
    }

    #[test]
    fn only_the_newest_twenty_screens_stay() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "0123456789abcdef");
        fs::create_dir_all(&state.root).unwrap();
        for n in 1..=23 {
            let rel = state.write_screen(b"png").unwrap();
            assert_eq!(
                rel,
                format!(".tool-computer/0123456789abcdef/screen-{n}.png")
            );
        }
        assert_eq!(state.screen_numbers(), (4..=23).collect::<Vec<_>>());
    }

    #[test]
    fn a_log_stops_at_one_mebibyte() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "x");
        fs::create_dir_all(&state.root).unwrap();
        let chunk = vec![b'a'; 300 * 1024];
        for _ in 0..5 {
            state.append_log("x.log", &chunk);
        }
        assert_eq!(fs::metadata(state.file("x.log")).unwrap().len(), LOG_CAP);
    }

    #[test]
    fn the_tail_is_the_last_non_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "x");
        fs::create_dir_all(&state.root).unwrap();
        state.append_log("d.log", b"one\n\ntwo\nthree\n\nfour\n");
        assert_eq!(state.log_tail("d.log", 3), ["two", "three", "four"]);
        assert!(state.log_tail("none.log", 3).is_empty());
    }
}
