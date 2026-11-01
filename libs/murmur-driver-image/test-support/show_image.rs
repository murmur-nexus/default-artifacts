//! `show-image`: a native Murmur tool that attaches one image from the work folder.
//!
//! Test support, compiled with plain `rustc` by the drivers' `mur_run_tool_image` tests and by
//! hand for the by-hand capsule runs; it is never an artifact of this repository. It depends on
//! nothing, so it reads its input with a scan rather than a JSON parser.
//!
//! Input (stdin): the native envelope `{"data": <call>, "log_path": …}`, where `<call>` is the
//! object `{"path": "<work-folder-relative path>", "media_type"?: "<type>"}` or that object
//! JSON-encoded as a string. Neither value may hold a quote or a backslash.
//!
//! Output (stdout): `{"status":"passed","summary":"attached <path>","data":"attached <path>",
//! "attachments":[{"media_type":"<type>","path":"<path>"}]}`, the media type defaulting to
//! `image/png`. murmur reads the file and checks it; this program never opens it.

use std::io::Read;

// The string value of `"key"` in `text`, once escaped quotes are unescaped.
fn string_field(text: &str, key: &str) -> Option<String> {
    let text = text.replace("\\\"", "\"");
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        println!(r#"{{"status":"error","summary":"show-image: could not read stdin"}}"#);
        return;
    }
    let Some(path) = string_field(&input, "path") else {
        println!(r#"{{"status":"error","summary":"show-image: the call names no path"}}"#);
        return;
    };
    let media_type = string_field(&input, "media_type").unwrap_or_else(|| "image/png".to_string());
    println!(
        r#"{{"status":"passed","summary":"attached {path}","data":"attached {path}","attachments":[{{"media_type":"{media_type}","path":"{path}"}}]}}"#
    );
}
