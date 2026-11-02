//! Computer use: the provider tool per model, the function-mode fallback, mapping Claude's
//! computer calls onto the tool, and rebuilding history in the provider's form.

use super::computer::{beta_header, computer_version, ComputerVersion, MODEL_COMPUTER_TOOLS};
use super::{
    anthropic_image_size, build_provider_body, parse_beta_features, process_anthropic_sse_line,
    translate_anthropic_response, AnthropicSseState, ProviderBody, RecordedSignal,
    MODEL_IMAGE_TIERS,
};
use murmur_driver_image::fixtures::{png, screenshot_like};
use murmur_driver_image::{decode_base64, dimensions, encode_base64};
use serde_json::{json, Value};

const NO_CACHE: &str = r#"{"prompt_cache": false}"#;

fn bash_tool() -> Value {
    json!({"name": "bash", "description": "Runs a shell command.",
           "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}})
}

fn computer_tool() -> Value {
    json!({
        "name": "tool-computer",
        "description": "Operates this capsule's own virtual display (1440x900).",
        "parameters": {"type": "object", "properties": {"action": {"type": "string"}},
                       "required": ["action"]},
        "provider_tool": {"kind": "computer", "display_width": 1440, "display_height": 900}
    })
}

fn request(model: &str, tools: Value, messages: Value) -> Value {
    json!({"model": model, "max_tokens": 1024, "messages": messages, "tools": tools})
}

fn first_turn() -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": "Fill in the form."}]}])
}

fn build(raw: &Value, driver_config: Option<&str>) -> ProviderBody {
    build_provider_body(&raw.to_string(), driver_config, Some(NO_CACHE)).unwrap()
}

fn without_provider_tool(raw: &Value) -> Value {
    let mut raw = raw.clone();
    for tool in raw["tools"].as_array_mut().unwrap() {
        tool.as_object_mut().unwrap().remove("provider_tool");
    }
    raw
}

fn body_bytes(raw: &Value, driver_config: Option<&str>, artifact: Option<&str>) -> Vec<u8> {
    let built = build_provider_body(&raw.to_string(), driver_config, artifact).unwrap();
    serde_json::to_vec(&built.body).unwrap()
}

fn png_block(width: u32, height: u32) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
           "data": encode_base64(&png(&screenshot_like(width, height)))}})
}

fn sent_size(block: &Value) -> (u32, u32) {
    let source = &block["source"];
    let bytes = decode_base64(source["data"].as_str().unwrap()).unwrap();
    dimensions(source["media_type"].as_str().unwrap(), &bytes).unwrap()
}

/// The size sonnet-4-6 (standard tier) sends a 1440x900 screenshot at.
const SONNET_4_6_SENT: (u32, u32) = (1389, 868);

// ── S1: the provider tool per model ─────────────────────────────────────────────

#[test]
fn s1_sonnet_5_5_gets_the_toolset_in_place_with_no_beta() {
    let raw = request(
        "claude-sonnet-5-5",
        json!([bash_tool(), computer_tool()]),
        first_turn(),
    );
    let built = build(&raw, None);
    let tools = built.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "bash");
    assert_eq!(tools[1], json!({"type": "computer_toolset_20260801"}));
    let plan = built.computer.expect("sonnet-5-5 is in provider mode");
    assert_eq!(plan.version, ComputerVersion::Toolset20260801);
    assert_eq!(plan.entry_name, "tool-computer");
    assert!(plan.scale.is_identity());
    assert_eq!(beta_header(None, Some(&plan)), None);
    assert_eq!(
        beta_header(Some("foo".into()), Some(&plan)),
        Some("foo".into())
    );
}

#[test]
fn s1_sonnet_4_6_gets_computer_20251124_at_the_sent_size_with_its_beta() {
    let raw = request(
        "claude-sonnet-4-6",
        json!([bash_tool(), computer_tool()]),
        first_turn(),
    );
    let config = r#"{"beta_features": ["foo"]}"#;
    let built = build(&raw, Some(config));
    let sent = anthropic_image_size("claude-sonnet-4-6", 1440, 900).unwrap();
    assert_eq!(sent, SONNET_4_6_SENT);
    assert_eq!(
        built.body["tools"][1],
        json!({"type": "computer_20251124", "name": "computer",
               "display_width_px": sent.0, "display_height_px": sent.1, "enable_zoom": true})
    );
    let plan = built.computer.unwrap();
    assert_eq!(plan.scale.sent(), sent);
    assert_eq!(plan.scale.display(), (1440, 900));
    assert_eq!(
        beta_header(parse_beta_features(config), Some(&plan)),
        Some("foo,computer-use-2025-11-24".into())
    );
    // Configured already: not repeated.
    assert_eq!(
        beta_header(Some("computer-use-2025-11-24, foo".into()), Some(&plan)),
        Some("computer-use-2025-11-24, foo".into())
    );
}

#[test]
fn s1_haiku_4_5_gets_computer_20250124_without_zoom() {
    let raw = request(
        "claude-haiku-4-5",
        json!([bash_tool(), computer_tool()]),
        first_turn(),
    );
    let built = build(&raw, None);
    let sent = anthropic_image_size("claude-haiku-4-5", 1440, 900).unwrap();
    assert_eq!(
        built.body["tools"][1],
        json!({"type": "computer_20250124", "name": "computer",
               "display_width_px": sent.0, "display_height_px": sent.1})
    );
    let plan = built.computer.unwrap();
    assert_eq!(
        beta_header(None, Some(&plan)),
        Some("computer-use-2025-01-24".into())
    );
}

#[test]
fn s1_the_cache_marker_lands_on_a_last_computer_entry() {
    let raw = request(
        "claude-sonnet-5-5",
        json!([bash_tool(), computer_tool()]),
        first_turn(),
    );
    let built = build_provider_body(&raw.to_string(), None, None).unwrap();
    assert_eq!(
        built.body["tools"][1],
        json!({"type": "computer_toolset_20260801", "cache_control": {"type": "ephemeral"}})
    );
}

#[test]
fn s1_computer_table_holds_the_image_table_ids_in_order() {
    let computer: Vec<&str> = MODEL_COMPUTER_TOOLS.iter().map(|(id, _)| *id).collect();
    let images: Vec<&str> = MODEL_IMAGE_TIERS.iter().map(|(id, _)| *id).collect();
    assert_eq!(computer, images);
    let toolset = |id: &str| computer_version(id) == Some(ComputerVersion::Toolset20260801);
    for id in [
        "claude-fable-5-1",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
    ] {
        assert!(toolset(id), "{id}");
    }
    for id in [
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-opus-4-5-20251101",
        "claude-opus-4-5",
    ] {
        assert_eq!(
            computer_version(id),
            Some(ComputerVersion::Computer20251124),
            "{id}"
        );
    }
    for id in [
        "claude-sonnet-4-5-20250929",
        "claude-sonnet-4-5",
        "claude-haiku-4-5-20251001",
        "claude-haiku-4-5",
    ] {
        assert_eq!(
            computer_version(id),
            Some(ComputerVersion::Computer20250124),
            "{id}"
        );
    }
    assert_eq!(computer_version("claude-3-7-sonnet-20250219"), None);
}

/// README.md's "Computer use" model table: `| `<id>` | `<type>` | <beta or none> |`.
#[test]
fn s1_readme_computer_use_table_matches_the_code_table() {
    let readme = include_str!("../README.md");
    let section = readme
        .split("\n## ")
        .find(|section| section.starts_with("Computer use\n"))
        .expect("README.md must have a ## Computer use section");
    let table = section
        .split("\n### ")
        .find(|sub| sub.starts_with("Models\n"))
        .expect("## Computer use must have a ### Models table");
    let mut rows = Vec::new();
    for row in table.lines() {
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        let [_, id, tool_type, beta, ..] = cells[..] else {
            continue;
        };
        let Some(id) = id
            .strip_prefix("`claude-")
            .and_then(|id| id.strip_suffix('`'))
        else {
            continue;
        };
        rows.push((
            format!("claude-{id}"),
            tool_type.trim_matches('`').to_string(),
            beta.trim_matches('`').to_string(),
        ));
    }
    let expected: Vec<(String, String, String)> = MODEL_COMPUTER_TOOLS
        .iter()
        .map(|(id, version)| {
            (
                id.to_string(),
                version.tool_type().to_string(),
                version.beta().unwrap_or("none").to_string(),
            )
        })
        .collect();
    assert_eq!(rows, expected);
}

// ── S2: function mode, byte for byte ────────────────────────────────────────────

#[test]
fn s2_a_model_outside_the_table_sends_the_function_byte_for_byte() {
    for artifact in [None, Some(NO_CACHE)] {
        let raw = request(
            "claude-3-7-sonnet-20250219",
            json!([bash_tool(), computer_tool()]),
            first_turn(),
        );
        let built = build_provider_body(&raw.to_string(), None, artifact).unwrap();
        assert!(built.computer.is_none());
        assert_eq!(
            serde_json::to_vec(&built.body).unwrap(),
            body_bytes(&without_provider_tool(&raw), None, artifact)
        );
        assert_eq!(built.body["tools"][1]["name"], "tool-computer");
        assert_eq!(
            beta_header(Some("foo".into()), built.computer.as_ref()),
            Some("foo".into())
        );
        assert_eq!(beta_header(None, built.computer.as_ref()), None);
    }
}

#[test]
fn s2_an_ordinary_tool_named_computer_keeps_function_mode() {
    let mut other = bash_tool();
    other["name"] = json!("computer");
    let raw = request(
        "claude-sonnet-5-5",
        json!([bash_tool(), computer_tool(), other]),
        first_turn(),
    );
    let built = build(&raw, None);
    assert!(built.computer.is_none());
    assert_eq!(
        serde_json::to_vec(&built.body).unwrap(),
        body_bytes(&without_provider_tool(&raw), None, Some(NO_CACHE))
    );
}

#[test]
fn s2_function_mode_history_is_unchanged() {
    // A computer call and its result on a model outside the table: no rescaling, no rewriting.
    let raw = request(
        "claude-3-7-sonnet-20250219",
        json!([computer_tool()]),
        history_messages(json!({"action": "left_click", "x": 719, "y": 450})),
    );
    let built = build(&raw, None);
    assert!(built.computer.is_none());
    assert_eq!(
        serde_json::to_vec(&built.body).unwrap(),
        body_bytes(&without_provider_tool(&raw), None, Some(NO_CACHE))
    );
    assert_eq!(
        built.body["messages"][1]["content"][1],
        json!({"type": "tool_use", "id": "toolu_1", "name": "tool-computer",
               "input": {"action": "left_click", "x": 719, "y": 450}})
    );
}

#[test]
fn s9_a_request_without_a_computer_tool_has_no_plan() {
    for model in ["claude-sonnet-5-5", "claude-sonnet-4-6", "claude-haiku-4-5"] {
        let raw = request(model, json!([bash_tool()]), first_turn());
        assert!(build(&raw, None).computer.is_none(), "{model}");
        let raw = request(model, json!([]), first_turn());
        assert!(build(&raw, None).computer.is_none(), "{model}");
        // A provider_tool of another kind, or with a bad display, is an ordinary tool.
        let mut odd = computer_tool();
        odd["provider_tool"] = json!({"kind": "browser"});
        let raw = request(model, json!([odd]), first_turn());
        assert!(build(&raw, None).computer.is_none(), "{model}");
        let mut odd = computer_tool();
        odd["provider_tool"]["display_width"] = json!(0);
        let raw = request(model, json!([odd]), first_turn());
        assert!(build(&raw, None).computer.is_none(), "{model}");
    }
}

#[test]
fn s9_a_second_computer_tool_stays_an_ordinary_function() {
    let mut second = computer_tool();
    second["name"] = json!("tool-computer-2");
    let raw = request(
        "claude-sonnet-5-5",
        json!([computer_tool(), second]),
        first_turn(),
    );
    let built = build(&raw, None);
    let plan = built.computer.unwrap();
    assert_eq!(plan.entry_index, 0);
    assert_eq!(built.body["tools"][0]["type"], "computer_toolset_20260801");
    assert_eq!(built.body["tools"][1]["name"], "tool-computer-2");
    assert!(built.body["tools"][1].get("provider_tool").is_none());
}

// ── S3: mapping Claude's computer calls ─────────────────────────────────────────

fn plan_for(model: &str) -> super::ComputerPlan {
    let raw = request(model, json!([bash_tool(), computer_tool()]), first_turn());
    build(&raw, None).computer.unwrap()
}

fn toolset_batch() -> Vec<Value> {
    vec![
        json!({"type": "text", "text": "Clicking."}),
        json!({"type": "tool_use", "id": "toolu_a", "name": "left_click", "toolset_name": "computer",
               "input": {"coordinate": [694, 434], "text": "shift"}}),
        json!({"type": "tool_use", "id": "toolu_b", "name": "type", "toolset_name": "computer",
               "input": {"text": "hi"}}),
        json!({"type": "tool_use", "id": "toolu_c", "name": "key", "toolset_name": "computer",
               "input": {"text": "Tab", "repeat": 3}}),
        json!({"type": "tool_use", "id": "toolu_d", "name": "zoom", "toolset_name": "computer",
               "input": {"region": [0, 0, 320, 200]}}),
        json!({"type": "tool_use", "id": "toolu_e", "name": "cursor_position",
               "toolset_name": "computer", "input": {}}),
        json!({"type": "tool_use", "id": "toolu_f", "name": "fly", "toolset_name": "computer",
               "input": {}}),
        json!({"type": "tool_use", "id": "toolu_g", "name": "bash",
               "input": {"command": "ls"}}),
    ]
}

/// The calls a toolset batch maps to, with `(x, y)` of the click and the zoom corner.
fn expected_batch(click: (i64, i64), zoom_edge: (i64, i64)) -> Vec<Value> {
    vec![
        json!({"type": "text", "text": "Clicking."}),
        json!({"type": "tool_call", "id": "toolu_a", "name": "tool-computer",
               "input": {"action": "left_click", "x": click.0, "y": click.1, "modifiers": "shift"}}),
        json!({"type": "tool_call", "id": "toolu_b", "name": "tool-computer",
               "input": {"action": "type", "text": "hi"}}),
        json!({"type": "tool_call", "id": "toolu_c", "name": "tool-computer",
               "input": {"action": "key", "key": "Tab Tab Tab"}}),
        json!({"type": "tool_call", "id": "toolu_d", "name": "tool-computer",
               "input": {"action": "zoom", "x": 0, "y": 0, "to_x": zoom_edge.0, "to_y": zoom_edge.1}}),
        json!({"type": "tool_call", "id": "toolu_e", "name": "tool-computer",
               "input": {"action": "screenshot"}}),
        json!({"type": "tool_call", "id": "toolu_f", "name": "tool-computer",
               "input": {"action": "fly"}}),
        json!({"type": "tool_call", "id": "toolu_g", "name": "bash",
               "input": {"command": "ls"}}),
    ]
}

fn buffered(blocks: Vec<Value>) -> Value {
    json!({"type": "message", "role": "assistant", "content": blocks,
           "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5}})
}

/// The SSE stream of a turn holding `blocks`: text blocks in one delta, `tool_use` blocks with
/// their input split in two deltas.
fn sse_of(blocks: &[Value]) -> String {
    let mut out = String::new();
    let mut event = |name: &str, data: Value| {
        out.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    };
    event(
        "message_start",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
    );
    for (index, block) in blocks.iter().enumerate() {
        match block["type"].as_str().unwrap() {
            "text" => {
                event(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index,
                           "content_block": {"type": "text", "text": ""}}),
                );
                event(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index,
                           "delta": {"type": "text_delta", "text": block["text"]}}),
                );
            }
            _ => {
                let mut start = block.clone();
                start["input"] = json!({});
                event(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index, "content_block": start}),
                );
                let input = block["input"].to_string();
                let (a, b) = input.split_at(input.len() / 2);
                for part in [a, b] {
                    event(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index,
                               "delta": {"type": "input_json_delta", "partial_json": part}}),
                    );
                }
            }
        }
        event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        );
    }
    event(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
               "usage": {"output_tokens": 5}}),
    );
    event("message_stop", json!({"type": "message_stop"}));
    out
}

fn streamed(
    body: &str,
    plan: Option<super::ComputerPlan>,
) -> (Result<Value, super::Failure>, Vec<RecordedSignal>) {
    let mut state = AnthropicSseState::with_computer(plan);
    let mut signals = Vec::new();
    for line in body.lines() {
        process_anthropic_sse_line(line, &mut state, &mut |_| {}, &mut |_| {}, &mut |s| {
            signals.push(RecordedSignal::from(s))
        });
        if state.done {
            break;
        }
    }
    (super::assemble_anthropic_streaming_response(state), signals)
}

fn assert_signals(signals: &[RecordedSignal], computer_ids: &[&str]) {
    for id in computer_ids {
        assert!(
            signals.contains(&RecordedSignal::Started {
                id: id.to_string(),
                name: "tool-computer".into()
            }),
            "{id} started under the entry name: {signals:?}"
        );
        assert!(
            !signals
                .iter()
                .any(|s| matches!(s, RecordedSignal::InputBytes { id: i, .. } if i == id)),
            "{id} is never sized: {signals:?}"
        );
    }
}

#[test]
fn s3_a_toolset_batch_maps_and_rescales_on_sonnet_4_6() {
    let plan = plan_for("claude-sonnet-4-6");
    assert_eq!(plan.scale.sent(), SONNET_4_6_SENT);
    // 694·1440/1389 = 719.4, 434·900/868 = 450; 320·1440/1389 = 331.7, 200·900/868 = 207.4.
    let expected = expected_batch((719, 450), (332, 207));

    let response = translate_anthropic_response(&buffered(toolset_batch()), Some(&plan)).unwrap();
    assert_eq!(response["stop_reason"], "tool_call");
    assert_eq!(response["content"], Value::Array(expected.clone()));

    let (result, signals) = streamed(&sse_of(&toolset_batch()), Some(plan));
    assert_eq!(result.unwrap()["content"], Value::Array(expected));
    assert_signals(
        &signals,
        &[
            "toolu_a", "toolu_b", "toolu_c", "toolu_d", "toolu_e", "toolu_f",
        ],
    );
    assert!(signals.contains(&RecordedSignal::Started {
        id: "toolu_g".into(),
        name: "bash".into()
    }));
    assert!(signals
        .iter()
        .any(|s| matches!(s, RecordedSignal::InputBytes { id, .. } if id == "toolu_g")));
}

#[test]
fn s3_a_toolset_batch_maps_unscaled_on_sonnet_5_5() {
    let plan = plan_for("claude-sonnet-5-5");
    let expected = expected_batch((694, 434), (320, 200));
    let response = translate_anthropic_response(&buffered(toolset_batch()), Some(&plan)).unwrap();
    assert_eq!(response["content"], Value::Array(expected.clone()));
    let (result, signals) = streamed(&sse_of(&toolset_batch()), Some(plan));
    assert_eq!(result.unwrap()["content"], Value::Array(expected));
    assert_signals(&signals, &["toolu_a", "toolu_f"]);
}

#[test]
fn s3_a_computer_20251124_drag_maps_and_rescales() {
    let plan = plan_for("claude-sonnet-4-6");
    let blocks = vec![
        json!({"type": "tool_use", "id": "toolu_d1", "name": "computer",
        "input": {"action": "left_click_drag", "start_coordinate": [100, 200],
                  "coordinate": [500, 300], "text": "ctrl"}}),
    ];
    let expected = json!([{"type": "tool_call", "id": "toolu_d1", "name": "tool-computer",
        "input": {"action": "left_click_drag", "x": 104, "y": 207, "to_x": 518, "to_y": 311,
                  "modifiers": "ctrl"}}]);
    let response = translate_anthropic_response(&buffered(blocks.clone()), Some(&plan)).unwrap();
    assert_eq!(response["content"], expected);
    let (result, signals) = streamed(&sse_of(&blocks), Some(plan));
    assert_eq!(result.unwrap()["content"], expected);
    assert_signals(&signals, &["toolu_d1"]);
}

#[test]
fn s3_every_action_of_the_table_maps() {
    let plan = plan_for("claude-sonnet-5-5");
    let map = |name: &str, input: Value| plan.tool_input(name, Some("computer"), &input);
    assert_eq!(
        map("screenshot", json!({})),
        json!({"action": "screenshot"})
    );
    for click in [
        "left_click",
        "right_click",
        "middle_click",
        "double_click",
        "triple_click",
    ] {
        assert_eq!(map(click, json!({})), json!({"action": click}));
        assert_eq!(
            map(click, json!({"coordinate": [5, 6]})),
            json!({"action": click, "x": 5, "y": 6})
        );
    }
    assert_eq!(
        map("mouse_move", json!({"coordinate": [5, 6]})),
        json!({"action": "mouse_move", "x": 5, "y": 6})
    );
    for button in ["left_mouse_down", "left_mouse_up"] {
        assert_eq!(map(button, json!({})), json!({"action": button}));
        assert_eq!(
            map(button, json!({"coordinate": [1, 2]})),
            json!({"action": button, "x": 1, "y": 2})
        );
    }
    assert_eq!(
        map(
            "scroll",
            json!({"coordinate": [5, 6], "scroll_direction": "down", "scroll_amount": 3, "text": "shift"})
        ),
        json!({"action": "scroll", "x": 5, "y": 6, "scroll_direction": "down", "scroll_amount": 3,
               "modifiers": "shift"})
    );
    assert_eq!(
        map("key", json!({"text": "ctrl+s"})),
        json!({"action": "key", "key": "ctrl+s"})
    );
    assert_eq!(
        map("key", json!({"text": "Tab", "repeat": 1})),
        json!({"action": "key", "key": "Tab"})
    );
    assert_eq!(
        map("hold_key", json!({"text": "shift", "duration": 2})),
        json!({"action": "hold_key", "key": "shift", "duration": 2})
    );
    assert_eq!(
        map("wait", json!({"duration": 1.5})),
        json!({"action": "wait", "duration": 1.5})
    );
    // The older versions name the action in the input.
    assert_eq!(
        plan.tool_input(
            "computer",
            None,
            &json!({"action": "left_click", "coordinate": [5, 6]})
        ),
        json!({"action": "left_click", "x": 5, "y": 6})
    );
}

#[test]
fn s3_what_the_table_cannot_read_passes_through_verbatim() {
    let plan = plan_for("claude-sonnet-4-6");
    let map = |name: &str, input: Value| plan.tool_input(name, Some("computer"), &input);
    // A malformed coordinate and an out-of-range repeat keep their own names.
    assert_eq!(
        map("left_click", json!({"coordinate": [5]})),
        json!({"action": "left_click", "coordinate": [5]})
    );
    assert_eq!(
        map("left_click", json!({"coordinate": "here", "text": 7})),
        json!({"action": "left_click", "coordinate": "here", "text": 7})
    );
    assert_eq!(
        map("key", json!({"text": "Tab", "repeat": 101})),
        json!({"action": "key", "key": "Tab", "repeat": 101})
    );
    assert_eq!(
        map("key", json!({"text": "Tab", "repeat": 0})),
        json!({"action": "key", "key": "Tab", "repeat": 0})
    );
    assert_eq!(
        map("zoom", json!({"region": [0, 0, 10]})),
        json!({"action": "zoom", "region": [0, 0, 10]})
    );
    // An unknown action and an unknown field.
    assert_eq!(
        map("fly", json!({"to": "moon"})),
        json!({"action": "fly", "to": "moon"})
    );
    assert_eq!(
        map("left_click", json!({"coordinate": [0, 0], "speed": 3})),
        json!({"action": "left_click", "x": 0, "y": 0, "speed": 3})
    );
    // Off the screenshot stays off the display: never clamped.
    assert_eq!(
        map("left_click", json!({"coordinate": [1389, 900]})),
        json!({"action": "left_click", "x": 1440, "y": 933})
    );
    // An older-version input with no action reaches the tool as it is.
    assert_eq!(
        plan.tool_input("computer", None, &json!({"coordinate": [1, 1]})),
        json!({"coordinate": [1, 1]})
    );
}

#[test]
fn s3_without_a_plan_a_toolset_block_is_untouched() {
    let blocks = vec![
        json!({"type": "tool_use", "id": "toolu_a", "name": "left_click",
        "toolset_name": "computer", "input": {"coordinate": [1, 2]}}),
    ];
    let response = translate_anthropic_response(&buffered(blocks.clone()), None).unwrap();
    assert_eq!(
        response["content"],
        json!([{"type": "tool_call", "id": "toolu_a", "name": "left_click",
                "input": {"coordinate": [1, 2]}}])
    );
    let (result, _) = streamed(&sse_of(&blocks), None);
    assert_eq!(result.unwrap()["content"], response["content"]);
}

// ── S4: history in the provider's form ──────────────────────────────────────────

fn history_messages(input: Value) -> Value {
    json!([
        {"role": "user", "content": [{"type": "text", "text": "Click the button."}]},
        {"role": "assistant", "content": [
            {"type": "text", "text": "Clicking."},
            {"type": "tool_call", "id": "toolu_1", "name": "tool-computer", "input": input}
        ]},
        {"role": "tool", "tool_call_id": "toolu_1", "is_error": false, "content": [
            {"type": "text", "text": "left_click at 719,450\ncursor: 719,450\nscreenshot: shot.png (1440x900)"},
            png_block(1440, 900)
        ]}
    ])
}

fn click() -> Value {
    json!({"action": "left_click", "x": 719, "y": 450})
}

#[test]
fn s4_sonnet_5_5_replays_a_toolset_member_and_echoes_toolset_name() {
    let raw = request(
        "claude-sonnet-5-5",
        json!([computer_tool()]),
        history_messages(click()),
    );
    let built = build(&raw, None);
    let messages = &built.body["messages"];
    assert_eq!(
        messages[1]["content"][1],
        json!({"type": "tool_use", "id": "toolu_1", "name": "left_click",
               "toolset_name": "computer", "input": {"coordinate": [719, 450]}})
    );
    let result = &messages[2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_1");
    assert_eq!(result["toolset_name"], "computer");
    assert_eq!(
        result["content"][0],
        json!({"type": "text", "text": "cursor: 719,450"})
    );
    assert_eq!(sent_size(&result["content"][1]), (1440, 900));
    assert_eq!(result["content"].as_array().unwrap().len(), 2);
}

#[test]
fn s4_sonnet_4_6_replays_the_computer_tool_in_screenshot_pixels() {
    let raw = request(
        "claude-sonnet-4-6",
        json!([computer_tool()]),
        history_messages(click()),
    );
    let built = build(&raw, None);
    let messages = &built.body["messages"];
    // 719·1389/1440 = 693.5, 450·868/900 = 434.
    assert_eq!(
        messages[1]["content"][1],
        json!({"type": "tool_use", "id": "toolu_1", "name": "computer",
               "input": {"action": "left_click", "coordinate": [694, 434]}})
    );
    let result = &messages[2]["content"][0];
    assert!(result.get("toolset_name").is_none());
    assert_eq!(
        result["content"][0],
        json!({"type": "text", "text": "cursor: 694,434"})
    );
    let entry = &built.body["tools"][0];
    let image = sent_size(&result["content"][1]);
    assert_eq!(
        (
            entry["display_width_px"].as_u64().unwrap() as u32,
            entry["display_height_px"].as_u64().unwrap() as u32
        ),
        image,
        "the pre-pass sends a display-size screenshot at exactly the entry's size"
    );
    assert_eq!(image, built.computer.unwrap().scale.sent());
    // The report records the resize.
    assert_eq!(built.images.len(), 1);
    let report = built.images[0].to_json();
    assert_eq!(report["placement"], "tool_result");
}

#[test]
fn s4_a_zoom_on_haiku_4_5_replays_as_an_ordinary_call() {
    let zoom = json!({"action": "zoom", "x": 0, "y": 0, "to_x": 320, "to_y": 200});
    let raw = request(
        "claude-haiku-4-5",
        json!([computer_tool()]),
        history_messages(zoom.clone()),
    );
    let built = build(&raw, None);
    assert!(built.computer.is_some());
    let messages = &built.body["messages"];
    assert_eq!(
        messages[1]["content"][1],
        json!({"type": "tool_use", "id": "toolu_1", "name": "tool-computer", "input": zoom})
    );
    let result = &messages[2]["content"][0];
    assert!(result.get("toolset_name").is_none());
    // An ordinary result: the tool's text as it is.
    assert_eq!(
        result["content"][0]["text"],
        "left_click at 719,450\ncursor: 719,450\nscreenshot: shot.png (1440x900)"
    );
}

#[test]
fn s4_the_same_request_builds_the_same_bytes() {
    for model in ["claude-sonnet-5-5", "claude-sonnet-4-6", "claude-haiku-4-5"] {
        let raw = request(
            model,
            json!([bash_tool(), computer_tool()]),
            history_messages(click()),
        );
        assert_eq!(
            body_bytes(&raw, None, None),
            body_bytes(&raw, None, None),
            "{model}"
        );
    }
}

#[test]
fn s4_an_empty_result_says_ok_and_a_refusal_keeps_its_text() {
    let mut messages = history_messages(click());
    messages[2]["content"] =
        json!([{"type": "text", "text": "left_click at 719,450\ncursor: 719,450"}]);
    let raw = request("claude-sonnet-5-5", json!([computer_tool()]), messages);
    let built = build(&raw, None);
    assert_eq!(
        built.body["messages"][2]["content"][0]["content"],
        json!([{"type": "text", "text": "cursor: 719,450"}])
    );

    let mut messages = history_messages(click());
    messages[2]["content"] = json!([]);
    let raw = request("claude-sonnet-5-5", json!([computer_tool()]), messages);
    let built = build(&raw, None);
    assert_eq!(
        built.body["messages"][2]["content"][0]["content"],
        json!([{"type": "text", "text": "OK"}])
    );

    let refusal = "tool-computer: x 1500 is outside the display (0-1439)";
    let mut messages = history_messages(click());
    messages[2]["content"] = json!([{"type": "text", "text": refusal}]);
    let raw = request("claude-sonnet-5-5", json!([computer_tool()]), messages);
    let built = build(&raw, None);
    assert_eq!(
        built.body["messages"][2]["content"][0]["content"],
        json!([{"type": "text", "text": refusal}])
    );
}

#[test]
fn s4_murmurs_fence_stays_around_the_rewritten_text_and_an_empty_one_says_ok() {
    let fence = |inner: &str| {
        format!("<untrusted-content source=tool:tool-computer>\n{inner}\n</untrusted-content>")
    };
    let mut messages = history_messages(click());
    messages[2]["content"][0]["text"] = json!(fence(
        "left_click at 719,450\ncursor: 719,450\nscreenshot: shot.png (1440x900)"
    ));
    let raw = request("claude-sonnet-4-6", json!([computer_tool()]), messages);
    let built = build(&raw, None);
    assert_eq!(
        built.body["messages"][2]["content"][0]["content"][0]["text"],
        fence("cursor: 694,434")
    );

    let mut messages = history_messages(click());
    messages[2]["content"] =
        json!([{"type": "text", "text": fence("left_click at 719,450\ncursor: 719,450")}]);
    messages[2]["content"][0]["text"] =
        json!(fence("moved\ncursor: 1,1").replace("cursor: 1,1", "screenshot: s.png (1x1)"));
    let raw = request("claude-sonnet-5-5", json!([computer_tool()]), messages);
    let built = build(&raw, None);
    assert_eq!(
        built.body["messages"][2]["content"][0]["content"],
        json!([{"type": "text", "text": fence("moved")}])
    );

    let mut messages = history_messages(click());
    messages[2]["content"] =
        json!([{"type": "text", "text": fence("screenshot: s.png (1440x900)")}]);
    let raw = request("claude-sonnet-5-5", json!([computer_tool()]), messages);
    let built = build(&raw, None);
    assert_eq!(
        built.body["messages"][2]["content"][0]["content"],
        json!([{"type": "text", "text": "OK"}])
    );
}

#[test]
fn s4_the_inverse_table_on_the_toolset() {
    let plan = plan_for("claude-sonnet-5-5");
    let call = |input: Value| plan.provider_call("t", &input);
    let member = |name: &str, input: Value| {
        Some(
            json!({"type": "tool_use", "id": "t", "name": name, "toolset_name": "computer",
                    "input": input}),
        )
    };
    assert_eq!(
        call(json!({"action": "screenshot"})),
        member("screenshot", json!({}))
    );
    assert_eq!(
        call(json!({"action": "double_click", "x": 3, "y": 4, "modifiers": "shift"})),
        member(
            "double_click",
            json!({"coordinate": [3, 4], "text": "shift"})
        )
    );
    assert_eq!(
        call(json!({"action": "left_click_drag", "x": 1, "y": 2, "to_x": 3, "to_y": 4})),
        member(
            "left_click_drag",
            json!({"start_coordinate": [1, 2], "coordinate": [3, 4]})
        )
    );
    assert_eq!(
        call(json!({"action": "mouse_move", "x": 1, "y": 2})),
        member("mouse_move", json!({"coordinate": [1, 2]}))
    );
    assert_eq!(
        call(json!({"action": "left_mouse_down"})),
        member("left_mouse_down", json!({}))
    );
    // The toolset's left_mouse_down/up take no point.
    assert_eq!(
        call(json!({"action": "left_mouse_up", "x": 1, "y": 2})),
        None
    );
    assert_eq!(
        call(
            json!({"action": "scroll", "x": 1, "y": 2, "scroll_direction": "up", "scroll_amount": 5})
        ),
        member(
            "scroll",
            json!({"coordinate": [1, 2], "scroll_direction": "up", "scroll_amount": 5})
        )
    );
    assert_eq!(
        call(json!({"action": "type", "text": "hi"})),
        member("type", json!({"text": "hi"}))
    );
    assert_eq!(
        call(json!({"action": "key", "key": "Tab Tab Tab"})),
        member("key", json!({"text": "Tab", "repeat": 3}))
    );
    assert_eq!(
        call(json!({"action": "key", "key": "ctrl+a Return"})),
        member("key", json!({"text": "ctrl+a Return"}))
    );
    assert_eq!(
        call(json!({"action": "key", "key": "Return"})),
        member("key", json!({"text": "Return"}))
    );
    assert_eq!(
        call(json!({"action": "hold_key", "key": "shift", "duration": 2})),
        member("hold_key", json!({"text": "shift", "duration": 2}))
    );
    assert_eq!(
        call(json!({"action": "wait", "duration": 1})),
        member("wait", json!({"duration": 1}))
    );
    assert_eq!(
        call(json!({"action": "zoom", "x": 0, "y": 0, "to_x": 320, "to_y": 200})),
        member("zoom", json!({"region": [0, 0, 320, 200]}))
    );
    // A field passed through on the way in comes back under its own name.
    assert_eq!(
        call(json!({"action": "left_click", "coordinate": "here"})),
        member("left_click", json!({"coordinate": "here"}))
    );
    // An action Claude never names replays as an ordinary call.
    assert_eq!(call(json!({"action": "fly"})), None);
    assert_eq!(call(json!({"x": 1})), None);
}

#[test]
fn s4_the_inverse_table_on_the_older_versions() {
    let plan = plan_for("claude-sonnet-4-6");
    let call = |input: Value| plan.provider_call("t", &input);
    let computer = |input: Value| {
        Some(json!({"type": "tool_use", "id": "t", "name": "computer", "input": input}))
    };
    assert_eq!(
        call(json!({"action": "left_mouse_down", "x": 719, "y": 450})),
        computer(json!({"action": "left_mouse_down", "coordinate": [694, 434]}))
    );
    assert_eq!(
        call(json!({"action": "key", "key": "Tab Tab"})),
        computer(json!({"action": "key", "text": "Tab Tab"}))
    );
    assert_eq!(
        call(json!({"action": "zoom", "x": 0, "y": 0, "to_x": 1440, "to_y": 900})),
        computer(json!({"action": "zoom", "region": [0, 0, 1388, 867]}))
    );
    let haiku = plan_for("claude-haiku-4-5");
    assert_eq!(
        haiku.provider_call(
            "t",
            &json!({"action": "zoom", "x": 0, "y": 0, "to_x": 9, "to_y": 9})
        ),
        None
    );
    assert!(haiku
        .provider_call("t", &json!({"action": "screenshot"}))
        .is_some());
}

#[test]
fn s4_a_round_trip_through_the_tool_input_comes_back_within_a_pixel() {
    let plan = plan_for("claude-sonnet-4-6");
    for (x, y) in [(0, 0), (694, 434), (1388, 867), (17, 851)] {
        let input = plan.tool_input(
            "left_click",
            Some("computer"),
            &json!({"coordinate": [x, y]}),
        );
        let back = plan.provider_call("t", &input).unwrap();
        let point = back["input"]["coordinate"].as_array().unwrap();
        let (bx, by) = (point[0].as_i64().unwrap(), point[1].as_i64().unwrap());
        assert!(
            (bx - x).abs() <= 1 && (by - y).abs() <= 1,
            "({x},{y}) -> ({bx},{by})"
        );
    }
}

// ── S11: bodies for the live probes ─────────────────────────────────────────────

/// Writes the live-probe bodies, built by the same path `run_inner` uses, to
/// `$COMPUTER_PROBE_DIR/<name>.body.json`, with the `anthropic-beta` value `run_inner` would send
/// in `<name>.beta` (empty for none). A script outside the repository posts them.
#[test]
#[ignore = "writes live-probe bodies; set COMPUTER_PROBE_DIR"]
fn s11_probe_bodies() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("COMPUTER_PROBE_DIR").expect("set COMPUTER_PROBE_DIR"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    let ask = json!({"role": "user", "content": [{"type": "text", "text":
        "You are operating a 1440x900 test desktop through the computer tool. Take a screenshot, \
         then click the centre of the screen. Do nothing else."}]});
    let click_history = |input: Value, id: &str| {
        vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "Clicking."},
                {"type": "tool_call", "id": id, "name": "tool-computer", "input": input}
            ]}),
            json!({"role": "tool", "tool_call_id": id, "is_error": false, "content": [
                {"type": "text", "text": "the action\ncursor: 719,450\nscreenshot: shot.png (1440x900)"},
                png_block(1440, 900)
            ]}),
        ]
    };
    let mut p1 = vec![ask.clone()];
    p1.extend(click_history(click(), "toolu_probe_1"));
    let mut p4 = vec![ask.clone()];
    p4.extend(click_history(
        json!({"action": "left_mouse_down", "x": 100, "y": 100}),
        "toolu_probe_0",
    ));
    p4.extend(click_history(click(), "toolu_probe_1"));
    // A toolset batch of two calls, each answered by its own tool message, as murmur records it,
    // with murmur's fence around each result's text.
    let fenced = |text: &str| {
        format!("<untrusted-content source=tool:tool-computer>\n{text}\n</untrusted-content>")
    };
    let p6 = vec![
        ask.clone(),
        json!({"role": "assistant", "content": [
            {"type": "tool_call", "id": "toolu_probe_a", "name": "tool-computer",
             "input": {"action": "left_click", "x": 720, "y": 450}},
            {"type": "tool_call", "id": "toolu_probe_b", "name": "tool-computer",
             "input": {"action": "type", "text": "hi"}}
        ]}),
        json!({"role": "tool", "tool_call_id": "toolu_probe_a", "is_error": false, "content": [
            {"type": "text", "text": fenced("left_click at 720,450\ncursor: 720,450\nscreenshot: s.png (1440x900)")},
            png_block(1440, 900)
        ]}),
        json!({"role": "tool", "tool_call_id": "toolu_probe_b", "is_error": false, "content": [
            {"type": "text", "text": fenced("typed 2 characters\ncursor: 720,450\nscreenshot: s.png (1440x900)")},
            png_block(1440, 900)
        ]}),
    ];
    let probes = [
        ("p6", "claude-haiku-5-5", p6),
        ("p1", "claude-haiku-5-5", p1.clone()),
        ("p2", "claude-sonnet-4-6", p1.clone()),
        ("p3", "claude-haiku-4-5", p1),
        ("p4", "claude-haiku-5-5", p4),
        ("p1b", "claude-haiku-5-5", vec![ask.clone()]),
        ("p2b", "claude-sonnet-4-6", vec![ask.clone()]),
        ("p3b", "claude-haiku-4-5", vec![ask.clone()]),
    ];
    for (name, model, messages) in probes {
        let raw = request(
            model,
            json!([bash_tool(), computer_tool()]),
            Value::Array(messages),
        );
        let built = build_provider_body(&raw.to_string(), None, None).unwrap();
        let beta = beta_header(None, built.computer.as_ref()).unwrap_or_default();
        std::fs::write(
            dir.join(format!("{name}.body.json")),
            serde_json::to_vec(&built.body).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{name}.beta")), beta).unwrap();
    }
}

/// Reads the live probes' captured streams from `$COMPUTER_PROBE_DIR` and maps each through the
/// driver under its model's plan, printing the murmur turn and the tool-call signals.
#[test]
#[ignore = "reads captured live-probe streams; set COMPUTER_PROBE_DIR"]
fn s11_probe_streams_map() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("COMPUTER_PROBE_DIR").expect("set COMPUTER_PROBE_DIR"),
    );
    for (name, model) in [
        ("p1b", "claude-haiku-5-5"),
        ("p2b", "claude-sonnet-4-6"),
        ("p3b", "claude-haiku-4-5"),
    ] {
        let body =
            std::fs::read_to_string(dir.join(format!("{name}.stream.response.txt"))).unwrap();
        let (result, signals) = streamed(&body, Some(plan_for(model)));
        let result = result.unwrap();
        println!("{name}: {} {signals:?}", result["content"]);
        let call = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_call")
            .expect("a computer call");
        assert_eq!(call["name"], "tool-computer");
        assert!(call["input"]["action"].is_string());
    }
}
