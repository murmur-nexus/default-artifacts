//! Opening every packfile under `objects/pack` once per push call, and reading a single object
//! out of whichever pack's index names it.
//!
//! Everything is read with `std::fs::read` into a `Vec<u8>` and handed to
//! `gix_pack::index::File::from_data` / `gix_pack::data::File::from_data` — never the `at(path)`
//! mmap constructors, which are a no-op stub on `wasm32-wasip2` (no `mmap` there). A `Vec<u8>`
//! satisfies `gix_pack::FileData` (anything that derefs to `[u8]`), so no adapter type is
//! needed.
//!
//! `PackSet::open` reads index and pack bytes once and keeps every opened pack for the rest of
//! the push call; nothing here re-opens or re-reads a pack file per object.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use super::object::{ObjectKind, RawObject};

/// Sum of every `objects/pack/*.pack` and `*.idx` file this tool will read into memory for one
/// push, above which the push is refused before any of those bytes are read. A normal clone's
/// pack set is a handful of files far under this; 256 MiB bounds the guest's memory budget
/// against a repository shaped to make this tool allocate past it.
pub const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;

/// Why [`PackSet::open`] refused the pack set, before any pack's bytes were read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// `objects/info/alternates` is present; this tool does not resolve objects through it.
    Alternates(PathBuf),
    /// A `.pack` with no matching `.idx`, or a `.idx` with no matching `.pack`.
    Unmatched(PathBuf),
    /// A pack file exists but its index or data could not be parsed.
    Corrupt(PathBuf),
    /// The pack files' total on-disk size exceeds [`MAX_PACK_BYTES`].
    TooLarge { observed: u64, ceiling: u64 },
}

struct OpenPack {
    idx: gix_pack::index::File<Vec<u8>>,
    data: gix_pack::data::File<Vec<u8>>,
}

/// Every packfile under one repository's `objects/pack`, opened once and reused for every
/// object a push's walk needs to read.
pub struct PackSet {
    packs: Vec<OpenPack>,
}

impl PackSet {
    /// Open every `.pack`/`.idx` pair under `git_dir/objects/pack`, refusing before reading a
    /// single pack byte if: an `alternates` file is present, a `.pack`/`.idx` is unpaired, or
    /// the total on-disk size exceeds [`MAX_PACK_BYTES`] (checked via `fs::metadata` alone).
    pub fn open(git_dir: &Path) -> Result<Self, OpenError> {
        let alternates = git_dir.join("objects").join("info").join("alternates");
        if alternates.is_file() {
            return Err(OpenError::Alternates(alternates));
        }

        let pack_dir = git_dir.join("objects").join("pack");
        let mut stems: BTreeMap<String, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
        if pack_dir.is_dir() {
            let entries =
                fs::read_dir(&pack_dir).map_err(|_| OpenError::Corrupt(pack_dir.clone()))?;
            for entry in entries {
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let (Some(ext), Some(stem)) = (
                    path.extension().and_then(OsStr::to_str),
                    path.file_stem().and_then(OsStr::to_str),
                ) else {
                    continue;
                };
                let slot = stems.entry(stem.to_string()).or_insert((None, None));
                match ext {
                    "pack" => slot.0 = Some(path),
                    "idx" => slot.1 = Some(path),
                    _ => {}
                }
            }
        }

        let mut pairs = Vec::new();
        for (pack_path, idx_path) in stems.into_values() {
            match (pack_path, idx_path) {
                (Some(pack_path), Some(idx_path)) => pairs.push((pack_path, idx_path)),
                (Some(unmatched), None) | (None, Some(unmatched)) => {
                    return Err(OpenError::Unmatched(unmatched))
                }
                (None, None) => {
                    unreachable!("a stem is only inserted alongside a pack or idx path")
                }
            }
        }

        let mut total: u64 = 0;
        for (pack_path, idx_path) in &pairs {
            let pack_len = fs::metadata(pack_path)
                .map_err(|_| OpenError::Corrupt(pack_path.clone()))?
                .len();
            let idx_len = fs::metadata(idx_path)
                .map_err(|_| OpenError::Corrupt(idx_path.clone()))?
                .len();
            total = total.saturating_add(pack_len).saturating_add(idx_len);
        }
        if total > MAX_PACK_BYTES {
            return Err(OpenError::TooLarge {
                observed: total,
                ceiling: MAX_PACK_BYTES,
            });
        }

        let mut packs = Vec::with_capacity(pairs.len());
        for (pack_path, idx_path) in pairs {
            let idx_bytes =
                fs::read(&idx_path).map_err(|_| OpenError::Corrupt(idx_path.clone()))?;
            let idx =
                gix_pack::index::File::from_data(idx_bytes, idx_path.clone(), gix_hash::Kind::Sha1)
                    .map_err(|_| OpenError::Corrupt(idx_path.clone()))?;
            let data_bytes =
                fs::read(&pack_path).map_err(|_| OpenError::Corrupt(pack_path.clone()))?;
            let data = gix_pack::data::File::from_data(
                data_bytes,
                pack_path.clone(),
                gix_hash::Kind::Sha1,
            )
            .map_err(|_| OpenError::Corrupt(pack_path.clone()))?;
            packs.push(OpenPack { idx, data });
        }

        Ok(Self { packs })
    }

    /// Read `sha_hex` from whichever opened pack's index names it, resolving any `OFS_DELTA`/
    /// `REF_DELTA` chain — a `REF_DELTA` base is looked up first as a loose object under
    /// `git_dir`, then across every pack in this set, so a base in a different pack from its
    /// delta resolves exactly like one in the same pack.
    ///
    /// `Ok(None)` means `sha_hex` is in none of this set's indices. `Err(())` means it was
    /// found but its entry (or a base it depends on) failed to decode; the caller reports it
    /// with the sha it asked for, so there is nothing more for the error to carry.
    #[allow(clippy::result_unit_err)]
    pub fn read(&self, sha_hex: &str, git_dir: &Path) -> Result<Option<RawObject>, ()> {
        let id = gix_hash::ObjectId::from_hex(sha_hex.as_bytes()).map_err(|_| ())?;
        for pack in &self.packs {
            let Some(entry_index) = pack.idx.lookup(id) else {
                continue;
            };
            let offset = pack.idx.pack_offset_at_index(entry_index);
            let entry = pack.data.entry(offset).map_err(|_| ())?;
            let mut out = Vec::new();
            let mut inflate = gix_zlib::Inflate::default();
            let mut cache = gix_pack::cache::Never;
            let resolve =
                |oid: &gix_hash::oid, out: &mut Vec<u8>| self.resolve_base(git_dir, oid, out);
            let outcome = pack
                .data
                .decode_entry(entry, &mut out, &mut inflate, &resolve, &mut cache)
                .map_err(|_| ())?;
            return Ok(Some(RawObject {
                kind: from_gix_kind(outcome.kind),
                content: out,
            }));
        }
        Ok(None)
    }

    /// A `REF_DELTA` base resolver: tries `git_dir`'s loose store, then every opened pack in
    /// turn, fully decoding the base (recursively resolving its own deltas the same way) and
    /// appending its content to `out` — never assuming the base lives in the same pack as the
    /// delta that names it.
    fn resolve_base(
        &self,
        git_dir: &Path,
        oid: &gix_hash::oid,
        out: &mut Vec<u8>,
    ) -> Option<gix_pack::data::decode::entry::ResolvedBase> {
        use gix_pack::data::decode::entry::ResolvedBase;

        let sha = oid.to_hex().to_string();
        if let Ok(raw) = super::object::read_loose(git_dir, &sha) {
            out.extend_from_slice(&raw.content);
            return Some(ResolvedBase::OutOfPack {
                kind: to_gix_kind(raw.kind),
                end: out.len(),
            });
        }

        for pack in &self.packs {
            let Some(entry_index) = pack.idx.lookup(oid) else {
                continue;
            };
            let offset = pack.idx.pack_offset_at_index(entry_index);
            let Ok(entry) = pack.data.entry(offset) else {
                continue;
            };
            let mut inflate = gix_zlib::Inflate::default();
            let mut cache = gix_pack::cache::Never;
            let resolve = |o: &gix_hash::oid, out: &mut Vec<u8>| self.resolve_base(git_dir, o, out);
            if let Ok(outcome) =
                pack.data
                    .decode_entry(entry, out, &mut inflate, &resolve, &mut cache)
            {
                return Some(ResolvedBase::OutOfPack {
                    kind: outcome.kind,
                    end: out.len(),
                });
            }
        }
        None
    }
}

fn to_gix_kind(kind: ObjectKind) -> gix_object::Kind {
    match kind {
        ObjectKind::Commit => gix_object::Kind::Commit,
        ObjectKind::Tree => gix_object::Kind::Tree,
        ObjectKind::Blob => gix_object::Kind::Blob,
        ObjectKind::Tag => gix_object::Kind::Tag,
    }
}

fn from_gix_kind(kind: gix_object::Kind) -> ObjectKind {
    match kind {
        gix_object::Kind::Commit => ObjectKind::Commit,
        gix_object::Kind::Tree => ObjectKind::Tree,
        gix_object::Kind::Blob => ObjectKind::Blob,
        gix_object::Kind::Tag => ObjectKind::Tag,
    }
}
