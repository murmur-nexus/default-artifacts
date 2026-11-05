//! The `symbol_history` suite (`tests/cases/symbol_history.rs`) through the compiled component in Wasmtime.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Component;

include!("cases/symbol_history.rs");
