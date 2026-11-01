//! Images: sizing to each model's rule, placement on both surfaces, and the report.

use super::{
    attach_image_report, build_request_body, openai_image_size, DriverConfig, ImageRule,
    MurmurRequest, GPT_5_2_2048_PX, HIGH_2048_PX, MODEL_ROUTES, ORIGINAL_6000_PX,
    ORIGINAL_65535_PX,
};
use murmur_driver_image::fixtures::{gif, jpeg, png, png_header_only, screenshot_like};
use murmur_driver_image::{decode_base64, dimensions, encode_base64, not_shown_line, sha256_hex};
use serde_json::{json, Value};

fn png_base64(width: u32, height: u32) -> String {
    encode_base64(&png(&screenshot_like(width, height)))
}

fn image(media_type: &str, data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
}

/// A request calling `calls` tools at once, each answered by a tool message holding `content`.
fn tool_request(model: &str, answers: Vec<(&str, Value)>) -> Value {
    let calls: Vec<Value> = answers
        .iter()
        .map(|(id, _)| json!({"type": "tool_call", "id": id, "name": "show-image", "input": {"path": "a.png"}}))
        .collect();
    let mut messages = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "Show me."}]}),
        json!({"role": "assistant", "content": calls}),
    ];
    for (id, content) in answers {
        messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
    }
    json!({
        "model": model,
        "max_tokens": 1024,
        "messages": messages,
        "tools": [{"name": "show-image", "parameters": {"type": "object"}}]
    })
}

fn build(request: &Value) -> (Value, Vec<Value>) {
    let request: MurmurRequest = serde_json::from_value(request.clone()).unwrap();
    let (_, body, reports) = build_request_body(request, &DriverConfig::default(), false).unwrap();
    (body, reports.iter().map(|r| r.to_json()).collect())
}

fn data_url_size(url: &str) -> (String, (u32, u32)) {
    let rest = url.strip_prefix("data:").unwrap();
    let (media_type, data) = rest.split_once(";base64,").unwrap();
    let bytes = decode_base64(data).unwrap();
    (
        media_type.to_string(),
        dimensions(media_type, &bytes).unwrap(),
    )
}

// ── S1: sizing ───────────────────────────────────────────────────────────────

#[test]
fn every_table_model_sizes_a_3000x2000_image_to_its_rule() {
    for row in MODEL_ROUTES {
        let expected = match row.images {
            Some(ORIGINAL_65535_PX) | Some(ORIGINAL_6000_PX) => Some((3000, 2000)),
            Some(HIGH_2048_PX) => Some((1920, 1280)),
            Some(GPT_5_2_2048_PX) => Some((2048, 1365)),
            Some(other) => panic!("{}: an unexpected rule {other:?}", row.id),
            None => None,
        };
        assert_eq!(
            openai_image_size(row.id, 3000, 2000),
            expected,
            "{}",
            row.id
        );
    }
    assert_eq!(openai_image_size("gpt-4o", 3000, 2000), None, "not routed");
}

#[test]
fn the_rules_are_the_vision_guides_numbers() {
    let rule = |id: &str| MODEL_ROUTES.iter().find(|row| row.id == id).unwrap().images;
    let r = |max_dimension, patch_budget| {
        Some(ImageRule {
            max_dimension,
            patch_budget,
        })
    };
    for id in [
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        assert_eq!(rule(id), r(65535, 30000), "{id}");
    }
    assert_eq!(rule("gpt-5.5"), r(6000, 10000));
    assert_eq!(rule("gpt-5.4"), r(2048, 2500));
    assert_eq!(rule("gpt-5.4-mini"), r(2048, 2500));
    assert_eq!(rule("gpt-5.2"), r(2048, 6144));
    for id in [
        "gpt-6-luna",
        "gpt-6-sol",
        "gpt-6.1-sol",
        "gpt-5.6-cyber",
        "gpt-5.5-pro",
        "gpt-5.4-pro",
        "gpt-5.2-pro",
        "chat-latest",
    ] {
        assert_eq!(
            rule(id),
            r(2048, 2500),
            "{id}: unlisted, the conservative rule"
        );
    }
    assert_eq!(rule("gpt-oss-120b"), None);
    assert_eq!(rule("gpt-oss-20b"), None);
}

#[test]
fn sizing_holds_the_patch_budget_and_the_dimension_cap() {
    assert_eq!(openai_image_size("gpt-5.4", 3840, 2160), Some((2048, 1152)));
    assert_eq!(openai_image_size("gpt-5.2", 3840, 2160), Some((2048, 1152)));
    assert_eq!(openai_image_size("gpt-5.5", 8000, 5000), Some((4032, 2520)));
    assert_eq!(
        openai_image_size("gpt-5.6-luna", 8000, 5000),
        Some((6976, 4360))
    );
    assert_eq!(openai_image_size("gpt-5.4", 1000, 3000), Some((683, 2048)));
    for (id, (w, h)) in [
        ("gpt-5.5", (4032u32, 2520u32)),
        ("gpt-5.6-luna", (6976, 4360)),
    ] {
        let rule = MODEL_ROUTES
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .images
            .unwrap();
        assert!(w.div_ceil(32) * h.div_ceil(32) <= rule.patch_budget, "{id}");
    }
}

// ── S3: Chat Completions ─────────────────────────────────────────────────────

#[test]
fn chat_puts_tool_images_in_one_user_message_after_the_tool_run() {
    let first = png_base64(3000, 2000);
    let second = png_base64(30, 20);
    let request = tool_request(
        "chat-latest",
        vec![
            (
                "call_1",
                json!([{"type": "text", "text": "attached a.png"}, image("image/png", &first)]),
            ),
            ("call_2", json!([image("image/png", &second)])),
        ],
    );
    let (body, reports) = build(&request);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5, "{body}");
    assert_eq!(
        messages[2],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "attached a.png"})
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_2", "content": ""})
    );
    let follow = &messages[4];
    assert_eq!(follow["role"], "user");
    let parts = follow["content"].as_array().unwrap();
    assert_eq!(parts.len(), 4);
    assert_eq!(
        parts[0],
        json!({"type": "text", "text": "Image attached to the result of tool call call_1:"})
    );
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(
        data_url_size(parts[1]["image_url"]["url"].as_str().unwrap()),
        ("image/png".to_string(), (1920, 1280))
    );
    assert_eq!(
        parts[2],
        json!({"type": "text", "text": "Image attached to the result of tool call call_2:"})
    );
    assert_eq!(
        parts[3]["image_url"]["url"],
        format!("data:image/png;base64,{second}")
    );
    assert!(
        parts[1]["image_url"].get("detail").is_none(),
        "no detail field"
    );

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["placement"], "user_after_tool");
    assert_eq!(reports[0]["outcome"], "resized");
    assert_eq!(reports[0]["sent_width"], 1920);
    assert_eq!(reports[0]["scale"], json!(0.64));
    assert_eq!(reports[1]["outcome"], "sent");
    assert_eq!(reports[1]["message"], 3);
}

#[test]
fn chat_closes_each_tool_run_with_its_own_user_message() {
    let a = png_base64(10, 10);
    let b = png_base64(12, 12);
    let request = json!({
        "model": "chat-latest",
        "max_tokens": 64,
        "messages": [
            {"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "t", "input": {}}]},
            {"role": "tool", "tool_call_id": "c1", "content": [image("image/png", &a)]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "c2", "name": "t", "input": {}}]},
            {"role": "tool", "tool_call_id": "c2", "content": [{"type": "text", "text": "ok"}]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "c3", "name": "t", "input": {}}]},
            {"role": "tool", "tool_call_id": "c3", "content": [image("image/png", &b)]}
        ]
    });
    let (body, _) = build(&request);
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "assistant",
            "tool",
            "user",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "user"
        ]
    );
    assert_eq!(
        body["messages"][2]["content"][0]["text"],
        "Image attached to the result of tool call c1:"
    );
    assert_eq!(
        body["messages"][7]["content"][0]["text"],
        "Image attached to the result of tool call c3:"
    );
}

#[test]
fn chat_sizes_a_user_image_in_place() {
    let request = json!({
        "model": "chat-latest",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What is this?"},
            image("image/jpeg", &encode_base64(&jpeg(&screenshot_like(3840, 2160))))
        ]}]
    });
    let (body, reports) = build(&request);
    let parts = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(
        data_url_size(parts[1]["image_url"]["url"].as_str().unwrap()),
        ("image/jpeg".to_string(), (2048, 1152))
    );
    assert_eq!(reports[0]["placement"], "user");
}

// ── S4: Responses ────────────────────────────────────────────────────────────

#[test]
fn responses_puts_tool_images_in_the_function_call_output_array() {
    let data = png_base64(3000, 2000);
    let request = tool_request(
        "gpt-5.4",
        vec![
            (
                "call_1",
                json!([{"type": "text", "text": "attached"}, {"type": "text", "text": "a.png"}, image("image/png", &data)]),
            ),
            ("call_2", json!([image("image/png", &png_base64(16, 16))])),
            ("call_3", json!([{"type": "text", "text": "plain"}])),
        ],
    );
    let (body, reports) = build(&request);
    let input = body["input"].as_array().unwrap();
    let outputs: Vec<&Value> = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 3);
    let first = outputs[0]["output"].as_array().unwrap();
    assert_eq!(
        first[0],
        json!({"type": "input_text", "text": "attached\na.png"})
    );
    assert_eq!(first[1]["type"], "input_image");
    assert_eq!(
        data_url_size(first[1]["image_url"].as_str().unwrap()),
        ("image/png".to_string(), (1920, 1280))
    );
    assert!(first[1].get("detail").is_none(), "no detail field");
    let second = outputs[1]["output"].as_array().unwrap();
    assert_eq!(second.len(), 1, "no input_text without text");
    assert_eq!(second[0]["type"], "input_image");
    assert_eq!(
        outputs[2]["output"], "plain",
        "a tool message without an image keeps a string"
    );
    assert_eq!(reports[0]["placement"], "function_call_output");
    assert_eq!(reports[0]["block"], 2);
}

#[test]
fn responses_keeps_a_fitting_image_byte_for_byte_and_sizes_user_images() {
    let data = png_base64(3000, 2000);
    let request = json!({
        "model": "gpt-5.6-luna",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [image("image/png", &data)]}]
    });
    let (body, reports) = build(&request);
    assert_eq!(
        body["input"][0]["content"][0],
        json!({"type": "input_image", "image_url": format!("data:image/png;base64,{data}")})
    );
    assert_eq!(reports[0]["outcome"], "sent");
    assert_eq!(reports[0]["placement"], "user");
    assert_eq!(reports[0]["scale"], json!(1.0));
}

#[test]
fn a_gif_is_always_sent_as_a_png_of_its_first_frame() {
    let data = encode_base64(&gif(&screenshot_like(40, 30)));
    let request = tool_request(
        "gpt-5.6-luna",
        vec![("call_1", json!([image("image/gif", &data)]))],
    );
    let (body, reports) = build(&request);
    let output = &body["input"][2]["output"][0];
    assert_eq!(
        data_url_size(output["image_url"].as_str().unwrap()),
        ("image/png".to_string(), (40, 30))
    );
    assert_eq!(reports[0]["outcome"], "resized");
    assert_eq!(reports[0]["sent_media_type"], "image/png");
}

// ── S5: a model without vision ───────────────────────────────────────────────

#[test]
fn a_model_without_vision_gets_one_line_per_image() {
    let bytes = png(&screenshot_like(30, 20));
    let request = tool_request(
        "gpt-oss-20b",
        vec![(
            "call_1",
            json!([{"type": "text", "text": "attached a.png"}, image("image/png", &encode_base64(&bytes))]),
        )],
    );
    let (body, reports) = build(&request);
    let reason = "model 'gpt-oss-20b' does not take image input in driver-openai's model table";
    let line = not_shown_line("image/png", bytes.len() as u64, &sha256_hex(&bytes), reason);
    assert_eq!(
        body["input"][2]["output"],
        format!("attached a.png\n{line}")
    );
    assert_eq!(reports[0]["outcome"], "replaced");
    assert_eq!(reports[0]["reason"], reason);
    assert!(reports[0].get("width").is_none());
}

// ── S7: unusable images ──────────────────────────────────────────────────────

#[test]
fn an_unusable_image_becomes_a_line_and_the_request_is_still_built() {
    let request = tool_request(
        "chat-latest",
        vec![(
            "call_1",
            json!([
                image("image/png", &encode_base64(b"hi there")),
                image("image/png", &encode_base64(&png_header_only(9000, 100))),
                image("image/png", &encode_base64(&png_header_only(2000, 2000)))
            ]),
        )],
    );
    let (body, reports) = build(&request);
    let tool = &body["messages"][2];
    let lines: Vec<&str> = tool["content"].as_str().unwrap().split('\n').collect();
    assert_eq!(lines.len(), 3);
    assert!(
        body["messages"].as_array().unwrap().len() == 3,
        "no image left to follow the run"
    );
    let expected = [
        "the image's dimensions could not be read: ",
        "the image is 9000x100 px, over the driver's decode ceiling",
        "the image could not be decoded: ",
    ];
    for ((line, report), prefix) in lines.iter().zip(&reports).zip(expected) {
        let reason = report["reason"].as_str().unwrap();
        assert!(reason.starts_with(prefix), "{reason}");
        assert!(line.starts_with("[image not shown: image/png, "));
        assert!(line.ends_with(&format!("; {reason}]")));
    }
}

#[test]
fn murmurs_aged_and_unavailable_lines_are_never_touched() {
    let aged = "[image not shown: image/png, 5120 bytes, sha256 ab; the 5 newer images in this conversation are shown instead]";
    let request = tool_request(
        "gpt-5.6-luna",
        vec![(
            "call_1",
            json!([{"type": "text", "text": aged}, image("image/png", &png_base64(8, 8))]),
        )],
    );
    let (body, reports) = build(&request);
    assert_eq!(
        body["input"][2]["output"][0],
        json!({"type": "input_text", "text": aged})
    );
    assert_eq!(reports.len(), 1);
}

// ── S8: determinism ──────────────────────────────────────────────────────────

#[test]
fn the_same_request_gives_the_same_bytes() {
    let request = tool_request(
        "gpt-5.4",
        vec![(
            "call_1",
            json!([image("image/png", &png_base64(2500, 1500))]),
        )],
    );
    let first = serde_json::to_vec(&build(&request).0).unwrap();
    let second = serde_json::to_vec(&build(&request).0).unwrap();
    assert_eq!(sha256_hex(&first), sha256_hex(&second));
}

// ── The report ───────────────────────────────────────────────────────────────

#[test]
fn the_report_is_added_only_when_the_request_held_an_image() {
    let request: MurmurRequest = serde_json::from_value(tool_request(
        "gpt-5.6-luna",
        vec![("call_1", json!([image("image/png", &png_base64(8, 8))]))],
    ))
    .unwrap();
    let (_, _, reports) = build_request_body(request, &DriverConfig::default(), false).unwrap();
    let mut response = json!({"stop_reason": "end_turn", "content": []});
    attach_image_report(&mut response, &reports);
    assert_eq!(response["images"][0]["placement"], "function_call_output");

    let mut plain = json!({"stop_reason": "end_turn", "content": []});
    attach_image_report(&mut plain, &[]);
    assert!(plain.get("images").is_none());
}

// ── S9: README.md's Images section ───────────────────────────────────────────

fn readme_images() -> &'static str {
    include_str!("../README.md")
        .split("\n## ")
        .find(|section| section.starts_with("Images\n"))
        .expect("README.md must have a ## Images section")
}

#[test]
fn readme_images_table_matches_the_model_table() {
    let documented: Vec<(String, String, String, String)> = readme_images()
        .lines()
        .filter_map(|row| {
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            let [_, id, reads, dimension, patches, ..] = cells[..] else {
                return None;
            };
            let id = id.strip_prefix('`')?.strip_suffix('`')?;
            MODEL_ROUTES.iter().any(|row| row.id == id).then(|| {
                (
                    id.to_string(),
                    reads.into(),
                    dimension.into(),
                    patches.into(),
                )
            })
        })
        .collect();
    let expected: Vec<(String, String, String, String)> = MODEL_ROUTES
        .iter()
        .map(|row| match row.images {
            Some(rule) => (
                row.id.to_string(),
                "yes".into(),
                rule.max_dimension.to_string(),
                rule.patch_budget.to_string(),
            ),
            None => (row.id.to_string(), "no".into(), "–".into(), "–".into()),
        })
        .collect();
    assert_eq!(documented, expected);
}

#[test]
fn readme_images_states_the_rules_report_and_lines_the_code_uses() {
    use murmur_driver_image::{
        JPEG_QUALITY, MAX_DECODE_ALLOC, MAX_DECODE_EDGE, MAX_DECODE_PIXELS, PNG_COMPRESSION,
        RESIZE_FILTER,
    };
    let section = readme_images();
    assert!(matches!(
        PNG_COMPRESSION,
        image::codecs::png::CompressionType::Level(1)
    ));
    for needle in [
        format!("{MAX_DECODE_EDGE} px a side and {MAX_DECODE_PIXELS} pixels"),
        format!("capped at {} MiB", MAX_DECODE_ALLOC / (1024 * 1024)),
        format!("quality {JPEG_QUALITY}"),
        RESIZE_FILTER.to_string(),
        "deflate level 1".to_string(),
        "`Image attached to the result of tool call <tool_call_id>:`".to_string(),
        "no `detail` field".to_string(),
        "[image not shown: <media_type>, <n> bytes, sha256 <hex>; <reason>]".to_string(),
        "`model '<model>' does not take image input in driver-openai's model table`".to_string(),
        "`the image data is not valid base64: <detail>`".to_string(),
        "`the image's dimensions could not be read: <detail>`".to_string(),
        format!(
            "`the image is <w>x<h> px, over the driver's decode ceiling of {MAX_DECODE_EDGE} px a side and {MAX_DECODE_PIXELS} pixels`"
        ),
        "`the image could not be decoded: <detail>`".to_string(),
        "`the image could not be re-encoded after resizing: <detail>`".to_string(),
        "### Cost".to_string(),
        "### Checked against the live API".to_string(),
    ] {
        assert!(section.contains(&needle), "README.md's Images section lacks {needle:?}");
    }
    for field in [
        "message",
        "block",
        "media_type",
        "bytes",
        "sha256",
        "outcome",
        "width",
        "height",
        "sent_media_type",
        "sent_width",
        "sent_height",
        "scale",
        "placement",
        "reason",
        "function_call_output",
        "user_after_tool",
        "user",
        "sent",
        "resized",
        "replaced",
    ] {
        assert!(
            section.contains(&format!("`{field}`")),
            "README.md's Images section lacks `{field}`"
        );
    }
}
