//! A capsule's own conclusion, written where a consumer can find it, packaged as a
//! `wasm32-wasip2` component exporting `murmur:tool/run` (world `tool`) and importing no
//! `murmur:*` interface.
//!
//! Two operations and no third: `report` files a terminal verdict, `progress` records that
//! work is advancing without concluding anything. Both write one fixed file,
//! `state/report.json`, inside the durable state store the capsule opens with
//! `capabilities.state`. Nothing else is written, no other file is read, and the trace is
//! untouched — the consumer reads the file.
//!
//! The file exists to make three states distinguishable: absent (the capsule never
//! reported), present with `concluded: false` (it filed progress notes and never
//! concluded), and present with `concluded: true` (its verdict is in `outcome`). Neither
//! of the first two may ever be read as success; coercing them would reintroduce the
//! `exit 0` mistake one layer up.
//!
//! `outcome` is a closed set and this crate branches on none of its members: what an
//! outcome *means*, and what to do about it, is entirely the consumer's configuration.
//!
//! Everything below this file is free of `cfg(target_arch)` so `cargo test` exercises it
//! natively. The state directory, the operator configuration and the run identity all
//! arrive as parameters, so nothing but the adapter here knows the guest path or reads the
//! process environment.

pub mod config;
pub mod ops;
pub mod report;
pub mod store;

/// The JSON type of a value, named as an operator or an agent would recognise it. Every
/// "must be a string, got …" message in the crate reads from this one list, so a caller
/// reading a config refusal and one reading a call refusal see the same words.
pub(crate) fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Guest path of the durable-state directory the report lives in, granted by the capsule's
/// `capabilities.state`. This is the only place the path is written down; every module
/// below takes the directory as a parameter.
///
/// It is never created. Without the grant this relative path resolves inside the workdir
/// preopen instead, and a report there would be one the agent can rewrite at will — so a
/// missing directory is reported as `state_unavailable`, not repaired.
pub const STATE_DIR: &str = "state";

/// Guest environment variable the runtime delivers this artifact's `config:` block in,
/// compact JSON, read off this artifact's own grant and no other's.
///
/// An artifact entry with no `config:` key gets no variable at all, which this tool reads
/// as the permissive default: any non-empty `kind` and any non-empty `stage` is accepted,
/// so adoption is incremental.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Guest environment variable carrying the id of the session that is running.
///
/// Best-effort: the durable state store is keyed by capsule and outlives every session, so
/// stamping this into the report is what lets a consumer tell this run's conclusion from a
/// stale one — but an absent variable is recorded as `""` and never fails a call.
pub const SESSION_ID_ENV: &str = "MURMUR_SESSION_ID";

/// Guest environment variable carrying the name of the capsule that is running. Read on
/// the same best-effort terms as [`SESSION_ID_ENV`].
pub const CAPSULE_NAME_ENV: &str = "MURMUR_CAPSULE_NAME";

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
    use crate::report::RunIdentity;

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The only environment read in the crate. Everything below this module takes
            // the configuration and the run identity as parameters, so the host tests
            // supply them directly.
            let config_json = std::env::var(crate::ARTIFACT_CONFIG_ENV).ok();
            let identity = RunIdentity::new(
                std::env::var(crate::SESSION_ID_ENV).ok(),
                std::env::var(crate::CAPSULE_NAME_ENV).ok(),
            );
            let response = ops::run(
                Path::new(crate::STATE_DIR),
                config_json.as_deref(),
                &identity,
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
