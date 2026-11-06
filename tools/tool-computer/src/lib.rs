//! `tool-computer`: a WASM murmur tool that operates the capsule's own virtual display, one
//! action per call.
//!
//! The first call starts Xvfb as the session process `display` through `murmur:process`, reads
//! its display number from stdout, and speaks the X protocol over a `connect` to its loopback
//! port; then it starts the window manager as `wm` and each `config.launch` program as
//! `launch-<i>`. Every later call finds `display` by name, reconnects with the cookie in
//! `.tool-computer/<id>/Xauthority`, and does one action. murmur stops every program when the
//! session ends.
//!
//! Everything but [`wasm_tool`] is portable and tested on the host target, against the
//! [`host::Processes`] and [`host::Clock`] traits.

pub mod config;
pub mod exec;
pub mod grants;
pub use murmur_process_host as host;
pub mod input;
pub mod keys;
pub mod png;
pub mod session;
pub mod state;
pub mod x11;
pub mod xbackend;

/// The virtual display's width in pixels; `provider_tool.display_width` in `murmur.yaml`.
pub const DISPLAY_WIDTH: u32 = 1440;
/// The virtual display's height in pixels; `provider_tool.display_height` in `murmur.yaml`.
pub const DISPLAY_HEIGHT: u32 = 900;

/// The environment variable murmur sets to an entry's `config:`, as compact JSON.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";
/// Where murmur preopens the session's runtime directory, read-only.
pub const RUNTIME_PREOPEN: &str = "murmur-runtime";

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::{Path, PathBuf};

    use exports::murmur::tool::run::{Attachment, Guest, Status, ToolInput, ToolResult};
    use murmur::process::process;

    use crate::host::{Conn, Output, ProcError, ProcState, Processes, Started, SystemClock};

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The only environment read in the crate.
            let config = std::env::var(crate::ARTIFACT_CONFIG_ENV).ok();
            let runtime = Path::new(crate::RUNTIME_PREOPEN);
            let clock = SystemClock::default();
            let random = |n: usize| wasip2::random::random::get_random_bytes(n as u64);
            let mut procs = WitProcesses;
            let mut ctx = crate::session::Ctx {
                procs: &mut procs,
                clock: &clock,
                random: &random,
                work: PathBuf::from("."),
                runtime: runtime.is_dir().then(|| runtime.to_path_buf()),
            };
            let reply = crate::session::call(
                input.data.as_deref().unwrap_or_default(),
                config.as_deref(),
                &mut ctx,
            );
            ToolResult {
                status: if reply.passed {
                    Status::Passed
                } else {
                    Status::Error
                },
                summary: Some(reply.summary),
                data: Some(reply.data),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: reply
                    .attachment
                    .map(|path| Attachment {
                        media_type: "image/png".into(),
                        path,
                    })
                    .into_iter()
                    .collect(),
            }
        }
    }

    export!(Component);

    fn proc_error(e: process::ProcessError) -> ProcError {
        match e {
            process::ProcessError::Refused(r) => ProcError::Refused {
                rule: r.rule,
                reason: r.reason,
            },
            process::ProcessError::UnknownHandle => ProcError::UnknownHandle,
            process::ProcessError::NotRunning => ProcError::NotRunning,
            process::ProcessError::TimedOut => ProcError::TimedOut,
            process::ProcessError::Failed(text) => ProcError::Failed(text),
        }
    }

    fn proc_state(s: process::State) -> ProcState {
        match s {
            process::State::Running => ProcState::Running,
            process::State::Exited(code) => ProcState::Exited(code),
        }
    }

    struct WitProcesses;

    impl Processes for WitProcesses {
        fn start(
            &mut self,
            name: &str,
            program: &str,
            args: &[String],
            env: &[(String, String)],
        ) -> Result<Started, ProcError> {
            process::start(&process::StartRequest {
                name: name.to_string(),
                program: program.to_string(),
                args: args.to_vec(),
                env: env.to_vec(),
            })
            .map(|s| Started {
                handle: s.handle,
                already_running: s.already_running,
            })
            .map_err(proc_error)
        }

        fn find(&mut self, name: &str) -> Option<String> {
            process::find(name)
        }

        fn status(&mut self, handle: &str) -> Result<ProcState, ProcError> {
            process::status(handle).map(proc_state).map_err(proc_error)
        }

        fn read(&mut self, handle: &str, max: u32) -> Result<Output, ProcError> {
            process::read(handle, max)
                .map(|o| Output {
                    stdout: o.stdout,
                    stderr: o.stderr,
                })
                .map_err(proc_error)
        }

        fn stop(&mut self, handle: &str) -> Result<ProcState, ProcError> {
            process::stop(handle).map(proc_state).map_err(proc_error)
        }

        fn connect(&mut self, handle: &str, port: u16) -> Result<Box<dyn Conn>, ProcError> {
            process::connect(handle, port)
                .map(|c| Box::new(WitConn(c)) as Box<dyn Conn>)
                .map_err(proc_error)
        }
    }

    struct WitConn(process::Connection);

    impl Conn for WitConn {
        fn read(&mut self, max: u32, timeout_ms: u32) -> Result<Vec<u8>, ProcError> {
            self.0.read(max, timeout_ms).map_err(proc_error)
        }

        fn write(&mut self, bytes: &[u8]) -> Result<(), ProcError> {
            self.0.write(bytes).map_err(proc_error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

    /// The manifest's `provider_tool:` block is the display this tool starts.
    #[test]
    fn manifest_provider_tool_matches_the_display_constants() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/murmur.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        let manifest: serde_norway::Value = serde_norway::from_str(&text).unwrap();
        let block = &manifest["provider_tool"];
        assert_eq!(block["kind"].as_str(), Some("computer"));
        assert_eq!(
            block["display_width"].as_u64(),
            Some(u64::from(DISPLAY_WIDTH))
        );
        assert_eq!(
            block["display_height"].as_u64(),
            Some(u64::from(DISPLAY_HEIGHT))
        );
        let keys: Vec<&str> = block
            .as_mapping()
            .unwrap()
            .keys()
            .map(|key| key.as_str().unwrap())
            .collect();
        assert_eq!(keys, ["kind", "display_width", "display_height"]);
    }
}
