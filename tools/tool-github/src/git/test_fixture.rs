//! A minimal, hand-rolled loose-object git repository for tests: no `git` binary, just the same
//! byte-level format [`super::object`] reads.
//!
//! Building fixtures this way (rather than shelling out to `git commit`) is what lets
//! `commit_signed` attach a `gpgsig` header with no real key. It cannot build a real packfile —
//! packed-object test coverage instead reads the checked-in, `git`-CLI-built repositories under
//! `tests/fixtures/` (see `tests/pack_reading.rs`). What this fixture *can* still do on its own
//! is stage the repo-level corruptions [`super::pack::PackSet::open`] refuses before reading
//! any pack byte: [`Fixture::write_alternates`], [`Fixture::write_unmatched_pack_file`] and
//! [`Fixture::write_oversized_pack_pair`] need no real pack format to exercise, since they are
//! all refused by file presence/size alone.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};
use tempfile::TempDir;

pub struct Fixture {
    dir: TempDir,
    /// path -> blob sha, the single flat directory this fixture builds trees from.
    files: RefCell<BTreeMap<String, String>>,
    /// The most recently created commit, chained onto by [`Fixture::commit`].
    head: RefCell<Option<String>>,
}

const AUTHOR_LINE: &str = "Fixture Author <author@example.com> 1690000000 +0000";
const COMMITTER_LINE: &str = "Fixture Committer <committer@example.com> 1690000100 +0000";

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("objects")).expect("mkdir objects");
        Self {
            dir,
            files: RefCell::new(BTreeMap::new()),
            head: RefCell::new(None),
        }
    }

    pub fn git_dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn write_object(&self, kind: &str, content: &[u8]) -> String {
        let header = format!("{kind} {}\0", content.len());
        let mut store = Vec::with_capacity(header.len() + content.len());
        store.extend_from_slice(header.as_bytes());
        store.extend_from_slice(content);

        let mut hasher = Sha1::new();
        hasher.update(&store);
        let sha = hex(&hasher.finalize());

        let path = self.dir.path().join("objects").join(&sha[..2]);
        fs::create_dir_all(&path).expect("mkdir object shard");
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&store).expect("deflate");
        let compressed = encoder.finish().expect("finish deflate");
        fs::write(path.join(&sha[2..]), compressed).expect("write loose object");
        sha
    }

    pub fn write_blob(&self, content: &[u8]) -> String {
        self.write_object("blob", content)
    }

    /// Stage a file's content at `path` (a single path component; this fixture builds one
    /// flat tree) for the next [`Fixture::commit`].
    pub fn write_file(&self, path: &str, content: &[u8]) {
        let sha = self.write_blob(content);
        self.files.borrow_mut().insert(path.to_string(), sha);
    }

    /// Stage a gitlink (submodule) entry pointing at `sha`, which need not exist anywhere.
    pub fn write_gitlink(&self, path: &str, sha: &str) {
        self.files
            .borrow_mut()
            .insert(format!("gitlink:{path}"), sha.to_string());
    }

    fn build_tree(&self) -> String {
        let mut content = Vec::new();
        for (path, sha) in self.files.borrow().iter() {
            let (mode, name) = match path.strip_prefix("gitlink:") {
                Some(name) => ("160000", name),
                None => ("100644", path.as_str()),
            };
            content.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            content.extend_from_slice(&hex_to_bytes(sha));
        }
        self.write_object("tree", &content)
    }

    fn commit_content(tree: &str, parents: &[String], message: &str, signed: bool) -> Vec<u8> {
        let mut header = format!("tree {tree}\n");
        for parent in parents {
            header.push_str(&format!("parent {parent}\n"));
        }
        header.push_str(&format!("author {AUTHOR_LINE}\n"));
        header.push_str(&format!("committer {COMMITTER_LINE}\n"));
        if signed {
            header.push_str(
                "gpgsig -----BEGIN PGP SIGNATURE-----\n abcdefgh\n -----END PGP SIGNATURE-----\n",
            );
        }
        format!("{header}\n{message}\n").into_bytes()
    }

    /// Commit the currently staged files as a child of this fixture's current head (or as a
    /// root commit, the first time this is called).
    pub fn commit(&self, message: &str) -> String {
        let parents: Vec<String> = self.head.borrow().clone().into_iter().collect();
        let sha = self.commit_with_parents(message, &parents, false);
        *self.head.borrow_mut() = Some(sha.clone());
        sha
    }

    /// Commit the currently staged files as a new root commit, independent of any earlier
    /// history this fixture built — for constructing a diverged, unrelated branch.
    pub fn commit_root(&self, message: &str) -> String {
        let sha = self.commit_with_parents(message, &[], false);
        *self.head.borrow_mut() = Some(sha.clone());
        sha
    }

    /// Like [`Fixture::commit`], but the commit carries a `gpgsig` header.
    pub fn commit_signed(&self, message: &str) -> String {
        let parents: Vec<String> = self.head.borrow().clone().into_iter().collect();
        let sha = self.commit_with_parents(message, &parents, true);
        *self.head.borrow_mut() = Some(sha.clone());
        sha
    }

    fn commit_with_parents(&self, message: &str, parents: &[String], signed: bool) -> String {
        let tree = self.build_tree();
        let content = Self::commit_content(&tree, parents, message, signed);
        self.write_object("commit", &content)
    }

    /// Write `objects/info/alternates` naming `target` (which need not exist): this alone
    /// triggers `PackSet::open`'s `OpenError::Alternates`, before any pack byte is read.
    pub fn write_alternates(&self, target: &str) {
        let info = self.dir.path().join("objects").join("info");
        fs::create_dir_all(&info).expect("mkdir objects/info");
        fs::write(info.join("alternates"), target).expect("write alternates");
    }

    /// Write a lone `.pack` file under `objects/pack` with no matching `.idx`, triggering
    /// `OpenError::Unmatched`. Its content is never valid pack data — that is fine, because the
    /// unmatched-file check runs before any pack byte is parsed.
    pub fn write_unmatched_pack_file(&self) {
        let pack_dir = self.dir.path().join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("mkdir objects/pack");
        fs::write(
            pack_dir.join(format!("pack-{}.pack", "0".repeat(40))),
            b"not a real pack",
        )
        .expect("write unmatched pack file");
    }

    /// Write a `.pack`/`.idx` pair under `objects/pack` whose combined logical size is
    /// `total_bytes`, triggering `OpenError::TooLarge` from `fs::metadata` alone. The pack file
    /// is created sparse (`set_len`, no bytes actually written to disk), matching the
    /// requirement that this check never reads pack bytes into memory.
    pub fn write_oversized_pack_pair(&self, total_bytes: u64) {
        let pack_dir = self.dir.path().join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("mkdir objects/pack");
        let stem = "1".repeat(40);
        let pack_path = pack_dir.join(format!("pack-{stem}.pack"));
        let idx_path = pack_dir.join(format!("pack-{stem}.idx"));
        let file = fs::File::create(&pack_path).expect("create oversized pack file");
        file.set_len(total_bytes)
            .expect("set oversized pack length");
        fs::write(&idx_path, b"idx").expect("write idx file");
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_to_bytes(sha: &str) -> Vec<u8> {
    (0..sha.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&sha[i..i + 2], 16).unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::object::{parse_commit, parse_tree, read_loose, ObjectKind};

    #[test]
    fn round_trips_a_commit_through_the_real_reader() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"hello");
        let sha = fx.commit("hello world");

        let raw = read_loose(&fx.git_dir(), &sha).unwrap();
        assert_eq!(raw.kind, ObjectKind::Commit);
        let commit = parse_commit(&raw.content).unwrap();
        assert!(commit.parents.is_empty());
        assert_eq!(commit.message, "hello world\n");

        let tree_raw = read_loose(&fx.git_dir(), &commit.tree).unwrap();
        let entries = parse_tree(&tree_raw.content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
    }
}
