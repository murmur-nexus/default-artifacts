//! Performs a checked [`Command`] through an [`XBackend`], then captures the screen.

use std::time::Duration;

use crate::input::{Command, Modifiers, Point};
use crate::keys::{self, Combo};
use crate::xbackend::{Frame, KeyboardMapping, XBackend, XResult};
use crate::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

const CLICK_GAP: Duration = Duration::from_millis(80);
const DRAG_STEPS: u32 = 10;
const DRAG_STEP_GAP: Duration = Duration::from_millis(20);
const TYPE_GAP: Duration = Duration::from_millis(10);
/// How long a remapped keycode is left alone before it is remapped again or restored, so a
/// client reads the press with the mapping it was made under.
const REMAP_GRACE: Duration = Duration::from_millis(50);

const SETTLE_FIRST: Duration = Duration::from_millis(150);
const SETTLE_STEP: Duration = Duration::from_millis(100);
const SETTLE_CAP: Duration = Duration::from_millis(2000);

fn with_modifiers(line: String, modifiers: &Modifiers) -> String {
    if modifiers.keys.is_empty() {
        line
    } else {
        format!("{line} with {}", modifiers.text)
    }
}

fn at(point: Option<Point>) -> String {
    match point {
        Some(p) => format!("at {p}"),
        None => "at the cursor".to_string(),
    }
}

/// Keysyms resolved to keycodes against the server's mapping, remapping a spare keycode for a
/// keysym no key carries. [`Keyboard::restore`] clears every remap.
struct Keyboard {
    mapping: KeyboardMapping,
    spare: Vec<u8>,
    remapped: Vec<(u8, u32)>,
}

impl Keyboard {
    fn new<B: XBackend>(backend: &mut B) -> XResult<Self> {
        let mapping = backend.keyboard_mapping()?;
        let spare = mapping.spare_keycodes();
        Ok(Self {
            mapping,
            spare,
            remapped: Vec::new(),
        })
    }

    /// The keycode for `keysym` and whether Shift must be held for it.
    fn resolve<B: XBackend>(&mut self, backend: &mut B, keysym: u32) -> XResult<(u8, bool)> {
        if let Some(found) = self.mapping.find(keysym) {
            return Ok(found);
        }
        if let Some((code, _)) = self.remapped.iter().find(|(_, sym)| *sym == keysym) {
            return Ok((*code, false));
        }
        let code = match self.spare.pop() {
            Some(code) => code,
            None => {
                if self.remapped.is_empty() {
                    return Err(format!(
                        "tool-computer: no key types keysym 0x{keysym:x} and the keymap has no \
                         spare keycode to map it to"
                    ));
                }
                let (code, _) = self.remapped.remove(0);
                backend.sleep(REMAP_GRACE);
                code
            }
        };
        backend.change_mapping(code, &[keysym, keysym])?;
        self.remapped.push((code, keysym));
        Ok((code, false))
    }

    fn shift<B: XBackend>(&mut self, backend: &mut B) -> XResult<u8> {
        self.resolve(backend, keys::SHIFT_L).map(|(code, _)| code)
    }

    /// The keycodes a combo presses, in order, with Shift added before a key that needs it.
    fn combo<B: XBackend>(&mut self, backend: &mut B, combo: &Combo) -> XResult<Vec<u8>> {
        let mut codes = Vec::new();
        let has_shift = combo.contains(&keys::SHIFT_L);
        for keysym in combo {
            let (code, shift) = self.resolve(backend, *keysym)?;
            if shift && !has_shift {
                let shift_code = self.shift(backend)?;
                if !codes.contains(&shift_code) {
                    codes.push(shift_code);
                }
            }
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
        Ok(codes)
    }

    fn restore<B: XBackend>(&mut self, backend: &mut B) -> XResult<()> {
        if self.remapped.is_empty() {
            return Ok(());
        }
        backend.sleep(REMAP_GRACE);
        for (code, _) in std::mem::take(&mut self.remapped) {
            backend.change_mapping(code, &[])?;
        }
        Ok(())
    }
}

fn press_all<B: XBackend>(backend: &mut B, codes: &[u8]) -> XResult<()> {
    codes.iter().try_for_each(|code| backend.key(*code, true))
}

fn release_all<B: XBackend>(backend: &mut B, codes: &[u8]) -> XResult<()> {
    codes
        .iter()
        .rev()
        .try_for_each(|code| backend.key(*code, false))
}

/// The keycodes for `modifiers`, pressed now; released by [`release_all`] in reverse.
fn hold_modifiers<B: XBackend>(backend: &mut B, modifiers: &Modifiers) -> XResult<Vec<u8>> {
    if modifiers.keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut keyboard = Keyboard::new(backend)?;
    let codes = keyboard.combo(backend, &modifiers.keys)?;
    press_all(backend, &codes)?;
    Ok(codes)
}

fn type_text<B: XBackend>(backend: &mut B, text: &str) -> XResult<()> {
    let mut keyboard = Keyboard::new(backend)?;
    let result = (|| {
        for c in text.chars().filter(|c| *c != '\r') {
            let keysym = keys::char_keysym(c).expect("checked at input");
            let (code, shift) = keyboard.resolve(backend, keysym)?;
            let shift_code = if shift {
                Some(keyboard.shift(backend)?)
            } else {
                None
            };
            if let Some(shift_code) = shift_code {
                backend.key(shift_code, true)?;
            }
            backend.key(code, true)?;
            backend.key(code, false)?;
            if let Some(shift_code) = shift_code {
                backend.key(shift_code, false)?;
            }
            backend.sleep(TYPE_GAP);
        }
        Ok(())
    })();
    keyboard.restore(backend)?;
    result
}

fn press_combos<B: XBackend>(backend: &mut B, combos: &[Combo], hold: Option<f64>) -> XResult<()> {
    let mut keyboard = Keyboard::new(backend)?;
    let result = (|| {
        if let Some(seconds) = hold {
            let mut codes = Vec::new();
            for combo in combos {
                for code in keyboard.combo(backend, combo)? {
                    if !codes.contains(&code) {
                        codes.push(code);
                    }
                }
            }
            press_all(backend, &codes)?;
            backend.sleep(Duration::from_secs_f64(seconds));
            return release_all(backend, &codes);
        }
        for (i, combo) in combos.iter().enumerate() {
            if i > 0 {
                backend.sleep(TYPE_GAP);
            }
            let codes = keyboard.combo(backend, combo)?;
            press_all(backend, &codes)?;
            release_all(backend, &codes)?;
        }
        Ok(())
    })();
    keyboard.restore(backend)?;
    result
}

fn lerp(from: u16, to: u16, step: u32, steps: u32) -> u16 {
    let (from, to) = (i64::from(from), i64::from(to));
    (from + (to - from) * i64::from(step) / i64::from(steps)) as u16
}

/// Performs `command` and returns its action line.
pub fn perform<B: XBackend>(backend: &mut B, command: &Command) -> XResult<String> {
    let line = match command {
        Command::Screenshot => "screenshot".to_string(),
        Command::Click {
            click,
            at: point,
            modifiers,
        } => {
            if let Some(p) = point {
                backend.motion(p.x, p.y)?;
            }
            let held = hold_modifiers(backend, modifiers)?;
            for i in 0..click.count() {
                if i > 0 {
                    backend.sleep(CLICK_GAP);
                }
                backend.button(click.button(), true)?;
                backend.button(click.button(), false)?;
            }
            release_all(backend, &held)?;
            let name = match click {
                crate::input::Click::Left => "left_click",
                crate::input::Click::Right => "right_click",
                crate::input::Click::Middle => "middle_click",
                crate::input::Click::Double => "double_click",
                crate::input::Click::Triple => "triple_click",
            };
            with_modifiers(format!("{name} {}", at(*point)), modifiers)
        }
        Command::Drag {
            from,
            to,
            modifiers,
        } => {
            backend.motion(from.x, from.y)?;
            let held = hold_modifiers(backend, modifiers)?;
            backend.button(1, true)?;
            for step in 1..=DRAG_STEPS {
                backend.sleep(DRAG_STEP_GAP);
                backend.motion(
                    lerp(from.x, to.x, step, DRAG_STEPS),
                    lerp(from.y, to.y, step, DRAG_STEPS),
                )?;
            }
            backend.button(1, false)?;
            release_all(backend, &held)?;
            with_modifiers(format!("left_click_drag from {from} to {to}"), modifiers)
        }
        Command::MouseDown { at: point } => {
            if let Some(p) = point {
                backend.motion(p.x, p.y)?;
            }
            backend.button(1, true)?;
            format!("left_mouse_down {}", at(*point))
        }
        Command::MouseUp { at: point } => {
            if let Some(p) = point {
                backend.motion(p.x, p.y)?;
            }
            backend.button(1, false)?;
            format!("left_mouse_up {}", at(*point))
        }
        Command::MouseMove { to } => {
            backend.motion(to.x, to.y)?;
            format!("mouse_move to {to}")
        }
        Command::Scroll {
            at: point,
            direction,
            amount,
            modifiers,
        } => {
            if let Some(p) = point {
                backend.motion(p.x, p.y)?;
            }
            let held = hold_modifiers(backend, modifiers)?;
            for _ in 0..*amount {
                backend.button(direction.button(), true)?;
                backend.button(direction.button(), false)?;
            }
            release_all(backend, &held)?;
            with_modifiers(
                format!("scroll {} by {amount} {}", direction.name(), at(*point)),
                modifiers,
            )
        }
        Command::Type { text } => {
            type_text(backend, text)?;
            let n = text.chars().count();
            format!("typed {n} character{}", if n == 1 { "" } else { "s" })
        }
        Command::Key { combos, text } => {
            press_combos(backend, combos, None)?;
            format!("key {text}")
        }
        Command::HoldKey {
            combos,
            text,
            seconds,
        } => {
            press_combos(backend, combos, Some(*seconds))?;
            format!("hold_key {text} for {seconds} s")
        }
        Command::Wait { seconds } => {
            backend.sleep(Duration::from_secs_f64(*seconds));
            format!("waited {seconds} s")
        }
        Command::Zoom { from, to } => {
            let (w, h) = (to.x - from.x, to.y - from.y);
            format!("zoom of {from} to {to} ({w}x{h} at 1:1)")
        }
    };
    Ok(line)
}

/// The screenshot a command returns. After an input action the root is sampled every 100 ms from
/// 150 ms on until two consecutive samples match, or 2 s; `screenshot` and `zoom` take one sample.
pub fn capture<B: XBackend>(backend: &mut B, command: &Command) -> XResult<Frame> {
    if let Command::Zoom { from, to } = command {
        return backend.get_image(from.x, from.y, to.x - from.x, to.y - from.y);
    }
    let (w, h) = (DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16);
    if !command.settles() {
        return backend.get_image(0, 0, w, h);
    }
    backend.sleep(SETTLE_FIRST);
    let mut waited = SETTLE_FIRST;
    let mut previous = backend.get_image(0, 0, w, h)?;
    while waited + SETTLE_STEP <= SETTLE_CAP {
        backend.sleep(SETTLE_STEP);
        waited += SETTLE_STEP;
        let next = backend.get_image(0, 0, w, h)?;
        if next == previous {
            return Ok(next);
        }
        previous = next;
    }
    Ok(previous)
}

#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Motion(u16, u16),
        Button(u8, bool),
        Key(u8, bool),
        Map(u8, Vec<u32>),
        Sleep(Duration),
        Image(u16, u16, u16, u16),
    }

    /// Records every call. Keycodes follow a US layout.
    pub struct FakeX {
        pub events: Vec<Event>,
        pub mapping: KeyboardMapping,
        pub pointer: (i32, i32),
        /// Frames `get_image` returns in turn; the last repeats.
        pub frames: Vec<Frame>,
    }

    pub const SHIFT: u8 = 50;
    pub const CTRL: u8 = 37;
    pub const KC_A: u8 = 38;
    pub const KC_B: u8 = 56;
    pub const KC_C: u8 = 54;
    pub const KC_1: u8 = 10;
    pub const KC_SPACE: u8 = 65;
    pub const KC_RETURN: u8 = 36;
    pub const SPARE: u8 = 255;

    impl Default for FakeX {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeX {
        pub fn new() -> Self {
            let (min, max) = (8u8, 255u8);
            let mut keysyms = vec![0u32; (max - min + 1) as usize * 2];
            let mut set = |code: u8, syms: [u32; 2]| {
                let i = (code - min) as usize * 2;
                keysyms[i] = syms[0];
                keysyms[i + 1] = syms[1];
            };
            set(SHIFT, [keys::SHIFT_L, 0]);
            set(CTRL, [keys::CONTROL_L, 0]);
            set(KC_A, [0x61, 0x41]);
            set(KC_B, [0x62, 0x42]);
            set(KC_C, [0x63, 0x43]);
            set(KC_1, [0x31, 0x21]);
            set(KC_SPACE, [0x20, 0x20]);
            set(KC_RETURN, [keys::RETURN, 0]);
            // Every other keycode up to 254 carries something, so 255 is the one spare.
            for code in min..max {
                let i = (code - min) as usize * 2;
                if keysyms[i] == 0 {
                    keysyms[i] = 0x1008_0000 + u32::from(code);
                }
            }
            Self {
                events: Vec::new(),
                mapping: KeyboardMapping {
                    min_keycode: min,
                    per_keycode: 2,
                    keysyms,
                },
                pointer: (0, 0),
                frames: vec![Frame {
                    width: 1,
                    height: 1,
                    rgb: vec![0, 0, 0],
                }],
            }
        }

        /// The events with sleeps and image reads left out.
        pub fn input(&self) -> Vec<Event> {
            self.events
                .iter()
                .filter(|e| !matches!(e, Event::Sleep(_) | Event::Image(..)))
                .cloned()
                .collect()
        }
    }

    impl XBackend for FakeX {
        fn motion(&mut self, x: u16, y: u16) -> XResult<()> {
            self.pointer = (i32::from(x), i32::from(y));
            self.events.push(Event::Motion(x, y));
            Ok(())
        }
        fn button(&mut self, button: u8, press: bool) -> XResult<()> {
            self.events.push(Event::Button(button, press));
            Ok(())
        }
        fn key(&mut self, keycode: u8, press: bool) -> XResult<()> {
            self.events.push(Event::Key(keycode, press));
            Ok(())
        }
        fn keyboard_mapping(&mut self) -> XResult<KeyboardMapping> {
            Ok(self.mapping.clone())
        }
        fn change_mapping(&mut self, keycode: u8, keysyms: &[u32]) -> XResult<()> {
            self.events.push(Event::Map(keycode, keysyms.to_vec()));
            Ok(())
        }
        fn query_pointer(&mut self) -> XResult<(i32, i32)> {
            Ok(self.pointer)
        }
        fn get_image(&mut self, x: u16, y: u16, w: u16, h: u16) -> XResult<Frame> {
            self.events.push(Event::Image(x, y, w, h));
            let frame = if self.frames.len() > 1 {
                self.frames.remove(0)
            } else {
                self.frames[0].clone()
            };
            Ok(frame)
        }
        fn sleep(&mut self, duration: Duration) {
            self.events.push(Event::Sleep(duration));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Event::*, *};
    use super::*;
    use crate::input::checked;
    use serde_json::json;

    fn run(input: serde_json::Value) -> (String, FakeX) {
        let (_, command) = checked(&input).unwrap();
        let mut x = FakeX::new();
        let line = perform(&mut x, &command).unwrap();
        (line, x)
    }

    #[test]
    fn typing_holds_shift_for_capitals_and_symbols_and_remaps_for_e_acute() {
        let (line, x) = run(json!({"action":"type","text":"ab C!é"}));
        assert_eq!(line, "typed 6 characters");
        assert_eq!(
            x.input(),
            vec![
                Key(KC_A, true),
                Key(KC_A, false),
                Key(KC_B, true),
                Key(KC_B, false),
                Key(KC_SPACE, true),
                Key(KC_SPACE, false),
                Key(SHIFT, true),
                Key(KC_C, true),
                Key(KC_C, false),
                Key(SHIFT, false),
                Key(SHIFT, true),
                Key(KC_1, true),
                Key(KC_1, false),
                Key(SHIFT, false),
                Map(SPARE, vec![0xe9, 0xe9]),
                Key(SPARE, true),
                Key(SPARE, false),
                Map(SPARE, vec![]),
            ]
        );
        let sleeps = x.events.iter().filter(|e| **e == Sleep(TYPE_GAP)).count();
        assert_eq!(sleeps, 6, "10 ms after each character");
    }

    #[test]
    fn a_double_click_is_two_pairs_80_ms_apart() {
        let (line, x) = run(json!({"action":"double_click","x":46,"y":60}));
        assert_eq!(line, "double_click at 46,60");
        assert_eq!(
            x.events,
            vec![
                Motion(46, 60),
                Button(1, true),
                Button(1, false),
                Sleep(CLICK_GAP),
                Button(1, true),
                Button(1, false),
            ]
        );
    }

    #[test]
    fn clicks_press_the_right_buttons() {
        let (line, x) = run(json!({"action":"triple_click"}));
        assert_eq!(line, "triple_click at the cursor");
        assert_eq!(x.input().len(), 6);
        let (_, x) = run(json!({"action":"right_click","x":1,"y":2}));
        assert_eq!(
            x.input(),
            vec![Motion(1, 2), Button(3, true), Button(3, false)]
        );
        let (_, x) = run(json!({"action":"middle_click"}));
        assert_eq!(x.input(), vec![Button(2, true), Button(2, false)]);
    }

    #[test]
    fn scrolling_down_twice_with_shift_presses_button_5_inside_shift() {
        let (line, x) = run(json!({
            "action":"scroll","scroll_direction":"down","scroll_amount":2,"modifiers":"shift"
        }));
        assert_eq!(line, "scroll down by 2 at the cursor with shift");
        assert_eq!(
            x.input(),
            vec![
                Key(SHIFT, true),
                Button(5, true),
                Button(5, false),
                Button(5, true),
                Button(5, false),
                Key(SHIFT, false),
            ]
        );
    }

    #[test]
    fn modifiers_are_released_in_reverse() {
        let (line, x) = run(json!({"action":"left_click","x":5,"y":6,"modifiers":"ctrl+shift"}));
        assert_eq!(line, "left_click at 5,6 with ctrl+shift");
        assert_eq!(
            x.input(),
            vec![
                Motion(5, 6),
                Key(CTRL, true),
                Key(SHIFT, true),
                Button(1, true),
                Button(1, false),
                Key(SHIFT, false),
                Key(CTRL, false),
            ]
        );
    }

    #[test]
    fn key_presses_each_combo_and_releases_it_in_reverse() {
        let (line, x) = run(json!({"action":"key","key":"ctrl+a Return"}));
        assert_eq!(line, "key ctrl+a Return");
        assert_eq!(
            x.input(),
            vec![
                Key(CTRL, true),
                Key(KC_A, true),
                Key(KC_A, false),
                Key(CTRL, false),
                Key(KC_RETURN, true),
                Key(KC_RETURN, false),
            ]
        );
    }

    #[test]
    fn hold_key_presses_sleeps_and_releases() {
        let (line, x) = run(json!({"action":"hold_key","key":"ctrl+shift","duration":0.2}));
        assert_eq!(line, "hold_key ctrl+shift for 0.2 s");
        assert_eq!(
            x.events,
            vec![
                Key(CTRL, true),
                Key(SHIFT, true),
                Sleep(Duration::from_millis(200)),
                Key(SHIFT, false),
                Key(CTRL, false),
            ]
        );
    }

    #[test]
    fn a_drag_moves_in_ten_steps_between_press_and_release() {
        let (line, x) = run(json!({"action":"left_click_drag","x":0,"y":0,"to_x":100,"to_y":50}));
        assert_eq!(line, "left_click_drag from 0,0 to 100,50");
        let input = x.input();
        assert_eq!(input[0], Motion(0, 0));
        assert_eq!(input[1], Button(1, true));
        assert_eq!(input[2], Motion(10, 5));
        assert_eq!(input[11], Motion(100, 50));
        assert_eq!(input[12], Button(1, false));
        assert_eq!(input.len(), 13);
    }

    #[test]
    fn mouse_down_and_up_may_come_apart() {
        let (line, x) = run(json!({"action":"left_mouse_down","x":3,"y":4}));
        assert_eq!(line, "left_mouse_down at 3,4");
        assert_eq!(x.input(), vec![Motion(3, 4), Button(1, true)]);
        let (line, x) = run(json!({"action":"left_mouse_up"}));
        assert_eq!(line, "left_mouse_up at the cursor");
        assert_eq!(x.input(), vec![Button(1, false)]);
    }

    #[test]
    fn wait_zoom_and_screenshot_lines() {
        assert_eq!(
            run(json!({"action":"wait","duration":0.1})).0,
            "waited 0.1 s"
        );
        assert_eq!(run(json!({"action":"screenshot"})).0, "screenshot");
        assert_eq!(
            run(json!({"action":"zoom","x":0,"y":0,"to_x":320,"to_y":200})).0,
            "zoom of 0,0 to 320,200 (320x200 at 1:1)"
        );
        assert_eq!(
            run(json!({"action":"mouse_move","x":700,"y":450})).0,
            "mouse_move to 700,450"
        );
    }

    #[test]
    fn capture_settles_until_two_samples_match() {
        let (_, command) = checked(&json!({"action":"mouse_move","x":1,"y":1})).unwrap();
        let mut x = FakeX::new();
        let frame = |v: u8| Frame {
            width: 1,
            height: 1,
            rgb: vec![v, v, v],
        };
        x.frames = vec![frame(1), frame(2), frame(3), frame(3)];
        let got = capture(&mut x, &command).unwrap();
        assert_eq!(got, frame(3));
        let images = x.events.iter().filter(|e| matches!(e, Image(..))).count();
        assert_eq!(images, 4);
        assert_eq!(x.events[0], Sleep(SETTLE_FIRST));
    }

    #[test]
    fn capture_stops_at_two_seconds_and_zoom_takes_one_region_sample() {
        let (_, command) = checked(&json!({"action":"wait","duration":0.1})).unwrap();
        let mut x = FakeX::new();
        x.frames = (0..100u8)
            .map(|v| Frame {
                width: 1,
                height: 1,
                rgb: vec![v, v, v],
            })
            .collect();
        capture(&mut x, &command).unwrap();
        let images = x.events.iter().filter(|e| matches!(e, Image(..))).count();
        assert_eq!(images, 1 + 18, "150 ms, then 100 ms steps up to 2 s");

        let (_, command) =
            checked(&json!({"action":"zoom","x":10,"y":20,"to_x":330,"to_y":220})).unwrap();
        let mut x = FakeX::new();
        capture(&mut x, &command).unwrap();
        assert_eq!(x.events, vec![Image(10, 20, 320, 200)]);
    }
}
