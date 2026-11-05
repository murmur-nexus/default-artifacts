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
//! The crate root holds the [`Tool`] descriptor, the host roots and the outcome types. What one
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

/// The preopen name every host root is opened under: `murmur-host` alone for the whole host,
/// `murmur-host` followed by the folder's absolute path for one host folder.
pub(crate) const HOST_ROOT_NAME: &str = "murmur-host";

/// A host folder the runtime opened for the tool, besides the work folder: the whole host under
/// `mur run --unconfined`, or one `host_paths` entry of the operator's config. An absolute path
/// input opens only inside one of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRoot {
    /// The folder's absolute host path, lexically normalised: `/` for the whole host.
    pub path: String,
    pub access: RootAccess,
}

/// Whether the tool may change files under a [`HostRoot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootAccess {
    Read,
    ReadWrite,
}

impl HostRoot {
    /// The root a WASI preopen named `name` opens, or `None` when the preopen is not a host
    /// root. `murmur-host` is the whole host, `/`; `murmur-host/<path>` is the host folder
    /// `/<path>`, normalised. Every other name — `.`, `state`, `murmur-runtime`, or one that only
    /// begins with the same letters, like `murmur-hostile` — is not a root. `writable` is whether
    /// the preopen's descriptor flags hold `mutate-directory`.
    pub fn from_preopen(name: &str, writable: bool) -> Option<HostRoot> {
        let rest = if name == HOST_ROOT_NAME {
            ""
        } else {
            name.strip_prefix(HOST_ROOT_NAME)?.strip_prefix('/')?
        };
        Some(HostRoot {
            path: render_absolute(&normalize_absolute(rest)),
            access: if writable {
                RootAccess::ReadWrite
            } else {
                RootAccess::Read
            },
        })
    }
}

/// The segments of absolute `value`, lexically normalised: `.` and empty segments dropped, each
/// `..` popping one segment, and a `..` at `/` staying at `/`.
pub(crate) fn normalize_absolute(value: &str) -> Vec<&str> {
    lexical_segments(value).0
}

/// The segments of `value` with `.` and empty segments dropped and each `..` popping the segment
/// before it, and whether some `..` found no segment left to pop.
pub(crate) fn lexical_segments(value: &str) -> (Vec<&str>, bool) {
    let mut segments: Vec<&str> = Vec::new();
    let mut climbed_above = false;
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => climbed_above |= segments.pop().is_none(),
            s => segments.push(s),
        }
    }
    (segments, climbed_above)
}

/// `segments` as an absolute path: `/` when there are none.
pub(crate) fn render_absolute(segments: &[&str]) -> String {
    format!("/{}", segments.join("/"))
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
