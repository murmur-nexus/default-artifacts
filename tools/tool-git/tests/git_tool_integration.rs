//! The original integration suite (`tests/cases/git_tool_integration.rs`) through the compiled component in Wasmtime.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Component;

include!("cases/git_tool_integration.rs");
