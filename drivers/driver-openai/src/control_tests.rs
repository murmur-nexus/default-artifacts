//! Golden provider bodies for requests that hold no image.
//!
//! Each `tests/fixtures/control_<name>.request.json` holds a murmur request plus the driver config
//! it runs under; `control_<name>.body.json` holds the exact bytes the driver put on the wire for
//! it before image support existed. A request with no image must keep producing those bytes.
//! `CONTROL_FIXTURES_WRITE=1` rewrites the body files from the current code, which is only correct
//! when that code is known to build the pre-image bodies.

use super::{build_request_body, parse_driver_config, store_opt_in, MurmurRequest};
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn provider_body(control: &Value) -> Vec<u8> {
    let raw = control["request"].to_string();
    // A null or absent config stands for the env var being unset.
    let driver_config = control
        .get("driver_config")
        .filter(|value| !value.is_null())
        .map(Value::to_string);

    let config = parse_driver_config(driver_config.as_deref()).unwrap();
    let store = store_opt_in(driver_config.as_deref());
    let request: MurmurRequest = serde_json::from_str(&raw).unwrap();
    let (_, body, images) = build_request_body(request, &config, store).unwrap();
    assert!(images.is_empty(), "a control request holds no image");
    serde_json::to_vec(&body).unwrap()
}

fn assert_control(name: &str) {
    let control: Value = serde_json::from_slice(
        &std::fs::read(fixture(&format!("control_{name}.request.json"))).unwrap(),
    )
    .unwrap();
    let body = provider_body(&control);
    let golden = fixture(&format!("control_{name}.body.json"));
    if std::env::var_os("CONTROL_FIXTURES_WRITE").is_some() {
        std::fs::write(&golden, &body).unwrap();
        return;
    }
    let expected = std::fs::read(&golden).unwrap();
    assert!(
        body == expected,
        "control_{name}: provider body differs from the golden bytes\n  got:      {}\n  expected: {}",
        String::from_utf8_lossy(&body),
        String::from_utf8_lossy(&expected)
    );
}

#[test]
fn control_chat_tool_round_trip() {
    assert_control("chat_tool_round_trip");
}

#[test]
fn control_chat_user_no_image() {
    assert_control("chat_user_no_image");
}

#[test]
fn control_responses_store_off() {
    assert_control("responses_store_off");
}

#[test]
fn control_responses_continuation() {
    assert_control("responses_continuation");
}

#[test]
fn control_responses_user_no_image() {
    assert_control("responses_user_no_image");
}
