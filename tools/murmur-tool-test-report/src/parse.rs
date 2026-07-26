//! The test-runner output parsers used by this tool now live in the shared
//! `murmur-test-parse` crate, so the WASM `murmur-hook-regression-verifier` can
//! reuse the exact same parsing logic without pulling in this tool's native-only
//! `rusqlite` dependency (which does not cross-compile to `wasm32-wasip2`).
//!
//! This module is a thin re-export that keeps every existing `crate::parse::…`
//! path (in `ops.rs` and `resolve.rs`) working unchanged. There is deliberately
//! no parsing code here — every parser is defined exactly once, in
//! `libs/murmur-test-parse/src/lib.rs`.

pub use murmur_test_parse::{
    detect_format, parse_cargo, parse_go, parse_jest, parse_pytest, Failure,
};
