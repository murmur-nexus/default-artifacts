//! `tool-write`, packaged as a `wasm32-wasip2` component exporting `murmur:tool/run` (world
//! `tool`).
//!
//! The file logic lives in the shared `murmur-file-tools` library (`libs/murmur-file-tools`).
//! This crate only binds its own WIT world, gathers the host roots from its WASI preopens, and
//! maps `murmur_file_tools::dispatch`'s outcome for `Tool::Write` onto the WIT `ToolResult`.

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use exports::murmur::tool::run::{Attachment, Guest, Status, ToolInput, ToolResult};
    use murmur_file_tools::{dispatch, HostRoot, OutcomeStatus, Tool, ToolOutcome};
    use wasip2::filesystem::{preopens, types::DescriptorFlags};

    struct Component;

    /// The host folders murmur opened for this call, in preopen order. A root is writable when
    /// its descriptor's flags hold `mutate-directory`; a flags error counts as read-only. Each
    /// descriptor is dropped here, so none outlives the call.
    fn host_roots() -> Vec<HostRoot> {
        preopens::get_directories()
            .into_iter()
            .filter_map(|(descriptor, name)| {
                let writable = descriptor
                    .get_flags()
                    .is_ok_and(|flags| flags.contains(DescriptorFlags::MUTATE_DIRECTORY));
                HostRoot::from_preopen(&name, writable)
            })
            .collect()
    }

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            let roots = host_roots();
            let ToolOutcome {
                status,
                summary,
                data,
                truncated,
                state_effect,
                attachments,
            } = dispatch(Tool::Write, input.data.as_deref(), &roots);
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
                attachments: attachments
                    .into_iter()
                    .map(|a| Attachment {
                        media_type: a.media_type,
                        path: a.path,
                    })
                    .collect(),
            }
        }
    }

    export!(Component);
}
