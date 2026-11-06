//! Where one path input of a wasm32-wasip2 tool opens: in the work folder, inside a host folder
//! the runtime opened for the tool, or nowhere.
//!
//! A WASM tool sees the work folder as the preopen `.` (its current directory) and each host
//! folder the session opened as a preopen named `murmur-host` (the whole host, under
//! `mur run --unconfined`) or `murmur-host/<absolute path>` (one `host_paths` entry of the
//! operator's config). wasi-libc resolves a relative path through the longest preopen name it
//! begins with, so the host folder `/home/op/repo` is reached by opening
//! `murmur-host/home/op/repo/…`. An absolute path opened as written does **not** reach the host:
//! it resolves into the work folder. A `murmur-host/…` path that no held root covers falls
//! through to the work folder too, where a write creates a stray folder. So a tool resolves every
//! value with [`locate`] and opens only what it returns, never an absolute value itself.
//!
//! The tool builds its [`HostRoot`]s from its preopens with [`HostRoot::from_preopen`], in
//! preopen order; that order breaks ties between two roots at the same path.

/// The preopen name every host root is opened under: `murmur-host` alone for the whole host,
/// `murmur-host` followed by the folder's absolute path for one host folder.
pub const HOST_ROOT_NAME: &str = "murmur-host";

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootAccess {
    Read,
    ReadWrite,
}

impl RootAccess {
    /// The access as the operator's `host_paths` entry spells it: `read` or `read-write`.
    pub fn label(self) -> &'static str {
        match self {
            RootAccess::Read => "read",
            RootAccess::ReadWrite => "read-write",
        }
    }
}

impl HostRoot {
    /// The root a WASI preopen named `name` opens, or `None` when the preopen is not a host
    /// root. `murmur-host` is the whole host, `/`; `murmur-host/<path>` is the host folder
    /// `/<path>`, normalised (`.` and empty segments dropped, `..` popping one segment and
    /// staying at `/`). Every other name — `.`, `state`, `murmur-runtime`, `/murmur-host`, `""`,
    /// or one that only begins with the same letters, like `murmur-hostile` — is not a root.
    /// `writable` is whether the preopen's descriptor flags hold `mutate-directory`.
    pub fn from_preopen(name: &str, writable: bool) -> Option<HostRoot> {
        let rest = if name == HOST_ROOT_NAME {
            ""
        } else {
            name.strip_prefix(HOST_ROOT_NAME)?.strip_prefix('/')?
        };
        Some(HostRoot {
            path: render_absolute(&lexical_segments(rest).0),
            access: if writable {
                RootAccess::ReadWrite
            } else {
                RootAccess::Read
            },
        })
    }
}

/// Where [`locate`] says a value opens.
#[derive(Debug, PartialEq, Eq)]
pub enum Located<'a> {
    /// A relative value, carried exactly as written: it opens in the work folder.
    Relative(String),
    /// An absolute value inside `root`. `open` is the path to hand to the filesystem:
    /// `murmur-host` followed by `/` and the value's normalised segments, or `murmur-host`
    /// alone for `/`. It is never shown to the model; messages name the value as written.
    Host { open: String, root: &'a HostRoot },
}

/// Why [`locate`] refused a value.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// An absolute value, and the session opened no host folder to the tool.
    NoRoots,
    /// An absolute value no held root covers.
    Uncovered,
    /// A relative value whose first normalised segment is `murmur-host` while a root is held:
    /// wasi-libc would reach a host root through it. `suggestion` is the absolute path the
    /// caller most likely meant: `/` followed by the remaining segments.
    ReservedName { suggestion: String },
}

/// Locate `value`, a path input, among the host roots the tool holds (`roots`, in preopen
/// order).
///
/// - A relative value opens in the work folder as written, unless a root is held and its first
///   segment after lexical normalisation is `murmur-host`: that is [`Refusal::ReservedName`]. A
///   relative value that climbs above the work folder is left to the filesystem.
/// - An absolute value is [`Refusal::NoRoots`] when no root is held. Otherwise it opens through
///   the held root covering it — compared whole component by whole component after lexical
///   normalisation, so `/a/b` covers `/a/b/c` but not `/a/bc`, and `..` cannot climb out of a
///   root — with the longest root winning and, on a tie, the first in preopen order. No covering
///   root is [`Refusal::Uncovered`].
pub fn locate<'a>(value: &str, roots: &'a [HostRoot]) -> Result<Located<'a>, Refusal> {
    if !value.starts_with('/') {
        if !roots.is_empty() {
            let (segments, climbed_above) = lexical_segments(value);
            if !climbed_above && segments.first() == Some(&HOST_ROOT_NAME) {
                return Err(Refusal::ReservedName {
                    suggestion: render_absolute(&segments[1..]),
                });
            }
        }
        return Ok(Located::Relative(value.to_string()));
    }

    if roots.is_empty() {
        return Err(Refusal::NoRoots);
    }
    let segments = lexical_segments(value).0;
    let mut covering: Option<&HostRoot> = None;
    for root in roots {
        let root_segments = lexical_segments(&root.path).0;
        let covers = segments.len() >= root_segments.len()
            && segments[..root_segments.len()] == root_segments[..];
        if covers && covering.is_none_or(|best| root.path.len() > best.path.len()) {
            covering = Some(root);
        }
    }
    let root = covering.ok_or(Refusal::Uncovered)?;
    let mut open = HOST_ROOT_NAME.to_string();
    for segment in &segments {
        open.push('/');
        open.push_str(segment);
    }
    Ok(Located::Host { open, root })
}

/// The held roots as a refusal lists them: `<path> (<read|read-write>)`, comma-separated, in
/// preopen order.
pub fn describe_roots(roots: &[HostRoot]) -> String {
    roots
        .iter()
        .map(|root| format!("{} ({})", root.path, root.access.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The SQLite URI that opens the database at `path` (a path [`locate`] returned) read-only with no
/// lock and no journal: `file:<path>?immutable=1`, with `%`, `?` and `#` in `path`
/// percent-encoded (`%25`, `%3F`, `%23`) so a path holding them is neither cut short at a query
/// or fragment nor decoded into a different path. Open it with `SQLITE_OPEN_READ_ONLY |
/// SQLITE_OPEN_URI`.
///
/// SQLite's WASI build locks with a `<db>.lock` dotfile, which cannot be created in a read-only
/// host folder and would otherwise appear beside the database; `immutable=1` reads without it.
pub fn sqlite_immutable_uri(path: &str) -> String {
    let mut uri = String::with_capacity(path.len() + 17);
    uri.push_str("file:");
    for c in path.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3F"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?immutable=1");
    uri
}

/// The segments of `value` with `.` and empty segments dropped and each `..` popping the segment
/// before it, and whether some `..` found no segment left to pop. Purely lexical: nothing is
/// read from the filesystem, so a symlink is a segment like any other.
pub fn lexical_segments(value: &str) -> (Vec<&str>, bool) {
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
fn render_absolute(segments: &[&str]) -> String {
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_uri_percent_encodes_only_what_ends_or_decodes_a_uri_path() {
        let uri = sqlite_immutable_uri;
        assert_eq!(
            uri(".murmur/code-graph.db"),
            "file:.murmur/code-graph.db?immutable=1"
        );
        assert_eq!(
            uri("murmur-host/srv/a b/x.db"),
            "file:murmur-host/srv/a b/x.db?immutable=1"
        );
        assert_eq!(uri("a%b?c#d"), "file:a%25b%3Fc%23d?immutable=1");
        assert_eq!(uri("%3F"), "file:%253F?immutable=1");
    }

    fn root(path: &str, access: RootAccess) -> HostRoot {
        HostRoot {
            path: path.to_string(),
            access,
        }
    }

    fn host<'a>(located: Result<Located<'a>, Refusal>) -> (String, &'a HostRoot) {
        match located {
            Ok(Located::Host { open, root }) => (open, root),
            other => panic!("expected a host location, got {other:?}"),
        }
    }

    #[test]
    fn from_preopen_accepts_exactly_the_host_root_names() {
        for (name, writable, expected) in [
            ("murmur-host", true, Some(root("/", RootAccess::ReadWrite))),
            ("murmur-host", false, Some(root("/", RootAccess::Read))),
            ("murmur-host/", true, Some(root("/", RootAccess::ReadWrite))),
            (
                "murmur-host/home/op/.cargo",
                false,
                Some(root("/home/op/.cargo", RootAccess::Read)),
            ),
            (
                "murmur-host/data//cargo/./bin/../",
                true,
                Some(root("/data/cargo", RootAccess::ReadWrite)),
            ),
            (
                "murmur-host/..",
                true,
                Some(root("/", RootAccess::ReadWrite)),
            ),
            ("murmur-hostile", true, None),
            ("murmur-hostile/x", true, None),
            (".", true, None),
            ("state", true, None),
            ("murmur-runtime", false, None),
            ("/murmur-host", true, None),
            ("", true, None),
        ] {
            assert_eq!(HostRoot::from_preopen(name, writable), expected, "{name:?}");
        }
    }

    #[test]
    fn labels_spell_the_host_paths_access() {
        assert_eq!(RootAccess::Read.label(), "read");
        assert_eq!(RootAccess::ReadWrite.label(), "read-write");
    }

    #[test]
    fn absolute_values_normalise_lexically() {
        let roots = vec![root("/", RootAccess::Read)];
        for (value, open) in [
            ("/", "murmur-host"),
            ("/a/b", "murmur-host/a/b"),
            ("//a//b/", "murmur-host/a/b"),
            ("/a/./b/.", "murmur-host/a/b"),
            ("/a/b/../c", "murmur-host/a/c"),
            ("/..", "murmur-host"),
            ("/../../a", "murmur-host/a"),
            ("/a/../..", "murmur-host"),
        ] {
            assert_eq!(host(locate(value, &roots)).0, open, "{value}");
        }
    }

    #[test]
    fn an_absolute_value_opens_through_the_longest_covering_root() {
        let roots = vec![
            root("/", RootAccess::Read),
            root("/srv/host", RootAccess::ReadWrite),
            root("/srv/host/deep", RootAccess::Read),
            root("/srv", RootAccess::ReadWrite),
        ];
        for (value, open, covering) in [
            ("/etc/hosts", "murmur-host/etc/hosts", "/"),
            ("/", "murmur-host", "/"),
            ("/srv/host/a.txt", "murmur-host/srv/host/a.txt", "/srv/host"),
            ("/srv/host", "murmur-host/srv/host", "/srv/host"),
            (
                "/srv/host/deep/x",
                "murmur-host/srv/host/deep/x",
                "/srv/host/deep",
            ),
            ("/srv/hostile/x", "murmur-host/srv/hostile/x", "/srv"),
            ("/srv/host/./b/../c", "murmur-host/srv/host/c", "/srv/host"),
            ("/srv/host/../other", "murmur-host/srv/other", "/srv"),
        ] {
            let (got_open, got_root) = host(locate(value, &roots));
            assert_eq!(got_open, open, "{value}");
            assert_eq!(got_root.path, covering, "{value}");
        }
    }

    #[test]
    fn on_a_tie_the_first_root_in_preopen_order_wins() {
        let same = vec![
            root("/srv/host", RootAccess::Read),
            root("/srv/host", RootAccess::ReadWrite),
        ];
        assert!(std::ptr::eq(host(locate("/srv/host/x", &same)).1, &same[0]));
    }

    #[test]
    fn a_root_covers_whole_components_only() {
        let roots = vec![root("/a/b", RootAccess::ReadWrite)];
        assert_eq!(host(locate("/a/b/c", &roots)).0, "murmur-host/a/b/c");
        assert_eq!(host(locate("/a/b", &roots)).0, "murmur-host/a/b");
        for value in ["/a/bc", "/a/bc/d", "/a", "/", "/x/a/b"] {
            assert_eq!(locate(value, &roots), Err(Refusal::Uncovered), "{value}");
        }
    }

    #[test]
    fn dot_dot_cannot_climb_out_of_a_root() {
        let roots = vec![root("/srv/ro", RootAccess::Read)];
        for value in [
            "/srv/ro/..",
            "/srv/ro/../x",
            "/srv/ro/a/../../../srv",
            "/srv/ro/../../srv/ro2",
        ] {
            assert_eq!(locate(value, &roots), Err(Refusal::Uncovered), "{value}");
        }
        assert_eq!(
            host(locate("/srv/ro/a/../b", &roots)).0,
            "murmur-host/srv/ro/b"
        );
    }

    #[test]
    fn an_absolute_value_with_no_root_is_refused() {
        for value in ["/", "/etc/hosts", "/srv/p/x"] {
            assert_eq!(locate(value, &[]), Err(Refusal::NoRoots), "{value}");
        }
    }

    #[test]
    fn a_relative_value_opens_as_written() {
        let roots = [root("/srv", RootAccess::Read)];
        for value in [
            "src/../a.txt",
            "./a.txt",
            "state/x",
            "murmur-runtime/r",
            ".",
            "a",
        ] {
            for held in [&roots[..], &[]] {
                assert_eq!(
                    locate(value, held),
                    Ok(Located::Relative(value.to_string())),
                    "{value}"
                );
            }
        }
    }

    #[test]
    fn murmur_host_is_a_reserved_first_segment_while_a_root_is_held() {
        let roots = vec![root("/srv/host", RootAccess::ReadWrite)];
        for (value, suggestion) in [
            ("murmur-host/srv/host/x.txt", "/srv/host/x.txt"),
            ("./murmur-host/srv/host/new.txt", "/srv/host/new.txt"),
            ("src/../murmur-host//etc/x", "/etc/x"),
            ("./murmur-host", "/"),
            ("murmur-host/", "/"),
            ("murmur-host", "/"),
        ] {
            assert_eq!(
                locate(value, &roots),
                Err(Refusal::ReservedName {
                    suggestion: suggestion.to_string()
                }),
                "{value}"
            );
        }
        // Only the first segment is reserved, and a value climbing above the work folder is
        // left to the filesystem.
        for value in [
            "docs/murmur-host/x.txt",
            "../murmur-host/x.txt",
            "murmur-hostile/x",
        ] {
            assert_eq!(
                locate(value, &roots),
                Ok(Located::Relative(value.to_string())),
                "{value}"
            );
        }
    }

    #[test]
    fn murmur_host_is_an_ordinary_name_with_no_root_held() {
        assert_eq!(
            locate("murmur-host/x", &[]),
            Ok(Located::Relative("murmur-host/x".to_string()))
        );
    }

    #[test]
    fn the_whole_host_root_covers_every_absolute_value() {
        let roots = vec![HostRoot::from_preopen("murmur-host", false).unwrap()];
        let (open, covering) = host(locate("/home/op/repo/.murmur/code-graph.db", &roots));
        assert_eq!(open, "murmur-host/home/op/repo/.murmur/code-graph.db");
        assert_eq!(covering.path, "/");
        assert_eq!(host(locate("/", &roots)).0, "murmur-host");
    }

    #[test]
    fn describe_roots_lists_paths_and_access_in_preopen_order() {
        let roots = vec![
            root("/srv/rw", RootAccess::ReadWrite),
            root("/srv/ro", RootAccess::Read),
        ];
        assert_eq!(
            describe_roots(&roots),
            "/srv/rw (read-write), /srv/ro (read)"
        );
        assert_eq!(describe_roots(&[]), "");
    }
}
