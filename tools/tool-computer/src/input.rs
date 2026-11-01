//! The tool's input: one flat object, `action` plus optional scalars, checked into a [`Command`].

use serde_json::{Map, Value};

use crate::keys::{self, Combo};
use crate::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

/// The 16 action kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Screenshot,
    LeftClick,
    RightClick,
    MiddleClick,
    DoubleClick,
    TripleClick,
    LeftClickDrag,
    LeftMouseDown,
    LeftMouseUp,
    MouseMove,
    Scroll,
    Type,
    Key,
    HoldKey,
    Wait,
    Zoom,
}

impl Action {
    pub const ALL: [Action; 16] = [
        Action::Screenshot,
        Action::LeftClick,
        Action::RightClick,
        Action::MiddleClick,
        Action::DoubleClick,
        Action::TripleClick,
        Action::LeftClickDrag,
        Action::LeftMouseDown,
        Action::LeftMouseUp,
        Action::MouseMove,
        Action::Scroll,
        Action::Type,
        Action::Key,
        Action::HoldKey,
        Action::Wait,
        Action::Zoom,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Action::Screenshot => "screenshot",
            Action::LeftClick => "left_click",
            Action::RightClick => "right_click",
            Action::MiddleClick => "middle_click",
            Action::DoubleClick => "double_click",
            Action::TripleClick => "triple_click",
            Action::LeftClickDrag => "left_click_drag",
            Action::LeftMouseDown => "left_mouse_down",
            Action::LeftMouseUp => "left_mouse_up",
            Action::MouseMove => "mouse_move",
            Action::Scroll => "scroll",
            Action::Type => "type",
            Action::Key => "key",
            Action::HoldKey => "hold_key",
            Action::Wait => "wait",
            Action::Zoom => "zoom",
        }
    }

    pub fn parse(name: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|action| action.name() == name)
    }

    /// The fields this action takes besides `action`, in table order.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            Action::Screenshot => &[],
            Action::LeftClick
            | Action::RightClick
            | Action::MiddleClick
            | Action::DoubleClick
            | Action::TripleClick => &["x", "y", "modifiers"],
            Action::LeftClickDrag => &["x", "y", "to_x", "to_y", "modifiers"],
            Action::LeftMouseDown | Action::LeftMouseUp => &["x", "y"],
            Action::MouseMove => &["x", "y"],
            Action::Scroll => &["x", "y", "modifiers", "scroll_direction", "scroll_amount"],
            Action::Type => &["text"],
            Action::Key => &["key"],
            Action::HoldKey => &["key", "duration"],
            Action::Wait => &["duration"],
            Action::Zoom => &["x", "y", "to_x", "to_y"],
        }
    }

    pub fn names() -> String {
        Action::ALL.map(Action::name).join(", ")
    }
}

/// Every field the input schema declares.
pub const FIELDS: [&str; 11] = [
    "action",
    "x",
    "y",
    "to_x",
    "to_y",
    "text",
    "key",
    "modifiers",
    "scroll_direction",
    "scroll_amount",
    "duration",
];

/// A point on the display, in display pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: u16,
    pub y: u16,
}

impl std::fmt::Display for Point {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{}", self.x, self.y)
    }
}

/// Which button a click presses and how often.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Left,
    Right,
    Middle,
    Double,
    Triple,
}

impl Click {
    pub fn button(self) -> u8 {
        match self {
            Click::Right => 3,
            Click::Middle => 2,
            _ => 1,
        }
    }

    pub fn count(self) -> u32 {
        match self {
            Click::Double => 2,
            Click::Triple => 3,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    /// The X button one unit of scrolling presses.
    pub fn button(self) -> u8 {
        match self {
            Direction::Up => 4,
            Direction::Down => 5,
            Direction::Left => 6,
            Direction::Right => 7,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Left => "left",
            Direction::Right => "right",
        }
    }
}

/// Modifier keysyms held around an action, with the text they were given as.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub keys: Vec<u32>,
    pub text: String,
}

/// A checked call.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Screenshot,
    Click {
        click: Click,
        at: Option<Point>,
        modifiers: Modifiers,
    },
    Drag {
        from: Point,
        to: Point,
        modifiers: Modifiers,
    },
    MouseDown {
        at: Option<Point>,
    },
    MouseUp {
        at: Option<Point>,
    },
    MouseMove {
        to: Point,
    },
    Scroll {
        at: Option<Point>,
        direction: Direction,
        amount: u32,
        modifiers: Modifiers,
    },
    Type {
        text: String,
    },
    Key {
        combos: Vec<Combo>,
        text: String,
    },
    HoldKey {
        combos: Vec<Combo>,
        text: String,
        seconds: f64,
    },
    Wait {
        seconds: f64,
    },
    Zoom {
        from: Point,
        to: Point,
    },
}

impl Command {
    /// Whether the action presses, moves or waits, so its screenshot waits for the screen to
    /// settle.
    pub fn settles(&self) -> bool {
        !matches!(self, Command::Screenshot | Command::Zoom { .. })
    }
}

/// Checks `data` (the envelope's `data` object) into its action kind and command, or the one
/// line naming what is wrong.
pub fn checked(data: &Value) -> Result<(Action, Command), String> {
    let map = match data {
        Value::Object(map) => map,
        _ => return Err(err("the input must be a JSON object with an action")),
    };
    let action = match map.get("action") {
        None | Some(Value::Null) => {
            return Err(err(&format!(
                "action is required; it is one of {}",
                Action::names()
            )))
        }
        Some(Value::String(name)) => Action::parse(name).ok_or_else(|| {
            err(&format!(
                "action '{name}' is not an action; the actions are {}",
                Action::names()
            ))
        })?,
        Some(_) => return Err(err("action must be a string")),
    };

    let takes = action.fields();
    for (field, value) in map {
        if field == "action" || value.is_null() {
            continue;
        }
        if !FIELDS.contains(&field.as_str()) {
            return Err(err(&format!(
                "'{field}' is not a field; the fields are {}",
                FIELDS.join(", ")
            )));
        }
        if !takes.contains(&field.as_str()) {
            let takes_line = match takes {
                [] => "it takes no field but action".to_string(),
                _ => format!("it takes {}", and_list(takes)),
            };
            return Err(err(&format!(
                "{} does not take {field}; {takes_line}",
                action.name()
            )));
        }
    }

    let fields = Fields { map };
    let command = match action {
        Action::Screenshot => Command::Screenshot,
        Action::LeftClick
        | Action::RightClick
        | Action::MiddleClick
        | Action::DoubleClick
        | Action::TripleClick => Command::Click {
            click: match action {
                Action::LeftClick => Click::Left,
                Action::RightClick => Click::Right,
                Action::MiddleClick => Click::Middle,
                Action::DoubleClick => Click::Double,
                _ => Click::Triple,
            },
            at: fields.point("x", "y", false)?,
            modifiers: fields.modifiers()?,
        },
        Action::LeftClickDrag => {
            let from = fields.point("x", "y", true)?.expect("required");
            let to = fields.point("to_x", "to_y", true)?.expect("required");
            Command::Drag {
                from,
                to,
                modifiers: fields.modifiers()?,
            }
        }
        Action::LeftMouseDown => Command::MouseDown {
            at: fields.point("x", "y", false)?,
        },
        Action::LeftMouseUp => Command::MouseUp {
            at: fields.point("x", "y", false)?,
        },
        Action::MouseMove => Command::MouseMove {
            to: fields.point("x", "y", true)?.expect("required"),
        },
        Action::Scroll => {
            let direction = match fields.string("scroll_direction")? {
                None => {
                    return Err(err(
                        "scroll needs scroll_direction: up, down, left or right",
                    ))
                }
                Some("up") => Direction::Up,
                Some("down") => Direction::Down,
                Some("left") => Direction::Left,
                Some("right") => Direction::Right,
                Some(other) => {
                    let line = format!(
                        "scroll_direction '{other}' is not a direction; it is up, down, left or \
                         right"
                    );
                    return Err(err(&line));
                }
            };
            let amount = match fields.integer("scroll_amount")? {
                None => 3,
                Some(n @ 1..=50) => n as u32,
                Some(n) => {
                    return Err(err(&format!("scroll_amount is {n}; scroll_amount is 1–50")))
                }
            };
            Command::Scroll {
                at: fields.point("x", "y", false)?,
                direction,
                amount,
                modifiers: fields.modifiers()?,
            }
        }
        Action::Type => {
            let text = fields
                .string("text")?
                .ok_or_else(|| err("type needs text, 1 to 5000 characters"))?;
            let count = text.chars().count();
            if count == 0 || count > 5000 {
                return Err(err(&format!(
                    "text holds {count} characters; text is 1 to 5000 characters"
                )));
            }
            if let Some(c) = text
                .chars()
                .find(|c| *c != '\r' && keys::char_keysym(*c).is_none())
            {
                return Err(err(&format!(
                    "text holds the control character U+{:04X}, which cannot be typed; use key for \
                     special keys",
                    c as u32
                )));
            }
            Command::Type {
                text: text.to_string(),
            }
        }
        Action::Key | Action::HoldKey => {
            let text = fields
                .string("key")?
                .ok_or_else(|| err(&format!("{} needs key", action.name())))?;
            let combos = keys::parse_combos(text).map_err(|e| err(&e))?;
            if action == Action::Key {
                Command::Key {
                    combos,
                    text: text.to_string(),
                }
            } else {
                let seconds = fields
                    .number("duration")?
                    .ok_or_else(|| err("hold_key needs duration, more than 0 and at most 30 s"))?;
                check_duration(seconds, 30.0)?;
                Command::HoldKey {
                    combos,
                    text: text.to_string(),
                    seconds,
                }
            }
        }
        Action::Wait => {
            let seconds = fields.number("duration")?.unwrap_or(1.0);
            check_duration(seconds, 60.0)?;
            Command::Wait { seconds }
        }
        Action::Zoom => {
            let from = fields.point("x", "y", true)?.expect("required");
            let to = fields.point("to_x", "to_y", true)?.expect("required");
            if to.x <= from.x || to.y <= from.y {
                return Err(err(&format!(
                    "zoom needs to_x greater than x and to_y greater than y (to_x,to_y is the \
                     exclusive bottom-right corner); got {from} to {to}"
                )));
            }
            Command::Zoom { from, to }
        }
    };
    Ok((action, command))
}

fn check_duration(seconds: f64, max: f64) -> Result<(), String> {
    if !(seconds > 0.0 && seconds <= max) {
        return Err(err(&format!(
            "duration is {seconds}; duration is more than 0 and at most {max} s"
        )));
    }
    Ok(())
}

fn err(line: &str) -> String {
    format!("tool-computer: {line}")
}

/// `a`, `a and b`, `a, b and c`.
pub fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

struct Fields<'a> {
    map: &'a Map<String, Value>,
}

impl Fields<'_> {
    fn get(&self, name: &str) -> Option<&Value> {
        self.map.get(name).filter(|value| !value.is_null())
    }

    fn integer(&self, name: &str) -> Result<Option<i64>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(value) => {
                if let Some(n) = value.as_i64() {
                    return Ok(Some(n));
                }
                match value.as_f64() {
                    Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => Ok(Some(f as i64)),
                    _ => Err(err(&format!("{name} must be an integer, got {value}"))),
                }
            }
        }
    }

    fn number(&self, name: &str) -> Result<Option<f64>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_f64()
                .map(Some)
                .ok_or_else(|| err(&format!("{name} must be a number of seconds, got {value}"))),
        }
    }

    fn string(&self, name: &str) -> Result<Option<&str>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(value) => Err(err(&format!("{name} must be a string, got {value}"))),
        }
    }

    fn coordinate(&self, name: &str, limit: u32) -> Result<Option<u16>, String> {
        match self.integer(name)? {
            None => Ok(None),
            Some(n) if n >= 0 && n < i64::from(limit) => Ok(Some(n as u16)),
            Some(n) => Err(err(&format!(
                "{name} is {n}, outside the display; {name} is 0–{}",
                limit - 1
            ))),
        }
    }

    /// The pair `x_name`,`y_name`: both or neither, and both when `required`.
    fn point(&self, x_name: &str, y_name: &str, required: bool) -> Result<Option<Point>, String> {
        let x = self.coordinate(x_name, DISPLAY_WIDTH)?;
        let y = self.coordinate(y_name, DISPLAY_HEIGHT)?;
        match (x, y) {
            (Some(x), Some(y)) => Ok(Some(Point { x, y })),
            (None, None) if !required => Ok(None),
            (None, None) => {
                let action = self.map["action"].as_str().unwrap_or_default();
                Err(err(&format!("{action} needs {x_name} and {y_name}")))
            }
            (Some(_), None) => Err(err(&format!(
                "{x_name} needs {y_name}; give both or neither"
            ))),
            (None, Some(_)) => Err(err(&format!(
                "{y_name} needs {x_name}; give both or neither"
            ))),
        }
    }

    fn modifiers(&self) -> Result<Modifiers, String> {
        match self.string("modifiers")? {
            None => Ok(Modifiers::default()),
            Some(text) => Ok(Modifiers {
                keys: keys::parse_modifiers(text).map_err(|e| err(&e))?,
                text: text.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refused(input: Value) -> String {
        checked(&input).expect_err("refused")
    }

    #[test]
    fn every_action_parses_with_its_fields() {
        let cases = [
            json!({"action":"screenshot"}),
            json!({"action":"left_click"}),
            json!({"action":"left_click","x":1,"y":2,"modifiers":"shift"}),
            json!({"action":"right_click","x":1,"y":2}),
            json!({"action":"middle_click"}),
            json!({"action":"double_click","x":1,"y":2}),
            json!({"action":"triple_click","x":1,"y":2}),
            json!({"action":"left_click_drag","x":1,"y":2,"to_x":30,"to_y":40}),
            json!({"action":"left_mouse_down"}),
            json!({"action":"left_mouse_up","x":5,"y":5}),
            json!({"action":"mouse_move","x":1439,"y":899}),
            json!({"action":"scroll","scroll_direction":"down","scroll_amount":2,"modifiers":"shift"}),
            json!({"action":"type","text":"ab C!é"}),
            json!({"action":"key","key":"ctrl+a Return"}),
            json!({"action":"hold_key","key":"shift","duration":0.2}),
            json!({"action":"wait"}),
            json!({"action":"zoom","x":0,"y":0,"to_x":320,"to_y":200}),
        ];
        for case in cases {
            checked(&case).unwrap_or_else(|e| panic!("{case}: {e}"));
        }
    }

    #[test]
    fn defaults_fill_scroll_amount_and_wait_duration() {
        let (_, scroll) = checked(&json!({"action":"scroll","scroll_direction":"up"})).unwrap();
        assert!(matches!(
            scroll,
            Command::Scroll {
                amount: 3,
                at: None,
                ..
            }
        ));
        let (_, wait) = checked(&json!({"action":"wait"})).unwrap();
        assert_eq!(wait, Command::Wait { seconds: 1.0 });
    }

    #[test]
    fn a_coordinate_out_of_range_names_the_field_and_bound() {
        let line = refused(json!({"action":"left_click","x":1440,"y":10}));
        assert!(
            line.contains("x is 1440") && line.contains("0–1439"),
            "{line}"
        );
        let line = refused(json!({"action":"mouse_move","x":0,"y":900}));
        assert!(
            line.contains("y is 900") && line.contains("0–899"),
            "{line}"
        );
        let line = refused(json!({"action":"mouse_move","x":-1,"y":0}));
        assert!(line.contains("0–1439"), "{line}");
    }

    #[test]
    fn half_a_pair_and_a_missing_pair_are_refused() {
        assert!(refused(json!({"action":"left_click","x":5})).contains("x needs y"));
        assert!(refused(json!({"action":"mouse_move"})).contains("mouse_move needs x and y"));
        assert!(refused(json!({"action":"left_click_drag","x":1,"y":1}))
            .contains("needs to_x and to_y"));
    }

    #[test]
    fn unknown_action_and_unknown_field_are_refused() {
        assert!(refused(json!({"action":"fly"})).contains("'fly' is not an action"));
        assert!(refused(json!({"action":"screenshot","size":1})).contains("'size' is not a field"));
        assert!(refused(json!({"x":1})).contains("action is required"));
        assert!(refused(json!({"action":3})).contains("action must be a string"));
    }

    #[test]
    fn a_field_the_action_does_not_take_lists_what_it_takes() {
        let line = refused(json!({"action":"left_click","x":1,"y":1,"text":"shift"}));
        assert_eq!(
            line,
            "tool-computer: left_click does not take text; it takes x, y and modifiers"
        );
        let line = refused(json!({"action":"screenshot","x":1}));
        assert!(line.contains("takes no field but action"), "{line}");
    }

    #[test]
    fn wrong_types_are_refused() {
        assert!(
            refused(json!({"action":"mouse_move","x":"1","y":1})).contains("x must be an integer")
        );
        assert!(
            refused(json!({"action":"mouse_move","x":1.5,"y":1})).contains("x must be an integer")
        );
        assert!(refused(json!({"action":"type","text":5})).contains("text must be a string"));
        assert!(
            refused(json!({"action":"wait","duration":"1"})).contains("duration must be a number")
        );
    }

    #[test]
    fn an_integral_float_and_a_null_field_are_accepted() {
        let (_, cmd) = checked(&json!({"action":"mouse_move","x":3.0,"y":4,"text":null})).unwrap();
        assert_eq!(
            cmd,
            Command::MouseMove {
                to: Point { x: 3, y: 4 }
            }
        );
    }

    #[test]
    fn keys_durations_zoom_and_scroll_bounds_are_checked() {
        assert!(refused(json!({"action":"key","key":"ctrl+bogus"})).contains("bogus"));
        assert!(refused(json!({"action":"key"})).contains("key needs key"));
        let line = refused(json!({"action":"zoom","x":10,"y":10,"to_x":5,"to_y":20}));
        assert!(line.contains("to_x greater than x"), "{line}");
        assert!(refused(json!({"action":"wait","duration":61})).contains("at most 60"));
        assert!(refused(json!({"action":"wait","duration":0})).contains("more than 0"));
        assert!(
            refused(json!({"action":"hold_key","key":"a","duration":31})).contains("at most 30")
        );
        assert!(refused(json!({"action":"hold_key","key":"a"})).contains("needs duration"));
        assert!(
            refused(json!({"action":"scroll","scroll_direction":"sideways"}))
                .contains("'sideways'")
        );
        assert!(refused(json!({"action":"scroll"})).contains("scroll_direction"));
        assert!(
            refused(json!({"action":"scroll","scroll_direction":"up","scroll_amount":51}))
                .contains("1–50")
        );
        assert!(refused(json!({"action":"type","text":""})).contains("1 to 5000"));
        let long = "a".repeat(5001);
        assert!(refused(json!({"action":"type","text":long})).contains("5001"));
        assert!(refused(json!({"action":"type","text":"a\u{7}"})).contains("U+0007"));
        assert!(refused(json!({"action":"left_click","modifiers":"ctrl+q"})).contains("'q'"));
    }
}
