//! Which browser a call may use: the entry's `config:`, the capsule's `shell.allow` grant as
//! murmur stages it, and the program each granted name resolves to.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The names a capsule may grant, in the order they are tried.
pub const BROWSER_NAMES: [&str; 4] = ["chromium", "chromium-browser", "google-chrome", "chrome"];

pub const NO_GRANT_LINE: &str = "tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow";

/// How the browser is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Start `program` on this host; `name` is the granted name it resolved from.
    Local { name: String, program: PathBuf },
    /// Connect to a browser elsewhere.
    Remote { endpoint: String },
}

/// The entry's `config:` as `MURMUR_ARTIFACT_CONFIG` carries it: `None` when unset, else the
/// remote endpoint it names, if any.
pub fn parse_config(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| format!("tool-browser: config: is not valid JSON: {e}"))?;
    let obj = match value {
        Value::Null => return Ok(None),
        Value::Object(obj) => obj,
        _ => {
            return Err("tool-browser: config: must be a mapping; the only key is endpoint".into())
        }
    };
    let unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "endpoint")
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "tool-browser: config: does not take {}; the only key is endpoint",
            unknown.join(", ")
        ));
    }
    match obj.get("endpoint") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(e)) => {
            let lower = e.to_ascii_lowercase();
            if lower.starts_with("ws://") || lower.starts_with("wss://") {
                Ok(Some(e.clone()))
            } else {
                let scheme = e.split_once("://").map_or(e.as_str(), |(s, _)| s);
                Err(format!(
                    "tool-browser: config: endpoint must start with ws:// or wss://, not {}",
                    crate::util::one_line(scheme, 30)
                ))
            }
        }
        Some(_) => Err("tool-browser: config: endpoint must be a string".into()),
    }
}

/// The browser names the capsule grants: those with a staged `tools/<name>/murmur.yaml` in the
/// runtime directory, which murmur writes for every `capabilities.shell.allow` entry.
pub fn granted_names(runtime_dir: Option<&Path>) -> Vec<&'static str> {
    let Some(dir) = runtime_dir else {
        return Vec::new();
    };
    BROWSER_NAMES
        .into_iter()
        .filter(|name| {
            dir.join("tools")
                .join(name)
                .join("murmur.yaml")
                .symlink_metadata()
                .is_ok_and(|m| m.is_file())
        })
        .collect()
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// `name` on `path_var`, the first executable match.
pub fn which(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// The app-bundle binary a name falls back to on macOS when it is not on `PATH`.
fn mac_fallback(name: &str) -> Option<&'static str> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    match name {
        "google-chrome" | "chrome" => {
            Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        }
        "chromium" => Some("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        _ => None,
    }
}

/// Decide how this call reaches a browser, or the one line that says why it cannot.
pub fn discover(
    endpoint: Option<String>,
    runtime_dir: Option<&Path>,
    path_var: Option<&std::ffi::OsStr>,
) -> Result<Launch, String> {
    if let Some(endpoint) = endpoint {
        return Ok(Launch::Remote { endpoint });
    }
    let granted = granted_names(runtime_dir);
    if granted.is_empty() {
        return Err(NO_GRANT_LINE.to_string());
    }
    for name in &granted {
        let program = which(name, path_var).or_else(|| {
            mac_fallback(name)
                .map(PathBuf::from)
                .filter(|p| is_executable_file(p))
        });
        if let Some(program) = program {
            return Ok(Launch::Local {
                name: (*name).to_string(),
                program,
            });
        }
    }
    Err(not_installed_line(&granted))
}

pub fn not_installed_line(granted: &[&str]) -> String {
    format!(
        "tool-browser: capabilities.shell.allow grants {}, but no {} is on PATH; install Chrome or Chromium on this host first (mur deploy run does not upload host programs)",
        crate::util::join_words(granted, "and"),
        crate::util::join_words(granted, "or"),
    )
}

/// The session id: the name of the runtime directory's parent, `~/.murmur/sessions/<id>/runtime`.
pub fn session_id(runtime_dir: Option<&Path>) -> String {
    runtime_dir
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && !n.starts_with('.') && !n.contains(['/', '\\']))
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_accepts_only_a_ws_endpoint() {
        assert_eq!(parse_config(None).unwrap(), None);
        assert_eq!(
            parse_config(Some(r#"{"endpoint":"ws://h:9222/devtools/browser/x"}"#)).unwrap(),
            Some("ws://h:9222/devtools/browser/x".into())
        );
        let e = parse_config(Some(r#"{"endpoint":"http://x","port":1}"#)).unwrap_err();
        assert!(e.contains("port"), "{e}");
        let e = parse_config(Some(r#"{"endpoint":"http://x"}"#)).unwrap_err();
        assert!(e.contains("ws:// or wss://") && e.contains("http"), "{e}");
    }

    #[test]
    fn grants_are_staged_manifests() {
        let dir = tempfile::tempdir().unwrap();
        assert!(granted_names(Some(dir.path())).is_empty());
        std::fs::create_dir_all(dir.path().join("tools/google-chrome")).unwrap();
        std::fs::write(dir.path().join("tools/google-chrome/murmur.yaml"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("tools/chromium/murmur.yaml")).unwrap();
        assert_eq!(granted_names(Some(dir.path())), vec!["google-chrome"]);
        assert!(granted_names(None).is_empty());
    }

    #[test]
    fn missing_program_line_names_every_grant() {
        assert_eq!(
            not_installed_line(&["chromium"]),
            "tool-browser: capabilities.shell.allow grants chromium, but no chromium is on PATH; install Chrome or Chromium on this host first (mur deploy run does not upload host programs)"
        );
        assert!(not_installed_line(&["chromium", "chrome"])
            .contains("grants chromium and chrome, but no chromium or chrome is on PATH"));
    }

    #[test]
    fn session_id_is_the_runtime_dir_parent() {
        assert_eq!(
            session_id(Some(Path::new("/h/.murmur/sessions/ses_1/runtime"))),
            "ses_1"
        );
        assert_eq!(session_id(None), "default");
    }
}
