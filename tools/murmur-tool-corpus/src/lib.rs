//! Append-only record store for Murmur capsules, packaged as a `wasm32-wasip2` component
//! exporting `murmur:tool/run` (world `tool`) and importing no `murmur:*` interface.
//!
//! The store is one JSON-lines file the capsule can trust rather than merely ask an agent
//! to respect. Append is the only write; the store — not the caller — assigns every
//! record's `id`, `created_at` and `schema_version`; deletion does not exist, only a
//! withdrawal record that is itself appended; and no operation returns the whole corpus.
//!
//! Everything below this file is free of `cfg(target_arch)` so `cargo test` exercises it
//! natively, and both the state directory and the operator configuration arrive as
//! parameters, so nothing but the adapter here knows the guest path or reads the process
//! environment. The tool is domain-blind throughout: `type` is an opaque tag, `body` is
//! arbitrary JSON, and every schema comes from the operator's capsule manifest.

pub mod config;
pub mod id;
pub mod ops;
pub mod record;
pub mod schema;
pub mod store;

/// Guest path of the durable-state directory the corpus lives in, granted by the capsule's
/// `capabilities.state`. This is the only place the path is written down; every module
/// below takes the directory as a parameter.
///
/// It is never created. Without the grant this relative path resolves inside the workdir
/// preopen instead, and a corpus there would be one the agent can rewrite at will — so a
/// missing directory is reported as `state_unavailable`, not repaired.
pub const STATE_DIR: &str = "state";

/// Guest environment variable the runtime delivers this artifact's `config:` block in,
/// compact JSON, read off this artifact's own grant and no other's.
///
/// It is runtime-owned: the runtime injects it ahead of the manifest's
/// `capabilities.env.allow` allowlist and the allowlist builder skips the name, so no host
/// value can reach the guest under it and no capability declares it. An artifact entry
/// with no `config:` key gets no variable at all, which is what `config_missing` reports.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::Path;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

    use crate::ops::{self, OpStatus};

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The only environment read in the crate. Everything below this module takes
            // the configuration as a parameter, so the host tests supply it directly.
            let config_json = std::env::var(crate::ARTIFACT_CONFIG_ENV).ok();
            let response = ops::run(
                Path::new(crate::STATE_DIR),
                config_json.as_deref(),
                input.data.as_deref().unwrap_or_default(),
            );

            ToolResult {
                status: match response.status {
                    OpStatus::Passed => Status::Passed,
                    OpStatus::Failed => Status::Failed,
                    OpStatus::Error => Status::Error,
                },
                summary: Some(response.summary),
                data: Some(response.data),
                data_path: None,
                truncated: false,
                metadata: response.metadata,
            }
        }
    }

    export!(Component);
}
