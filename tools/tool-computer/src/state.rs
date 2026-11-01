//! The state folder, `<work folder>/.tool-computer/<session-id>/`, and every file in it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Logs are cut at this size; later output is discarded.
pub const LOG_CAP: u64 = 1024 * 1024;
/// How many `screen-<n>.png` files stay; older ones are deleted after each write.
pub const SCREENS_KEPT: usize = 20;

pub const DISPLAY_JSON: &str = "display.json";
pub const STOPPED_JSON: &str = "stopped.json";
pub const XAUTHORITY: &str = "Xauthority";
pub const KEEPER_LOG: &str = "keeper.log";
pub const START_LOCK: &str = "start.lock";
pub const EXITS: &str = "exits.jsonl";
pub const EXITS_SEEN: &str = "exits.seen";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowManager {
    pub name: String,
    pub pid: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Launched {
    pub index: usize,
    pub argv: Vec<String>,
    pub pid: i32,
    /// Work-folder-relative path of the program's log.
    pub log: String,
}

/// `display.json`, written by the keeper once the display is ready.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub display: u16,
    /// `127.0.0.1:<display>`.
    pub address: String,
    pub width: u32,
    pub height: u32,
    pub keeper_pid: i32,
    pub xvfb_pid: i32,
    pub window_manager: Option<WindowManager>,
    pub launched: Vec<Launched>,
    /// Unix seconds.
    pub started_at: u64,
}

/// `stopped.json`, written by the keeper when the display stops or fails to start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stopped {
    pub reason: String,
    /// `false` when the display never started.
    pub was_running: bool,
    /// Xvfb's exit status, when it exited.
    pub xvfb_status: Option<i32>,
}

/// One line of `exits.jsonl`. `index` is the `launch` entry, or `None` for the window manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exit {
    pub index: Option<usize>,
    pub argv0: String,
    pub status: i32,
}

/// The paths of one session's state folder.
#[derive(Debug, Clone)]
pub struct StateDir {
    /// Absolute.
    pub abs: PathBuf,
    /// Relative to the work folder, as attachments and `data` name it.
    pub rel: PathBuf,
}

impl StateDir {
    pub fn new(workdir: &Path, session_id: &str) -> Self {
        let rel = Path::new(".tool-computer").join(session_id);
        Self {
            abs: workdir.join(&rel),
            rel,
        }
    }

    /// From the keeper's absolute path argument; `rel` is the last two components.
    pub fn from_abs(abs: &Path) -> Self {
        let session = abs.file_name().map(PathBuf::from).unwrap_or_default();
        Self {
            abs: abs.to_path_buf(),
            rel: Path::new(".tool-computer").join(session),
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.abs.join(name)
    }

    pub fn rel_path(&self, name: &str) -> String {
        self.rel.join(name).to_string_lossy().into_owned()
    }

    pub fn create(&self) -> io::Result<()> {
        fs::create_dir_all(&self.abs)?;
        fs::set_permissions(&self.abs, fs::Permissions::from_mode(0o700))
    }

    pub fn read_display(&self) -> Option<DisplayInfo> {
        let text = fs::read_to_string(self.path(DISPLAY_JSON)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn read_stopped(&self) -> Option<Stopped> {
        let text = fs::read_to_string(self.path(STOPPED_JSON)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write_display(&self, info: &DisplayInfo) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(info).expect("display.json serializes");
        write_atomic(&self.path(DISPLAY_JSON), &json, 0o600)
    }

    pub fn write_stopped(&self, stopped: &Stopped) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(stopped).expect("stopped.json serializes");
        write_atomic(&self.path(STOPPED_JSON), &json, 0o600)
    }

    /// Removes `display.json` only when it names `keeper_pid`, so a stopping keeper never
    /// removes the file of the keeper that replaced it.
    pub fn remove_display_if_kept_by(&self, keeper_pid: i32) {
        if self
            .read_display()
            .is_some_and(|info| info.keeper_pid == keeper_pid)
        {
            let _ = fs::remove_file(self.path(DISPLAY_JSON));
        }
    }

    pub fn append_exit(&self, exit: &Exit) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.path(EXITS))?;
        let mut line = serde_json::to_string(exit).expect("exit serializes");
        line.push('\n');
        file.write_all(line.as_bytes())
    }

    /// The exits not yet reported, which this call now reports.
    pub fn take_new_exits(&self) -> Vec<Exit> {
        let Ok(file) = File::open(self.path(EXITS)) else {
            return Vec::new();
        };
        let all: Vec<Exit> = io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();
        let seen: usize = fs::read_to_string(self.path(EXITS_SEEN))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if all.len() <= seen {
            return Vec::new();
        }
        let _ = write_atomic(
            &self.path(EXITS_SEEN),
            all.len().to_string().as_bytes(),
            0o600,
        );
        all[seen..].to_vec()
    }

    /// Writes the next `screen-<n>.png` and keeps only the newest [`SCREENS_KEPT`]; returns its
    /// work-folder-relative path.
    pub fn write_screen(&self, png: &[u8]) -> io::Result<String> {
        let mut numbers = self.screen_numbers();
        let n = numbers.last().copied().unwrap_or(0) + 1;
        let name = format!("screen-{n}.png");
        let path = self.path(&name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(png)?;
        numbers.push(n);
        if numbers.len() > SCREENS_KEPT {
            for old in &numbers[..numbers.len() - SCREENS_KEPT] {
                let _ = fs::remove_file(self.path(&format!("screen-{old}.png")));
            }
        }
        Ok(self.rel_path(&name))
    }

    fn screen_numbers(&self) -> Vec<u64> {
        let mut numbers: Vec<u64> = fs::read_dir(&self.abs)
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

    /// The last `n` lines of `keeper.log`.
    pub fn keeper_log_tail(&self, n: usize) -> Vec<String> {
        let text = fs::read_to_string(self.path(KEEPER_LOG)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(n)..]
            .iter()
            .map(|l| l.to_string())
            .collect()
    }
}

/// Writes `bytes` to a temporary file beside `path` and renames it over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
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

/// An append-only log cut at [`LOG_CAP`], shared by the threads that drain a child's pipes.
#[derive(Clone)]
pub struct CappedLog {
    file: Arc<Mutex<File>>,
}

impl CappedLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn write(&self, bytes: &[u8]) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(LOG_CAP);
        if len >= LOG_CAP {
            return;
        }
        let room = (LOG_CAP - len) as usize;
        let _ = file.write_all(&bytes[..bytes.len().min(room)]);
    }

    /// Copies `reader` into the log on a thread until it reaches end of file.
    pub fn drain(&self, mut reader: impl io::Read + Send + 'static) {
        let log = self.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log.write(&buf[..n]),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn exits_are_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "ses_x");
        state.create().unwrap();
        assert!(state.take_new_exits().is_empty());
        let exit = Exit {
            index: Some(0),
            argv0: "xmessage".into(),
            status: 0,
        };
        state.append_exit(&exit).unwrap();
        assert_eq!(state.take_new_exits(), vec![exit.clone()]);
        assert!(state.take_new_exits().is_empty());
        state.append_exit(&exit).unwrap();
        assert_eq!(state.take_new_exits().len(), 1);
    }

    #[test]
    fn only_the_newest_twenty_screens_stay() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "ses_x");
        state.create().unwrap();
        for n in 1..=23 {
            let rel = state.write_screen(b"png").unwrap();
            assert_eq!(rel, format!(".tool-computer/ses_x/screen-{n}.png"));
        }
        assert_eq!(state.screen_numbers(), (4..=23).collect::<Vec<_>>());
    }

    #[test]
    fn display_json_is_removed_only_by_its_keeper() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path(), "ses_x");
        state.create().unwrap();
        let info = DisplayInfo {
            display: 3,
            address: "127.0.0.1:3".into(),
            width: 1440,
            height: 900,
            keeper_pid: 10,
            xvfb_pid: 11,
            window_manager: None,
            launched: Vec::new(),
            started_at: 0,
        };
        state.write_display(&info).unwrap();
        let mode = fs::metadata(state.path(DISPLAY_JSON))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        state.remove_display_if_kept_by(99);
        assert_eq!(state.read_display(), Some(info));
        state.remove_display_if_kept_by(10);
        assert_eq!(state.read_display(), None);
    }

    #[test]
    fn a_capped_log_stops_at_one_mebibyte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        let log = CappedLog::open(&path).unwrap();
        let chunk = vec![b'a'; 300 * 1024];
        for _ in 0..5 {
            log.write(&chunk);
        }
        assert_eq!(fs::metadata(&path).unwrap().len(), LOG_CAP);
    }
}
