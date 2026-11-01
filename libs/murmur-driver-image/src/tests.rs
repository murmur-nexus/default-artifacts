use super::fixtures::{gif, jpeg, png, png_header_only, screenshot_like, webp};
use super::*;
use serde_json::json;

// Anthropic's standard-tier predicate, used here only as a realistic `fits`.
fn standard(w: u32, h: u32) -> bool {
    w.div_ceil(28) * 28 <= 1568
        && h.div_ceil(28) * 28 <= 1568
        && w.div_ceil(28) * h.div_ceil(28) <= 1568
}

fn edge_1000(w: u32, h: u32) -> bool {
    w <= 1000 && h <= 1000
}

fn sized<'a>(fits: &'a dyn Fn(u32, u32) -> bool) -> Rule<'a> {
    Rule::Sized(SizeRule {
        fits,
        always_reencode: &[],
        max_base64_len: None,
    })
}

#[test]
fn largest_fit_returns_an_image_that_already_fits_unchanged() {
    assert_eq!(largest_fit(1000, 1000, standard), (1000, 1000));
    assert_eq!(largest_fit(200, 200, standard), (200, 200));
}

#[test]
fn largest_fit_matches_anthropics_reference_search() {
    assert_eq!(largest_fit(1920, 1080, standard), (1456, 819));
    assert_eq!(largest_fit(1075, 1520, standard), (924, 1307));
    // The vision page's cost table lists 1269x952 for this size; its own reference implementation
    // returns 1270x952, and the search follows the implementation.
    assert_eq!(largest_fit(2000, 1500, standard), (1270, 952));
    assert_eq!(largest_fit(3840, 2160, standard), (1456, 819));
}

#[test]
fn largest_fit_swaps_axes_for_a_tall_image_and_never_enlarges() {
    let (w, h) = largest_fit(1080, 1920, standard);
    assert_eq!((w, h), (819, 1456));
    assert_eq!(largest_fit(3000, 10, edge_1000), (1000, 3));
    assert_eq!(largest_fit(10, 3000, edge_1000), (3, 1000));
    // An extreme aspect ratio keeps its short edge at 1.
    assert_eq!(largest_fit(5000, 1, edge_1000), (1000, 1));
    for (w, h) in [(1, 1), (999, 1000), (1000, 1000)] {
        assert_eq!(largest_fit(w, h, edge_1000), (w, h));
    }
}

#[test]
fn dimensions_read_every_supported_format_from_its_header() {
    let image = screenshot_like(64, 40);
    assert_eq!(dimensions("image/png", &png(&image)), Ok((64, 40)));
    assert_eq!(dimensions("image/jpeg", &jpeg(&image)), Ok((64, 40)));
    assert_eq!(dimensions("image/gif", &gif(&image)), Ok((64, 40)));
    assert_eq!(dimensions("image/webp", &webp(&image)), Ok((64, 40)));
    assert_eq!(
        dimensions("image/png", &png_header_only(9000, 7000)),
        Ok((9000, 7000))
    );
}

#[test]
fn dimensions_refuse_an_unsupported_type_and_garbage() {
    assert_eq!(
        dimensions("image/bmp", b"BM"),
        Err("unsupported media type 'image/bmp'".to_string())
    );
    assert!(dimensions("image/png", b"not a png").is_err());
    // A JPEG labelled PNG is read as a PNG and refused.
    assert!(dimensions("image/png", &jpeg(&screenshot_like(8, 8))).is_err());
}

#[test]
fn decode_ceiling_holds_both_the_edge_and_the_pixel_count() {
    assert!(within_decode_ceiling(8000, 5000));
    assert!(!within_decode_ceiling(8001, 10));
    assert!(!within_decode_ceiling(10, 8001));
    assert!(!within_decode_ceiling(7000, 6000));
}

#[test]
fn resize_and_encode_produces_the_exact_size_in_the_reencoded_type() {
    let image = screenshot_like(300, 200);
    for (media_type, bytes, sent) in [
        ("image/png", png(&image), "image/png"),
        ("image/jpeg", jpeg(&image), "image/jpeg"),
        ("image/gif", gif(&image), "image/png"),
        ("image/webp", webp(&image), "image/png"),
    ] {
        assert_eq!(reencoded_media_type(media_type), sent);
        let out = resize_and_encode(media_type, &bytes, (150, 100)).unwrap();
        assert_eq!(dimensions(sent, &out), Ok((150, 100)), "{media_type}");
    }
    // A same-size re-encode (a GIF converted to PNG) keeps every pixel.
    let out = resize_and_encode("image/gif", &gif(&image), (300, 200)).unwrap();
    let decoded = image::load_from_memory_with_format(&out, ImageFormat::Png)
        .unwrap()
        .into_rgb8();
    assert_eq!(decoded, image);
}

#[test]
fn resize_keeps_a_flat_colour_flat() {
    let flat = image::RgbImage::from_pixel(97, 61, image::Rgb([200, 30, 90]));
    let out = resize_and_encode("image/png", &png(&flat), (40, 25)).unwrap();
    let decoded = image::load_from_memory(&out).unwrap().into_rgb8();
    assert!(decoded.pixels().all(|p| p.0 == [200, 30, 90]));
}

#[test]
fn resize_is_deterministic() {
    let bytes = png(&screenshot_like(640, 400));
    let first = resize_and_encode("image/png", &bytes, (333, 208)).unwrap();
    let second = resize_and_encode("image/png", &bytes, (333, 208)).unwrap();
    assert_eq!(sha256_hex(&first), sha256_hex(&second));
}

#[test]
fn resize_refuses_bytes_that_do_not_decode() {
    let err = resize_and_encode("image/png", &png_header_only(20, 20), (10, 10)).unwrap_err();
    assert!(err.starts_with("could not be decoded: "), "{err}");
}

#[test]
fn not_shown_line_has_murmurs_shape() {
    assert_eq!(
        not_shown_line("image/png", 12, "ab", "a reason"),
        "[image not shown: image/png, 12 bytes, sha256 ab; a reason]"
    );
}

#[test]
fn prepare_keeps_an_image_that_fits_byte_for_byte() {
    let bytes = png(&screenshot_like(100, 50));
    let data = encode_base64(&bytes);
    let prepared = prepare_image(2, 1, "image/png", &data, "tool_result", &sized(&edge_1000));
    assert_eq!(prepared.action, Action::Keep);
    assert_eq!(
        prepared.report.to_json(),
        json!({
            "message": 2, "block": 1, "media_type": "image/png",
            "bytes": bytes.len(), "sha256": sha256_hex(&bytes), "outcome": "sent",
            "width": 100, "height": 50,
            "sent_media_type": "image/png", "sent_width": 100, "sent_height": 50,
            "scale": 1.0, "placement": "tool_result",
        })
    );
}

#[test]
fn prepare_resizes_an_image_over_the_rule() {
    let bytes = png(&screenshot_like(3000, 2000));
    let data = encode_base64(&bytes);
    let prepared = prepare_image(0, 0, "image/png", &data, "user", &sized(&edge_1000));
    let Action::Replace { media_type, data } = &prepared.action else {
        panic!("expected a replacement, got {:?}", prepared.action);
    };
    assert_eq!(media_type, "image/png");
    assert_eq!(
        dimensions("image/png", &decode_base64(data).unwrap()),
        Ok((1000, 667))
    );
    let report = prepared.report.to_json();
    assert_eq!(report["outcome"], "resized");
    assert_eq!(report["width"], 3000);
    assert_eq!(report["sent_width"], 1000);
    assert_eq!(report["sent_height"], 667);
    assert_eq!(report["scale"], json!(1000.0 / 3000.0));
    assert!(report.get("reason").is_none());
}

#[test]
fn prepare_reencodes_an_always_converted_type_even_when_it_fits() {
    let data = encode_base64(&gif(&screenshot_like(40, 30)));
    let rule = Rule::Sized(SizeRule {
        fits: &edge_1000,
        always_reencode: &["image/gif"],
        max_base64_len: None,
    });
    let prepared = prepare_image(0, 0, "image/gif", &data, "user", &rule);
    let Action::Replace { media_type, .. } = &prepared.action else {
        panic!("expected a replacement");
    };
    assert_eq!(media_type, "image/png");
    assert_eq!(prepared.report.outcome, Outcome::Resized);
    assert_eq!(prepared.report.to_json()["scale"], json!(1.0));
}

#[test]
fn prepare_replaces_every_unusable_image_with_a_line() {
    let no_vision = Rule::NoVision {
        reason: "no images here".to_string(),
    };
    let fit = sized(&edge_1000);
    let ok = encode_base64(&png(&screenshot_like(10, 10)));
    let cases: Vec<(&str, String, &Rule, &str)> = vec![
        ("image/png", ok.clone(), &no_vision, "no images here"),
        (
            "image/png",
            "%%%".to_string(),
            &fit,
            "the image data is not valid base64: ",
        ),
        (
            "image/png",
            encode_base64(b"garbage"),
            &fit,
            "the image's dimensions could not be read: ",
        ),
        (
            "image/png",
            encode_base64(&png_header_only(9000, 100)),
            &fit,
            "the image is 9000x100 px, over the driver's decode ceiling of 8000 px a side and 40000000 pixels",
        ),
        (
            "image/png",
            encode_base64(&png_header_only(7000, 6000)),
            &fit,
            "the image is 7000x6000 px, over the driver's decode ceiling",
        ),
        (
            "image/png",
            encode_base64(&png_header_only(2000, 2000)),
            &fit,
            "the image could not be decoded: ",
        ),
    ];
    for (media_type, data, rule, reason) in cases {
        let prepared = prepare_image(1, 0, media_type, &data, "user", rule);
        let report = prepared.report.to_json();
        assert_eq!(report["outcome"], "replaced", "{reason}");
        assert!(report.get("placement").is_none());
        assert!(report.get("sent_media_type").is_none());
        let got = report["reason"].as_str().unwrap();
        assert!(
            got.starts_with(reason),
            "{got:?} does not start with {reason:?}"
        );
        let Action::Line(line) = prepared.action else {
            panic!("expected a line");
        };
        assert_eq!(
            line,
            not_shown_line(
                media_type,
                prepared.report.bytes,
                &prepared.report.sha256,
                got
            )
        );
    }
}

#[test]
fn prepare_replaces_an_image_over_the_base64_limit() {
    let data = encode_base64(&png(&screenshot_like(100, 100)));
    let rule = Rule::Sized(SizeRule {
        fits: &edge_1000,
        always_reencode: &[],
        max_base64_len: Some((data.len() - 1, "the test's limit")),
    });
    let prepared = prepare_image(0, 0, "image/png", &data, "user", &rule);
    assert_eq!(prepared.report.outcome, Outcome::Replaced);
    assert_eq!(
        prepared.report.reason.as_deref(),
        Some(
            format!(
                "the image is {} bytes as base64, over the test's limit of {} bytes",
                data.len(),
                data.len() - 1
            )
            .as_str()
        )
    );
    assert_eq!(prepared.report.size, Some((100, 100)));
}

#[test]
fn an_unmeasured_source_is_sent_as_it_is_with_no_size() {
    let url = "https://example.test/a.png";
    let prepared = prepare_unmeasured(0, 3, "image/png", url, "user", &sized(&edge_1000));
    assert_eq!(prepared.action, Action::Keep);
    assert_eq!(
        prepared.report.to_json(),
        json!({
            "message": 0, "block": 3, "media_type": "image/png",
            "bytes": 26, "sha256": sha256_hex(url.as_bytes()),
            "outcome": "sent", "sent_media_type": "image/png", "placement": "user",
        })
    );
}

#[test]
fn an_unmeasured_source_becomes_a_line_for_a_model_without_vision() {
    let url = "https://example.test/a.png";
    let rule = Rule::NoVision {
        reason: "no images here".to_string(),
    };
    let prepared = prepare_unmeasured(0, 0, "image/png", url, "user", &rule);
    assert_eq!(
        prepared.action,
        Action::Line(not_shown_line(
            "image/png",
            26,
            &sha256_hex(url.as_bytes()),
            "no images here"
        ))
    );
    assert_eq!(prepared.report.to_json()["outcome"], "replaced");
}

#[test]
fn reports_json_is_an_array_in_order() {
    let fit = sized(&edge_1000);
    let a = prepare_unmeasured(0, 0, "image/png", "a", "user", &fit).report;
    let b = prepare_unmeasured(1, 0, "image/png", "b", "user", &fit).report;
    let value = reports_json(&[a, b]);
    assert_eq!(value[0]["message"], 0);
    assert_eq!(value[1]["message"], 1);
}
