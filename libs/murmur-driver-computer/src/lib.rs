//! The provider-neutral half of offering a computer tool as a provider's own computer tool.
//!
//! A tool artifact declaring `provider_tool: {kind: computer, display_width, display_height}`
//! reaches a driver as a `tools` entry carrying that block. A driver that offers it as its
//! provider's computer tool sends screenshots at the size its image pre-pass sizes a
//! display-size image to, and the model answers in that screenshot's pixel space. This crate
//! holds what both drivers share:
//!
//! - [`computer_display`] reads the declared display size;
//! - [`Scale`] maps a point between the screenshot the model sees and the display the tool runs;
//! - [`TOOL_ACTIONS`] is the tool's action vocabulary;
//! - [`rewrite_result_text`] turns the tool's result text into what the model should read.
//!
//! The model tables and wire shapes stay in each driver.

use serde_json::Value;

/// `(display_width, display_height)` of a `provider_tool` block, when its `kind` is `computer`
/// and both sizes are integers from 1 to 65535; `None` for anything else.
pub fn computer_display(provider_tool: &Value) -> Option<(u32, u32)> {
    if provider_tool.get("kind").and_then(Value::as_str) != Some("computer") {
        return None;
    }
    let side = |key: &str| {
        provider_tool
            .get(key)
            .and_then(Value::as_u64)
            .filter(|n| (1..=65535).contains(n))
            .map(|n| n as u32)
    };
    Some((side("display_width")?, side("display_height")?))
}

/// The display a computer tool runs, and the size its screenshots are sent to the model at.
///
/// A point the model names is in the sent screenshot's pixel space; the tool takes display
/// pixels. Nothing here clamps a point on its way to the display: a point off the screenshot
/// stays off the display, so the tool refuses it rather than acting somewhere the model did not
/// mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    display: (u32, u32),
    sent: (u32, u32),
}

impl Scale {
    /// A display of `display` px sent as screenshots of `sent` px. A zero side is read as 1.
    pub fn new(display: (u32, u32), sent: (u32, u32)) -> Self {
        Self {
            display: (display.0.max(1), display.1.max(1)),
            sent: (sent.0.max(1), sent.1.max(1)),
        }
    }

    /// The display size, `(W, H)`.
    pub fn display(&self) -> (u32, u32) {
        self.display
    }

    /// The size a display-size screenshot is sent at, `(sw, sh)`.
    pub fn sent(&self) -> (u32, u32) {
        self.sent
    }

    /// Whether screenshots go out at the display's own size.
    pub fn is_identity(&self) -> bool {
        self.display == self.sent
    }

    /// A screenshot point as display pixels: `(round(x·W/sw), round(y·H/sh))`, never clamped.
    pub fn to_display(&self, x: f64, y: f64) -> (i64, i64) {
        (
            scale_round(x, self.display.0, self.sent.0),
            scale_round(y, self.display.1, self.sent.1),
        )
    }

    /// A zoom region's exclusive bottom-right corner as display pixels, rounded as
    /// [`Scale::to_display`] rounds and never clamped.
    pub fn to_display_edge(&self, x: f64, y: f64) -> (i64, i64) {
        self.to_display(x, y)
    }

    /// A display point as screenshot pixels: `(round(x·sw/W), round(y·sh/H))`, clamped onto the
    /// screenshot. Used only to replay history, where a point the tool already took is shown
    /// back to the model.
    pub fn to_sent(&self, x: i64, y: i64) -> (i64, i64) {
        let one = |v: i64, sent: u32, display: u32| {
            scale_round(v as f64, sent, display).clamp(0, i64::from(sent) - 1)
        };
        (
            one(x, self.sent.0, self.display.0),
            one(y, self.sent.1, self.display.1),
        )
    }
}

fn scale_round(v: f64, numerator: u32, denominator: u32) -> i64 {
    (v * f64::from(numerator) / f64::from(denominator)).round() as i64
}

/// The actions the computer tool (`tools/tool-computer`) takes, in its own order.
pub const TOOL_ACTIONS: [&str; 16] = [
    "screenshot",
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "triple_click",
    "left_click_drag",
    "left_mouse_down",
    "left_mouse_up",
    "mouse_move",
    "scroll",
    "type",
    "key",
    "hold_key",
    "wait",
    "zoom",
];

/// A computer tool's result text as the model should read it in its provider's form.
///
/// The tool writes, in order: optional preface lines (a restart notice, a display banner), the
/// action line (`left_click at 46,60`), `cursor: X,Y`, `screenshot: <path> (<w>x<h>)`, then zero
/// or more lines about launched programs. The action line and the cursor are in display pixels
/// and the screenshot line names a file the model never sees, so:
///
/// - every line starting `screenshot: ` is dropped;
/// - the line immediately before a line starting `cursor: ` is dropped;
/// - the `cursor: X,Y` line becomes `cursor: <x>,<y>` in screenshot pixels when `keep_cursor`,
///   and is dropped otherwise;
/// - every other line is kept, in order;
/// - murmur's fence around the text, an `<untrusted-content source=…>` line and its
///   `</untrusted-content>`, is kept around what is left, and dropped when nothing is left
///   inside it.
///
/// Text with no `cursor:` line, such as a refusal, is returned unchanged apart from its
/// `screenshot:` lines.
pub fn rewrite_result_text(text: &str, scale: &Scale, keep_cursor: bool) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let is_cursor = |line: &str| line.starts_with("cursor: ");
    let mut kept = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("screenshot: ") {
            continue;
        }
        if lines.get(index + 1).is_some_and(|next| is_cursor(next)) {
            continue;
        }
        if is_cursor(line) {
            if keep_cursor {
                kept.push(rescaled_cursor(line, scale));
            }
            continue;
        }
        kept.push((*line).to_string());
    }
    // A fence with nothing left inside it says nothing.
    while let Some(open) = kept
        .windows(2)
        .position(|pair| pair[0].starts_with(FENCE_OPEN) && pair[1] == FENCE_CLOSE)
    {
        kept.drain(open..open + 2);
    }
    kept.join("\n")
}

/// How murmur opens the fence it puts around every tool result's text: the line is
/// `<untrusted-content source=tool:<name>>`.
const FENCE_OPEN: &str = "<untrusted-content source=";

/// How murmur closes that fence.
const FENCE_CLOSE: &str = "</untrusted-content>";

/// `cursor: X,Y` in screenshot pixels, or the line as it is when it does not hold two integers.
fn rescaled_cursor(line: &str, scale: &Scale) -> String {
    let point = line["cursor: ".len()..]
        .split_once(',')
        .and_then(|(x, y)| Some((x.trim().parse::<i64>().ok()?, y.trim().parse::<i64>().ok()?)));
    match point {
        Some((x, y)) => {
            let (sx, sy) = scale.to_sent(x, y);
            format!("cursor: {sx},{sy}")
        }
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn computer_display_reads_only_a_well_formed_computer_block() {
        let block = json!({"kind": "computer", "display_width": 1440, "display_height": 900});
        assert_eq!(computer_display(&block), Some((1440, 900)));
        assert_eq!(
            computer_display(
                &json!({"kind": "computer", "display_width": 1, "display_height": 65535})
            ),
            Some((1, 65535))
        );
        for bad in [
            json!({"kind": "browser", "display_width": 1440, "display_height": 900}),
            json!({"display_width": 1440, "display_height": 900}),
            json!({"kind": "computer", "display_width": 0, "display_height": 900}),
            json!({"kind": "computer", "display_width": 1440, "display_height": 65536}),
            json!({"kind": "computer", "display_width": 1440.5, "display_height": 900}),
            json!({"kind": "computer", "display_width": "1440", "display_height": 900}),
            json!({"kind": "computer", "display_width": -1, "display_height": 900}),
            json!({"kind": "computer", "display_width": 1440}),
            json!(null),
            json!("computer"),
        ] {
            assert_eq!(computer_display(&bad), None, "{bad}");
        }
    }

    #[test]
    fn scale_accessors_and_identity() {
        let identity = Scale::new((1440, 900), (1440, 900));
        assert!(identity.is_identity());
        assert_eq!(identity.display(), (1440, 900));
        assert_eq!(identity.sent(), (1440, 900));
        let smaller = Scale::new((1440, 900), (1372, 858));
        assert!(!smaller.is_identity());
        assert_eq!(smaller.sent(), (1372, 858));
        assert_eq!(Scale::new((0, 0), (0, 0)).sent(), (1, 1));
    }

    #[test]
    fn to_display_rounds_and_never_clamps() {
        let scale = Scale::new((1440, 900), (1372, 858));
        assert_eq!(scale.to_display(694.0, 434.0), (728, 455));
        assert_eq!(scale.to_display(0.0, 0.0), (0, 0));
        // Off the screenshot stays off the display.
        assert_eq!(scale.to_display(2000.0, -10.0), (2099, -10));
        let identity = Scale::new((1440, 900), (1440, 900));
        assert_eq!(identity.to_display(694.0, 434.0), (694, 434));
        assert_eq!(identity.to_display(1500.0, 900.0), (1500, 900));
        assert_eq!(identity.to_display(10.4, 10.6), (10, 11));
        // Ties round away from zero, as f64::round does.
        let half = Scale::new((3, 3), (2, 2));
        assert_eq!(half.to_display(1.0, 3.0), (2, 5));
    }

    #[test]
    fn to_display_edge_rounds_like_to_display() {
        let scale = Scale::new((1440, 900), (1372, 858));
        assert_eq!(scale.to_display_edge(320.0, 200.0), (336, 210));
        assert_eq!(scale.to_display_edge(1372.0, 858.0), (1440, 900));
    }

    #[test]
    fn to_sent_rounds_and_clamps_onto_the_screenshot() {
        let scale = Scale::new((1440, 900), (1372, 858));
        assert_eq!(scale.to_sent(719, 450), (685, 429));
        assert_eq!(scale.to_sent(1440, 900), (1371, 857));
        assert_eq!(scale.to_sent(-5, 5000), (0, 857));
        let big = Scale::new((3000, 2000), (1920, 1280));
        assert_eq!(big.to_sent(1500, 1000), (960, 640));
        assert_eq!(big.to_display(960.0, 640.0), (1500, 1000));
    }

    #[test]
    fn to_sent_of_to_display_is_within_one_pixel() {
        for (display, sent) in [
            ((1440, 900), (1372, 858)),
            ((1440, 900), (1440, 900)),
            ((3000, 2000), (1920, 1280)),
            ((1280, 800), (1092, 683)),
            ((7, 5), (3, 2)),
        ] {
            let scale = Scale::new(display, sent);
            for sx in (0..sent.0).step_by(((sent.0 / 97).max(1)) as usize) {
                for sy in (0..sent.1).step_by(((sent.1 / 89).max(1)) as usize) {
                    let (dx, dy) = scale.to_display(f64::from(sx), f64::from(sy));
                    let (bx, by) = scale.to_sent(dx, dy);
                    assert!(
                        (bx - i64::from(sx)).abs() <= 1 && (by - i64::from(sy)).abs() <= 1,
                        "{display:?}/{sent:?}: ({sx},{sy}) -> ({dx},{dy}) -> ({bx},{by})"
                    );
                }
            }
        }
    }

    #[test]
    fn tool_actions_are_the_tools_sixteen_in_order() {
        assert_eq!(TOOL_ACTIONS.len(), 16);
        assert_eq!(TOOL_ACTIONS[0], "screenshot");
        assert_eq!(TOOL_ACTIONS[6], "left_click_drag");
        assert_eq!(TOOL_ACTIONS[15], "zoom");
        let mut sorted = TOOL_ACTIONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 16);
    }

    fn scale_down() -> Scale {
        Scale::new((1440, 900), (1372, 858))
    }

    #[test]
    fn rewrite_keeps_only_the_rescaled_cursor_of_a_plain_result() {
        let text = "left_click at 719,450\ncursor: 719,450\nscreenshot: .tool-computer/s/screen-1.png (1440x900)";
        assert_eq!(
            rewrite_result_text(text, &scale_down(), true),
            "cursor: 685,429"
        );
        assert_eq!(
            rewrite_result_text(text, &Scale::new((1440, 900), (1440, 900)), true),
            "cursor: 719,450"
        );
        assert_eq!(rewrite_result_text(text, &scale_down(), false), "");
    }

    #[test]
    fn rewrite_keeps_preface_and_launch_lines_in_order() {
        let text = "the previous display stopped; programs on it are gone\n\
                    display: Xvfb 127.0.0.1:3, 1440x900x24; window manager openbox; launched zenity; shell commands reach it with DISPLAY=127.0.0.1:3 XAUTHORITY=/w/x\n\
                    screenshot\n\
                    cursor: 0,0\n\
                    screenshot: .tool-computer/s/screen-1.png (1440x900)\n\
                    launched zenity (launch[0]) exited with status 0; output in .tool-computer/s/launch-0-zenity.log";
        let kept = rewrite_result_text(text, &scale_down(), true);
        assert_eq!(
            kept,
            "the previous display stopped; programs on it are gone\n\
             display: Xvfb 127.0.0.1:3, 1440x900x24; window manager openbox; launched zenity; shell commands reach it with DISPLAY=127.0.0.1:3 XAUTHORITY=/w/x\n\
             cursor: 0,0\n\
             launched zenity (launch[0]) exited with status 0; output in .tool-computer/s/launch-0-zenity.log"
        );
        let dropped = rewrite_result_text(text, &scale_down(), false);
        assert!(!dropped.contains("cursor:"));
        assert!(dropped.starts_with("the previous display stopped"));
        assert!(dropped.ends_with("launch-0-zenity.log"));
    }

    #[test]
    fn rewrite_drops_a_zoom_action_line() {
        let text = "zoom of 0,0 to 336,210 (336x210 at 1:1)\ncursor: 12,40\nscreenshot: .tool-computer/s/zoom-2.png (336x210)";
        assert_eq!(
            rewrite_result_text(text, &scale_down(), true),
            "cursor: 11,38"
        );
    }

    #[test]
    fn rewrite_leaves_a_refusal_alone() {
        let text = "tool-computer: key 'ctrl+bogus' names no key it knows: bogus";
        assert_eq!(rewrite_result_text(text, &scale_down(), true), text);
        assert_eq!(rewrite_result_text(text, &scale_down(), false), text);
        let held = "tool-computer: the screenshot line\nscreenshot: x.png (1x1)";
        assert_eq!(
            rewrite_result_text(held, &scale_down(), true),
            "tool-computer: the screenshot line"
        );
    }

    #[test]
    fn rewrite_keeps_murmurs_fence_around_what_is_left_and_drops_it_when_empty() {
        let fenced = |inner: &str| {
            format!("<untrusted-content source=tool:tool-computer>\n{inner}\n</untrusted-content>")
        };
        let plain = fenced("left_click at 719,450\ncursor: 719,450\nscreenshot: s.png (1440x900)");
        assert_eq!(
            rewrite_result_text(&plain, &scale_down(), true),
            fenced("cursor: 685,429")
        );
        assert_eq!(rewrite_result_text(&plain, &scale_down(), false), "");
        let refusal = fenced("tool-computer: key 'ctrl+bogus' names no key it knows: bogus");
        assert_eq!(rewrite_result_text(&refusal, &scale_down(), false), refusal);
        let banner = fenced("display: Xvfb 127.0.0.1:3, 1440x900x24\nscreenshot\ncursor: 0,0\nscreenshot: s.png (1440x900)");
        assert_eq!(
            rewrite_result_text(&banner, &scale_down(), false),
            fenced("display: Xvfb 127.0.0.1:3, 1440x900x24")
        );
    }

    #[test]
    fn rewrite_keeps_an_unreadable_cursor_line_as_it_is() {
        let text = "moved\ncursor: here";
        assert_eq!(
            rewrite_result_text(text, &scale_down(), true),
            "cursor: here"
        );
    }
}
