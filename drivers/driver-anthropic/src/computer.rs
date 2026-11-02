//! Computer use: a tool declaring `provider_tool: {kind: computer, …}` offered as Anthropic's own
//! computer tool.
//!
//! In the conversation a computer call is always a `tool_call` named after the tool's entry, with
//! the tool's own input in display pixels. Each request rebuilds the provider form from it — the
//! toolset's member `tool_use` blocks, or the older versions' `computer` blocks — in the pixel
//! space of the screenshots the image pre-pass sends, and each response maps Claude's blocks back.
//! The scale is a pure function of the declared display and the model, so it is recomputed on
//! every request and never stored.
//!
//! A request outside provider mode (see [`computer_plan`]) never reaches this module: its body is
//! the function-mode body, byte for byte.

use murmur_driver_computer::{computer_display, rewrite_result_text, Scale};
use serde_json::{json, Map, Value};

use super::{anthropic_image_size, image_tier, MurmurTool};

/// The computer tool type a model accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComputerVersion {
    /// `computer_toolset_20260801`: generally available, no beta header; one member `tool_use`
    /// per action, named after the action and carrying `"toolset_name":"computer"`.
    Toolset20260801,
    /// `computer_20251124` under `anthropic-beta: computer-use-2025-11-24`: one `computer` tool
    /// whose input names the action; has `zoom` behind `enable_zoom`.
    Computer20251124,
    /// `computer_20250124` under `anthropic-beta: computer-use-2025-01-24`: as 20251124 without
    /// `zoom`.
    Computer20250124,
}

impl ComputerVersion {
    /// The `type` of the `tools` entry.
    pub(crate) fn tool_type(self) -> &'static str {
        match self {
            Self::Toolset20260801 => "computer_toolset_20260801",
            Self::Computer20251124 => "computer_20251124",
            Self::Computer20250124 => "computer_20250124",
        }
    }

    /// The `anthropic-beta` value the version needs, if any.
    pub(crate) fn beta(self) -> Option<&'static str> {
        match self {
            Self::Toolset20260801 => None,
            Self::Computer20251124 => Some("computer-use-2025-11-24"),
            Self::Computer20250124 => Some("computer-use-2025-01-24"),
        }
    }

    /// The action names Claude calls under this version: the toolset's 17 members, or the older
    /// versions' `input.action` values.
    fn actions(self) -> &'static [&'static str] {
        match self {
            Self::Toolset20260801 | Self::Computer20251124 => &PROVIDER_ACTIONS,
            Self::Computer20250124 => &PROVIDER_ACTIONS[..PROVIDER_ACTIONS.len() - 1],
        }
    }
}

/// Every action Claude names on some version; `zoom` last, since 20250124 lacks it.
const PROVIDER_ACTIONS: [&str; 17] = [
    "screenshot",
    "cursor_position",
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "triple_click",
    "left_click_drag",
    "mouse_move",
    "left_mouse_down",
    "left_mouse_up",
    "scroll",
    "type",
    "key",
    "hold_key",
    "wait",
    "zoom",
];

/// The computer tool each model accepts on the Claude API, from the "Earlier tool versions" and
/// "Availability" sections of
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool>, read
/// 2026-11-01. It holds exactly `MODEL_IMAGE_TIERS`'s ids, in its order; a test keeps the two, and
/// README.md's "Computer use" table, in step. An id outside it has no provider tool.
pub(crate) const MODEL_COMPUTER_TOOLS: &[(&str, ComputerVersion)] = &[
    ("claude-fable-5-1", ComputerVersion::Toolset20260801),
    ("claude-opus-5-5", ComputerVersion::Toolset20260801),
    ("claude-sonnet-5-5", ComputerVersion::Toolset20260801),
    ("claude-haiku-5-5", ComputerVersion::Toolset20260801),
    ("claude-fable-5", ComputerVersion::Toolset20260801),
    ("claude-opus-5", ComputerVersion::Toolset20260801),
    ("claude-sonnet-5", ComputerVersion::Toolset20260801),
    ("claude-opus-4-8", ComputerVersion::Toolset20260801),
    ("claude-opus-4-7", ComputerVersion::Computer20251124),
    ("claude-opus-4-6", ComputerVersion::Computer20251124),
    ("claude-sonnet-4-6", ComputerVersion::Computer20251124),
    (
        "claude-opus-4-5-20251101",
        ComputerVersion::Computer20251124,
    ),
    ("claude-opus-4-5", ComputerVersion::Computer20251124),
    (
        "claude-sonnet-4-5-20250929",
        ComputerVersion::Computer20250124,
    ),
    ("claude-sonnet-4-5", ComputerVersion::Computer20250124),
    (
        "claude-haiku-4-5-20251001",
        ComputerVersion::Computer20250124,
    ),
    ("claude-haiku-4-5", ComputerVersion::Computer20250124),
];

/// The computer tool `model` accepts, or `None` for an id outside [`MODEL_COMPUTER_TOOLS`].
pub(crate) fn computer_version(model: &str) -> Option<ComputerVersion> {
    MODEL_COMPUTER_TOOLS
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, version)| *version)
}

/// How one request offers its computer tool: which `tools` entry it is, the version sent in its
/// place, and the scale between the display and the screenshots Claude sees.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComputerPlan {
    /// The index of the computer entry in the request's `tools`.
    pub(crate) entry_index: usize,
    /// The entry's `name`, which every computer call carries in the conversation.
    pub(crate) entry_name: String,
    pub(crate) version: ComputerVersion,
    pub(crate) scale: Scale,
}

/// The plan for a request on `model` offering `tools`, or `None` (function mode) unless all of:
///
/// 1. a tool's `provider_tool` is a computer block with a valid display; the first such tool is
///    the computer entry, and any later one stays an ordinary function;
/// 2. `model` is in [`MODEL_COMPUTER_TOOLS`];
/// 3. `model` reads images in `MODEL_IMAGE_TIERS`;
/// 4. no other tool is named `computer`, which the provider tool would collide with.
pub(crate) fn computer_plan(model: &str, tools: &[MurmurTool]) -> Option<ComputerPlan> {
    let (entry_index, display) = tools.iter().enumerate().find_map(|(index, tool)| {
        let display = computer_display(tool.provider_tool.as_ref()?)?;
        Some((index, display))
    })?;
    let version = computer_version(model)?;
    image_tier(model).limits()?;
    let collides = tools
        .iter()
        .enumerate()
        .any(|(index, tool)| index != entry_index && tool.name == "computer");
    if collides {
        return None;
    }
    let sent = anthropic_image_size(model, display.0, display.1)?;
    Some(ComputerPlan {
        entry_index,
        entry_name: tools[entry_index].name.clone(),
        version,
        scale: Scale::new(display, sent),
    })
}

/// The one `anthropic-beta` header value: the configured `beta_features` first, then the plan's
/// beta when the configured list does not already hold it. Without a plan beta the configured
/// value goes out unchanged.
pub(crate) fn beta_header(
    configured: Option<String>,
    plan: Option<&ComputerPlan>,
) -> Option<String> {
    let Some(beta) = plan.and_then(|plan| plan.version.beta()) else {
        return configured;
    };
    match configured {
        Some(list) if list.split(',').map(str::trim).any(|item| item == beta) => Some(list),
        Some(list) => Some(format!("{list},{beta}")),
        None => Some(beta.to_string()),
    }
}

/// A `[x, y]` pair of numbers.
fn number_pair(value: Option<&Value>) -> Option<(f64, f64)> {
    match value?.as_array()?.as_slice() {
        [x, y] => Some((x.as_f64()?, y.as_f64()?)),
        _ => None,
    }
}

/// Two integer fields of the tool's input.
fn int_pair(input: &Map<String, Value>, x: &str, y: &str) -> Option<(i64, i64)> {
    Some((input.get(x)?.as_i64()?, input.get(y)?.as_i64()?))
}

/// `fields` minus `consumed` and `action`, added to `out` under their own names where `out` does
/// not already hold the key.
fn pass_through(out: &mut Map<String, Value>, fields: &Map<String, Value>, consumed: &[&str]) {
    for (key, value) in fields {
        if key == "action" || consumed.contains(&key.as_str()) || out.contains_key(key) {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
}

impl ComputerPlan {
    /// The `tools` entry sent in the computer entry's place.
    pub(crate) fn tool_entry(&self) -> Value {
        let (sw, sh) = self.scale.sent();
        match self.version {
            ComputerVersion::Toolset20260801 => json!({"type": self.version.tool_type()}),
            ComputerVersion::Computer20251124 => json!({
                "type": self.version.tool_type(),
                "name": "computer",
                "display_width_px": sw,
                "display_height_px": sh,
                "enable_zoom": true,
            }),
            ComputerVersion::Computer20250124 => json!({
                "type": self.version.tool_type(),
                "name": "computer",
                "display_width_px": sw,
                "display_height_px": sh,
            }),
        }
    }

    /// Whether a `tool_use` named `name`, with `toolset_name` when it carries one, is a computer
    /// call: a toolset member, or the older versions' `computer` tool. No ordinary tool is named
    /// `computer` in provider mode.
    pub(crate) fn is_computer_call(&self, name: &str, toolset_name: Option<&str>) -> bool {
        toolset_name == Some("computer") || name == "computer"
    }

    /// The tool's input for a computer `tool_use` named `name` with `input`, coordinates in
    /// display pixels.
    ///
    /// A field the mapping does not consume or cannot read passes through under its own name, and
    /// an action the mapping does not know passes through as `{"action": <name>, …fields}`, so
    /// the tool's refusal names what it could not take. Nothing is clamped or made up.
    pub(crate) fn tool_input(
        &self,
        name: &str,
        toolset_name: Option<&str>,
        input: &Value,
    ) -> Value {
        let empty = Map::new();
        let fields = input.as_object().unwrap_or(&empty);
        let action = if toolset_name == Some("computer") {
            name
        } else {
            match fields.get("action").and_then(Value::as_str) {
                Some(action) => action,
                None => return input.clone(),
            }
        };
        self.map_action(action, fields)
    }

    fn map_action(&self, action: &str, fields: &Map<String, Value>) -> Value {
        let scale = &self.scale;
        let mut out = Map::new();
        let mut consumed: Vec<&str> = Vec::new();
        // A `[x, y]` field in screenshot pixels as two display-pixel fields.
        let point = |out: &mut Map<String, Value>, from: &str, x: &str, y: &str| -> bool {
            match number_pair(fields.get(from)) {
                Some((px, py)) => {
                    let (dx, dy) = scale.to_display(px, py);
                    out.insert(x.to_string(), Value::from(dx));
                    out.insert(y.to_string(), Value::from(dy));
                    true
                }
                None => false,
            }
        };
        let modifiers = |out: &mut Map<String, Value>| -> bool {
            match fields.get("text").filter(|text| text.is_string()) {
                Some(text) => {
                    out.insert("modifiers".to_string(), text.clone());
                    true
                }
                None => false,
            }
        };

        let tool_action = match action {
            "screenshot" | "cursor_position" => "screenshot",
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
                if point(&mut out, "coordinate", "x", "y") {
                    consumed.push("coordinate");
                }
                if modifiers(&mut out) {
                    consumed.push("text");
                }
                action
            }
            "left_click_drag" => {
                if point(&mut out, "start_coordinate", "x", "y") {
                    consumed.push("start_coordinate");
                }
                if point(&mut out, "coordinate", "to_x", "to_y") {
                    consumed.push("coordinate");
                }
                if modifiers(&mut out) {
                    consumed.push("text");
                }
                action
            }
            "mouse_move" | "left_mouse_down" | "left_mouse_up" => {
                if point(&mut out, "coordinate", "x", "y") {
                    consumed.push("coordinate");
                }
                action
            }
            "scroll" => {
                if point(&mut out, "coordinate", "x", "y") {
                    consumed.push("coordinate");
                }
                if modifiers(&mut out) {
                    consumed.push("text");
                }
                action
            }
            "type" => action,
            "key" => {
                if let Some(text) = fields.get("text").and_then(Value::as_str) {
                    let repeat = fields
                        .get("repeat")
                        .and_then(Value::as_u64)
                        .filter(|n| (1..=100).contains(n));
                    let key = match repeat {
                        Some(n) => {
                            consumed.push("repeat");
                            vec![text; n as usize].join(" ")
                        }
                        None => text.to_string(),
                    };
                    out.insert("key".to_string(), Value::String(key));
                    consumed.push("text");
                }
                action
            }
            "hold_key" => {
                if let Some(text) = fields.get("text").filter(|text| text.is_string()) {
                    out.insert("key".to_string(), text.clone());
                    consumed.push("text");
                }
                action
            }
            "wait" => action,
            "zoom" => {
                let region = fields
                    .get("region")
                    .and_then(Value::as_array)
                    .and_then(|r| match r.as_slice() {
                        [x0, y0, x1, y1] => {
                            Some((x0.as_f64()?, y0.as_f64()?, x1.as_f64()?, y1.as_f64()?))
                        }
                        _ => None,
                    });
                if let Some((x0, y0, x1, y1)) = region {
                    // The region's bottom-right corner is exclusive on both sides; it scales
                    // like any other point.
                    let (x, y) = scale.to_display(x0, y0);
                    let (to_x, to_y) = scale.to_display(x1, y1);
                    out.insert("x".to_string(), Value::from(x));
                    out.insert("y".to_string(), Value::from(y));
                    out.insert("to_x".to_string(), Value::from(to_x));
                    out.insert("to_y".to_string(), Value::from(to_y));
                    consumed.push("region");
                }
                action
            }
            other => other,
        };
        out.insert("action".to_string(), Value::String(tool_action.to_string()));
        pass_through(&mut out, fields, &consumed);
        Value::Object(out)
    }

    /// The provider-form `tool_use` for a computer call with `id` and the tool's `input`, in
    /// screenshot pixels, or `None` when this version cannot express it: `zoom` on 20250124,
    /// `left_mouse_down`/`left_mouse_up` at a point on the toolset, an action Claude never names
    /// under this version, or an input with no action.
    pub(crate) fn provider_call(&self, id: &str, input: &Value) -> Option<Value> {
        let fields = input.as_object()?;
        let action = fields.get("action")?.as_str()?;
        if !self.version.actions().contains(&action) {
            return None;
        }
        let toolset = self.version == ComputerVersion::Toolset20260801;
        let scale = &self.scale;
        let mut out = Map::new();
        let mut consumed: Vec<&str> = Vec::new();
        let sent_point = |x: i64, y: i64| {
            let (sx, sy) = scale.to_sent(x, y);
            json!([sx, sy])
        };
        // Two display-pixel fields as one `[x, y]` field in screenshot pixels.
        let coordinate = |out: &mut Map<String, Value>, x: &str, y: &str, to: &str| -> bool {
            match int_pair(fields, x, y) {
                Some((px, py)) => {
                    out.insert(to.to_string(), sent_point(px, py));
                    true
                }
                None => false,
            }
        };
        let modifiers = |out: &mut Map<String, Value>| -> bool {
            match fields.get("modifiers").filter(|m| m.is_string()) {
                Some(m) => {
                    out.insert("text".to_string(), m.clone());
                    true
                }
                None => false,
            }
        };

        match action {
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
            | "scroll" => {
                if coordinate(&mut out, "x", "y", "coordinate") {
                    consumed.extend(["x", "y"]);
                }
                if modifiers(&mut out) {
                    consumed.push("modifiers");
                }
            }
            "left_click_drag" => {
                if coordinate(&mut out, "x", "y", "start_coordinate") {
                    consumed.extend(["x", "y"]);
                }
                if coordinate(&mut out, "to_x", "to_y", "coordinate") {
                    consumed.extend(["to_x", "to_y"]);
                }
                if modifiers(&mut out) {
                    consumed.push("modifiers");
                }
            }
            // The toolset's left_mouse_down/up take no point.
            "left_mouse_down" | "left_mouse_up"
                if toolset && (fields.contains_key("x") || fields.contains_key("y")) =>
            {
                return None;
            }
            "mouse_move" | "left_mouse_down" | "left_mouse_up" => {
                let placed = coordinate(&mut out, "x", "y", "coordinate");
                if placed {
                    consumed.extend(["x", "y"]);
                }
            }
            "key" => {
                if let Some(key) = fields.get("key").and_then(Value::as_str) {
                    let combos: Vec<&str> = key.split(' ').collect();
                    let repeated = toolset
                        && (2..=100).contains(&combos.len())
                        && !combos[0].is_empty()
                        && combos.iter().all(|combo| *combo == combos[0]);
                    if repeated {
                        out.insert("text".to_string(), Value::String(combos[0].to_string()));
                        out.insert("repeat".to_string(), Value::from(combos.len()));
                    } else {
                        out.insert("text".to_string(), Value::String(key.to_string()));
                    }
                    consumed.push("key");
                }
            }
            "hold_key" => {
                if let Some(key) = fields.get("key").filter(|key| key.is_string()) {
                    out.insert("text".to_string(), key.clone());
                    consumed.push("key");
                }
            }
            "zoom" => {
                if let (Some((x0, y0)), Some((x1, y1))) =
                    (int_pair(fields, "x", "y"), int_pair(fields, "to_x", "to_y"))
                {
                    let (sx0, sy0) = scale.to_sent(x0, y0);
                    let (sx1, sy1) = scale.to_sent(x1, y1);
                    out.insert("region".to_string(), json!([sx0, sy0, sx1, sy1]));
                    consumed.extend(["x", "y", "to_x", "to_y"]);
                }
            }
            // `screenshot`, `type`, `wait` and the rest carry their fields under the same names.
            _ => {}
        }
        pass_through(&mut out, fields, &consumed);

        Some(if toolset {
            json!({
                "type": "tool_use",
                "id": id,
                "name": action,
                "toolset_name": "computer",
                "input": out,
            })
        } else {
            out.insert("action".to_string(), Value::String(action.to_string()));
            json!({
                "type": "tool_use",
                "id": id,
                "name": "computer",
                "input": out,
            })
        })
    }

    /// The `tool_result` answering a provider-form call: `parts` are the tool message's text and
    /// image blocks, in order. Text is rewritten into screenshot pixels and dropped when nothing
    /// is left of it; a result left with nothing at all says `OK`. On the toolset it echoes
    /// `"toolset_name":"computer"`.
    pub(crate) fn tool_result(&self, tool_use_id: &str, parts: Vec<Value>) -> Value {
        let mut content = Vec::with_capacity(parts.len());
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    let text = rewrite_result_text(text, &self.scale, true);
                    if !text.is_empty() {
                        content.push(json!({"type": "text", "text": text}));
                    }
                }
                _ => content.push(part),
            }
        }
        if content.is_empty() {
            content.push(json!({"type": "text", "text": "OK"}));
        }
        let mut result = json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
        });
        if self.version == ComputerVersion::Toolset20260801 {
            result["toolset_name"] = Value::String("computer".to_string());
        }
        result
    }
}
