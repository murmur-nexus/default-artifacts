//! Which browser a call may start: the entry's `config:`, and the capsule's
//! `capabilities.shell.allow` grant as murmur stages it in the session's runtime directory.

use std::path::Path;

use serde_json::Value;

/// The names a capsule may grant, in the order they are tried.
pub const BROWSER_NAMES: [&str; 4] = ["chromium", "chromium-browser", "google-chrome", "chrome"];

pub const NO_GRANT_LINE: &str = "tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow";

/// The answer when the runtime directory is not preopened, which murmur does for an entry that
/// declares `capabilities.filesystem.scope`.
pub const SCOPE_LINE: &str = "tool-browser cannot see which programs capabilities.shell.allow grants, because its entry declares capabilities.filesystem.scope; tool-browser runs only from an entry without one";

/// The entry's `config:` as `MURMUR_ARTIFACT_CONFIG` carries it. Absent, empty, `null` and `{}`
/// pass; every key is refused.
pub fn check_config(raw: Option<&str>) -> Result<(), String> {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| format!("tool-browser: config: is not valid JSON: {e}"))?;
    match value {
        Value::Null => Ok(()),
        Value::Object(obj) if obj.is_empty() => Ok(()),
        Value::Object(obj) => Err(format!(
            "tool-browser: config: does not take {}; tool-browser takes no config: keys",
            obj.keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => Err(
            "tool-browser: config: must be a mapping; tool-browser takes no config: keys".into(),
        ),
    }
}

/// The browser names the capsule grants: those with a staged `tools/<name>/murmur.yaml` under
/// `runtime_dir`, which murmur writes for every `capabilities.shell.allow` entry.
pub fn granted_names(runtime_dir: &Path) -> Vec<&'static str> {
    BROWSER_NAMES
        .into_iter()
        .filter(|name| {
            runtime_dir
                .join("tools")
                .join(name)
                .join("murmur.yaml")
                .symlink_metadata()
                .is_ok_and(|m| m.is_file())
        })
        .collect()
}

pub fn not_installed_line(granted: &[&str]) -> String {
    format!(
        "tool-browser: capabilities.shell.allow grants {}, but no {} is on PATH; install Chrome or Chromium on this host first (mur deploy run does not upload host programs)",
        crate::util::join_words(granted, "and"),
        crate::util::join_words(granted, "or"),
    )
}

pub use murmur_process_host::is_not_found;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_takes_no_keys() {
        for ok in [None, Some(""), Some("{}"), Some("null"), Some(" ")] {
            assert_eq!(check_config(ok), Ok(()), "{ok:?}");
        }
        assert_eq!(
            check_config(Some(r#"{"endpoint":"ws://h:9222/devtools/browser/x"}"#)).unwrap_err(),
            "tool-browser: config: does not take endpoint; tool-browser takes no config: keys"
        );
        let e = check_config(Some(r#"{"endpoint":"x","port":1}"#)).unwrap_err();
        assert!(e.contains("endpoint, port"), "{e}");
        assert!(!e.contains("remov") && !e.contains("deprecat"));
        assert!(check_config(Some("[1]")).unwrap_err().contains("mapping"));
        assert!(check_config(Some("{"))
            .unwrap_err()
            .contains("not valid JSON"));
    }

    #[test]
    fn grants_are_staged_manifests() {
        let dir = tempfile::tempdir().unwrap();
        assert!(granted_names(dir.path()).is_empty());
        std::fs::create_dir_all(dir.path().join("tools/google-chrome")).unwrap();
        std::fs::write(dir.path().join("tools/google-chrome/murmur.yaml"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("tools/chromium/murmur.yaml")).unwrap();
        std::fs::create_dir_all(dir.path().join("tools/chrome")).unwrap();
        std::fs::write(dir.path().join("tools/chrome/murmur.yaml"), "x").unwrap();
        assert_eq!(granted_names(dir.path()), vec!["google-chrome", "chrome"]);
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
}
