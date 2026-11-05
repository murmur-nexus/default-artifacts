//! The file logic of the four flat file tools — `tool-read`, `tool-write`, `tool-edit` and
//! `tool-find` — held once and shared by their components at compile time.
//!
//! Each tool's operation is a module behind a cargo feature of the same name — [`read`],
//! [`write`], [`edit`] and [`find`] — together with the dependencies only that operation uses:
//! `find` brings `regex` and `walkdir`, `read` the image-header parser. Each module exposes two
//! entry points, `run` and `dispatch`, bound to its own handler, so nothing reachable from them
//! chooses among operations at run time. A tool crate enables only its own feature, and its
//! component links only its own operation. The default features enable all four, which is what
//! the tests build.
//!
//! Each tool crate is a thin `wasm32-wasip2` component: it binds its own WIT world, calls its
//! module's `dispatch`, and maps the [`ToolOutcome`] field for field onto the WIT `ToolResult`,
//! its [`attachments`](ToolOutcome::attachments) included. Everything else — the stdin envelope,
//! the field checks, every limit, message and filesystem call — lives here, with no WASM
//! bindings, so it is unit tested on the host with `cargo test`.
//!
//! A component also hands `dispatch` the host folders the runtime opened for it, as
//! [`HostRoot`]s it builds with [`HostRoot::from_preopen`] from its WASI preopens. A relative path
//! opens in the work folder; an absolute one opens only inside one of those roots.
//!
//! The crate root holds the [`Tool`] descriptor, the host roots and the outcome types, and,
//! compiled with no tool feature, the pieces a tool outside this crate reuses: [`envelope`] and
//! [`NativeOutcome`], the native-tool envelope and output mapping, and [`open_in_host_root`],
//! where an absolute path opens. What one
//! call runs through whichever tool it is — the envelope, the outcome mapping, the error kinds,
//! the output constructors and the helpers the tools share — lives in the `call` module, and the
//! resolution of one path input in the `resolve` module. Both are compiled only with at least
//! one tool feature, and a shared item only some tools use only with their features.

// Without a tool feature there is no call to run, so the code every call runs through is
// compiled only with at least one. `call`'s items are used from the root, as the tool modules
// and the tests name them.
#[cfg(any(
    feature = "read",
    feature = "write",
    feature = "edit",
    feature = "find"
))]
mod call;
#[cfg(any(
    feature = "read",
    feature = "write",
    feature = "edit",
    feature = "find"
))]
use call::*;
#[cfg(any(
    feature = "read",
    feature = "write",
    feature = "edit",
    feature = "find"
))]
mod resolve;

#[cfg(feature = "edit")]
pub mod edit;
#[cfg(feature = "find")]
pub mod find;
#[cfg(feature = "read")]
mod image;
#[cfg(feature = "read")]
pub mod read;
#[cfg(test)]
mod test_png;
#[cfg(test)]
mod tests;
#[cfg(feature = "write")]
pub mod write;

use serde_json::{json, Value};

// ── The tools ───────────────────────────────────────────────────────────────────

/// One of the four file tools. Its [`name`](Tool::name) is the bare artifact name the model
/// calls it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Read,
    Write,
    Edit,
    Find,
}

impl Tool {
    pub const ALL: [Tool; 4] = [Tool::Read, Tool::Write, Tool::Edit, Tool::Find];

    /// The tool's name: its artifact name, its staged directory, and so the name murmur gives
    /// the model.
    pub const fn name(self) -> &'static str {
        match self {
            Tool::Read => "tool-read",
            Tool::Write => "tool-write",
            Tool::Edit => "tool-edit",
            Tool::Find => "tool-find",
        }
    }

    /// The top-level fields the tool accepts, in the order its `input_schema` lists them. Every
    /// other key is refused before a file is touched, and the refusal lists these.
    pub const fn fields(self) -> &'static [&'static str] {
        match self {
            Tool::Read => &["path", "start_line", "end_line"],
            Tool::Write => &["path", "content"],
            Tool::Edit => &["path", "edits"],
            Tool::Find => &["pattern", "dir", "glob", "recursive", "context_lines"],
        }
    }

    /// The `state_effect` a successful call declares through the runtime's reserved metadata
    /// key (see the host's wit/tool.wit), which lets `mur trace` redundant-call detection
    /// reason about the tools without hardcoding their names. A failed call declares none: a
    /// failed read did not read, and a failed write did not mutate.
    pub const fn state_effect(self) -> &'static str {
        match self {
            Tool::Read | Tool::Find => "read",
            Tool::Write | Tool::Edit => "mutate",
        }
    }
}

// ── Host roots ──────────────────────────────────────────────────────────────────

// The host-root rules (which preopens are host folders, and where a path value opens among
// them) live in libs/murmur-host-roots, shared with every other wasm32-wasip2 tool. The tool
// crates build their roots with `murmur_file_tools::HostRoot::from_preopen`.
#[cfg(any(
    feature = "read",
    feature = "write",
    feature = "edit",
    feature = "find"
))]
pub(crate) use murmur_host_roots::HOST_ROOT_NAME;
pub use murmur_host_roots::{HostRoot, RootAccess};

/// Where the absolute path `value` opens, and the root it opens through, or `None` when no root
/// among `roots` covers it.
///
/// `value` is normalised lexically (`.` and empty segments dropped, each `..` popping one
/// segment and never climbing above `/`) and compared component by component with each root's
/// path, so `/a/b` covers `/a/b/c` and not `/a/bc`; the longest covering root wins. The open path
/// is `murmur-host` followed by the normalised segments, which wasi-libc resolves through the
/// longest preopen name it begins with. A caller holds `value` absolute: a relative value never
/// opens through a host root. Compiled with no tool feature, so a tool outside this crate links
/// it alone.
pub fn open_in_host_root<'a>(value: &str, roots: &'a [HostRoot]) -> Option<(String, &'a HostRoot)> {
    match murmur_host_roots::locate(value, roots) {
        Ok(murmur_host_roots::Located::Host { open, root }) => Some((open, root)),
        _ => None,
    }
}

/// The keys an element of `tool-edit`'s `edits` accepts, in the order its `input_schema` lists
/// them.
#[cfg(feature = "edit")]
pub const EDIT_PAIR_FIELDS: &[&str] = &["old", "new"];

/// A tool call's result as a component's `Guest::run` hands it to the WIT `ToolResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: OutcomeStatus,
    /// The result's `summary`: on a failure, the message.
    pub summary: Option<String>,
    /// The result's `data`, serialized: a read's content and line counts, a find's matches;
    /// `None` for a write, an edit and every failure.
    pub data: Option<String>,
    /// Whether `data` carries a read's `truncated` note or a find's `capped` note.
    pub truncated: bool,
    /// The tool's [`state_effect`](Tool::state_effect) on success, `None` otherwise.
    pub state_effect: Option<&'static str>,
    /// The files murmur is asked to show the model as images, in order: an image read's one
    /// file, empty for every other call. murmur checks and stores each, or adds a line saying
    /// why it was not attached.
    pub attachments: Vec<OutcomeAttachment>,
}

/// One entry of [`ToolOutcome::attachments`], mapped onto the WIT `attachment` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeAttachment {
    /// The type the file's leading bytes declare: `image/png`, `image/jpeg`, `image/gif` or
    /// `image/webp`.
    pub media_type: String,
    /// The file, as the call named it: relative to the work folder. An image under a host root
    /// is never attached.
    pub path: String,
}

/// [`ToolOutcome::status`], one variant per WIT `status` case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeStatus {
    Passed,
    Failed,
    Error,
}

// ── Shared with tools outside this crate ────────────────────────────────────────

/// `ToolInput.data` as the stdin envelope a native tool read: `{"data": <value>}`, where the
/// value is `data` parsed as JSON, or `data` itself as a JSON string when it does not parse; an
/// absent `data` is `{}`, an envelope with no `data` key.
pub fn envelope(data: Option<&str>) -> String {
    match data {
        Some(d) => {
            let data_val =
                serde_json::from_str::<Value>(d).unwrap_or_else(|_| Value::String(d.to_string()));
            json!({ "data": data_val }).to_string()
        }
        None => "{}".to_string(),
    }
}

/// The fields the host's native dispatch (crates/capsule-runtime `dispatch_native_tool`) kept of
/// a tool's output object, and nothing else of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOutcome {
    /// `"status"`: `passed`, `failed`, and anything else, a missing one included, `error`.
    pub status: OutcomeStatus,
    /// `"summary"` when it is a string.
    pub summary: Option<String>,
    /// `"data"`: a string verbatim, else a non-null value as compact JSON, else `None`.
    pub data: Option<String>,
}

impl NativeOutcome {
    /// The fields of `result`, a tool's output object, as the native dispatch read them.
    pub fn of(result: &Value) -> NativeOutcome {
        let status = match result.get("status").and_then(Value::as_str) {
            Some("passed") => OutcomeStatus::Passed,
            Some("failed") => OutcomeStatus::Failed,
            _ => OutcomeStatus::Error,
        };
        let summary = result
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string);
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                result
                    .get("data")
                    .filter(|d| !d.is_null())
                    .map(|d| d.to_string())
            });
        NativeOutcome {
            status,
            summary,
            data,
        }
    }
}
