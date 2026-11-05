//! Where one path input opens: in the work folder, inside a host root, or nowhere.
//!
//! wasi-libc resolves a relative path through the longest preopen name it begins with, so the
//! host root `murmur-host/home/op` is reached by opening `murmur-host/home/op/…`. A path that
//! begins with `murmur-host` but that no held root covers falls through to the work folder, and
//! an absolute path opened without the prefix resolves into the work folder too. Each value is
//! therefore checked against the held roots before anything is opened.

use std::path::Path;

use serde_json::Value;

use crate::{err, err_result, lexical_segments, open_in_host_root};
use crate::{HostRoot, RootAccess, Tool, HOST_ROOT_NAME};

/// Where a path input opens, once [`resolve_path`] accepted it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved<'a> {
    /// The path handed to `std::fs`: a relative value exactly as written, or `murmur-host`
    /// followed by an absolute value's normalised segments. Never shown to the model.
    pub(crate) open: String,
    /// The root covering an absolute value; `None` for a relative one.
    pub(crate) root: Option<&'a HostRoot>,
}

/// Resolve `value`, the `location` input (`path` or `dir`) of a call to `tool`, against the
/// host roots the tool holds.
///
/// A relative value opens as written, unless a root is held and its first normalised segment is
/// `murmur-host`, where wasi-libc would reach a root through it: that is refused
/// `reserved_name`. An absolute value opens through the longest held root covering it, compared
/// component by component after lexical normalisation, and is refused `absolute_path` when none
/// does. Every message names `value` as the caller wrote it.
pub(crate) fn resolve_path<'a>(
    location: &str,
    value: &str,
    tool: Tool,
    roots: &'a [HostRoot],
) -> Result<Resolved<'a>, Value> {
    if !Path::new(value).is_absolute() {
        if let Some(refusal) = reserved_prefix(location, value, roots) {
            return Err(refusal);
        }
        return Ok(Resolved {
            open: value.to_string(),
            root: None,
        });
    }

    match open_in_host_root(value, roots) {
        Some((open, root)) => Ok(Resolved {
            open,
            root: Some(root),
        }),
        None => Err(outside_every_root(location, value, tool, roots)),
    }
}

/// The `reserved_name` refusal for a relative `value` whose first normalised segment is
/// `murmur-host` while at least one root is held, or `None`. A value that climbs above the work
/// folder is left to the filesystem, as before.
fn reserved_prefix(location: &str, value: &str, roots: &[HostRoot]) -> Option<Value> {
    if roots.is_empty() {
        return None;
    }
    let normalized = normalize_dir(value)?;
    let mut segments = normalized.split('/');
    if segments.next() != Some(HOST_ROOT_NAME) {
        return None;
    }
    let suggestion = format!("/{}", segments.collect::<Vec<_>>().join("/"));
    Some(err_result(
        err::RESERVED_NAME,
        format!(
            "'{location}' starts with {HOST_ROOT_NAME}, where this tool's host folders are \
             opened; name the host path absolutely: '{suggestion}'"
        ),
    ))
}

impl RootAccess {
    /// The access as the operator's `host_paths` entry spells it.
    const fn label(&self) -> &'static str {
        match self {
            RootAccess::Read => "read",
            RootAccess::ReadWrite => "read-write",
        }
    }
}

/// The `absolute_path` refusal for an absolute `value` no held root covers.
fn outside_every_root(location: &str, value: &str, tool: Tool, roots: &[HostRoot]) -> Value {
    let name = tool.name();
    let message = if roots.is_empty() {
        format!(
            "'{location}' must be relative to the work folder: this session opens no host \
             folder to {name}; got '{value}'{}",
            file_name_suggestion(value)
        )
    } else {
        let listed = roots
            .iter()
            .map(|root| format!("{} ({})", root.path, root.access.label()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "'{location}' is outside every host folder {name} may open: {listed}; got \
             '{value}'. A relative path opens in the work folder"
        )
    };
    err_result(err::ABSOLUTE_PATH, message)
}

/// The refusal for an absolute `glob`, or `None` when it is relative. A glob is matched against
/// the paths below `dir`, so a host folder is named in `dir`, never in `glob`.
#[cfg(feature = "find")]
pub(crate) fn reject_absolute_glob(glob: &str) -> Option<Value> {
    if !Path::new(glob).is_absolute() {
        return None;
    }
    Some(err_result(
        err::ABSOLUTE_PATH,
        format!(
            "'glob' is matched against paths below 'dir' and must be relative; got \
             '{glob}'{}. Name a host folder in 'dir'",
            file_name_suggestion(glob)
        ),
    ))
}

/// The `permission_denied` refusal for a write or edit of `value` under a `read` root, or
/// `None`. Runs before the file or its parent folders are touched.
#[cfg(any(feature = "write", feature = "edit"))]
pub(crate) fn refuse_read_only(value: &str, resolved: &Resolved<'_>) -> Option<Value> {
    let root = resolved.root?;
    if root.access != RootAccess::Read {
        return None;
    }
    Some(err_result(
        err::PERMISSION_DENIED,
        format!(
            "'path' '{value}' is in {}, which this session opens read-only",
            root.path
        ),
    ))
}

/// ` (did you mean '<final component>'?)`, or nothing when `value` has no final component
/// (`/`, or a value ending in `..`). The final component is the likeliest file the caller
/// meant in the work folder.
fn file_name_suggestion(value: &str) -> String {
    match Path::new(value).file_name().and_then(|n| n.to_str()) {
        Some(name) => format!(" (did you mean '{name}'?)"),
        None => String::new(),
    }
}

/// `dir` with its `.` and empty segments dropped and each `..` applied to the segment
/// before it, `/`-joined. `""` is the workdir root. `None` when a `..` has no segment left
/// to pop, i.e. the value climbs above the workdir.
pub(crate) fn normalize_dir(dir: &str) -> Option<String> {
    let (segments, climbed_above) = lexical_segments(dir);
    (!climbed_above).then(|| segments.join("/"))
}
