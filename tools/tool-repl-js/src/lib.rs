//! tool-repl-js: a code-mode REPL. A `wasm32-wasip2` component built against murmur's
//! `code-mode` world: it exports `murmur:tool/run@0.2.0` and imports `murmur:bus/bus@0.1.0` and
//! `murmur:process/process@0.2.0`.
//!
//! Each call runs its `code` as a global script in one QuickJS context that lasts as long as the
//! component instance, which murmur keeps for the session. The program reaches the capsule's
//! other tools through the bus (`tools`, `call`, `callAll`), allowlisted programs through
//! `murmur:process` `run` (`run`), and the model through `print` and the value of its last
//! expression.
//!
//! Everything but the import adapter is plain Rust over the [`Host`] trait, so it runs and is
//! tested on the host target; `component` (wasm32 only) implements [`Host`] over the imports.

mod engine;
mod globals;
pub mod host;
mod render;

pub use engine::{Engine, Outcome};
pub use host::{
    Attachment, CallError, Envelope, Host, Operation, ProcessError, ResultStatus, RunOutput,
    RunRequest,
};
pub use render::cut;

/// The environment variable murmur sets to the artifact entry's `config:` as compact JSON; an
/// entry without `config:` gets none.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

#[cfg(target_arch = "wasm32")]
mod component;
