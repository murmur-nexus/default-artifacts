//! `tool-browser`: a native murmur tool that drives a headless Chrome or Chromium one call at a
//! time.
//!
//! A call process reads the native envelope on stdin, validates the request, and relays it to
//! the session's supervisor (`tool-browser --supervise <state folder>`), starting one on the
//! first call. It prints exactly one JSON object on stdout.

mod buffers;
mod cdp;
mod control;
mod discovery;
mod envelope;
mod page;
mod request;
mod snapshot;
mod state;
mod supervisor;
mod util;
mod ws;

use std::{io::Read, path::PathBuf};

use serde_json::Value;

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() == 3 && args[1] == "--supervise" {
        supervisor::run(PathBuf::from(&args[2]));
    }
    let mut raw = String::new();
    let result = match std::io::stdin().read_to_string(&mut raw) {
        Ok(_) => run_call(&raw),
        Err(e) => envelope::error(&format!("tool-browser: cannot read stdin: {e}")),
    };
    println!(
        "{}",
        serde_json::to_string(&result).unwrap_or_else(|_| {
            r#"{"status":"error","summary":"tool-browser: cannot serialize its result","data":"tool-browser: cannot serialize its result"}"#.into()
        })
    );
}

fn run_call(raw: &str) -> Value {
    let params = match request::params_from_envelope(raw) {
        Ok(p) => p,
        Err(e) => return envelope::error(&e),
    };
    let req = match request::parse_request(&params) {
        Ok(r) => r,
        Err(e) => return envelope::error(&e),
    };
    let endpoint =
        match discovery::parse_config(std::env::var("MURMUR_ARTIFACT_CONFIG").ok().as_deref()) {
            Ok(e) => e,
            Err(e) => return envelope::error(&e),
        };
    let workdir = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            return envelope::error(&format!("tool-browser: cannot read the work folder: {e}"))
        }
    };
    if let (request::Action::Open, Some(url)) = (req.action, req.url.as_deref()) {
        match request::open_target(url, &workdir) {
            Err(e) => return envelope::error(&e),
            Ok(request::OpenTarget::WorkFile(_)) if endpoint.is_some() => {
                return envelope::error(&format!(
                    "open refuses {url}: a remote browser cannot see the work folder; serve the page over http instead"
                ))
            }
            Ok(_) => {}
        }
    }
    let runtime_dir = std::env::var_os("MURMUR_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let session = discovery::session_id(runtime_dir.as_deref());
    let state = state::StateDir::new(&workdir, &session);
    let path_var = std::env::var_os("PATH");
    control::call(
        &state,
        &params,
        || discovery::discover(endpoint, runtime_dir.as_deref(), path_var.as_deref()),
        &workdir,
        runtime_dir.as_deref(),
    )
}
