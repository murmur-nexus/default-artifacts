//! Times the image pre-pass a driver runs on one 3840×2160 screenshot-like PNG.
//!
//! Each case runs [`prepare_image`] whole — base64 decode, sha256, header read and, when the rule
//! asks for it, decode, resize, re-encode and base64 encode — the given number of times, and
//! prints one JSON line with every run's wall time in milliseconds. tests/wasm_timing.rs runs it
//! as a wasm32-wasip2 command under wasmtime and as a host binary.
//!
//! Usage: timing [<runs>]   (default 7)

use murmur_driver_image::{encode_base64, fixtures, prepare_image, Action, Rule, SizeRule};
use serde_json::json;
use std::time::Instant;

// The two drivers' predicates, restated: the crate itself holds no provider rule.
fn anthropic(max_edge: u32, max_tokens: u32) -> impl Fn(u32, u32) -> bool {
    move |w, h| {
        w.div_ceil(28) * 28 <= max_edge
            && h.div_ceil(28) * 28 <= max_edge
            && w.div_ceil(28) * h.div_ceil(28) <= max_tokens
    }
}

fn openai(max_dimension: u32, patch_budget: u32) -> impl Fn(u32, u32) -> bool {
    move |w, h| {
        w <= max_dimension && h <= max_dimension && w.div_ceil(32) * h.div_ceil(32) <= patch_budget
    }
}

type Fits<'a> = &'a dyn Fn(u32, u32) -> bool;

fn main() {
    let runs: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(7);
    let png = fixtures::png(&fixtures::screenshot_like(3840, 2160));
    let data = encode_base64(&png);

    let high = anthropic(2576, 4784);
    let standard = anthropic(1568, 1568);
    let patches_2500 = openai(2048, 2500);
    let patches_30000 = openai(65535, 30000);
    let cases: [(&str, Fits, (u32, u32)); 4] = [
        ("anthropic-high-resolution", &high, (2576, 1449)),
        ("anthropic-standard", &standard, (1456, 819)),
        ("openai-2048px-2500-patches", &patches_2500, (2048, 1152)),
        ("header-only-fits", &patches_30000, (3840, 2160)),
    ];

    for (case, fits, expected) in cases {
        let rule = Rule::Sized(SizeRule {
            fits,
            always_reencode: &[],
            max_base64_len: None,
        });
        let mut runs_ms = Vec::with_capacity(runs);
        let mut sent_bytes = 0;
        for _ in 0..runs {
            let started = Instant::now();
            let prepared = prepare_image(0, 0, "image/png", &data, "user", &rule);
            runs_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let sent = prepared.report.sent.as_ref().expect("the image is sent");
            assert_eq!(sent.size, Some(expected), "{case}");
            sent_bytes = match &prepared.action {
                Action::Replace { data, .. } => data.len() * 3 / 4,
                _ => png.len(),
            };
        }
        println!(
            "{}",
            json!({
                "case": case,
                "input": "3840x2160",
                "input_png_bytes": png.len(),
                "sent": format!("{}x{}", expected.0, expected.1),
                "sent_bytes": sent_bytes,
                "runs_ms": runs_ms,
            })
        );
    }
}
