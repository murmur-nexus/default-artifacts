//! Golden provider bodies for requests that hold no image.
//!
//! Each `tests/fixtures/control_<name>.request.json` holds a murmur request plus the driver and
//! artifact config it runs under; `control_<name>.body.json` holds the exact bytes the driver put
//! on the wire for it before image support existed. A request with no image must keep producing
//! those bytes. `CONTROL_FIXTURES_WRITE=1` rewrites the body files from the current code, which is
//! only correct when that code is known to build the pre-image bodies.

use super::build_provider_body;
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// A config member that is null or absent stands for the env var being unset.
fn config_string(control: &Value, key: &str) -> Option<String> {
    control
        .get(key)
        .filter(|value| !value.is_null())
        .map(Value::to_string)
}

fn provider_body(control: &Value) -> Vec<u8> {
    let raw = control["request"].to_string();
    let driver_config = config_string(control, "driver_config");
    let artifact_config = config_string(control, "artifact_config");
    let (body, images) =
        build_provider_body(&raw, driver_config.as_deref(), artifact_config.as_deref()).unwrap();
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
fn control_cache_on() {
    assert_control("cache_on");
}

#[test]
fn control_cache_off() {
    assert_control("cache_off");
}

#[test]
fn control_thinking_on() {
    assert_control("thinking_on");
}

#[test]
fn control_tool_round_trip() {
    assert_control("tool_round_trip");
}

#[test]
fn control_system_prompt() {
    assert_control("system_prompt");
}
