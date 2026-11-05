//! The rules suite (`tests/cases/rules.rs`) through the compiled component in Wasmtime:
//! refusals, `failed`, time-outs and output caps around each git command, and the places the
//! component opens files itself — the code-graph database, `clone`'s destination and `log_path`.

#![cfg(unix)]

mod common;

const BACKEND: common::Backend = common::Backend::Component;

include!("cases/rules.rs");
