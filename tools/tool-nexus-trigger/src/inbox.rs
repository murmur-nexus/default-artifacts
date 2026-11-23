//! One exchange with a Nexus window over the trigger inbox: write the request, wait for the
//! answer, read it and clean up.
//!
//! The inbox is the folder at `root`: the guest's `.`, which the operator's
//! `capabilities.filesystem.scope: .nexus/triggers/inbox` mounts, or a temp folder in a host test.
//! Each exchange touches three names under it, all built by [`Names`], and nothing else: it never
//! lists the folder, never creates a folder, and never touches the names only a window writes
//! (`<rid>.taken`, `<rid>.answer.tmp`).
//!
//! Protocol v1 order, the writer's side:
//!
//! 1. create `<rid>.tmp` exclusively, write the request and flush it;
//! 2. rename `<rid>.tmp` to `<rid>.json`;
//! 3. a window renames `<rid>.json` to `<rid>.taken`, acts, and renames its own temp file to
//!    `<rid>.answer.json`;
//! 4. read `<rid>.answer.json`, then remove it.
//!
//! When the wait runs out, the writer removes its own `<rid>.json`. If that remove finds nothing,
//! a window took the request, and may have acted on it.
//!
//! Time, sleep and randomness are injected, so the same code runs in the guest (on
//! `wasi:clocks` and `wasi:random`) and in host tests (on a fake clock).

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::ops::{Exchange, Exchanged};

/// How long one call waits for the answer, on the monotonic clock. The protocol asks a writer to
/// wait at least 5 seconds; a window reads the inbox about once a second.
pub const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// How often the answer's name is checked while waiting.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Largest answer read, in bytes. An answer holds the lines `nexus trigger` prints, which for a
/// folder of at most 16 live triggers is a few KiB.
pub const ANSWER_MAX_BYTES: u64 = 256 * 1024;

/// Random bytes in a request id; the id is their lowercase hex, 16 digits (`^[0-9a-f]{16}$`).
pub const RID_BYTES: usize = 8;

/// The request under its temporary name, created exclusively by the writer.
pub const REQUEST_TMP_SUFFIX: &str = ".tmp";

/// The request, renamed from its temporary name.
pub const REQUEST_SUFFIX: &str = ".json";

/// The answer, renamed into place by the window.
pub const ANSWER_SUFFIX: &str = ".answer.json";

/// The request id for `bytes`: 16 lowercase hex digits.
pub fn rid(bytes: [u8; RID_BYTES]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The three paths one exchange uses, each `root.join(<rid><suffix>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    pub request_tmp: PathBuf,
    pub request: PathBuf,
    pub answer: PathBuf,
}

impl Names {
    pub fn new(root: &Path, rid: &str) -> Self {
        Self {
            request_tmp: root.join(format!("{rid}{REQUEST_TMP_SUFFIX}")),
            request: root.join(format!("{rid}{REQUEST_SUFFIX}")),
            answer: root.join(format!("{rid}{ANSWER_SUFFIX}")),
        }
    }
}

/// A monotonic clock and a way to wait on it.
pub trait Clock {
    /// Time since an arbitrary fixed origin; never goes backwards.
    fn elapsed(&self) -> Duration;
    /// Block for `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// The real clock: `std::time::Instant` and `std::thread::sleep`, which a `wasm32-wasip2` guest
/// runs on `wasi:clocks/monotonic-clock`.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The inbox at `root`, with the clock and the randomness one exchange uses.
pub struct Inbox<'a> {
    root: &'a Path,
    random: &'a mut dyn FnMut() -> [u8; RID_BYTES],
    clock: &'a mut dyn Clock,
    wait: Duration,
}

impl<'a> Inbox<'a> {
    /// An inbox that waits [`ANSWER_WAIT`] for each answer.
    pub fn new(
        root: &'a Path,
        random: &'a mut dyn FnMut() -> [u8; RID_BYTES],
        clock: &'a mut dyn Clock,
    ) -> Self {
        Self {
            root,
            random,
            clock,
            wait: ANSWER_WAIT,
        }
    }

    /// The same inbox with a different wait, for tests that would otherwise sleep 5 seconds.
    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }
}

impl Exchange for Inbox<'_> {
    fn exchange(&mut self, request: &[u8]) -> Exchanged {
        let names = Names::new(self.root, &rid((self.random)()));

        // A `<rid>.tmp` that create_new found already there is not this call's to remove.
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&names.request_tmp)
        {
            Ok(file) => file,
            Err(err) => return Exchanged::CannotWrite(err.to_string()),
        };
        if let Err(err) = write_request(file, &names, request) {
            remove_if_present(&names.request_tmp);
            return Exchanged::CannotWrite(err.to_string());
        }

        let start = self.clock.elapsed();
        loop {
            if let Some(found) = take_answer(&names) {
                return found;
            }
            let waited = self.clock.elapsed().saturating_sub(start);
            if waited >= self.wait {
                break;
            }
            self.clock.sleep(POLL_INTERVAL.min(self.wait - waited));
        }

        match fs::remove_file(&names.request) {
            Ok(()) => Exchanged::NoAnswer,
            // A window renamed the request to `<rid>.taken`; its answer may have landed since the
            // last look.
            Err(err) if err.kind() == ErrorKind::NotFound => {
                take_answer(&names).unwrap_or(Exchanged::TakenNoAnswer)
            }
            Err(err) => Exchanged::NotRemoved(err.to_string()),
        }
    }
}

/// Steps 1 and 2, after the exclusive create: write and flush `<rid>.tmp`, close it, and rename
/// it to `<rid>.json`.
fn write_request(mut file: fs::File, names: &Names, request: &[u8]) -> io::Result<()> {
    file.write_all(request)?;
    file.flush()?;
    drop(file);
    fs::rename(&names.request_tmp, &names.request)
}

/// The answer, when something is at its name. A regular file of at most [`ANSWER_MAX_BYTES`] is
/// read and removed. A link is removed without being followed; a folder is left alone. `None`
/// while nothing is there.
fn take_answer(names: &Names) -> Option<Exchanged> {
    let meta = match fs::symlink_metadata(&names.answer) {
        Ok(meta) => meta,
        Err(_) => return None,
    };
    let file_type = meta.file_type();
    if !file_type.is_file() {
        if file_type.is_symlink() {
            remove_if_present(&names.answer);
        }
        // Not a window's answer: make sure no window acts on the request later either.
        remove_if_present(&names.request);
        return Some(Exchanged::Unreadable(
            if file_type.is_symlink() {
                "a link, not a file"
            } else {
                "not a regular file"
            }
            .to_string(),
        ));
    }
    if meta.len() > ANSWER_MAX_BYTES {
        remove_if_present(&names.answer);
        return Some(Exchanged::Unreadable(too_large()));
    }

    let read = read_capped(&names.answer);
    remove_if_present(&names.answer);
    Some(match read {
        Ok(bytes) if bytes.len() as u64 > ANSWER_MAX_BYTES => Exchanged::Unreadable(too_large()),
        Ok(bytes) => Exchanged::Answer(bytes),
        Err(err) => Exchanged::Unreadable(format!("cannot be read ({err})")),
    })
}

/// At most one byte more than [`ANSWER_MAX_BYTES`] of the file, so a file that grew after its
/// size was checked is still caught.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(ANSWER_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn too_large() -> String {
    format!("larger than {} KiB", ANSWER_MAX_BYTES / 1024)
}

/// Remove a file of this exchange's own; one already gone is fine.
fn remove_if_present(path: &Path) {
    let _ = fs::remove_file(path);
}
