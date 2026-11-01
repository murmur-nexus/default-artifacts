//! On every OS but Linux, each call is refused with one line and nothing starts.

#![cfg(not(target_os = "linux"))]

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn every_call_is_refused_off_linux() {
    let work = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tool-computer"))
        .current_dir(work.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"data":{"action":"screenshot"}}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let line =
        "tool-computer runs only on Linux, in a sealed capsule; on macOS or Windows, run the \
                capsule on a Linux VM";
    assert_eq!(result["status"], "error");
    assert_eq!(result["summary"], line);
    assert_eq!(result["data"], line);
    assert!(!work.path().join(".tool-computer").exists());
}
