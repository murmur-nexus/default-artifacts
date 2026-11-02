//! Computer use: the provider tool per model, a `computer_call` as an ordered group of tool
//! calls, folding the group's results into one `computer_call_output`, and safety checks.

use super::computer::{
    derived_id, split_derived_id, BLANK_SCREENSHOT_URL, COMPUTER_CALL_OUTPUT_PLACEMENT,
    DEFAULT_CHECK_MESSAGE,
};
use super::{
    build_request_body, model_route, openai_image_size, parse_responses_sse_body_with_plan,
    translate_responses, ApiSurface, ComputerPlan, DriverConfig, MurmurRequest, RecordedSignal,
    RequestBody, MODEL_ROUTES,
};
use murmur_driver_image::fixtures::{png, screenshot_like};
use murmur_driver_image::{decode_base64, dimensions, encode_base64};
use serde_json::{json, Value};

fn bash_tool() -> Value {
    json!({"name": "bash", "description": "Runs a shell command.",
           "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}})
}

fn computer_tool(width: u32, height: u32) -> Value {
    json!({
        "name": "tool-computer",
        "description": "Operates this capsule's own virtual display.",
        "parameters": {"type": "object", "properties": {"action": {"type": "string"}},
                       "required": ["action"]},
        "provider_tool": {"kind": "computer", "display_width": width, "display_height": height}
    })
}

fn request(model: &str, tools: Value, messages: Value) -> Value {
    json!({"model": model, "max_tokens": 1024, "messages": messages, "tools": tools})
}

fn first_turn() -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": "Search for penguin."}]}])
}

fn build_with(raw: &Value, store: bool) -> RequestBody {
    let request: MurmurRequest = serde_json::from_value(raw.clone()).unwrap();
    build_request_body(request, &DriverConfig::default(), store).unwrap()
}

fn build(raw: &Value) -> RequestBody {
    build_with(raw, false)
}

fn bytes(raw: &Value) -> Vec<u8> {
    serde_json::to_vec(&build(raw).body).unwrap()
}

fn without_provider_tool(raw: &Value) -> Value {
    let mut raw = raw.clone();
    for tool in raw["tools"].as_array_mut().unwrap() {
        tool.as_object_mut().unwrap().remove("provider_tool");
    }
    raw
}

fn plan_for(model: &str, display: (u32, u32)) -> ComputerPlan {
    let raw = request(
        model,
        json!([bash_tool(), computer_tool(display.0, display.1)]),
        first_turn(),
    );
    build(&raw).computer.expect("provider mode")
}

fn png_block(width: u32, height: u32) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
           "data": encode_base64(&png(&screenshot_like(width, height)))}})
}

fn url_size(url: &str) -> (u32, u32) {
    let (head, data) = url
        .strip_prefix("data:")
        .unwrap()
        .split_once(";base64,")
        .unwrap();
    dimensions(head, &decode_base64(data).unwrap()).unwrap()
}

// ── S5: the provider tool per model ─────────────────────────────────────────────

#[test]
fn s5_gpt_5_6_luna_gets_the_computer_tool_in_place() {
    let raw = request(
        "gpt-5.6-luna",
        json!([bash_tool(), computer_tool(1440, 900)]),
        first_turn(),
    );
    let built = build(&raw);
    assert_eq!(built.surface, ApiSurface::Responses);
    let tools = built.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "bash");
    assert_eq!(tools[1], json!({"type": "computer"}));
    let plan = built.computer.unwrap();
    assert_eq!(plan.entry_name, "tool-computer");
    assert_eq!(plan.entry_index, 1);
    assert!(plan.scale.is_identity());
}

#[test]
fn s5_models_without_the_computer_tool_send_the_function_byte_for_byte() {
    for model in ["gpt-5.2", "chat-latest", "gpt-oss-20b", "gpt-5.5-pro"] {
        let raw = request(
            model,
            json!([bash_tool(), computer_tool(1440, 900)]),
            first_turn(),
        );
        let built = build(&raw);
        assert!(built.computer.is_none(), "{model}");
        assert_eq!(
            serde_json::to_vec(&built.body).unwrap(),
            bytes(&without_provider_tool(&raw)),
            "{model}"
        );
    }
}

#[test]
fn s5_function_mode_history_is_unchanged() {
    // A full computer group in history on a model without the tool: translated as ordinary
    // function calls and outputs, exactly as without the block.
    let raw = request("gpt-5.2", json!([computer_tool(1440, 900)]), s7_messages());
    let built = build(&raw);
    assert!(built.computer.is_none());
    assert_eq!(
        serde_json::to_vec(&built.body).unwrap(),
        bytes(&without_provider_tool(&raw))
    );
    assert!(!String::from_utf8(bytes(&raw))
        .unwrap()
        .contains("computer_call"));
}

#[test]
fn s5_every_computer_model_reads_a_1440x900_display_at_its_own_size() {
    for route in MODEL_ROUTES.iter().filter(|route| route.computer) {
        assert_eq!(route.surface, ApiSurface::Responses, "{}", route.id);
        assert_eq!(
            openai_image_size(route.id, 1440, 900),
            Some((1440, 900)),
            "{}",
            route.id
        );
    }
}

#[test]
fn s5_the_computer_column_is_the_models_listing_computer_use() {
    let listed: Vec<&str> = MODEL_ROUTES
        .iter()
        .filter(|route| route.computer)
        .map(|route| route.id)
        .collect();
    assert_eq!(
        listed,
        [
            "gpt-6-astra",
            "gpt-6.1-sol",
            "gpt-6-luna",
            "gpt-5.6-cyber",
            "gpt-6-sol",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-pro",
            "gpt-5.4-mini",
        ]
    );
}

/// README.md's "Computer use" model table: `| `<id>` | yes/no |`.
#[test]
fn s5_readme_computer_use_table_matches_the_route_table() {
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
        let [_, id, computer, ..] = cells[..] else {
            continue;
        };
        let Some(id) = id.strip_prefix('`').and_then(|id| id.strip_suffix('`')) else {
            continue;
        };
        let computer = match computer {
            "yes" => true,
            "no" => false,
            other => panic!("README.md row '{id}': '{other}' is not yes or no"),
        };
        rows.push((id.to_string(), computer));
    }
    let expected: Vec<(String, bool)> = MODEL_ROUTES
        .iter()
        .map(|route| (route.id.to_string(), route.computer))
        .collect();
    assert_eq!(rows, expected);
}

#[test]
fn s9_a_request_without_a_computer_tool_has_no_plan() {
    for model in ["gpt-5.6-luna", "gpt-5.4"] {
        assert!(build(&request(model, json!([bash_tool()]), first_turn()))
            .computer
            .is_none());
        let mut odd = computer_tool(1440, 900);
        odd["provider_tool"]["kind"] = json!("browser");
        assert!(build(&request(model, json!([odd]), first_turn()))
            .computer
            .is_none());
    }
    assert!(model_route("gpt-5.6-luna").unwrap().computer);
}

// ── S6: a batch as an ordered group ─────────────────────────────────────────────

fn s6_actions() -> Value {
    json!([
        {"type": "click", "button": "left", "x": 405, "y": 157, "keys": ["SHIFT"]},
        {"type": "type", "text": "penguin"},
        {"type": "keypress", "keys": ["CTRL", "A"]},
        {"type": "scroll", "x": 500, "y": 400, "scroll_x": -200, "scroll_y": 300},
        {"type": "drag", "path": [{"x": 1, "y": 1}, {"x": 5, "y": 5}, {"x": 9, "y": 9}]},
        {"type": "move", "x": 3, "y": 3, "keys": ["ALT"]},
        {"type": "wait"},
        {"type": "double_click", "x": 7, "y": 7},
        {"type": "click", "button": "back", "x": 1, "y": 1},
        {"type": "screenshot"}
    ])
}

fn s6_item() -> Value {
    json!({"type": "computer_call", "id": "cu_1", "call_id": "call_7", "actions": s6_actions(),
           "pending_safety_checks": [], "status": "completed"})
}

fn s6_inputs() -> Vec<Value> {
    vec![
        json!({"action": "left_click", "x": 405, "y": 157, "modifiers": "shift"}),
        json!({"action": "type", "text": "penguin"}),
        json!({"action": "key", "key": "ctrl+a"}),
        json!({"action": "scroll", "x": 500, "y": 400, "scroll_direction": "down", "scroll_amount": 3}),
        json!({"action": "scroll", "x": 500, "y": 400, "scroll_direction": "left", "scroll_amount": 2}),
        json!({"action": "left_click_drag", "x": 1, "y": 1, "to_x": 9, "to_y": 9}),
        json!({"action": "mouse_move", "x": 3, "y": 3}),
        json!({"action": "wait"}),
        json!({"action": "double_click", "x": 7, "y": 7}),
        json!({"action": "click", "button": "back", "x": 1, "y": 1}),
        json!({"action": "screenshot"}),
    ]
}

fn s6_calls() -> Vec<Value> {
    s6_inputs()
        .into_iter()
        .enumerate()
        .map(|(i, input)| {
            json!({"type": "tool_call", "id": derived_id("call_7", i + 1), "name": "tool-computer",
                   "input": input, "openai_item_id": "cu_1"})
        })
        .collect()
}

fn function_item() -> Value {
    json!({"type": "function_call", "id": "fc_9", "call_id": "call_9", "name": "bash",
           "arguments": "{\"command\":\"ls\"}", "status": "completed"})
}

fn s6_response() -> Value {
    json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {"type": "message", "id": "msg_1", "role": "assistant",
             "content": [{"type": "output_text", "text": "Searching."}]},
            s6_item(),
            function_item()
        ]
    })
}

#[test]
fn s6_a_buffered_computer_call_becomes_an_ordered_group() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let response = translate_responses(&s6_response(), Some(&plan)).unwrap();
    assert_eq!(response["stop_reason"], "tool_call");
    let mut expected = vec![json!({"type": "text", "text": "Searching."})];
    expected.extend(s6_calls());
    expected.push(
        json!({"type": "tool_call", "id": "call_9", "name": "bash", "input": {"command": "ls"}}),
    );
    assert_eq!(response["content"], Value::Array(expected));
    let ids: Vec<&str> = response["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["id"].as_str())
        .collect();
    assert_eq!(ids[0], "call_7.cu1");
    assert_eq!(ids[10], "call_7.cu11");
}

#[test]
fn s6_a_computer_call_alone_stops_for_a_tool_call() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let response = json!({"id": "resp_1", "status": "completed", "output": [s6_item()]});
    let translated = translate_responses(&response, Some(&plan)).unwrap();
    assert_eq!(translated["stop_reason"], "tool_call");
    assert_eq!(translated["content"].as_array().unwrap().len(), 11);
    // Without a plan the item is dropped and the turn ends.
    let translated = translate_responses(&response, None).unwrap();
    assert_eq!(translated["stop_reason"], "end_turn");
    assert_eq!(translated["content"], json!([]));
}

#[test]
fn s6_a_legacy_action_and_an_empty_batch() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let legacy = json!({"type": "computer_call", "id": "cu_2", "call_id": "call_8",
                        "action": {"type": "click", "button": "right", "x": 10, "y": 20}});
    assert_eq!(
        plan.tool_calls(&legacy),
        vec![
            json!({"type": "tool_call", "id": "call_8.cu1", "name": "tool-computer",
                   "input": {"action": "right_click", "x": 10, "y": 20}, "openai_item_id": "cu_2"})
        ]
    );
    for empty in [
        json!({"type": "computer_call", "id": "cu_3", "call_id": "call_9", "actions": []}),
        json!({"type": "computer_call", "id": "cu_3", "call_id": "call_9"}),
    ] {
        assert_eq!(
            plan.tool_calls(&empty),
            vec![
                json!({"type": "tool_call", "id": "call_9.cu1", "name": "tool-computer",
                       "input": {"action": "screenshot"}, "openai_item_id": "cu_3"})
            ]
        );
    }
}

fn responses_event(event: Value) -> String {
    format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().unwrap()
    )
}

fn s6_stream() -> String {
    let message = json!({"type": "message", "id": "msg_1", "role": "assistant",
        "content": [{"type": "output_text", "text": "Searching."}]});
    let mut computer_added = s6_item();
    computer_added["actions"] = json!([]);
    computer_added["status"] = json!("in_progress");
    let mut function_added = function_item();
    function_added["arguments"] = json!("");
    [
        responses_event(
            json!({"type": "response.output_item.added", "output_index": 0, "item": message}),
        ),
        responses_event(
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Searching."}),
        ),
        responses_event(
            json!({"type": "response.output_item.done", "output_index": 0, "item": message}),
        ),
        responses_event(
            json!({"type": "response.output_item.added", "output_index": 1, "item": computer_added}),
        ),
        responses_event(
            json!({"type": "response.output_item.done", "output_index": 1, "item": s6_item()}),
        ),
        responses_event(
            json!({"type": "response.output_item.added", "output_index": 2, "item": function_added}),
        ),
        responses_event(json!({"type": "response.function_call_arguments.delta",
                               "output_index": 2, "delta": "{\"command\":"})),
        responses_event(json!({"type": "response.function_call_arguments.done",
                               "output_index": 2, "arguments": "{\"command\":\"ls\"}"})),
        responses_event(
            json!({"type": "response.output_item.done", "output_index": 2, "item": function_item()}),
        ),
        responses_event(json!({"type": "response.completed",
                               "response": {"id": "resp_1", "status": "completed"}})),
    ]
    .concat()
}

#[test]
fn s6_a_streamed_computer_call_becomes_the_same_group() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let (result, signals) =
        parse_responses_sse_body_with_plan(&s6_stream(), Some(&plan), &mut |_| {}, &mut |_| {});
    let result = result.unwrap();
    assert_eq!(result["stop_reason"], "tool_call");
    let mut expected = s6_calls();
    expected.push(
        json!({"type": "tool_call", "id": "call_9", "name": "bash", "input": {"command": "ls"}}),
    );
    // The stream returns tool calls alone when the turn holds any, as before.
    assert_eq!(result["content"], Value::Array(expected));

    let started: Vec<(String, String)> = signals
        .iter()
        .filter_map(|signal| match signal {
            RecordedSignal::Started { id, name } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();
    let mut expected_started: Vec<(String, String)> = (1..=11)
        .map(|i| (derived_id("call_7", i), "tool-computer".to_string()))
        .collect();
    expected_started.push(("call_9".into(), "bash".into()));
    assert_eq!(started, expected_started);
    assert!(!signals.iter().any(
        |signal| matches!(signal, RecordedSignal::InputBytes { id, .. } if id.starts_with("call_7"))
    ));
    assert!(signals
        .iter()
        .any(|signal| matches!(signal, RecordedSignal::InputBytes { id, .. } if id == "call_9")));
}

#[test]
fn s6_a_streamed_computer_call_alone_stops_for_a_tool_call() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let body = [
        responses_event(
            json!({"type": "response.output_item.done", "output_index": 0, "item": s6_item()}),
        ),
        responses_event(json!({"type": "response.completed",
                               "response": {"id": "resp_1", "status": "completed"}})),
    ]
    .concat();
    let (result, _) =
        parse_responses_sse_body_with_plan(&body, Some(&plan), &mut |_| {}, &mut |_| {});
    let result = result.unwrap();
    assert_eq!(result["stop_reason"], "tool_call");
    assert_eq!(result["content"].as_array().unwrap().len(), 11);
    let (result, signals) =
        parse_responses_sse_body_with_plan(&body, None, &mut |_| {}, &mut |_| {});
    assert_eq!(result.unwrap()["stop_reason"], "end_turn");
    assert!(signals.is_empty());
}

#[test]
fn s6_keys_map_to_the_tools_names() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let key = |keys: Value| plan.tool_inputs(&json!({"type": "keypress", "keys": keys}));
    assert_eq!(
        key(json!(["control", "Option", "CMD", "a"])),
        vec![json!({"action": "key", "key": "ctrl+alt+super+a"})]
    );
    assert_eq!(
        key(json!(["META", "COMMAND", "WIN", "SUPER", "SHIFT", "Z"])),
        vec![json!({"action": "key", "key": "super+super+super+super+shift+z"})]
    );
    assert_eq!(
        key(json!(["ENTER"])),
        vec![json!({"action": "key", "key": "ENTER"})]
    );
    assert_eq!(
        key(json!(["ARROWUP"])),
        vec![json!({"action": "key", "key": "ARROWUP"})]
    );
}

#[test]
fn s6_scroll_amounts_and_unreadable_actions() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let scroll = |dx: i64, dy: i64| {
        plan.tool_inputs(&json!({"type": "scroll", "x": 1, "y": 2, "scroll_x": dx, "scroll_y": dy}))
    };
    assert_eq!(
        scroll(0, -40),
        vec![
            json!({"action": "scroll", "x": 1, "y": 2, "scroll_direction": "up", "scroll_amount": 1})
        ]
    );
    assert_eq!(
        scroll(9000, 0),
        vec![
            json!({"action": "scroll", "x": 1, "y": 2, "scroll_direction": "right", "scroll_amount": 50})
        ]
    );
    assert_eq!(
        scroll(0, 0),
        vec![json!({"action": "mouse_move", "x": 1, "y": 2})]
    );
    // A known action whose fields cannot be read reaches the tool under its provider name.
    assert_eq!(
        plan.tool_inputs(&json!({"type": "click", "button": "left", "x": "here"})),
        vec![json!({"action": "click", "button": "left", "x": "here"})]
    );
    assert_eq!(
        plan.tool_inputs(&json!({"type": "drag", "path": [{"x": 1, "y": 1}]})),
        vec![json!({"action": "drag", "path": [{"x": 1, "y": 1}]})]
    );
    assert_eq!(
        plan.tool_inputs(&json!({"type": "teleport", "x": 1})),
        vec![json!({"action": "teleport", "x": 1})]
    );
    // A drag path of `[x, y]` pairs and a wheel click.
    assert_eq!(
        plan.tool_inputs(&json!({"type": "drag", "path": [[1, 2], [3, 4]], "keys": ["CTRL"]})),
        vec![
            json!({"action": "left_click_drag", "x": 1, "y": 2, "to_x": 3, "to_y": 4, "modifiers": "ctrl"})
        ]
    );
    assert_eq!(
        plan.tool_inputs(&json!({"type": "click", "button": "wheel", "x": 1, "y": 2})),
        vec![json!({"action": "middle_click", "x": 1, "y": 2})]
    );
}

#[test]
fn s6_derived_ids_split_back() {
    assert_eq!(derived_id("call_7", 3), "call_7.cu3");
    assert_eq!(split_derived_id("call_7.cu3"), Some(("call_7", 3)));
    assert_eq!(split_derived_id("call_7.cu11"), Some(("call_7", 11)));
    assert_eq!(split_derived_id("a.cu1.cu2"), Some(("a.cu1", 2)));
    for other in ["call_7", "call_7.cu", ".cu1", "call_7.cux", "call_7.cu1a"] {
        assert_eq!(split_derived_id(other), None, "{other}");
    }
}

// ── S7: folding a group's results ───────────────────────────────────────────────

const BANNER: &str = "display: Xvfb 127.0.0.1:3, 1440x900x24; window manager openbox; launched zenity; shell commands reach it with DISPLAY=127.0.0.1:3 XAUTHORITY=/w/x";
const BAD_KEY: &str = "tool-computer: key 'ctrl+bogus' names no key it knows: bogus";

fn tool_message(index: usize, content: Value, is_error: bool) -> Value {
    json!({"role": "tool", "tool_call_id": derived_id("call_7", index), "is_error": is_error,
           "content": content})
}

/// The assistant turn S6 returned and the eleven tool messages answering it. Each has text and
/// a 1440x900 image except `cu3`, a refusal, and `cu5`, whose text opens with the display banner.
fn s7_messages() -> Value {
    let mut assistant = vec![json!({"type": "text", "text": "Searching."})];
    assistant.extend(s6_calls());
    let mut messages = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "Search for penguin."}]}),
        json!({"role": "assistant", "content": assistant}),
    ];
    for (i, input) in s6_inputs().iter().enumerate() {
        let index = i + 1;
        let action = input["action"].as_str().unwrap();
        let message = match index {
            3 => tool_message(index, json!([{"type": "text", "text": BAD_KEY}]), true),
            5 => tool_message(
                index,
                json!([{"type": "text", "text": format!(
                    "{BANNER}\nscroll left by 2 at 500,400\ncursor: 500,400\nscreenshot: s.png (1440x900)")},
                       png_block(1440, 900)]),
                false,
            ),
            _ => tool_message(
                index,
                json!([{"type": "text", "text": format!(
                    "{action} at 1,1\ncursor: 1,1\nscreenshot: s.png (1440x900)")},
                       png_block(1440, 900)]),
                false,
            ),
        };
        messages.push(message);
    }
    Value::Array(messages)
}

fn s7_request(model: &str, display: (u32, u32)) -> Value {
    request(
        model,
        json!([bash_tool(), computer_tool(display.0, display.1)]),
        s7_messages(),
    )
}

fn s7_notes() -> String {
    format!(
        "computer_call call_7 returned more than its screenshot:\n3. key: {BAD_KEY}\n5. scroll: {BANNER}"
    )
}

#[test]
fn s7_full_resend_rebuilds_the_computer_call_and_folds_its_results() {
    let built = build(&s7_request("gpt-5.6-luna", (1440, 900)));
    let input = built.body["input"].as_array().unwrap();
    // user, assistant message, computer_call, computer_call_output, notes.
    assert_eq!(input.len(), 5, "{input:#?}");
    assert_eq!(input[1]["type"], "message");
    let call = &input[2];
    assert_eq!(
        call,
        &json!({
            "type": "computer_call",
            "id": "cu_1",
            "call_id": "call_7",
            "actions": [
                {"type": "click", "button": "left", "x": 405, "y": 157, "keys": ["SHIFT"]},
                {"type": "type", "text": "penguin"},
                {"type": "keypress", "keys": ["CTRL", "A"]},
                {"type": "scroll", "x": 500, "y": 400, "scroll_x": 0, "scroll_y": 300},
                {"type": "scroll", "x": 500, "y": 400, "scroll_x": -200, "scroll_y": 0},
                {"type": "drag", "path": [{"x": 1, "y": 1}, {"x": 9, "y": 9}]},
                {"type": "move", "x": 3, "y": 3},
                {"type": "wait"},
                {"type": "double_click", "x": 7, "y": 7},
                {"type": "click", "button": "back", "x": 1, "y": 1}
            ],
            "pending_safety_checks": [],
            "status": "completed"
        })
    );
    let output = &input[3];
    assert_eq!(output["type"], "computer_call_output");
    assert_eq!(output["call_id"], "call_7");
    assert_eq!(output["output"]["type"], "computer_screenshot");
    assert_eq!(
        url_size(output["output"]["image_url"].as_str().unwrap()),
        (1440, 900)
    );
    assert!(output.get("acknowledged_safety_checks").is_none());
    assert_eq!(
        input[4],
        json!({"role": "user", "content": [{"type": "input_text", "text": s7_notes()}]})
    );
    let text = serde_json::to_string(&built.body).unwrap();
    assert!(!text.contains("cursor:"));
    assert!(!text.contains("screenshot: "));
    assert!(!text.contains("function_call"));
    // Only the last image is sent and reported.
    assert_eq!(text.matches("data:image/png").count(), 1);
    assert_eq!(built.images.len(), 1);
    let report = built.images[0].to_json();
    assert_eq!(report["placement"], COMPUTER_CALL_OUTPUT_PLACEMENT);
}

#[test]
fn s7_continuation_folds_the_results_without_the_assistant_turn() {
    let mut raw = s7_request("gpt-5.6-luna", (1440, 900));
    raw["continuation_id"] = json!("resp_1");
    let built = build_with(&raw, true);
    assert_eq!(built.body["previous_response_id"], "resp_1");
    let input = built.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{input:#?}");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "computer_call_output");
    assert_eq!(input[2]["content"][0]["text"], s7_notes());

    // murmur's continuation tail starts after the held assistant turn: the notes name no action.
    let mut tail = raw.clone();
    let messages = tail["messages"].as_array_mut().unwrap();
    messages.drain(..2);
    let built = build_with(&tail, true);
    let input = built.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(
        input[1]["content"][0]["text"],
        format!(
            "computer_call call_7 returned more than its screenshot:\n3. {BAD_KEY}\n5. {BANNER}"
        )
    );
}

#[test]
fn s7_an_all_error_run_gets_the_blank_screenshot_and_a_line_saying_so() {
    let mut messages = s7_messages();
    for message in messages.as_array_mut().unwrap().iter_mut().skip(2) {
        message["is_error"] = json!(true);
        message["content"] =
            json!([{"type": "text", "text": "tool-computer: the display is gone"}]);
    }
    let raw = request("gpt-5.6-luna", json!([computer_tool(1440, 900)]), messages);
    let built = build(&raw);
    let input = built.body["input"].as_array().unwrap();
    let output = &input[3];
    assert_eq!(output["output"]["image_url"], BLANK_SCREENSHOT_URL);
    assert_eq!(url_size(BLANK_SCREENSHOT_URL), (1, 1));
    let notes = input[4]["content"][0]["text"].as_str().unwrap();
    assert!(notes.starts_with("computer_call call_7 returned more than its screenshot:\n1. left_click: tool-computer: the display is gone\n"));
    assert!(notes.ends_with(
        "\nno action in this call returned a screenshot, so the screenshot sent with it is blank"
    ));
    assert!(built.images.is_empty());
}

#[test]
fn s7_a_run_with_nothing_more_than_its_screenshot_has_no_notes() {
    let mut messages = s7_messages();
    for (i, message) in messages
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .skip(2)
        .enumerate()
    {
        message["content"] = json!([{"type": "text", "text": format!("x at 1,1\ncursor: 1,1\nscreenshot: s{i}.png (1440x900)")},
                                    png_block(1440, 900)]);
    }
    let raw = request("gpt-5.6-luna", json!([computer_tool(1440, 900)]), messages);
    let input = build(&raw).body["input"].as_array().unwrap().clone();
    assert_eq!(input.len(), 4);
    assert_eq!(input[3]["type"], "computer_call_output");
}

#[test]
fn s7_murmurs_fence_adds_no_notes_line_for_a_plain_result() {
    let fence = |inner: &str| {
        format!("<untrusted-content source=tool:tool-computer>\n{inner}\n</untrusted-content>")
    };
    let mut messages = s7_messages();
    for message in messages.as_array_mut().unwrap().iter_mut().skip(2) {
        let text = message["content"][0]["text"].as_str().unwrap().to_string();
        message["content"][0]["text"] = json!(fence(&text));
    }
    let raw = request("gpt-5.6-luna", json!([computer_tool(1440, 900)]), messages);
    let input = build(&raw).body["input"].as_array().unwrap().clone();
    assert_eq!(
        input[4]["content"][0]["text"],
        format!(
            "computer_call call_7 returned more than its screenshot:\n3. key: {}\n5. scroll: {}",
            fence(BAD_KEY),
            fence(BANNER)
        )
    );
}

#[test]
fn s7_gpt_5_4_scales_a_3000x2000_display() {
    let plan = plan_for("gpt-5.4", (3000, 2000));
    assert_eq!(plan.scale.sent(), (1920, 1280));
    let item = json!({"type": "computer_call", "id": "cu_5", "call_id": "call_5",
                      "actions": [{"type": "click", "button": "left", "x": 960, "y": 640}]});
    let calls = plan.tool_calls(&item);
    assert_eq!(
        calls[0]["input"],
        json!({"action": "left_click", "x": 1500, "y": 1000})
    );

    let raw = request(
        "gpt-5.4",
        json!([computer_tool(3000, 2000)]),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Click."}]},
            {"role": "assistant", "content": calls},
            {"role": "tool", "tool_call_id": "call_5.cu1", "content": [
                {"type": "text", "text": "left_click at 1500,1000\ncursor: 1500,1000\nscreenshot: s.png (3000x2000)"},
                png_block(3000, 2000)
            ]}
        ]),
    );
    let built = build(&raw);
    let input = built.body["input"].as_array().unwrap();
    assert_eq!(
        input[1]["actions"],
        json!([{"type": "click", "button": "left", "x": 960, "y": 640}])
    );
    let url = input[2]["output"]["image_url"].as_str().unwrap();
    assert_eq!(url_size(url), (1920, 1280));
    assert_eq!(input.len(), 3, "nothing more than the screenshot: no notes");
}

#[test]
fn s7_a_screenshot_is_rebuilt_only_alone() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let shot = json!({"action": "screenshot"});
    let wait = json!({"action": "wait"});
    let item = plan.computer_call_item("c", &[(&shot, None), (&shot, None)]);
    assert_eq!(item["actions"], json!([{"type": "screenshot"}]));
    let item = plan.computer_call_item("c", &[(&shot, None), (&wait, None)]);
    assert_eq!(item["actions"], json!([{"type": "wait"}]));
    assert!(item.get("id").is_none());
}

#[test]
fn s7_the_same_request_builds_the_same_bytes() {
    let raw = s7_request("gpt-5.6-luna", (1440, 900));
    assert_eq!(bytes(&raw), bytes(&raw));
}

#[test]
fn s7_an_unsuffixed_call_of_the_entry_stays_a_function_call() {
    let raw = request(
        "gpt-5.6-luna",
        json!([computer_tool(1440, 900)]),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Click."}]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "call_1",
                "name": "tool-computer", "input": {"action": "screenshot"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "ok"}]}
        ]),
    );
    let input = build(&raw).body["input"].as_array().unwrap().clone();
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], "ok");
}

// ── S8: safety checks ───────────────────────────────────────────────────────────

fn s8_item() -> Value {
    json!({"type": "computer_call", "id": "cu_8", "call_id": "call_8",
    "actions": [{"type": "click", "button": "left", "x": 10, "y": 10},
                {"type": "type", "text": "card number"}],
    "pending_safety_checks": [
        {"id": "sc_1", "code": "malicious_instructions", "message": "The page asks for a card number."},
        {"id": "sc_2", "code": null, "message": null}
    ]})
}

fn s8_checks() -> Value {
    json!([
        {"id": "sc_1", "code": "malicious_instructions", "message": "The page asks for a card number."},
        {"id": "sc_2", "message": DEFAULT_CHECK_MESSAGE}
    ])
}

#[test]
fn s8_every_call_of_a_flagged_group_carries_the_checks() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let calls = plan.tool_calls(&s8_item());
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call["safety_checks"], s8_checks());
    }
    assert_eq!(
        DEFAULT_CHECK_MESSAGE,
        "OpenAI asks a person to confirm this computer action"
    );
    // Empty strings count as absent.
    let item = json!({"type": "computer_call", "call_id": "c",
                      "pending_safety_checks": [{"id": "", "code": "", "message": ""}]});
    assert_eq!(
        plan.tool_calls(&item)[0]["safety_checks"],
        json!([{"message": DEFAULT_CHECK_MESSAGE}])
    );
}

#[test]
fn s8_a_response_without_checks_puts_no_key_on_any_call() {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    for call in plan.tool_calls(&s6_item()) {
        assert!(call.get("safety_checks").is_none());
    }
    let mut item = s6_item();
    item.as_object_mut()
        .unwrap()
        .remove("pending_safety_checks");
    for call in plan.tool_calls(&item) {
        assert!(call.get("safety_checks").is_none());
    }
}

const DECLINE: &str = "the person did not acknowledge the safety check on this call, so it was not run: malicious_instructions: The page asks for a card number.; their reply: no";

fn s8_request(acknowledged: [bool; 2]) -> Value {
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let calls = plan.tool_calls(&s8_item());
    let mut messages = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "Pay."}]}),
        json!({"role": "assistant", "content": calls}),
    ];
    for (i, acked) in acknowledged.iter().enumerate() {
        let index = i + 1;
        let mut message = if *acked {
            tool_message(
                index,
                json!([{"type": "text", "text": "x at 10,10\ncursor: 10,10\nscreenshot: s.png (1440x900)"},
                       png_block(1440, 900)]),
                false,
            )
        } else {
            tool_message(index, json!([{"type": "text", "text": DECLINE}]), true)
        };
        message["tool_call_id"] = json!(derived_id("call_8", index));
        if *acked {
            message["acknowledged_safety_checks"] = s8_checks();
        }
        messages.push(message);
    }
    request(
        "gpt-5.6-luna",
        json!([computer_tool(1440, 900)]),
        Value::Array(messages),
    )
}

/// OpenAI's `computer` tool refuses `pending_safety_checks` and `acknowledged_safety_checks` in its
/// input (live probes P5f–P5h, HTTP 400), so neither goes back, whatever the person answered: a
/// yes is what let the call run, and a decline reaches the model in the notes.
#[test]
fn s8_no_check_goes_back_to_openai_whatever_the_person_answered() {
    for answers in [[true, true], [true, false], [false, false]] {
        let built = build(&s8_request(answers));
        let text = serde_json::to_string(&built.body).unwrap();
        assert!(!text.contains("acknowledged_safety_checks"), "{answers:?}");
        assert!(!text.contains("sc_1"), "{answers:?}");
        let input = built.body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "computer_call");
        assert_eq!(input[1]["pending_safety_checks"], json!([]));
        assert_eq!(input[2]["type"], "computer_call_output");
    }

    let input = build(&s8_request([true, true])).body["input"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(input.len(), 3, "both calls ran: no notes");

    let input = build(&s8_request([true, false])).body["input"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        input[3]["content"][0]["text"],
        format!("computer_call call_8 returned more than its screenshot:\n2. type: {DECLINE}")
    );

    let input = build(&s8_request([false, false])).body["input"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(input[2]["output"]["image_url"], BLANK_SCREENSHOT_URL);
}

// ── S11: bodies for the live probes ─────────────────────────────────────────────

/// Writes live-probe bodies, built by the same path `run_inner` uses, to `$COMPUTER_PROBE_DIR`.
/// Phase 1 (no `p5a.response.json` there): `p5a.body.json`, a first computer request. Phase 2:
/// reads OpenAI's answer from `p5a.response.json`, runs its `computer_call` through the driver,
/// answers each derived call with a 1440x900 screenshot, and writes the full resend with the
/// item's `id` (`p5b.body.json`) and without it (`p5c.body.json`).
#[test]
#[ignore = "writes live-probe bodies; set COMPUTER_PROBE_DIR"]
fn s11_probe_bodies() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("COMPUTER_PROBE_DIR").expect("set COMPUTER_PROBE_DIR"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    let ask = json!({"role": "user", "content": [{"type": "text", "text":
        "You are operating a 1440x900 test desktop through the computer tool. Take a screenshot, \
         then click the centre of the screen and type hello. Do nothing else."}]});
    let tools = json!([bash_tool(), computer_tool(1440, 900)]);
    let response_path = dir.join("p5a.response.json");
    if !response_path.exists() {
        let raw = request("gpt-5.6-luna", tools, json!([ask]));
        let built = build(&raw);
        std::fs::write(
            dir.join("p5a.body.json"),
            serde_json::to_vec(&built.body).unwrap(),
        )
        .unwrap();
        return;
    }
    let response: Value = serde_json::from_slice(&std::fs::read(response_path).unwrap()).unwrap();
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let turn = translate_responses(&response, Some(&plan)).unwrap();
    std::fs::write(dir.join("p5a.murmur.json"), turn.to_string()).unwrap();
    let content = turn["content"].as_array().unwrap().clone();
    let mut messages = vec![
        ask,
        json!({"role": "assistant", "content": content.clone()}),
    ];
    for (i, call) in content
        .iter()
        .filter(|block| block["type"] == "tool_call")
        .enumerate()
    {
        let text = if i == 0 {
            format!("{BANNER}\nscreenshot\ncursor: 720,450\nscreenshot: s.png (1440x900)")
        } else {
            "x at 720,450\ncursor: 720,450\nscreenshot: s.png (1440x900)".to_string()
        };
        messages.push(
            json!({"role": "tool", "tool_call_id": call["id"], "is_error": false,
            "content": [{"type": "text", "text": text}, png_block(1440, 900)]}),
        );
    }
    let raw = request("gpt-5.6-luna", tools, Value::Array(messages));
    let mut body = build(&raw).body;
    std::fs::write(
        dir.join("p5b.body.json"),
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    for item in body["input"].as_array_mut().unwrap() {
        if item["type"] == "computer_call" {
            item.as_object_mut().unwrap().remove("id");
        }
    }
    std::fs::write(
        dir.join("p5c.body.json"),
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();

    // S7's eleven-action group, every action shape the inverse table writes, without an item id.
    let mut messages = s7_messages();
    for block in messages[1]["content"].as_array_mut().unwrap() {
        block.as_object_mut().unwrap().remove("openai_item_id");
    }
    let raw = request(
        "gpt-5.6-luna",
        json!([bash_tool(), computer_tool(1440, 900)]),
        messages,
    );
    std::fs::write(dir.join("p5d.body.json"), bytes(&raw)).unwrap();

    // S8's acknowledged group, without an item id: the acknowledgement's shape.
    let mut raw = s8_request([true, true]);
    for block in raw["messages"][1]["content"].as_array_mut().unwrap() {
        block.as_object_mut().unwrap().remove("openai_item_id");
    }
    std::fs::write(dir.join("p5f.body.json"), bytes(&raw)).unwrap();
}

/// Maps the live probe's captured stream, `$COMPUTER_PROBE_DIR/p5a.stream.response.txt`, through
/// the driver, printing the murmur turn and the tool-call signals.
#[test]
#[ignore = "reads a captured live-probe stream; set COMPUTER_PROBE_DIR"]
fn s11_probe_stream_maps() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("COMPUTER_PROBE_DIR").expect("set COMPUTER_PROBE_DIR"),
    );
    let body = std::fs::read_to_string(dir.join("p5a.stream.response.txt")).unwrap();
    let plan = plan_for("gpt-5.6-luna", (1440, 900));
    let (result, signals) =
        parse_responses_sse_body_with_plan(&body, Some(&plan), &mut |_| {}, &mut |_| {});
    let result = result.unwrap();
    println!("p5a.stream: {} {signals:?}", result["content"]);
    assert_eq!(result["stop_reason"], "tool_call");
    let call = &result["content"].as_array().unwrap()[0];
    assert_eq!(call["name"], "tool-computer");
    assert!(call["id"].as_str().unwrap().ends_with(".cu1"));
}
