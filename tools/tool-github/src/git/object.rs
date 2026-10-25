//! Byte-level reading of git's loose-object format: `zlib("<type> <size>\0<content>")` at
//! `.git/objects/<sha[0:2]>/<sha[2:]>`.
//!
//! This module only ever reads the loose store: an object not found as a loose file is reported
//! as [`ReadError::Missing`], leaving it to [`super::store::ObjectStore`] to try the pack set
//! next.

use std::fs;
use std::io::Read;
use std::path::Path;

use flate2::read::ZlibDecoder;

use super::date::{parse_identity_line, Identity};

/// A git object's type, from its loose-object header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }
}

/// A loose object's header and raw content, decompressed but otherwise unparsed.
pub struct RawObject {
    pub kind: ObjectKind,
    pub content: Vec<u8>,
}

/// A failure reading an object by SHA from the loose store alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// No loose object file exists for this SHA — it may still be reachable through a pack, or
    /// nowhere at all; [`super::store::ObjectStore`] decides which.
    Missing(String),
    /// The loose file exists but is not valid zlib, or its header is malformed.
    Corrupt(String),
}

impl ReadError {
    pub fn sha(&self) -> &str {
        match self {
            Self::Missing(sha) | Self::Corrupt(sha) => sha,
        }
    }
}

/// `.git/objects/<sha[0:2]>/<sha[2:]>` under `git_dir`.
fn loose_path(git_dir: &Path, sha: &str) -> Option<std::path::PathBuf> {
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(git_dir.join("objects").join(&sha[..2]).join(&sha[2..]))
}

/// Read and decompress the loose object named `sha`, or classify why it could not be read.
pub fn read_loose(git_dir: &Path, sha: &str) -> Result<RawObject, ReadError> {
    let path = loose_path(git_dir, sha).ok_or_else(|| ReadError::Corrupt(sha.to_string()))?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Err(ReadError::Missing(sha.to_string())),
    };
    let mut decoder = ZlibDecoder::new(&bytes[..]);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|_| ReadError::Corrupt(sha.to_string()))?;
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| ReadError::Corrupt(sha.to_string()))?;
    let header =
        std::str::from_utf8(&raw[..nul]).map_err(|_| ReadError::Corrupt(sha.to_string()))?;
    let (kind_str, size_str) = header
        .split_once(' ')
        .ok_or_else(|| ReadError::Corrupt(sha.to_string()))?;
    let size: usize = size_str
        .parse()
        .map_err(|_| ReadError::Corrupt(sha.to_string()))?;
    let content = raw[nul + 1..].to_vec();
    if content.len() != size {
        return Err(ReadError::Corrupt(sha.to_string()));
    }
    let kind = match kind_str {
        "commit" => ObjectKind::Commit,
        "tree" => ObjectKind::Tree,
        "blob" => ObjectKind::Blob,
        "tag" => ObjectKind::Tag,
        _ => return Err(ReadError::Corrupt(sha.to_string())),
    };
    Ok(RawObject { kind, content })
}

/// A parsed commit object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Identity,
    pub committer: Identity,
    pub message: String,
    /// Whether the commit carries a `gpgsig` (GPG or SSH signature) header.
    pub signed: bool,
}

/// Parse a commit object's content (the bytes after the loose-object header).
pub fn parse_commit(content: &[u8]) -> Result<Commit, String> {
    let text = std::str::from_utf8(content).map_err(|_| "commit is not valid UTF-8".to_string())?;
    let (header, message) = text
        .split_once("\n\n")
        .ok_or_else(|| "commit has no header/message separator".to_string())?;

    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    let mut signed = false;

    let mut lines = header.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("tree ") {
            tree = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("parent ") {
            parents.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("author ") {
            author = Some(parse_identity_line(rest)?);
        } else if let Some(rest) = line.strip_prefix("committer ") {
            committer = Some(parse_identity_line(rest)?);
        } else if line.starts_with("gpgsig") {
            signed = true;
            // A signature's continuation lines are indented with a single space; consume them
            // so they are not mistaken for further headers.
            while lines.peek().is_some_and(|next| next.starts_with(' ')) {
                lines.next();
            }
        }
        // Other headers (encoding, mergetag, gpgsig-sha256) are ignored: they do not change
        // whether this tool can byte-reproduce the commit, since the message/tree/parents/
        // identities are unaffected and a signed commit is refused regardless.
    }

    Ok(Commit {
        tree: tree.ok_or("commit has no tree header")?,
        parents,
        author: author.ok_or("commit has no author header")?,
        committer: committer.ok_or("commit has no committer header")?,
        message: message.to_string(),
        signed,
    })
}

/// One entry of a tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub sha: String,
}

/// The git mode marking a gitlink (submodule) entry.
pub const GITLINK_MODE: &str = "160000";

impl TreeEntry {
    pub fn is_gitlink(&self) -> bool {
        self.mode == GITLINK_MODE
    }

    pub fn is_tree(&self) -> bool {
        self.mode == "40000" || self.mode == "040000"
    }

    /// The Git Data API's `mode` for this entry. A tree object stores a directory's mode as
    /// `40000`, but the create-tree API accepts only the six-digit `040000` and rejects the
    /// short form with `422 Must supply a valid tree.mode`. GitHub writes the short form back
    /// into the object it stores, so the resulting tree sha still matches the local one.
    pub fn api_mode(&self) -> &str {
        if self.is_tree() {
            "040000"
        } else {
            &self.mode
        }
    }

    /// The Git Data API's `type` for this entry: `"tree"`, `"blob"` (regular files, executables
    /// and symlinks alike are all blobs to the tree API) or `"commit"` (a gitlink).
    pub fn api_type(&self) -> &'static str {
        if self.is_tree() {
            "tree"
        } else if self.is_gitlink() {
            "commit"
        } else {
            "blob"
        }
    }
}

/// Parse a tree object's content: repeated `"<mode> <name>\0<20-byte sha>"`.
pub fn parse_tree(content: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or("tree entry has no mode/name separator")?;
        let mode = std::str::from_utf8(&rest[..space])
            .map_err(|_| "tree entry mode is not UTF-8")?
            .to_string();
        let nul = rest[space + 1..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("tree entry has no name/sha separator")?;
        let name = std::str::from_utf8(&rest[space + 1..space + 1 + nul])
            .map_err(|_| "tree entry name is not UTF-8")?
            .to_string();
        let sha_start = space + 1 + nul + 1;
        if rest.len() < sha_start + 20 {
            return Err("tree entry sha is truncated".to_string());
        }
        let sha = hex(&rest[sha_start..sha_start + 20]);
        entries.push(TreeEntry { mode, name, sha });
        rest = &rest[sha_start + 20..];
    }
    Ok(entries)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_commit_with_no_parents() {
        let content = b"tree aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
author A <a@b.com> 1000 +0000\n\
committer A <a@b.com> 1000 +0000\n\n\
initial commit\n";
        let commit = parse_commit(content).unwrap();
        assert_eq!(commit.tree, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(commit.parents.is_empty());
        assert_eq!(commit.message, "initial commit\n");
        assert!(!commit.signed);
    }

    #[test]
    fn detects_a_signed_commit() {
        let content = b"tree aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
parent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
author A <a@b.com> 1000 +0000\n\
committer A <a@b.com> 1000 +0000\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n\
 abcdef\n\
 -----END PGP SIGNATURE-----\n\n\
signed commit\n";
        let commit = parse_commit(content).unwrap();
        assert!(commit.signed);
        assert_eq!(
            commit.parents,
            vec!["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
        );
    }

    #[test]
    fn parses_tree_entries_including_a_gitlink() {
        let mut content = Vec::new();
        content.extend_from_slice(b"100644 file.txt\0");
        content.extend_from_slice(&[0x11; 20]);
        content.extend_from_slice(b"160000 submodule\0");
        content.extend_from_slice(&[0x22; 20]);
        let entries = parse_tree(&content).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "file.txt");
        assert!(!entries[0].is_gitlink());
        assert_eq!(entries[1].name, "submodule");
        assert!(entries[1].is_gitlink());
        assert_eq!(entries[1].sha, "22".repeat(20));
    }
}
