//! `tool-computer`: the capsule's own virtual display, driven one action per call.
//!
//! A call process reads one `{"data": …}` envelope on stdin, checks it, and prints one
//! `{"status","summary","data","attachments"}` object on stdout. The first call that passes
//! every check re-executes this binary as `tool-computer --keep <state folder>`: a detached
//! keeper that starts Xvfb, the window manager and the configured programs, and outlives the
//! call. Later calls connect to that display over loopback TCP with the keeper's cookie.
//!
//! Only Linux runs a display. Every other OS compiles `platform::not_linux_line` alone.

mod platform;

#[cfg(target_os = "linux")]
mod call;
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod display;
#[cfg(target_os = "linux")]
mod exec;
#[cfg(target_os = "linux")]
mod grants;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod keeper;
#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod png;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod xbackend;

use std::io::Read;

/// The virtual display's width in pixels; `provider_tool.display_width` in `murmur.yaml`.
pub const DISPLAY_WIDTH: u32 = 1440;
/// The virtual display's height in pixels; `provider_tool.display_height` in `murmur.yaml`.
pub const DISPLAY_HEIGHT: u32 = 900;

fn main() {
    #[cfg(target_os = "linux")]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() == Some(std::ffi::OsStr::new("--keep")) {
            let Some(state_dir) = args.next() else {
                eprintln!("tool-computer --keep needs the state folder");
                std::process::exit(2);
            };
            std::process::exit(keeper::run(std::path::Path::new(&state_dir)));
        }
    }

    let mut raw = String::new();
    let read = std::io::stdin().read_to_string(&mut raw);

    #[cfg(target_os = "linux")]
    let envelope = match read {
        Ok(_) => call::run(&raw),
        Err(err) => platform::Envelope::error(format!("tool-computer: cannot read stdin: {err}")),
    };
    #[cfg(not(target_os = "linux"))]
    let envelope = {
        let _ = read;
        platform::Envelope::error(platform::not_linux_line().to_string())
    };

    println!("{}", envelope.to_json());
}

#[cfg(test)]
mod tests {
    use super::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

    /// The manifest's `provider_tool:` block is the display this binary starts.
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
