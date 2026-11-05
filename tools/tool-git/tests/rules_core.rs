//! The rules suite (`tests/cases/rules.rs`) through the portable core on the host, asserting
//! each `error_kind` too.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Core;

include!("cases/rules.rs");
