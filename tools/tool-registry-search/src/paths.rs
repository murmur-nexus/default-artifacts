//! Where an `index` file path opens: in the current directory, inside a host root, or nowhere.
//!
//! murmur opens the work folder (or the entry's `capabilities.filesystem.scope`) at `.`, and each
//! host folder the session lends at `murmur-host` followed by its absolute host path — `murmur-host`
//! alone under `mur run --unconfined`. Which root covers an absolute path, and the path that opens
//! it, is `murmur_file_tools::open_in_host_root`, the resolver the file tools use; this module
//! adds only the refusal an index read gives.

use murmur_file_tools::{open_in_host_root, HostRoot};
use serde_json::Value;

use crate::search::{err, err_result};

/// The path to hand `std::fs` for `source`, the `index` value as written.
///
/// A relative value is returned unchanged. An absolute one opens through the root among `roots`
/// that covers it ([`open_in_host_root`]). With no root held, or none covering, it is an
/// `io_error` naming `source`.
pub fn open_path(source: &str, roots: &[HostRoot]) -> Result<String, Value> {
    if !source.starts_with('/') {
        return Ok(source.to_string());
    }
    if let Some((open, _)) = open_in_host_root(source, roots) {
        return Ok(open);
    }

    let message = if roots.is_empty() {
        format!(
            "failed to read {source}: this session opens no host folder to this tool, so an \
             absolute path cannot be read; a relative path opens in the work folder"
        )
    } else {
        let listed = roots
            .iter()
            .map(|root| root.path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "failed to read {source}: it is outside every host folder this session opens to \
             this tool ({listed}); a relative path opens in the work folder"
        )
    };
    Err(err_result(err::IO_ERROR, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host roots the preopens `names` give, as the component lists them.
    fn roots(names: &[&str]) -> Vec<HostRoot> {
        names
            .iter()
            .filter_map(|name| HostRoot::from_preopen(name, false))
            .collect()
    }

    fn refusal(source: &str, held: &[&str]) -> String {
        let out = open_path(source, &roots(held)).unwrap_err();
        assert_eq!(out["error_kind"], err::IO_ERROR, "{out}");
        assert_eq!(out["status"], "error");
        out["summary"].as_str().unwrap().to_string()
    }

    #[test]
    fn relative_values_are_unchanged() {
        for value in [
            "a.json",
            "sub/../a.json",
            "./a.json",
            "murmur-host/x",
            "../up.json",
        ] {
            assert_eq!(open_path(value, &[]).unwrap(), value);
            assert_eq!(open_path(value, &roots(&["murmur-host"])).unwrap(), value);
        }
    }

    #[test]
    fn no_root_refuses_every_absolute_path() {
        assert_eq!(
            refusal("/etc/artifacts-index.json", &[]),
            "failed to read /etc/artifacts-index.json: this session opens no host folder to this \
             tool, so an absolute path cannot be read; a relative path opens in the work folder"
        );
        // A preopen that only looks like a root is not one.
        assert!(
            refusal("/x", &[".", "state", "murmur-runtime", "murmur-hostile"])
                .contains("opens no host folder")
        );
    }

    #[test]
    fn the_whole_host_covers_everything() {
        let held = roots(&["murmur-host"]);
        assert_eq!(
            open_path("/etc/i.json", &held).unwrap(),
            "murmur-host/etc/i.json"
        );
        assert_eq!(open_path("/", &held).unwrap(), "murmur-host");
    }

    #[test]
    fn a_root_covers_whole_components_only() {
        let held = roots(&["murmur-host/a/b"]);
        assert_eq!(open_path("/a/b/c", &held).unwrap(), "murmur-host/a/b/c");
        assert_eq!(open_path("/a/b", &held).unwrap(), "murmur-host/a/b");
        assert_eq!(
            refusal("/a/bc", &["murmur-host/a/b"]),
            "failed to read /a/bc: it is outside every host folder this session opens to this \
             tool (/a/b); a relative path opens in the work folder"
        );
        assert!(refusal("/a", &["murmur-host/a/b"]).contains("(/a/b)"));
    }

    #[test]
    fn the_refusal_lists_every_held_root() {
        let message = refusal(
            "/z/i.json",
            &[".", "murmur-host/a/b", "murmur-host/c/", "state"],
        );
        assert!(message.contains("(/a/b, /c)"), "{message}");
    }

    #[test]
    fn the_longest_root_covers() {
        // Opening is by the normalised path either way; the longest root is what wasi-libc picks.
        let held = roots(&["murmur-host/a", "murmur-host/a/b"]);
        assert_eq!(open_path("/a/b/c", &held).unwrap(), "murmur-host/a/b/c");
        assert_eq!(open_path("/a/x", &held).unwrap(), "murmur-host/a/x");
    }

    #[test]
    fn dot_segments_are_normalised_before_the_check() {
        let held = roots(&["murmur-host/a/b"]);
        assert_eq!(
            open_path("/a/./b//c/../d.json", &held).unwrap(),
            "murmur-host/a/b/d.json"
        );
        assert_eq!(
            open_path("/../../a/b/x", &held).unwrap(),
            "murmur-host/a/b/x"
        );
        // `..` climbs out of the root lexically, so the path is refused, never opened.
        assert!(refusal("/a/b/../../etc/passwd", &["murmur-host/a/b"]).contains("outside"));
        // A root name is normalised the same way.
        assert_eq!(
            open_path("/a/b/x", &roots(&["murmur-host/a/./b/"])).unwrap(),
            "murmur-host/a/b/x"
        );
    }
}
