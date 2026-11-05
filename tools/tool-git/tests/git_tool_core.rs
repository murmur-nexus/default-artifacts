//! The tool's behaviour suite (`tests/cases/git_tool.rs`) through the portable core on the host, asserting each `error_kind` too.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Core;

include!("cases/git_tool.rs");
