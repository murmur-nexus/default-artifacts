//! A call against fake session processes and a fake X server: the start, its refusals and
//! failures, the restart, and the lines each gives (S12).

use super::*;
use crate::host::{Conn, Output, Started};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use x11rb_protocol::protocol::xproto::{
    Depth, Format, GetImageReply, GetInputFocusReply, GetKeyboardMappingReply, ImageOrder,
    QueryExtensionReply, QueryPointerReply, QueryTreeReply, Screen, Setup, VisualClass, Visualtype,
};
use x11rb_protocol::x11_utils::Serialize;

struct FakeClock {
    now: Cell<u64>,
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
    fn sleep_ms(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }
    fn unix_secs(&self) -> u64 {
        1_700_000_000
    }
}

/// An X server answering the requests the tool sends, with a black 1440x900 root.
#[derive(Default)]
struct FakeX {
    out: VecDeque<u8>,
    sequence: u16,
    set_up: bool,
}

fn setup_bytes() -> Vec<u8> {
    let setup = Setup {
        status: 1,
        protocol_major_version: 11,
        resource_id_mask: 0x1f_ffff,
        maximum_request_length: 65_535,
        image_byte_order: ImageOrder::LSB_FIRST,
        bitmap_format_bit_order: ImageOrder::LSB_FIRST,
        bitmap_format_scanline_unit: 32,
        bitmap_format_scanline_pad: 32,
        min_keycode: 8,
        max_keycode: 255,
        vendor: b"fake".to_vec(),
        pixmap_formats: vec![Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
        }],
        roots: vec![Screen {
            root: 1,
            width_in_pixels: 1440,
            height_in_pixels: 900,
            root_visual: 0x21,
            root_depth: 24,
            allowed_depths: vec![Depth {
                depth: 24,
                visuals: vec![Visualtype {
                    visual_id: 0x21,
                    class: VisualClass::TRUE_COLOR,
                    bits_per_rgb_value: 8,
                    colormap_entries: 256,
                    red_mask: 0xff_0000,
                    green_mask: 0xff00,
                    blue_mask: 0xff,
                }],
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut bytes = setup.serialize();
    let length = ((bytes.len() - 8) / 4) as u16;
    bytes[6..8].copy_from_slice(&length.to_ne_bytes());
    bytes
}

impl Conn for FakeX {
    fn read(&mut self, max: u32, _timeout_ms: u32) -> Result<Vec<u8>, ProcError> {
        if self.out.is_empty() {
            return Err(ProcError::TimedOut);
        }
        let n = self.out.len().min(max as usize);
        Ok(self.out.drain(..n).collect())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), ProcError> {
        if !self.set_up {
            self.set_up = true;
            self.out.extend(setup_bytes());
            return Ok(());
        }
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;
        let reply = match bytes[0] {
            98 => QueryExtensionReply {
                sequence,
                length: 0,
                present: true,
                major_opcode: 132,
                first_event: 0,
                first_error: 0,
            }
            .serialize()
            .to_vec(),
            43 => GetInputFocusReply {
                revert_to: Default::default(),
                sequence,
                length: 0,
                focus: 1,
            }
            .serialize()
            .to_vec(),
            101 => {
                let count = usize::from(bytes[5]);
                GetKeyboardMappingReply {
                    keysyms_per_keycode: 2,
                    sequence,
                    keysyms: vec![0; count * 2],
                }
                .serialize()
            }
            38 => QueryPointerReply {
                same_screen: true,
                sequence,
                length: 0,
                root: 1,
                child: 0,
                root_x: 12,
                root_y: 34,
                win_x: 12,
                win_y: 34,
                mask: Default::default(),
            }
            .serialize()
            .to_vec(),
            73 => {
                let width = usize::from(u16::from_ne_bytes([bytes[12], bytes[13]]));
                let height = usize::from(u16::from_ne_bytes([bytes[14], bytes[15]]));
                GetImageReply {
                    depth: 24,
                    sequence,
                    visual: 0x21,
                    data: vec![0; width * height * 4],
                }
                .serialize()
            }
            15 => QueryTreeReply {
                sequence,
                length: 0,
                root: 1,
                parent: 0,
                children: Vec::new(),
            }
            .serialize(),
            _ => Vec::new(),
        };
        let mut reply = reply;
        if !reply.is_empty() && reply.len() < 32 {
            // x11rb-protocol serializes a fixed-size reply without the padding to 32 bytes the
            // wire carries.
            reply.resize(32, 0);
        }
        self.out.extend(reply);
        Ok(())
    }
}

/// One start: name, program, args, env.
type StartRecord = (String, String, Vec<String>, Vec<(String, String)>);

struct FakeProc {
    name: String,
    state: ProcState,
    output: VecDeque<Output>,
}

/// Session processes: Xvfb prints display number 5 unless told otherwise; every program starts
/// unless refused or missing.
#[derive(Default)]
struct FakeProcs {
    procs: Vec<(String, FakeProc)>,
    starts: Vec<StartRecord>,
    stops: Vec<String>,
    /// Names refused, with rule and reason.
    refuse: HashMap<String, (String, String)>,
    /// Programs not on PATH.
    missing: HashSet<String>,
    /// What `display` prints, and its exit once that is read; `None` prints `5\n`.
    display_output: Option<(Vec<Output>, Option<i32>)>,
    /// How many random draws the calls so far made; draw `n` is `n` repeated.
    draws: u8,
}

impl FakeProcs {
    fn handle_of(&self, name: &str) -> Option<String> {
        self.procs
            .iter()
            .rev()
            .find(|(_, p)| p.name == name)
            .map(|(h, _)| h.clone())
    }

    fn get(&mut self, handle: &str) -> Result<&mut FakeProc, ProcError> {
        self.procs
            .iter_mut()
            .find(|(h, _)| h == handle)
            .map(|(_, p)| p)
            .ok_or(ProcError::UnknownHandle)
    }

    fn exit(&mut self, name: &str, code: i32) {
        let handle = self.handle_of(name).unwrap();
        self.get(&handle).unwrap().state = ProcState::Exited(code);
    }
}

impl Processes for FakeProcs {
    fn start(
        &mut self,
        name: &str,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Started, ProcError> {
        self.starts
            .push((name.into(), program.into(), args.to_vec(), env.to_vec()));
        if let Some((rule, reason)) = self.refuse.get(name) {
            return Err(ProcError::Refused {
                rule: rule.clone(),
                reason: reason.clone(),
            });
        }
        if self.missing.contains(program) {
            return Err(ProcError::Failed(
                "No such file or directory (os error 2)".into(),
            ));
        }
        if let Some(handle) = self.handle_of(name) {
            if self.get(&handle)?.state == ProcState::Running {
                return Ok(Started {
                    handle,
                    already_running: true,
                });
            }
        }
        let handle = format!("prc_{:08x}", self.procs.len() + 1);
        let output = if name == DISPLAY_NAME {
            match &self.display_output {
                Some((out, _)) => out.clone().into(),
                None => VecDeque::from([Output {
                    stdout: b"5\n".to_vec(),
                    stderr: Vec::new(),
                }]),
            }
        } else {
            VecDeque::new()
        };
        self.procs.push((
            handle.clone(),
            FakeProc {
                name: name.into(),
                state: ProcState::Running,
                output,
            },
        ));
        Ok(Started {
            handle,
            already_running: false,
        })
    }

    fn find(&mut self, name: &str) -> Option<String> {
        self.handle_of(name)
    }

    fn status(&mut self, handle: &str) -> Result<ProcState, ProcError> {
        let exit = self.display_output.as_ref().and_then(|(_, e)| *e);
        let p = self.get(handle)?;
        if p.name == DISPLAY_NAME && p.output.is_empty() {
            if let Some(code) = exit {
                p.state = ProcState::Exited(code);
            }
        }
        Ok(p.state)
    }

    fn read(&mut self, handle: &str, _max: u32) -> Result<Output, ProcError> {
        Ok(self.get(handle)?.output.pop_front().unwrap_or_default())
    }

    fn stop(&mut self, handle: &str) -> Result<ProcState, ProcError> {
        self.stops.push(handle.into());
        let p = self.get(handle)?;
        if p.state == ProcState::Running {
            p.state = ProcState::Exited(143);
        }
        Ok(p.state)
    }

    fn connect(&mut self, handle: &str, port: u16) -> Result<Box<dyn Conn>, ProcError> {
        assert_eq!(port, 6005, "the port is 6000 + the display number");
        match self.get(handle)?.state {
            ProcState::Running => Ok(Box::new(FakeX::default())),
            ProcState::Exited(_) => Err(ProcError::NotRunning),
        }
    }
}

/// A work folder and a runtime directory granting `granted`.
fn folders(granted: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let runtime = dir.path().join("runtime");
    fs::create_dir_all(&work).unwrap();
    for name in granted {
        fs::create_dir_all(runtime.join("tools").join(name)).unwrap();
        fs::write(runtime.join("tools").join(name).join("murmur.yaml"), "x").unwrap();
    }
    fs::create_dir_all(runtime.join("tools")).unwrap();
    (dir, work, runtime)
}

const DISPLAY: [&str; 2] = ["Xvfb", "xkbcomp"];

fn run_with(
    procs: &mut FakeProcs,
    work: &Path,
    runtime: Option<&Path>,
    config: Option<&str>,
    data: &str,
) -> Reply {
    let clock = FakeClock { now: Cell::new(0) };
    let counter = Cell::new(procs.draws);
    let random = |n: usize| {
        counter.set(counter.get() + 1);
        vec![counter.get(); n]
    };
    let mut ctx = Ctx {
        procs,
        clock: &clock,
        random: &random,
        work: work.to_path_buf(),
        runtime: runtime.map(Path::to_path_buf),
    };
    let reply = call(data, config, &mut ctx);
    procs.draws = counter.get();
    reply
}

const SCREENSHOT: &str = r#"{"action":"screenshot"}"#;

fn state_dirs(work: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(work.join(STATE_ROOT))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn a_first_call_starts_xvfb_with_its_arguments_and_passes() {
    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = FakeProcs::default();
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(reply.passed, "{reply:?}");
    let id = "0101010101010101";
    let lines: Vec<&str> = reply.data.lines().collect();
    assert_eq!(
        lines,
        [
            format!(
                "display: Xvfb 127.0.0.1:5, 1440x900x24; window manager none (grant one of \
                 openbox, fluxbox, icewm, twm or matchbox-window-manager); launched nothing; \
                 shell commands reach it with DISPLAY=127.0.0.1:5 \
                 XAUTHORITY=.tool-computer/{id}/Xauthority"
            )
            .as_str(),
            "screenshot",
            "cursor: 12,34",
            &format!("screenshot: .tool-computer/{id}/screen-1.png (1440x900)"),
        ]
    );
    assert_eq!(reply.summary, "screenshot");
    assert_eq!(
        reply.attachment.as_deref(),
        Some(format!(".tool-computer/{id}/screen-1.png").as_str())
    );
    let (name, program, args, env) = &procs.starts[0];
    assert_eq!((name.as_str(), program.as_str()), ("display", "Xvfb"));
    assert_eq!(
        args.join(" "),
        format!(
            "-displayfd 1 -screen 0 1440x900x24 -auth .tool-computer/{id}/Xauthority -nolisten \
             unix -nolisten local -listen tcp -noreset -dpi 96 +extension XTEST"
        )
    );
    assert!(env.is_empty());
    let dir = StateDir::new(&work, id);
    let state = dir.load().unwrap();
    assert_eq!(
        (state.display, state.port, state.address.as_str()),
        (5, 6005, "127.0.0.1:5")
    );
    assert_eq!(state.handle, "prc_00000001");
    assert_eq!(state.started_at, 1_700_000_000);
    assert_eq!(
        state::read_cookie(&dir.file(XAUTHORITY)).unwrap(),
        vec![2u8; 16]
    );
    assert_eq!(
        &fs::read(dir.file(XAUTHORITY)).unwrap()[4..7],
        &[0, 1, b'5'],
        "the cookie names the display once it is known"
    );

    let again = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(again.passed);
    assert_eq!(again.data.lines().next(), Some("screenshot"), "no banner");
    assert_eq!(procs.starts.len(), 1, "no second start");
}

#[test]
fn the_window_manager_and_programs_get_the_display_and_cookie() {
    let (_d, work, runtime) = folders(&["Xvfb", "xkbcomp", "fluxbox", "twm", "xmessage"]);
    let mut procs = FakeProcs::default();
    procs.missing.insert("fluxbox".into());
    let config = r#"{"launch":[["xmessage","-print","hi"]]}"#;
    let reply = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(reply.passed, "{reply:?}");
    assert!(
        reply
            .data
            .contains("window manager twm; launched xmessage;"),
        "{}",
        reply.data
    );
    let env = vec![
        ("DISPLAY".to_string(), "127.0.0.1:5".to_string()),
        (
            "XAUTHORITY".to_string(),
            ".tool-computer/0101010101010101/Xauthority".to_string(),
        ),
    ];
    let names: Vec<(&str, &str)> = procs
        .starts
        .iter()
        .map(|(n, p, _, _)| (n.as_str(), p.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("display", "Xvfb"),
            ("wm", "fluxbox"),
            ("wm", "twm"),
            ("launch-0", "xmessage")
        ]
    );
    assert_eq!(procs.starts[2].3, env);
    assert_eq!(procs.starts[3].2, ["-print", "hi"]);
    assert_eq!(procs.starts[3].3, env);

    // The program prints and exits: its exit is reported once, its output is in its log.
    let handle = procs.handle_of("launch-0").unwrap();
    procs.get(&handle).unwrap().output.push_back(Output {
        stdout: b"Press\n".to_vec(),
        stderr: Vec::new(),
    });
    procs.exit("launch-0", 0);
    let exit_line = "launched xmessage (launch[0]) exited with status 0; output in \
                     .tool-computer/0101010101010101/launch-0-xmessage.log";
    let next = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert_eq!(next.data.lines().last(), Some(exit_line), "{}", next.data);
    let log =
        fs::read_to_string(work.join(".tool-computer/0101010101010101/launch-0-xmessage.log"))
            .unwrap();
    assert_eq!(log, "Press\n");
    let after = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(!after.data.contains("exited"), "{}", after.data);

    procs.exit("wm", 1);
    let wm = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(
        wm.data.ends_with(
            "window manager twm exited with status 1; output in \
             .tool-computer/0101010101010101/display.log"
        ),
        "{}",
        wm.data
    );
}

#[test]
fn s12_a_launch_refused_by_the_process_cap_passes_with_one_line() {
    let (_d, work, runtime) = folders(&["Xvfb", "xkbcomp", "xmessage", "xclock"]);
    let mut procs = FakeProcs::default();
    procs.refuse.insert(
        "launch-1".into(),
        (
            "session-process-cap".into(),
            "this session already holds 8 running processes, the most it may; stop one first"
                .into(),
        ),
    );
    let config = r#"{"launch":[["xmessage","hi"],["xclock"]]}"#;
    let reply = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(reply.passed, "{reply:?}");
    let line = "launched xclock (launch[1]) was not started: murmur refused it: \
                session-process-cap: this session already holds 8 running processes, the most \
                it may; stop one first";
    assert_eq!(reply.data.matches(line).count(), 1, "{}", reply.data);
    assert!(
        reply.data.contains("; launched xmessage; "),
        "{}",
        reply.data
    );
    let state = StateDir::new(&work, "0101010101010101").load().unwrap();
    assert_eq!(state.launched[1].handle, None);
    assert_eq!(
        state.launched[1].refused.as_deref(),
        Some(
            "session-process-cap: this session already holds 8 running processes, the most it \
             may; stop one first"
        )
    );
    let next = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(!next.data.contains("was not started"), "{}", next.data);
}

#[test]
fn s12_a_launch_not_found_is_logged_and_reported_as_status_127() {
    let (_d, work, runtime) = folders(&["Xvfb", "xkbcomp", "xmessage"]);
    let mut procs = FakeProcs::default();
    procs.missing.insert("xmessage".into());
    let config = r#"{"launch":[["xmessage","hi"]]}"#;
    let reply = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(reply.passed, "{reply:?}");
    assert_eq!(
        reply.data.lines().last(),
        Some(
            "launched xmessage (launch[0]) exited with status 127; output in \
             .tool-computer/0101010101010101/launch-0-xmessage.log"
        )
    );
    let log =
        fs::read_to_string(work.join(".tool-computer/0101010101010101/launch-0-xmessage.log"))
            .unwrap();
    assert_eq!(
        log,
        "tool-computer: cannot start xmessage: No such file or directory (os error 2)\n"
    );
    let next = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(
        !next.data.contains("exited"),
        "reported once: {}",
        next.data
    );
}

#[test]
fn s12_a_refused_xvfb_names_the_rule_and_leaves_no_folder() {
    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = FakeProcs::default();
    procs.refuse.insert(
        "display".into(),
        (
            "capabilities.filesystem.read_only".into(),
            "Xvfb writes .tool-computer".into(),
        ),
    );
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(!reply.passed);
    assert_eq!(
        reply.data,
        "tool-computer: murmur refused to start Xvfb: capabilities.filesystem.read_only: Xvfb \
         writes .tool-computer"
    );
    assert_eq!(reply.summary, reply.data);
    assert!(reply.attachment.is_none());
    assert!(!work.join(STATE_ROOT).exists());
}

#[test]
fn xvfb_missing_from_path_names_its_package_and_leaves_no_folder() {
    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = FakeProcs::default();
    procs.missing.insert("Xvfb".into());
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert_eq!(reply.data, grants::not_installed_line("Xvfb", Some("xvfb")));
    assert!(!work.join(STATE_ROOT).exists());
}

fn exiting_xvfb(stderr: &str) -> FakeProcs {
    FakeProcs {
        display_output: Some((
            vec![Output {
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            }],
            Some(1),
        )),
        ..Default::default()
    }
}

const KEYMAP_STDERR: &str = "sh: 1: /usr/bin/xkbcomp: not found\nXKB: Failed to compile keymap\n\
                             Keyboard initialization failed. This could be a missing or incorrect setup of xkeyboard-config.\n\
                             (EE) Fatal server error:\n(EE) Failed to activate virtual core keyboard: 2(EE)\n";

#[test]
fn s12_an_exited_xvfb_with_keymap_stderr_gives_both_hints() {
    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = exiting_xvfb(KEYMAP_STDERR);
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(!reply.passed);
    assert_eq!(
        reply.data,
        "tool-computer: the display did not start: Xvfb exited with status 1; the last lines of \
         .tool-computer/0101010101010101/display.log: Keyboard initialization failed. This \
         could be a missing or incorrect setup of xkeyboard-config. | (EE) Fatal server error: \
         | (EE) Failed to activate virtual core keyboard: 2(EE); Xvfb compiles its keymap by \
         running xkbcomp through sh, so add sh to capabilities.shell.allow; install the \
         x11-xkb-utils package on this host first"
    );

    let (_d, work, runtime) = folders(&["Xvfb", "xkbcomp", "sh"]);
    let mut procs = exiting_xvfb(KEYMAP_STDERR);
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(
        reply
            .data
            .ends_with("2(EE); install the x11-xkb-utils package on this host first"),
        "{}",
        reply.data
    );

    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = exiting_xvfb(
        "XKB: Failed to compile keymap\n(EE) Failed to activate virtual core keyboard\n",
    );
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(
        reply.data.ends_with(
            "keyboard; Xvfb compiles its keymap by running xkbcomp through sh, so add sh to \
             capabilities.shell.allow"
        ),
        "{}",
        reply.data
    );

    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = exiting_xvfb("");
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(
        reply.data.ends_with("display.log: nothing"),
        "{}",
        reply.data
    );
}

#[test]
fn a_silent_xvfb_is_stopped_after_fifteen_seconds() {
    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = FakeProcs {
        display_output: Some((Vec::new(), None)),
        ..Default::default()
    };
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert!(
        reply.data.starts_with(
            "tool-computer: the display did not start: Xvfb gave no display number within 15 s; \
             the last lines of .tool-computer/0101010101010101/display.log: nothing"
        ),
        "{}",
        reply.data
    );
    assert_eq!(procs.stops, ["prc_00000001"]);
}

#[test]
fn a_running_display_without_state_json_is_stopped() {
    let (_d, work, runtime) = folders(&DISPLAY);
    let mut procs = FakeProcs::default();
    procs.start("display", "Xvfb", &[], &[]).unwrap();
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert_eq!(
        reply.data,
        "tool-computer: the running display (prc_00000001) has no state.json under \
         .tool-computer/, so it was stopped; the next call starts a new one"
    );
    assert_eq!(procs.stops, ["prc_00000001"]);
}

#[test]
fn a_stopped_display_is_replaced_and_its_programs_stopped_first() {
    let (_d, work, runtime) = folders(&["Xvfb", "xkbcomp", "twm", "xmessage"]);
    let config = r#"{"launch":[["xmessage","hi"]]}"#;
    let mut procs = FakeProcs::default();
    run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    procs.exit("display", 137);
    let reply = run_with(&mut procs, &work, Some(&runtime), Some(config), SCREENSHOT);
    assert!(reply.passed, "{reply:?}");
    let lines: Vec<&str> = reply.data.lines().collect();
    assert_eq!(lines[0], RESTART_LINE);
    assert!(
        lines[1].starts_with("display: Xvfb 127.0.0.1:5")
            && lines[1].contains("launched xmessage;"),
        "{}",
        lines[1]
    );
    assert!(
        lines[1].contains(".tool-computer/0303030303030303/Xauthority"),
        "a new folder: {}",
        lines[1]
    );
    assert_eq!(
        procs.stops,
        ["prc_00000002", "prc_00000003"],
        "wm, launch-0"
    );
    let launch_starts = procs.starts.iter().filter(|s| s.0 == "launch-0").count();
    assert_eq!(launch_starts, 2);
    assert_eq!(state_dirs(&work).len(), 2);
}

#[test]
fn the_checks_come_before_any_start() {
    let (_d, work, runtime) = folders(&["Xvfb"]);
    let mut procs = FakeProcs::default();
    let reply = run_with(&mut procs, &work, Some(&runtime), None, SCREENSHOT);
    assert_eq!(
        reply.data,
        "tool-computer needs a display server: add xkbcomp to capabilities.shell.allow"
    );
    let reply = run_with(&mut procs, &work, None, None, SCREENSHOT);
    assert_eq!(reply.data, SCOPE_LINE);
    let reply = run_with(
        &mut procs,
        &work,
        None,
        Some(r#"{"confirm":["screenshot"]}"#),
        SCREENSHOT,
    );
    assert!(reply
        .data
        .starts_with("tool-computer did not run screenshot"));
    let reply = run_with(
        &mut procs,
        &work,
        Some(&runtime),
        None,
        r#"{"action":"fly"}"#,
    );
    assert!(
        reply.data.contains("'fly' is not an action"),
        "{}",
        reply.data
    );
    let reply = run_with(&mut procs, &work, Some(&runtime), None, "");
    assert_eq!(reply.data, "tool-computer: no input");
    let reply = run_with(
        &mut procs,
        &work,
        Some(&runtime),
        None,
        r#""{\"action\":\"wait\",\"duration\":0.1}""#,
    );
    assert!(
        reply.data.contains("needs a display server"),
        "a JSON string is data"
    );
    assert!(procs.starts.is_empty());
    assert!(!work.join(STATE_ROOT).exists());
}

#[test]
fn display_numbers_are_the_first_stdout_line() {
    assert_eq!(parse_display_number(b"7\n"), Some(7));
    assert_eq!(parse_display_number(b" 12 \nmore"), Some(12));
    assert_eq!(parse_display_number(b"7"), None);
    assert_eq!(parse_display_number(b"x\n"), None);
}
