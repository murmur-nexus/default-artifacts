//! Rebuilding git objects, byte for byte, from GitHub's API views of them, for `fetch`.
//!
//! GitHub's API never hands back a raw object, so each one is re-serialised from its JSON view
//! and accepted only when the bytes hash to the sha GitHub named — a fetched object is either
//! the exact object the remote holds or it is refused. Where a view drops a detail the bytes
//! depend on, each plausible reading is tried in turn:
//!
//! - GraphQL drops a commit message's trailing newline, so an unsigned commit's message is tried
//!   with one and then without.
//! - GraphQL drops a trailing empty line from a signature (GitHub's own web-flow signatures end
//!   in one), so the `gpgsig` header is tried as given and then with one more empty
//!   continuation line.
//!
//! The REST commit view reports every date in UTC, losing the offset the identity line carries,
//! which is why commits come from GraphQL: its `GitTimestamp` keeps the offset as written.

use sha1::{Digest, Sha1};

use super::date::{from_iso8601, to_identity_line, Identity};
use super::object::{ObjectKind, TreeEntry};

/// A commit author or committer as GraphQL's `GitActor` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIdentity {
    pub name: String,
    pub email: String,
    /// A `GitTimestamp`: ISO 8601 with the offset the commit was written with.
    pub date: String,
}

/// A commit's signature as GraphQL's `GitSignature` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSignature {
    /// The armored signature, the `gpgsig` header's value.
    pub signature: String,
    /// The exact commit object the signature covers: the commit without its `gpgsig` header.
    pub payload: String,
}

/// A commit as GitHub's GraphQL API describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommit {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author: RemoteIdentity,
    pub committer: RemoteIdentity,
    pub message: String,
    pub signature: Option<RemoteSignature>,
}

/// The sha git names an object by: SHA-1 over `"<kind> <len>\0<content>"`.
pub fn object_sha(kind: ObjectKind, content: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("{} {}\0", kind.as_str(), content.len()).as_bytes());
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `content` if it hashes to `sha` as a `kind` object, or why it was refused.
pub fn verified(kind: ObjectKind, sha: &str, content: Vec<u8>) -> Result<Vec<u8>, String> {
    let actual = object_sha(kind, &content);
    if actual == sha {
        Ok(content)
    } else {
        Err(format!(
            "{} {sha} rebuilt from GitHub's response hashes to {actual}; it cannot be written \
             as the object GitHub named",
            kind.as_str()
        ))
    }
}

/// A tree's raw content from its entries: repeated `"<mode> <name>\0<20-byte sha>"`, entries in
/// the order given. `mode` is git's on-disk form (`40000` for a directory, not `040000`).
pub fn encode_tree(entries: &[TreeEntry]) -> Result<Vec<u8>, String> {
    let mut content = Vec::new();
    for entry in entries {
        content.extend_from_slice(format!("{} {}\0", entry.mode, entry.name).as_bytes());
        content.extend_from_slice(&sha_bytes(&entry.sha)?);
    }
    Ok(content)
}

/// The commit's raw content: the first reading of GitHub's view whose bytes hash to
/// `commit.sha`.
pub fn rebuild_commit(commit: &RemoteCommit) -> Result<Vec<u8>, String> {
    let candidates = match &commit.signature {
        Some(signature) => signed_candidates(signature)?,
        None => unsigned_candidates(commit)?,
    };
    candidates
        .into_iter()
        .find(|content| object_sha(ObjectKind::Commit, content) == commit.sha)
        .ok_or_else(|| {
            format!(
                "commit {} could not be rebuilt byte for byte from GitHub's description of it \
                 (it may carry a header GitHub's API does not report, such as encoding or \
                 mergetag)",
                commit.sha
            )
        })
}

fn unsigned_candidates(commit: &RemoteCommit) -> Result<Vec<Vec<u8>>, String> {
    let mut header = format!("tree {}\n", commit.tree);
    for parent in &commit.parents {
        header.push_str(&format!("parent {parent}\n"));
    }
    header.push_str(&format!("author {}\n", identity_line(&commit.author)?));
    header.push_str(&format!(
        "committer {}\n",
        identity_line(&commit.committer)?
    ));
    Ok([format!("{}\n", commit.message), commit.message.clone()]
        .into_iter()
        .map(|message| format!("{header}\n{message}").into_bytes())
        .collect())
}

/// The payload is the commit as written, minus its `gpgsig` header; git appends that header
/// last, so it goes back in just before the blank line ending the headers.
fn signed_candidates(signature: &RemoteSignature) -> Result<Vec<Vec<u8>>, String> {
    let (headers, message) = signature
        .payload
        .split_once("\n\n")
        .ok_or("the signed payload GitHub returned has no header/message separator")?;
    let armor = signature
        .signature
        .strip_suffix('\n')
        .unwrap_or(&signature.signature);
    let lines: Vec<&str> = armor.split('\n').collect();
    let with_empty_tail: Vec<&str> = lines.iter().copied().chain([""]).collect();
    Ok([lines, with_empty_tail]
        .into_iter()
        .map(|lines| {
            let gpgsig = format!("gpgsig {}\n", lines.join("\n "));
            format!("{headers}\n{gpgsig}\n{message}").into_bytes()
        })
        .collect())
}

fn identity_line(identity: &RemoteIdentity) -> Result<String, String> {
    let (seconds, offset_minutes, negative_zero) = from_iso8601(&identity.date)
        .map_err(|e| format!("GitHub's date {:?} is not ISO 8601: {e}", identity.date))?;
    Ok(to_identity_line(&Identity {
        name: identity.name.clone(),
        email: identity.email.clone(),
        seconds,
        offset_minutes,
        negative_zero,
    }))
}

fn sha_bytes(sha: &str) -> Result<Vec<u8>, String> {
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{sha:?} is not a 40-character hex sha"));
    }
    Ok((0..40)
        .step_by(2)
        .map(|i| u8::from_str_radix(&sha[i..i + 2], 16).unwrap())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(date: &str) -> RemoteIdentity {
        RemoteIdentity {
            name: "Ada".to_string(),
            email: "ada@example.com".to_string(),
            date: date.to_string(),
        }
    }

    fn unsigned(raw: &str, message: &str) -> RemoteCommit {
        RemoteCommit {
            sha: object_sha(ObjectKind::Commit, raw.as_bytes()),
            tree: "a".repeat(40),
            parents: vec!["b".repeat(40)],
            author: identity("2026-10-16T08:44:18-05:00"),
            committer: identity("2026-10-16T08:44:18-05:00"),
            message: message.to_string(),
            signature: None,
        }
    }

    #[test]
    fn hashes_like_git() {
        // `printf 'hello' | git hash-object --stdin`
        assert_eq!(
            object_sha(ObjectKind::Blob, b"hello"),
            "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0"
        );
    }

    #[test]
    fn rebuilds_an_unsigned_commit_keeping_its_offset_and_trailing_newline() {
        let raw = format!(
            "tree {}\nparent {}\nauthor Ada <ada@example.com> 1792158258 -0500\n\
             committer Ada <ada@example.com> 1792158258 -0500\n\nsubject\n\nbody\n",
            "a".repeat(40),
            "b".repeat(40)
        );
        let commit = unsigned(&raw, "subject\n\nbody");
        assert_eq!(rebuild_commit(&commit).unwrap(), raw.as_bytes());
    }

    #[test]
    fn rebuilds_an_unsigned_commit_whose_message_has_no_trailing_newline() {
        let raw = format!(
            "tree {}\nparent {}\nauthor Ada <ada@example.com> 1792158258 -0500\n\
             committer Ada <ada@example.com> 1792158258 -0500\n\nsubject",
            "a".repeat(40),
            "b".repeat(40)
        );
        let commit = unsigned(&raw, "subject");
        assert_eq!(rebuild_commit(&commit).unwrap(), raw.as_bytes());
    }

    #[test]
    fn rebuilds_a_signed_commit_whose_stored_signature_ends_in_an_empty_line() {
        let payload = format!(
            "tree {}\nparent {}\nauthor Ada <ada@example.com> 1792159408 -0500\n\
             committer GitHub <noreply@github.com> 1792159408 -0500\n\nSquashed (#1)",
            "a".repeat(40),
            "b".repeat(40)
        );
        let (headers, message) = payload.split_once("\n\n").unwrap();
        let raw = format!(
            "{headers}\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n wsFcBAAB\n =67Gc\n \
             -----END PGP SIGNATURE-----\n \n\n{message}"
        );
        let commit = RemoteCommit {
            sha: object_sha(ObjectKind::Commit, raw.as_bytes()),
            signature: Some(RemoteSignature {
                signature: "-----BEGIN PGP SIGNATURE-----\n\nwsFcBAAB\n=67Gc\n-----END PGP SIGNATURE-----\n".to_string(),
                payload,
            }),
            ..unsigned("", "Squashed (#1)")
        };
        assert_eq!(rebuild_commit(&commit).unwrap(), raw.as_bytes());
    }

    #[test]
    fn a_commit_no_reading_reproduces_is_refused() {
        let mut commit = unsigned("tree x\n\nnot this commit\n", "subject");
        commit.author.date = "2026-10-16T13:44:18Z".to_string();
        let err = rebuild_commit(&commit).unwrap_err();
        assert!(err.contains(&commit.sha), "{err}");
    }

    #[test]
    fn encodes_a_tree_with_the_on_disk_directory_mode() {
        let entries = vec![
            TreeEntry {
                mode: "100644".to_string(),
                name: "a.txt".to_string(),
                sha: "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0".to_string(),
            },
            TreeEntry {
                mode: "40000".to_string(),
                name: "sub".to_string(),
                sha: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
            },
        ];
        let content = encode_tree(&entries).unwrap();
        assert!(content.starts_with(b"100644 a.txt\0"));
        assert_eq!(crate::git::object::parse_tree(&content).unwrap(), entries);
    }
}
