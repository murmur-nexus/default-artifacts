//! `tool-read`, packaged as a `wasm32-wasip2` component exporting `murmur:tool/run` (world
//! `tool`).
//!
//! The file logic lives in the shared `murmur-file-tools` library (`libs/murmur-file-tools`).
//! This crate only binds its own WIT world and maps `murmur_file_tools::dispatch`'s outcome for
//! `Tool::Read` onto the WIT `ToolResult`.

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use murmur_file_tools::{dispatch, OutcomeStatus, Tool, ToolOutcome};

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            let ToolOutcome {
                status,
                summary,
                data,
                truncated,
                state_effect,
            } = dispatch(Tool::Read, input.data.as_deref());
            ToolResult {
                status: match status {
                    OutcomeStatus::Passed => Status::Passed,
                    OutcomeStatus::Failed => Status::Failed,
                    OutcomeStatus::Error => Status::Error,
                },
                summary,
                data,
                data_path: None,
                truncated,
                metadata: state_effect
                    .map(|effect| vec![("state_effect".to_string(), effect.to_string())])
                    .unwrap_or_default(),
            }
        }
    }

    export!(Component);
}
