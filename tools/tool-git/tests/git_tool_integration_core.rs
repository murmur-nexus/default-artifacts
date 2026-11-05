//! The original integration suite (`tests/cases/git_tool_integration.rs`) through the portable core on the host.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Core;

include!("cases/git_tool_integration.rs");
