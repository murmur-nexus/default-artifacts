//! Computer use: a tool declaring `provider_tool: {kind: computer, …}` offered as OpenAI's own
//! `computer` tool on the Responses surface.
//!
//! The tool takes one action per call, and OpenAI batches several actions into one
//! `computer_call`. So each `computer_call` becomes an ordered group of murmur tool calls sharing
//! its `call_id`, `<call_id>.cu1`, `<call_id>.cu2`, …, and the group's tool messages fold back into
//! the one `computer_call_output` OpenAI expects, holding the group's last screenshot. In the
//! conversation every call is the tool's own input in display pixels; each request rebuilds the
//! provider form from it in the pixel space of the screenshots the image pre-pass sends. The scale
//! is a pure function of the declared display and the model, so it is recomputed on every request
//! and never stored.
//!
//! A request outside provider mode (see [`computer_plan`]) never reaches this module: its body is
//! the function-mode body, byte for byte.

use murmur_driver_computer::{computer_display, rewrite_result_text, Scale};
use serde_json::{json, Map, Value};

use super::{model_route, openai_image_size, ApiSurface, MurmurTool};

/// How one request offers its computer tool: which `tools` entry it is, and the scale between
/// the display and the screenshots the model sees.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComputerPlan {
    /// The index of the computer entry in the request's `tools`.
    pub(crate) entry_index: usize,
    /// The entry's `name`, which every derived call carries in the conversation.
    pub(crate) entry_name: String,
    pub(crate) scale: Scale,
}

/// The plan for a request on `model` offering `tools`, or `None` (function mode) unless all of:
///
/// 1. a tool's `provider_tool` is a computer block with a valid display; the first such tool is
///    the computer entry, and any later one stays an ordinary function;
/// 2. `model`'s route goes to the Responses surface and has `computer` set;
/// 3. `model` reads images in its route.
pub(crate) fn computer_plan(model: &str, tools: &[MurmurTool]) -> Option<ComputerPlan> {
    let (entry_index, display) = tools.iter().enumerate().find_map(|(index, tool)| {
        let display = computer_display(tool.provider_tool.as_ref()?)?;
        Some((index, display))
    })?;
    let route = model_route(model)?;
    if route.surface != ApiSurface::Responses || !route.computer {
        return None;
    }
    route.images?;
    let sent = openai_image_size(model, display.0, display.1)?;
    Some(ComputerPlan {
        entry_index,
        entry_name: tools[entry_index].name.clone(),
        scale: Scale::new(display, sent),
    })
}

/// The `message` a safety check gets when OpenAI's is missing or empty.
pub(crate) const DEFAULT_CHECK_MESSAGE: &str =
    "OpenAI asks a person to confirm this computer action";

/// A 1×1 black PNG, sent as the screenshot of a `computer_call_output` whose group returned none.
pub(crate) const BLANK_SCREENSHOT_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR42mNgAAAAAgAB5Sfe/AAAAABJRU5ErkJggg==";

/// The image placement a group's one screenshot is reported under.
pub(crate) const COMPUTER_CALL_OUTPUT_PLACEMENT: &str = "computer_call_output";

/// The id of the `index`th (1-based) call derived from the `computer_call` with `call_id`.
pub(crate) fn derived_id(call_id: &str, index: usize) -> String {
    format!("{call_id}.cu{index}")
}

/// `(call_id, index)` of a derived id, or `None` for any other id.
pub(crate) fn split_derived_id(id: &str) -> Option<(&str, usize)> {
    let (call_id, index) = id.rsplit_once(".cu")?;
    if call_id.is_empty() || index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((call_id, index.parse().ok()?))
}

/// A key name OpenAI sends as the tool spells it: modifiers by their tool names, a single
/// character lower-cased, anything else as given (the tool matches names case-insensitively).
fn tool_key(key: &str) -> String {
    match key.to_ascii_uppercase().as_str() {
        "CTRL" | "CONTROL" => "ctrl".to_string(),
        "SHIFT" => "shift".to_string(),
        "ALT" | "OPTION" => "alt".to_string(),
        "META" | "CMD" | "COMMAND" | "SUPER" | "WIN" => "super".to_string(),
        _ if key.chars().count() == 1 => key.to_lowercase(),
        _ => key.to_string(),
    }
}

/// A tool key name as OpenAI spells it, for history.
fn openai_key(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "ctrl" => "CTRL".to_string(),
        "shift" => "SHIFT".to_string(),
        "alt" => "ALT".to_string(),
        "super" => "META".to_string(),
        _ if key.chars().count() == 1 => key.to_uppercase(),
        _ => key.to_string(),
    }
}

/// An action's `keys` as the tool's `+`-joined names, or `None` when it has none.
fn joined_keys(action: &Map<String, Value>) -> Option<Result<String, ()>> {
    let keys = action.get("keys")?;
    if keys.is_null() {
        return None;
    }
    let Some(keys) = keys.as_array() else {
        return Some(Err(()));
    };
    if keys.is_empty() {
        return None;
    }
    let mut names = Vec::with_capacity(keys.len());
    for key in keys {
        match key.as_str() {
            Some(key) => names.push(tool_key(key)),
            None => return Some(Err(())),
        }
    }
    Some(Ok(names.join("+")))
}

/// A `{x, y}` object or an `[x, y]` pair.
fn path_point(point: &Value) -> Option<(f64, f64)> {
    match point {
        Value::Object(p) => Some((p.get("x")?.as_f64()?, p.get("y")?.as_f64()?)),
        Value::Array(p) if p.len() == 2 => Some((p[0].as_f64()?, p[1].as_f64()?)),
        _ => None,
    }
}

impl ComputerPlan {
    /// The `tools` entry sent in the computer entry's place.
    pub(crate) fn tool_entry(&self) -> Value {
        json!({"type": "computer"})
    }

    /// The murmur `tool_call` blocks a `computer_call` item becomes, in order: one per tool
    /// action, ids `<call_id>.cu<i>` from 1, each carrying the item's `id` as `openai_item_id` and,
    /// when OpenAI sent pending safety checks, every one of them as `safety_checks`.
    pub(crate) fn tool_calls(&self, item: &Value) -> Vec<Value> {
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let item_id = item.get("id").and_then(Value::as_str);
        let checks = safety_checks(item.get("pending_safety_checks"));

        let actions: Vec<Value> = match (item.get("actions"), item.get("action")) {
            (Some(Value::Array(actions)), _) if !actions.is_empty() => actions.clone(),
            (None | Some(Value::Null), Some(action)) if !action.is_null() => vec![action.clone()],
            _ => vec![json!({"type": "screenshot"})],
        };
        let inputs = actions
            .iter()
            .flat_map(|action| self.tool_inputs(action))
            .collect::<Vec<_>>();

        inputs
            .into_iter()
            .enumerate()
            .map(|(index, input)| {
                let mut call = Map::new();
                call.insert("type".into(), json!("tool_call"));
                call.insert("id".into(), json!(derived_id(call_id, index + 1)));
                call.insert("name".into(), json!(self.entry_name));
                call.insert("input".into(), input);
                if let Some(item_id) = item_id {
                    call.insert("openai_item_id".into(), json!(item_id));
                }
                if let Some(checks) = &checks {
                    call.insert("safety_checks".into(), checks.clone());
                }
                Value::Object(call)
            })
            .collect()
    }

    /// The tool inputs one OpenAI action becomes: one, or two for a scroll on both axes.
    /// Coordinates go to display pixels. An action the driver cannot map, or one whose fields it
    /// cannot read, reaches the tool as `{"action": <type>, …other fields}`, so the tool's refusal
    /// names it.
    pub(crate) fn tool_inputs(&self, action: &Value) -> Vec<Value> {
        let empty = Map::new();
        let fields = action.as_object().unwrap_or(&empty);
        let kind = fields
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.map_action(kind, fields)
            .unwrap_or_else(|| vec![pass_through(kind, fields)])
    }

    fn map_action(&self, kind: &str, fields: &Map<String, Value>) -> Option<Vec<Value>> {
        let point = |fields: &Map<String, Value>| -> Option<(i64, i64)> {
            let x = fields.get("x")?.as_f64()?;
            let y = fields.get("y")?.as_f64()?;
            Some(self.scale.to_display(x, y))
        };
        let with_keys = |mut input: Map<String, Value>| -> Option<Value> {
            if let Some(keys) = joined_keys(fields) {
                input.insert("modifiers".into(), Value::String(keys.ok()?));
            }
            Some(Value::Object(input))
        };
        let at = |action: &str, (x, y): (i64, i64)| -> Map<String, Value> {
            let mut input = Map::new();
            input.insert("action".into(), json!(action));
            input.insert("x".into(), json!(x));
            input.insert("y".into(), json!(y));
            input
        };

        let inputs = match kind {
            "click" => {
                let action = match fields
                    .get("button")
                    .and_then(Value::as_str)
                    .unwrap_or("left")
                {
                    "left" => "left_click",
                    "right" => "right_click",
                    "wheel" => "middle_click",
                    _ => return None,
                };
                vec![with_keys(at(action, point(fields)?))?]
            }
            "double_click" => vec![with_keys(at("double_click", point(fields)?))?],
            "drag" => {
                let path = fields.get("path")?.as_array()?;
                if path.len() < 2 {
                    return None;
                }
                let (fx, fy) = path_point(path.first()?)?;
                let (tx, ty) = path_point(path.last()?)?;
                let (fx, fy) = self.scale.to_display(fx, fy);
                let (tx, ty) = self.scale.to_display(tx, ty);
                let mut input = at("left_click_drag", (fx, fy));
                input.insert("to_x".into(), json!(tx));
                input.insert("to_y".into(), json!(ty));
                vec![with_keys(input)?]
            }
            // `keys` has no counterpart on a pointer move.
            "move" => vec![Value::Object(at("mouse_move", point(fields)?))],
            "scroll" => {
                let at_point = point(fields)?;
                let delta = |key: &str| -> Option<f64> {
                    match fields.get(key) {
                        None | Some(Value::Null) => Some(0.0),
                        Some(value) => value.as_f64(),
                    }
                };
                let (dx, dy) = (delta("scroll_x")?, delta("scroll_y")?);
                let mut inputs = Vec::new();
                for (amount, positive, negative) in [(dy, "down", "up"), (dx, "right", "left")] {
                    if amount == 0.0 {
                        continue;
                    }
                    let mut input = at("scroll", at_point);
                    let direction = if amount > 0.0 { positive } else { negative };
                    let steps = (amount.abs() / 100.0).round().clamp(1.0, 50.0) as i64;
                    input.insert("scroll_direction".into(), json!(direction));
                    input.insert("scroll_amount".into(), json!(steps));
                    inputs.push(with_keys(input)?);
                }
                if inputs.is_empty() {
                    inputs.push(Value::Object(at("mouse_move", at_point)));
                }
                inputs
            }
            "keypress" => {
                let keys = match joined_keys(fields) {
                    Some(Ok(keys)) => keys,
                    _ => return None,
                };
                vec![json!({"action": "key", "key": keys})]
            }
            "type" => {
                let text = fields.get("text")?.as_str()?;
                vec![json!({"action": "type", "text": text})]
            }
            "wait" => vec![json!({"action": "wait"})],
            "screenshot" => vec![json!({"action": "screenshot"})],
            _ => return None,
        };
        Some(inputs)
    }

    /// The `computer_call` input item a group of derived calls was, rebuilt from the calls'
    /// tool inputs in screenshot pixels, without a `screenshot` action beside other actions.
    /// `calls` are `(input, openai_item_id)` of each call in order; the item's `id` comes from the
    /// first. `pending_safety_checks` is always `[]`: the `computer` tool refuses any check in its
    /// input (HTTP 400, "`pending_safety_checks` is not supported for the \"computer\" tool.").
    pub(crate) fn computer_call_item(
        &self,
        call_id: &str,
        calls: &[(&Value, Option<&str>)],
    ) -> Value {
        let mut actions = calls
            .iter()
            .map(|(input, _)| self.openai_action(input))
            .collect::<Vec<_>>();
        // OpenAI refuses a `screenshot` beside other actions in one `computer_call` (HTTP 400, "A
        // screenshot must be the only action in a computer call's `actions` list."), and never
        // sends one; the group's output carries the screenshot anyway.
        if actions.iter().any(|action| action["type"] != "screenshot") {
            actions.retain(|action| action["type"] != "screenshot");
        } else {
            actions.truncate(1);
        }
        let item_id = calls.first().and_then(|(_, item_id)| *item_id);
        let mut item = Map::new();
        item.insert("type".into(), json!("computer_call"));
        if let Some(item_id) = item_id {
            item.insert("id".into(), json!(item_id));
        }
        item.insert("call_id".into(), json!(call_id));
        item.insert("actions".into(), Value::Array(actions));
        item.insert("pending_safety_checks".into(), json!([]));
        item.insert("status".into(), json!("completed"));
        Value::Object(item)
    }

    /// The OpenAI action a tool input was, coordinates in screenshot pixels. An action OpenAI has
    /// no counterpart for is rebuilt as `{"type": <action>, …fields}`.
    pub(crate) fn openai_action(&self, input: &Value) -> Value {
        let empty = Map::new();
        let fields = input.as_object().unwrap_or(&empty);
        let action = fields
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let point = |out: &mut Map<String, Value>, x: &str, y: &str| -> Option<(i64, i64)> {
            let (px, py) = (fields.get(x)?.as_i64()?, fields.get(y)?.as_i64()?);
            let (sx, sy) = self.scale.to_sent(px, py);
            out.insert("x".into(), json!(sx));
            out.insert("y".into(), json!(sy));
            Some((sx, sy))
        };
        let keys = |out: &mut Map<String, Value>| {
            if let Some(modifiers) = fields.get("modifiers").and_then(Value::as_str) {
                let keys: Vec<String> = modifiers.split('+').map(openai_key).collect();
                out.insert("keys".into(), json!(keys));
            }
        };

        let mut out = Map::new();
        match action {
            "left_click" | "right_click" | "middle_click" => {
                let button = match action {
                    "left_click" => "left",
                    "right_click" => "right",
                    _ => "wheel",
                };
                out.insert("type".into(), json!("click"));
                out.insert("button".into(), json!(button));
                point(&mut out, "x", "y");
                keys(&mut out);
            }
            "double_click" => {
                out.insert("type".into(), json!("double_click"));
                point(&mut out, "x", "y");
                keys(&mut out);
            }
            "left_click_drag" => {
                out.insert("type".into(), json!("drag"));
                let mut from = Map::new();
                let mut to = Map::new();
                point(&mut from, "x", "y");
                point(&mut to, "to_x", "to_y");
                out.insert(
                    "path".into(),
                    json!([Value::Object(from), Value::Object(to)]),
                );
                keys(&mut out);
            }
            "mouse_move" => {
                out.insert("type".into(), json!("move"));
                point(&mut out, "x", "y");
            }
            "scroll" => {
                out.insert("type".into(), json!("scroll"));
                point(&mut out, "x", "y");
                let amount = fields
                    .get("scroll_amount")
                    .and_then(Value::as_i64)
                    .unwrap_or(3)
                    * 100;
                let (sx, sy) = match fields.get("scroll_direction").and_then(Value::as_str) {
                    Some("up") => (0, -amount),
                    Some("left") => (-amount, 0),
                    Some("right") => (amount, 0),
                    _ => (0, amount),
                };
                out.insert("scroll_x".into(), json!(sx));
                out.insert("scroll_y".into(), json!(sy));
                keys(&mut out);
            }
            "key" => {
                out.insert("type".into(), json!("keypress"));
                let key = fields
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let keys: Vec<String> = key.split('+').map(openai_key).collect();
                out.insert("keys".into(), json!(keys));
            }
            "type" => {
                out.insert("type".into(), json!("type"));
                out.insert(
                    "text".into(),
                    fields.get("text").cloned().unwrap_or_else(|| json!("")),
                );
            }
            "wait" => {
                out.insert("type".into(), json!("wait"));
            }
            "screenshot" => {
                out.insert("type".into(), json!("screenshot"));
            }
            other => {
                out.insert("type".into(), json!(other));
                for (key, value) in fields {
                    if key != "action" {
                        out.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        Value::Object(out)
    }

    /// The text a member's result text leaves for the notes: no cursor, no action line, no
    /// screenshot path.
    pub(crate) fn notes_text(&self, text: &str) -> String {
        rewrite_result_text(text, &self.scale, false)
    }
}

/// `{"action": <type>, …fields but type}`: an action the driver cannot map, as the tool sees it.
fn pass_through(kind: &str, fields: &Map<String, Value>) -> Value {
    let mut input = Map::new();
    input.insert("action".into(), json!(kind));
    for (key, value) in fields {
        if key != "type" {
            input.insert(key.clone(), value.clone());
        }
    }
    Value::Object(input)
}

/// OpenAI's `pending_safety_checks` as murmur's `safety_checks`, or `None` when there are none:
/// `id` and `code` only when they are non-empty strings, `message` OpenAI's when it is a non-empty
/// string and [`DEFAULT_CHECK_MESSAGE`] otherwise.
pub(crate) fn safety_checks(pending: Option<&Value>) -> Option<Value> {
    let pending = pending?.as_array().filter(|checks| !checks.is_empty())?;
    let non_empty = |check: &Value, key: &str| {
        check
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Value::Array(
        pending
            .iter()
            .map(|check| {
                let mut out = Map::new();
                if let Some(id) = non_empty(check, "id") {
                    out.insert("id".into(), json!(id));
                }
                if let Some(code) = non_empty(check, "code") {
                    out.insert("code".into(), json!(code));
                }
                let message = non_empty(check, "message")
                    .unwrap_or_else(|| DEFAULT_CHECK_MESSAGE.to_string());
                out.insert("message".into(), json!(message));
                Value::Object(out)
            })
            .collect(),
    ))
}
