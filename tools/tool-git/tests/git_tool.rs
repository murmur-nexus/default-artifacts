//! The tool's behaviour suite (`tests/cases/git_tool.rs`) through the compiled component in Wasmtime.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Component;

include!("cases/git_tool.rs");
