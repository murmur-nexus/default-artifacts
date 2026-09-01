//! Protected-path policy: the matcher `murmur-hook-protect-tool` and
//! `murmur-hook-protect-shell` both decide with.
//!
//! Both artifacts declare `commit_policy: deny`, and the runtime's decision seam
//! is fail-closed and not configurable: a gated call proceeds only on a clean
//! `hook-output::none`, and a trap, a panic, an epoch-deadline expiry, a
//! memory-limit kill or any `Err` all refuse it. A bug in this crate therefore
//! does not degrade a capsule, it stops one. Two consequences run through every
//! module here:
//!
//!  * **No panic on any path reachable from a decision.** No `unwrap`, no
//!    `expect`, no indexing that can go out of range, no slicing that can
//!    straddle a char boundary. The crate-level lint block below is what holds
//!    that, and the test modules are the only places that opt out.
//!  * **No `cfg` gate.** Every item is compiled for the host, so plain
//!    `cargo test` exercises the whole decision path. A policy hook whose logic
//!    lives behind `cfg(target_arch = "wasm32")` reports a green test run having
//!    run nothing.
//!
//! The layout follows the decision, in order:
//!
//! | Module | Holds |
//! |---|---|
//! | [`config`] | the `MURMUR_ARTIFACT_CONFIG` parser and the config types |
//! | [`glob`] | the hand-rolled, non-backtracking glob matcher |
//! | [`path`] | lexical path normalization and the escape rule |
//! | [`tool`] | the tool-input write-target extractor |
//! | [`shell`] | the shell write-form recognizer |
//! | [`decide`] | the two decision functions and the reason strings |
//!
//! The two hooks are independent at runtime: neither requires the other to be
//! installed, and neither reads the other's config — [`config::PolicySide`] is
//! what keeps one side's key an unknown key on the other.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unreachable,
    clippy::todo
)]

pub mod config;
pub mod decide;
pub mod glob;
pub mod path;
pub mod shell;
pub mod tool;

pub use config::{
    parse_config, ConfigError, PolicyConfig, PolicySide, ToolRule, WriteWhen, ARTIFACT_CONFIG_ENV,
    DEFAULT_PROTECT,
};
pub use decide::{
    decide_shell_call, decide_tool_call, judge_path, Decision, PolicyState, Verdict,
    HOOK_PROTECT_SHELL, HOOK_PROTECT_TOOL,
};
pub use glob::{Pattern, PatternError, PatternKind};
pub use path::{normalize, PathTarget};
pub use shell::{shell_write_targets, ShellTarget};
pub use tool::{tool_write_targets, ToolTargets, ToolWriteTarget};
