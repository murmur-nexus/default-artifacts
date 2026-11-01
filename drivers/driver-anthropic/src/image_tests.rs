//! Images: sizing to each model's tier, placement in the provider body, and the report.

use super::{
    anthropic_image_size, build_provider_body, prepare_images,
    translate_murmur_request_to_anthropic, ImageTier, ModelFamily, MurmurContentBlock,
    MurmurRequest, PromptCacheConfig, ThinkingConfig, MAX_IMAGE_BASE64_BYTES, MODEL_IMAGE_TIERS,
};
use murmur_driver_image::fixtures::{gif, png, png_header_only, screenshot_like};
use murmur_driver_image::{
    attach_image_report, decode_base64, dimensions, encode_base64, not_shown_line, sha256_hex,
};
use serde_json::{json, Value};

fn png_base64(width: u32, height: u32) -> String {
    encode_base64(&png(&screenshot_like(width, height)))
}

fn image(media_type: &str, data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
}

/// A tool round trip whose tool message holds `content`.
fn tool_request(model: &str, content: Value) -> String {
    json!({
        "model": model,
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Show me."}]},
            {"role": "assistant", "content": [
                {"type": "tool_call", "id": "toolu_1", "name": "show-image", "input": {"path": "a.png"}}
            ]},
            {"role": "tool", "tool_call_id": "toolu_1", "content": content}
        ],
        "tools": [{"name": "show-image", "parameters": {"type": "object"}}]
    })
    .to_string()
}

fn body(raw: &str) -> (Value, Vec<Value>) {
    let (body, reports) = build_provider_body(raw, None, None).unwrap();
    (body, reports.iter().map(|r| r.to_json()).collect())
}

fn sent_size(block: &Value) -> (u32, u32) {
    let source = &block["source"];
    let bytes = decode_base64(source["data"].as_str().unwrap()).unwrap();
    dimensions(source["media_type"].as_str().unwrap(), &bytes).unwrap()
}

// ── S1: sizing ───────────────────────────────────────────────────────────────

#[test]
fn every_table_model_sizes_a_3000x2000_image_to_its_tier() {
    // The vision-coordinates page's reference implementation gives 2352x1568 at 2576 px / 4784
    // tokens and 1344x896 at 1568 px / 1568 tokens.
    for (model, tier) in MODEL_IMAGE_TIERS {
        let expected = match tier {
            ImageTier::HighResolution => (2352, 1568),
            ImageTier::Standard => (1344, 896),
            ImageTier::None => unreachable!("no table model is without vision"),
        };
        assert_eq!(
            anthropic_image_size(model, 3000, 2000),
            Some(expected),
            "{model}"
        );
    }
    // An id outside the table reads at the standard tier.
    assert_eq!(
        anthropic_image_size("claude-mythos-5-1", 3000, 2000),
        Some((1344, 896))
    );
}

#[test]
fn the_vision_pages_documented_examples_hold() {
    assert_eq!(
        anthropic_image_size("claude-haiku-4-5", 1920, 1080),
        Some((1456, 819))
    );
    assert_eq!(
        anthropic_image_size("claude-sonnet-4-6", 1075, 1520),
        Some((924, 1307))
    );
    assert_eq!(
        anthropic_image_size("claude-opus-5-5", 3840, 2160),
        Some((2576, 1449))
    );
    // A size inside the tier is sent as it is.
    assert_eq!(
        anthropic_image_size("claude-opus-5-5", 1920, 1080),
        Some((1920, 1080))
    );
    assert_eq!(
        anthropic_image_size("claude-opus-4-7", 1075, 1520),
        Some((1075, 1520))
    );
}

#[test]
fn the_tier_table_holds_exactly_the_manifest_ids_once_each() {
    let manifest = include_str!("../murmur.yaml");
    let section = &manifest[manifest.find("\nmodels:\n").unwrap()..];
    let declared: Vec<&str> = section
        .lines()
        .filter_map(|line| line.strip_prefix("  ")?.strip_suffix(':'))
        .filter(|id| !id.starts_with(' '))
        .collect();
    let table: Vec<&str> = MODEL_IMAGE_TIERS.iter().map(|(id, _)| *id).collect();
    assert_eq!(table, declared);
}

// ── S2: placement ────────────────────────────────────────────────────────────

#[test]
fn a_tool_image_joins_the_tool_result_after_its_text() {
    let raw = tool_request(
        "claude-sonnet-5-5",
        json!([{"type": "text", "text": "attached a.png"}, image("image/png", &png_base64(3000, 2000))]),
    );
    let (body, reports) = body(&raw);
    let tool_turn = &body["messages"][2];
    assert_eq!(tool_turn["role"], "user");
    let results = tool_turn["content"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one tool_result: {tool_turn}");
    let result = &results[0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_1");
    let content = result["content"].as_array().unwrap();
    assert_eq!(
        content[0],
        json!({"type": "text", "text": "attached a.png"})
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(sent_size(&content[1]), (2352, 1568));

    // The cache marker stays on the tool_result, never on the nested image.
    assert_eq!(result["cache_control"], json!({"type": "ephemeral"}));
    assert!(content[1].get("cache_control").is_none());

    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(report["message"], 2);
    assert_eq!(report["block"], 1);
    assert_eq!(report["outcome"], "resized");
    assert_eq!(report["placement"], "tool_result");
    assert_eq!(
        (report["width"].clone(), report["height"].clone()),
        (json!(3000), json!(2000))
    );
    assert_eq!(report["sent_width"], 2352);
    assert_eq!(report["sent_height"], 1568);
    assert_eq!(report["sent_media_type"], "image/png");
    assert_eq!(report["scale"], json!(2352.0 / 3000.0));
}

#[test]
fn several_text_blocks_and_images_share_one_tool_result_in_order() {
    let first = png_base64(40, 30);
    let second = png_base64(20, 10);
    let raw = tool_request(
        "claude-haiku-4-5",
        json!([
            {"type": "text", "text": "one"},
            image("image/png", &first),
            {"type": "text", "text": "two"},
            image("image/png", &second)
        ]),
    );
    let (body, reports) = body(&raw);
    let results = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one tool_result per tool_use_id");
    assert_eq!(results[0]["tool_use_id"], "toolu_1");
    let content = results[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 4);
    assert_eq!(content[0], json!({"type": "text", "text": "one"}));
    // Both fit the tier: sent byte for byte, each where murmur put it.
    assert_eq!(content[1]["source"]["data"], first);
    assert_eq!(content[2], json!({"type": "text", "text": "two"}));
    assert_eq!(content[3]["source"]["data"], second);
    assert_eq!(reports[0]["outcome"], "sent");
    assert_eq!(reports[0]["block"], 1);
    assert_eq!(reports[1]["block"], 3);
    assert_eq!(reports[1]["scale"], json!(1.0));
}

#[test]
fn a_tool_message_with_only_images_makes_one_tool_result_of_them() {
    let data = png_base64(30, 30);
    let raw = tool_request("claude-opus-5-5", json!([image("image/png", &data)]));
    let (body, _) = body(&raw);
    let results = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_use_id"], "toolu_1");
    assert_eq!(results[0]["content"], json!([image("image/png", &data)]));
}

#[test]
fn a_user_image_is_sized_and_stays_in_the_user_message() {
    let raw = json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What is this?"},
            image("image/jpeg", &encode_base64(&murmur_driver_image::fixtures::jpeg(&screenshot_like(1920, 1080))))
        ]}]
    })
    .to_string();
    let (body, reports) = body(&raw);
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
    assert_eq!(sent_size(&content[1]), (1456, 819));
    assert_eq!(reports[0]["placement"], "user");
    assert_eq!(reports[0]["sent_media_type"], "image/jpeg");
}

#[test]
fn a_gif_that_fits_is_sent_as_it_is_and_one_over_the_tier_becomes_png() {
    let small = encode_base64(&gif(&screenshot_like(60, 40)));
    let (body_small, reports) = body(&tool_request(
        "claude-opus-5-5",
        json!([image("image/gif", &small)]),
    ));
    assert_eq!(
        body_small["messages"][2]["content"][0]["content"][0]["source"]["data"],
        small
    );
    assert_eq!(reports[0]["outcome"], "sent");

    let large = encode_base64(&gif(&screenshot_like(1700, 100)));
    let (body_large, reports) = body(&tool_request(
        "claude-haiku-4-5",
        json!([image("image/gif", &large)]),
    ));
    let sent = &body_large["messages"][2]["content"][0]["content"][0];
    assert_eq!(sent["source"]["media_type"], "image/png");
    assert_eq!(sent_size(sent), (1568, 92));
    assert_eq!(reports[0]["sent_media_type"], "image/png");
}

// ── S5: a model without vision ───────────────────────────────────────────────

#[test]
fn a_model_without_vision_gets_one_line_per_image() {
    let bytes = png(&screenshot_like(30, 20));
    let raw = tool_request(
        "claude-future-text-only",
        json!([{"type": "text", "text": "attached a.png"}, image("image/png", &encode_base64(&bytes))]),
    );
    let mut request: MurmurRequest = serde_json::from_str(&raw).unwrap();
    let reports = prepare_images(&mut request, ImageTier::None);
    let reason =
        "model 'claude-future-text-only' does not take image input in driver-anthropic's model table";
    let line = not_shown_line("image/png", bytes.len() as u64, &sha256_hex(&bytes), reason);
    assert!(matches!(
        &request.messages[2].content[1],
        MurmurContentBlock::Text { text } if *text == line
    ));
    let report = reports[0].to_json();
    assert_eq!(report["outcome"], "replaced");
    assert_eq!(report["reason"], reason);
    assert!(
        report.get("width").is_none(),
        "nothing past the bytes is read"
    );

    // The line joins the tool's text in the call's one tool_result.
    let body = translate_murmur_request_to_anthropic(
        &request,
        ModelFamily::Claude4Plus,
        &ThinkingConfig::disabled(),
        &PromptCacheConfig::default(),
    )
    .unwrap();
    let results = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one tool_result per tool_use_id");
    assert_eq!(
        results[0]["content"],
        json!([{"type": "text", "text": "attached a.png"}, {"type": "text", "text": line}])
    );
}

// ── S7: unusable images ──────────────────────────────────────────────────────

#[test]
fn an_unusable_image_becomes_a_line_and_the_request_is_still_built() {
    let garbage = encode_base64(b"hi there");
    let huge = encode_base64(&png_header_only(9000, 100));
    let raw = tool_request(
        "claude-sonnet-5-5",
        json!([
            image("image/png", &garbage),
            image("image/png", &huge),
            image("image/png", "@@not base64@@")
        ]),
    );
    let (body, reports) = body(&raw);
    let results = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one tool_result per tool_use_id");
    let content = results[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3, "one line per image");
    let reasons: Vec<&str> = reports
        .iter()
        .map(|r| r["reason"].as_str().unwrap())
        .collect();
    assert!(
        reasons[0].starts_with("the image's dimensions could not be read: "),
        "{}",
        reasons[0]
    );
    assert!(
        reasons[1].starts_with("the image is 9000x100 px, over the driver's decode ceiling"),
        "{}",
        reasons[1]
    );
    assert!(
        reasons[2].starts_with("the image data is not valid base64: "),
        "{}",
        reasons[2]
    );
    for (block, report) in content.iter().zip(&reports) {
        let text = block["text"].as_str().unwrap();
        assert!(text.starts_with("[image not shown: image/png, "), "{text}");
        assert!(text.ends_with(&format!("; {}]", report["reason"].as_str().unwrap())));
        assert_eq!(report["outcome"], "replaced");
    }
    assert_eq!(reports[1]["width"], 9000);
}

#[test]
fn an_image_over_anthropics_per_image_limit_becomes_a_line() {
    // Noise does not compress: a 2352x1568 RGB PNG of it fits the high-resolution tier, so it is
    // kept as it is, and its base64 is over 10,000,000 bytes.
    let mut state = 0x2545_f491_u32;
    let noise = image::RgbImage::from_fn(2352, 1568, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let [r, g, b, _] = state.to_le_bytes();
        image::Rgb([r, g, b])
    });
    let data = encode_base64(&png(&noise));
    assert!(data.len() > MAX_IMAGE_BASE64_BYTES);
    let (body, reports) = body(&tool_request(
        "claude-opus-5-5",
        json!([image("image/png", &data)]),
    ));
    assert_eq!(
        reports[0]["reason"],
        format!(
            "the image is {} bytes as base64, over Anthropic's per-image limit of 10000000 bytes",
            data.len()
        )
    );
    assert_eq!(reports[0]["width"], 2352);
    let text = body["messages"][2]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("[image not shown: image/png, "));
}

#[test]
fn a_source_that_is_not_base64_passes_through_unmeasured() {
    let raw = tool_request(
        "claude-sonnet-5-5",
        json!([{"type": "image", "source": {"type": "url", "media_type": "image/png", "data": "https://example.test/a.png"}}]),
    );
    let (body, reports) = body(&raw);
    assert_eq!(
        body["messages"][2]["content"][0]["content"][0]["source"],
        json!({"type": "url", "media_type": "image/png", "data": "https://example.test/a.png"})
    );
    assert_eq!(reports[0]["outcome"], "sent");
    assert!(reports[0].get("width").is_none());
    assert!(reports[0].get("scale").is_none());
    assert_eq!(reports[0]["placement"], "tool_result");
}

#[test]
fn murmurs_aged_and_unavailable_lines_are_never_touched() {
    let aged = "[image not shown: image/png, 5120 bytes, sha256 ab; the 5 newer images in this conversation are shown instead]";
    let missing = "[image unavailable: image/jpeg, 900 bytes, sha256 cd]";
    let raw = tool_request(
        "claude-sonnet-5-5",
        json!([{"type": "text", "text": aged}, {"type": "text", "text": missing}, image("image/png", &png_base64(8, 8))]),
    );
    let (body, reports) = body(&raw);
    let results = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one tool_result per tool_use_id");
    let content = results[0]["content"].as_array().unwrap();
    assert_eq!(content[0], json!({"type": "text", "text": aged}));
    assert_eq!(content[1], json!({"type": "text", "text": missing}));
    assert_eq!(content[2]["type"], "image");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["block"], 2);
}

#[test]
fn a_tool_message_whose_images_all_aged_out_answers_with_one_tool_result() {
    // No image block is left, only murmur's line beside the tool's text: still one tool_result,
    // which is the only shape Anthropic accepts for one tool_use_id.
    let aged = "[image not shown: image/png, 5120 bytes, sha256 ab; the 5 newer images in this conversation are shown instead]";
    let raw = tool_request(
        "claude-sonnet-5-5",
        json!([{"type": "text", "text": "attached a.png"}, {"type": "text", "text": aged}]),
    );
    let (body, reports) = body(&raw);
    assert!(reports.is_empty(), "no image block, no report");
    assert_eq!(
        body["messages"][2]["content"],
        json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": [
                {"type": "text", "text": "attached a.png"},
                {"type": "text", "text": aged}
            ],
            "cache_control": {"type": "ephemeral"}
        }])
    );
}

// ── S8: determinism ──────────────────────────────────────────────────────────

#[test]
fn the_same_request_gives_the_same_bytes() {
    let raw = tool_request(
        "claude-haiku-4-5",
        json!([{"type": "text", "text": "x"}, image("image/png", &png_base64(1700, 1100))]),
    );
    let first = serde_json::to_vec(&build_provider_body(&raw, None, None).unwrap().0).unwrap();
    let second = serde_json::to_vec(&build_provider_body(&raw, None, None).unwrap().0).unwrap();
    assert_eq!(sha256_hex(&first), sha256_hex(&second));
}

// ── The report ───────────────────────────────────────────────────────────────

#[test]
fn the_report_is_added_only_when_the_request_held_an_image() {
    let raw = tool_request(
        "claude-haiku-4-5",
        json!([image("image/png", &png_base64(8, 8))]),
    );
    let (_, reports) = build_provider_body(&raw, None, None).unwrap();
    let mut response = json!({"stop_reason": "end_turn", "content": []});
    attach_image_report(&mut response, &reports);
    assert_eq!(response["images"][0]["outcome"], "sent");
    assert_eq!(response["images"][0]["sent_width"], 8);

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
fn readme_images_table_matches_the_tier_table() {
    let documented: Vec<(String, String, String, String)> = readme_images()
        .lines()
        .filter_map(|row| {
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            let [_, id, reads, edge, tokens, ..] = cells[..] else {
                return None;
            };
            let id = id.strip_prefix("`claude-")?.strip_suffix('`')?;
            Some((
                format!("claude-{id}"),
                reads.into(),
                edge.into(),
                tokens.into(),
            ))
        })
        .collect();
    let expected: Vec<(String, String, String, String)> = MODEL_IMAGE_TIERS
        .iter()
        .map(|(id, tier)| match tier.limits() {
            Some((edge, tokens)) => (
                id.to_string(),
                "yes".into(),
                edge.to_string(),
                tokens.to_string(),
            ),
            None => (id.to_string(), "no".into(), "–".into(), "–".into()),
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
        format!("{MAX_IMAGE_BASE64_BYTES} bytes of base64"),
        format!("quality {JPEG_QUALITY}"),
        RESIZE_FILTER.to_string(),
        "deflate level 1".to_string(),
        "[image not shown: <media_type>, <n> bytes, sha256 <hex>; <reason>]".to_string(),
        "`model '<model>' does not take image input in driver-anthropic's model table`".to_string(),
        "`the image data is not valid base64: <detail>`".to_string(),
        "`the image's dimensions could not be read: <detail>`".to_string(),
        format!(
            "`the image is <w>x<h> px, over the driver's decode ceiling of {MAX_DECODE_EDGE} px a side and {MAX_DECODE_PIXELS} pixels`"
        ),
        "`the image could not be decoded: <detail>`".to_string(),
        "`the image could not be re-encoded after resizing: <detail>`".to_string(),
        format!(
            "`the image is <n> bytes as base64, over Anthropic's per-image limit of {MAX_IMAGE_BASE64_BYTES} bytes`"
        ),
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
        "tool_result",
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
