//! One object store for a push call: the loose store first, the pack set (opened once) next.
//!
//! [`ObjectStore::open`] is the single place a push's pack set is opened — it is built once at
//! the start of `plan_push` and threaded through every recursive walk helper, so a pack file is
//! read once per push call, never once per object.

use std::path::Path;

use super::object::{self, RawObject};
use super::pack::{self, PackSet};

/// Why the pack set under `git_dir/objects/pack` could not be opened, before any pack byte was
/// read. Re-exported so [`super::walk`] can name it in [`super::walk::PlanError`].
pub type OpenError = pack::OpenError;

/// A failure reading an object by SHA from the combined loose-and-packed store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// `sha` names no loose object and is in none of this store's opened pack indices.
    NotFound(String),
    /// A loose file or a packed entry exists for `sha` but failed to parse.
    Corrupt(String),
}

/// A repository's objects, readable by SHA from either its loose files or its packs.
pub struct ObjectStore<'a> {
    git_dir: &'a Path,
    packs: PackSet,
}

impl<'a> ObjectStore<'a> {
    /// Open the pack set once for the whole push call this store serves.
    pub fn open(git_dir: &'a Path) -> Result<Self, OpenError> {
        Ok(Self {
            git_dir,
            packs: PackSet::open(git_dir)?,
        })
    }

    /// Read `sha`, trying the loose store first and this store's already-opened packs next.
    pub fn read(&self, sha: &str) -> Result<RawObject, ReadError> {
        match object::read_loose(self.git_dir, sha) {
            Ok(raw) => Ok(raw),
            Err(object::ReadError::Corrupt(sha)) => Err(ReadError::Corrupt(sha)),
            Err(object::ReadError::Missing(sha)) => match self.packs.read(&sha, self.git_dir) {
                Ok(Some(raw)) => Ok(raw),
                Ok(None) => Err(ReadError::NotFound(sha)),
                Err(()) => Err(ReadError::Corrupt(sha)),
            },
        }
    }
}
