//! Writing fetched objects and a remote-tracking ref into a local repository the way git itself
//! does: each object as a zlib-compressed loose file renamed into place, and the ref through a
//! `.lock` file renamed over it, so a concurrent reader never sees a partial file.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use super::object::ObjectKind;

/// Write `content` as the loose object `sha`. An object already present is left as it is.
pub fn write_loose(git_dir: &Path, kind: ObjectKind, sha: &str, content: &[u8]) -> io::Result<()> {
    let shard = git_dir.join("objects").join(&sha[..2]);
    let path = shard.join(&sha[2..]);
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(&shard)?;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(format!("{} {}\0", kind.as_str(), content.len()).as_bytes())?;
    encoder.write_all(content)?;
    let temp = shard.join(format!("tmp_obj_{}", &sha[2..]));
    fs::write(&temp, encoder.finish()?)?;
    fs::rename(&temp, &path)
}

/// The sha `name` (`refs/...`) points at, from its loose file or else `packed-refs`; `None` when
/// the ref does not exist.
pub fn read_ref(git_dir: &Path, name: &str) -> Option<String> {
    if let Ok(text) = fs::read_to_string(git_dir.join(name)) {
        return Some(text.trim().to_string());
    }
    let packed = fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, ref_name) = line.split_once(' ')?;
        (ref_name == name && !sha.starts_with('#')).then(|| sha.to_string())
    })
}

/// Point `name` (`refs/...`) at `sha`. A loose ref takes precedence over a `packed-refs` entry
/// for the same name, so a stale packed value needs no rewrite.
pub fn write_ref(git_dir: &Path, name: &str, sha: &str) -> io::Result<()> {
    let path = git_dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock = git_dir.join(format!("{name}.lock"));
    fs::write(&lock, format!("{sha}\n"))?;
    fs::rename(&lock, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::object::read_loose;
    use crate::git::rebuild::object_sha;

    #[test]
    fn a_written_object_reads_back_through_the_loose_reader() {
        let dir = tempfile::tempdir().unwrap();
        let sha = object_sha(ObjectKind::Blob, b"hello");
        write_loose(dir.path(), ObjectKind::Blob, &sha, b"hello").unwrap();
        let raw = read_loose(dir.path(), &sha).unwrap();
        assert_eq!(raw.kind, ObjectKind::Blob);
        assert_eq!(raw.content, b"hello");
        assert!(!dir
            .path()
            .join("objects")
            .join(&sha[..2])
            .join(format!("tmp_obj_{}", &sha[2..]))
            .exists());
    }

    #[test]
    fn a_loose_ref_overrides_its_packed_value() {
        let dir = tempfile::tempdir().unwrap();
        let name = "refs/remotes/origin/main";
        fs::write(
            dir.path().join("packed-refs"),
            format!("# pack-refs with: peeled\n{} {name}\n", "a".repeat(40)),
        )
        .unwrap();
        assert_eq!(read_ref(dir.path(), name), Some("a".repeat(40)));
        write_ref(dir.path(), name, &"b".repeat(40)).unwrap();
        assert_eq!(read_ref(dir.path(), name), Some("b".repeat(40)));
        assert!(!dir.path().join(format!("{name}.lock")).exists());
        assert_eq!(read_ref(dir.path(), "refs/remotes/origin/other"), None);
    }
}
