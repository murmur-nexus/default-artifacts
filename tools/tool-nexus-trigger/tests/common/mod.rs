//! A fake Nexus window on a temp inbox: the window side of murmur-nexus's trigger inbox
//! protocol v1 (`src/triggers/inbox.ts` at `1a8aba3`), cut down to what the tool's tests need.
//!
//! It polls the folder, takes each `<rid>.json` by renaming it to `<rid>.taken`, reads and
//! removes that, acts on a small in-memory trigger list, and answers by creating
//! `<rid>.answer.tmp` exclusively and renaming it to `<rid>.answer.json`. Other modes misbehave on
//! purpose.

#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

/// How the fake answers each request it takes.
#[derive(Clone)]
pub enum Mode {
    /// As a window does.
    Answer,
    /// Takes the request (rename, read, remove) and never answers.
    TakeNoAnswer,
    /// Answers with these bytes.
    Raw(Vec<u8>),
    /// Puts a link to this file at the answer's name.
    Symlink(PathBuf),
    /// Puts a folder at the answer's name.
    Folder,
}

pub struct FakeNexus {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeNexus {
    pub fn start(dir: &Path, mode: Mode) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (dir, stop_flag, recorded) =
            (dir.to_path_buf(), Arc::clone(&stop), Arc::clone(&requests));
        let handle = thread::spawn(move || {
            let mut window = Window::default();
            while !stop_flag.load(Ordering::SeqCst) {
                window.pass(&dir, &mode, &recorded);
                thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            stop,
            handle: Some(handle),
            requests,
        }
    }

    /// Every request body taken so far, as JSON.
    pub fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|bytes| serde_json::from_slice(bytes).expect("a request is JSON"))
            .collect()
    }

    /// Every request body taken so far, as bytes.
    pub fn raw_requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeNexus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
struct Window {
    next: u32,
    triggers: Vec<(String, String)>,
}

fn is_request(name: &str) -> Option<&str> {
    let rid = name.strip_suffix(".json")?;
    (rid.len() == 16
        && rid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(rid)
}

impl Window {
    fn pass(&mut self, dir: &Path, mode: &Mode, recorded: &Mutex<Vec<Vec<u8>>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            let Some(rid) = is_request(&name) else {
                continue;
            };
            let taken = dir.join(format!("{rid}.taken"));
            if fs::rename(dir.join(&name), &taken).is_err() {
                continue;
            }
            let Ok(bytes) = fs::read(&taken) else {
                continue;
            };
            let _ = fs::remove_file(&taken);
            recorded.lock().unwrap().push(bytes.clone());
            let answer_path = dir.join(format!("{rid}.answer.json"));
            match mode {
                Mode::Answer => {
                    let body = self.act(dir, &bytes);
                    write_answer(dir, rid, body.as_bytes());
                }
                Mode::TakeNoAnswer => {}
                Mode::Raw(raw) => write_answer(dir, rid, raw),
                Mode::Symlink(target) => {
                    std::os::unix::fs::symlink(target, &answer_path).unwrap();
                }
                Mode::Folder => fs::create_dir(&answer_path).unwrap(),
            }
        }
    }

    /// The answer `nexus trigger` would give, as protocol v1 JSON and a newline.
    fn act(&mut self, dir: &Path, bytes: &[u8]) -> String {
        let request: Value = serde_json::from_slice(bytes).unwrap();
        let dir = dir.display();
        let text = |key: &str| request.get(key).and_then(Value::as_str).map(str::to_string);
        let (exit, out, err): (i64, Vec<String>, Vec<String>) =
            match request["op"].as_str().unwrap() {
                "add" => {
                    self.next += 1;
                    let id = format!("t{}", self.next);
                    let label = text("label")
                        .or_else(|| text("until"))
                        .unwrap_or_else(|| "clock".into());
                    self.triggers.push((id.clone(), label.clone()));
                    (
                        0,
                        vec![
                            format!("trigger {id} awaits approval: a person types /trigger approve {id} in a nexus window open on {dir}"),
                            format!("label {label}"),
                        ],
                        vec![],
                    )
                }
                "list" if self.triggers.is_empty() => (0, vec![format!("no triggers in {dir}")], vec![]),
                "list" => (
                    0,
                    self.triggers
                        .iter()
                        .map(|(id, label)| {
                            format!("{id}  awaiting-approval  {label} · project · awaits approval in a nexus window: /trigger approve {id}")
                        })
                        .collect(),
                    vec![],
                ),
                "show" | "cancel" => {
                    let id = text("id").unwrap();
                    match self.triggers.iter().position(|(t, _)| *t == id) {
                        None => (1, vec![], vec![format!("no trigger {id} in {dir}")]),
                        Some(at) if request["op"] == "show" => {
                            (0, vec![format!("{id}  awaiting-approval  {}", self.triggers[at].1)], vec![])
                        }
                        Some(at) if request.get("yes") != Some(&Value::Bool(true)) => (
                            1,
                            vec![],
                            vec![format!("cancel trigger {id} · {}? nothing was cancelled; pass --yes to cancel it without a terminal", self.triggers[at].1)],
                        ),
                        Some(at) => {
                            let (_, label) = self.triggers.remove(at);
                            (0, vec![format!("cancelled trigger {id} · {label}")], vec![])
                        }
                    }
                }
                other => panic!("the fake got op {other}"),
            };
        format!(
            "{}\n",
            json!({"v": 1, "exit": exit, "out": out, "err": err})
        )
    }
}

/// `<rid>.answer.tmp` created exclusively and renamed to `<rid>.answer.json`.
fn write_answer(dir: &Path, rid: &str, body: &[u8]) {
    let tmp = dir.join(format!("{rid}.answer.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .unwrap();
    file.write_all(body).unwrap();
    drop(file);
    fs::rename(&tmp, dir.join(format!("{rid}.answer.json"))).unwrap();
}

/// The names in `dir`, sorted.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}
