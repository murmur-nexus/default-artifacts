//! Append-only record store for Murmur capsules, packaged as a `wasm32-wasip2` component
//! exporting `murmur:tool/run` (world `tool`) and importing no `murmur:*` interface.
//!
//! The store is one JSON-lines file the capsule can trust rather than merely ask an agent
//! to respect. Append is the only write; the store — not the caller — assigns every
//! record's `id`, `created_at` and `schema_version`; deletion does not exist, only a
//! withdrawal record that is itself appended; and no operation returns the whole corpus.
//!
//! Everything below this file is free of `cfg(target_arch)` so `cargo test` exercises it
//! natively, and file access is rooted at a caller-supplied `&Path` so nothing but the
//! adapter here knows the guest path. The tool is domain-blind throughout: `type` is an
//! opaque tag, `body` is arbitrary JSON, and every schema comes from the operator.

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
            let response = ops::run(
                Path::new(crate::STATE_DIR),
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
