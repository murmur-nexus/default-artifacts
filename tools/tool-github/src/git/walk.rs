//! Walk the objects a local commit needs that the remote does not have yet, reading through
//! both the loose store and any opened packs, and refusing anything this tool cannot safely
//! reproduce: a signed commit, a tree with a gitlink, or an object this repository cannot
//! account for at all.
//!
//! The walk is the only place that reads objects, so every refusal in this module fires
//! before [`crate::github`] builds a single upload request.

use std::collections::HashSet;

use super::object::{Commit, ObjectKind, TreeEntry};
use super::store::{ObjectStore, ReadError};

/// Why a push cannot proceed, named the way the object it is about is named in the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// A commit in the range carries a `gpgsig` header.
    Signed(String),
    /// A tree in the range has a gitlink (submodule) entry, named `<tree-sha>:<path>`.
    Gitlink(String),
    /// A located object (loose or packed) failed to parse as well-formed git object content.
    Corrupt(String),
    /// The local range does not build on the remote tip, and force was not requested/allowed.
    NotFastForward,
    /// The repository's object store could not answer a read: a full, ready-to-report
    /// `repo_unreadable` message (naming the object, file or size at fault).
    Unreadable(String),
}

/// One blob to upload: its sha (for a post-upload identity check) and raw content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBlob {
    pub sha: String,
    pub content: Vec<u8>,
}

/// One tree to upload, entries in on-disk order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTree {
    pub sha: String,
    pub entries: Vec<TreeEntry>,
}

/// One commit to upload, oldest-parent-first order relative to the rest of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCommit {
    pub sha: String,
    pub commit: Commit,
}

/// Everything a push needs to send, in the order it must be sent: blobs, then trees (children
/// before parents), then commits (parents before children).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PushPlan {
    pub blobs: Vec<PlannedBlob>,
    pub trees: Vec<PlannedTree>,
    pub commits: Vec<PlannedCommit>,
    /// Whether `remote_sha` is an ancestor of `head_sha` — a fast-forward. Always `true` when
    /// `remote_sha` is `None` (the branch does not exist yet).
    pub fast_forward: bool,
}

impl PushPlan {
    /// Total objects this plan uploads.
    pub fn object_count(&self) -> usize {
        self.blobs.len() + self.trees.len() + self.commits.len()
    }
}

/// Plan a push of `head_sha`: every commit reachable from it that the remote does not already
/// have, and the trees and blobs those commits need.
///
/// `on_remote(sha)` answers whether the remote already has a commit. The walk stops at any
/// commit it has, without reading or vetting it: GitHub holding a commit means it holds that
/// commit's whole history and every object under it. So a signed commit GitHub made itself (a
/// web merge) is never re-uploaded, and neither is anything already pushed on another branch.
/// `remote_sha`, the branch's current tip, counts as on the remote without asking.
///
/// Trees and blobs under the commits the walk stopped at are left out of the plan too, as far
/// as the local store holds them, so a push uploads only what changed since.
///
/// When `remote_sha` is set and is not an ancestor of `head_sha` in the local history, the push
/// is not a fast-forward and is refused, before `on_remote` is ever asked, unless `force`.
pub fn plan_push<E: From<PlanError>>(
    store: &ObjectStore,
    head_sha: &str,
    remote_sha: Option<&str>,
    force: bool,
    on_remote: &mut dyn FnMut(&str) -> Result<bool, E>,
) -> Result<PushPlan, E> {
    let fast_forward = match remote_sha {
        Some(remote_sha) => is_ancestor(store, remote_sha, head_sha)?,
        None => true,
    };
    if !fast_forward && !force {
        return Err(PlanError::NotFastForward.into());
    }

    let mut walk = CommitWalk {
        store,
        remote_sha,
        on_remote,
        visited: HashSet::new(),
        boundary: Vec::new(),
        new: Vec::new(),
    };
    walk.visit(head_sha)?;

    let mut plan = PushPlan {
        fast_forward,
        ..Default::default()
    };
    let mut object_seen = HashSet::new();
    for sha in &walk.boundary {
        mark_present(store, sha, &mut object_seen);
    }
    for (sha, commit) in walk.new {
        collect_tree(store, &commit.tree, &mut object_seen, &mut plan)?;
        plan.commits.push(PlannedCommit { sha, commit });
    }
    Ok(plan)
}

/// Whether `ancestor` is `head` or reachable from it through local parents. A parent missing
/// from the store (a shallow clone's edge) ends that line of the search rather than failing it.
fn is_ancestor(store: &ObjectStore, ancestor: &str, head: &str) -> Result<bool, PlanError> {
    let mut visited = HashSet::new();
    let mut pending = vec![head.to_string()];
    while let Some(sha) = pending.pop() {
        if sha == ancestor {
            return Ok(true);
        }
        if !visited.insert(sha.clone()) {
            continue;
        }
        let commit = match read_commit(store, &sha) {
            Ok(commit) => commit,
            Err(PlanError::Unreadable(_)) if sha != head => continue,
            Err(error) => return Err(error),
        };
        pending.extend(commit.parents);
    }
    Ok(false)
}

/// The depth-first walk from the pushed commit down to the commits the remote already has.
struct CommitWalk<'a, 's, E> {
    store: &'a ObjectStore<'s>,
    remote_sha: Option<&'a str>,
    on_remote: &'a mut dyn FnMut(&str) -> Result<bool, E>,
    visited: HashSet<String>,
    /// Commits the remote has, where the walk stopped.
    boundary: Vec<String>,
    /// Commits the remote lacks, ancestors first.
    new: Vec<(String, Commit)>,
}

impl<E: From<PlanError>> CommitWalk<'_, '_, E> {
    fn visit(&mut self, sha: &str) -> Result<(), E> {
        if !self.visited.insert(sha.to_string()) {
            return Ok(());
        }
        if Some(sha) == self.remote_sha || (self.on_remote)(sha)? {
            self.boundary.push(sha.to_string());
            return Ok(());
        }
        let commit = read_commit(self.store, sha)?;
        if commit.signed {
            return Err(PlanError::Signed(sha.to_string()).into());
        }
        for parent in &commit.parents {
            self.visit(parent)?;
        }
        self.new.push((sha.to_string(), commit));
        Ok(())
    }
}

fn read_commit(store: &ObjectStore, sha: &str) -> Result<Commit, PlanError> {
    let raw = store.read(sha).map_err(read_error)?;
    if raw.kind != ObjectKind::Commit {
        return Err(PlanError::Corrupt(sha.to_string()));
    }
    super::object::parse_commit(&raw.content).map_err(|_| PlanError::Corrupt(sha.to_string()))
}

/// Mark the tree of `commit_sha`, a commit the remote has, and everything under it as already
/// on the remote. Best effort: whatever the local store cannot read is simply not marked, and
/// so is uploaded again if a new commit needs it.
fn mark_present(store: &ObjectStore, commit_sha: &str, seen: &mut HashSet<String>) {
    if let Ok(commit) = read_commit(store, commit_sha) {
        mark_tree(store, &commit.tree, seen);
    }
}

fn mark_tree(store: &ObjectStore, sha: &str, seen: &mut HashSet<String>) {
    if !seen.insert(sha.to_string()) {
        return;
    }
    let Ok(raw) = store.read(sha) else { return };
    let Ok(entries) = super::object::parse_tree(&raw.content) else {
        return;
    };
    for entry in entries {
        if entry.is_tree() {
            mark_tree(store, &entry.sha, seen);
        } else if !entry.is_gitlink() {
            seen.insert(entry.sha);
        }
    }
}

/// Depth-first, children-before-parent collection of a tree and everything under it, deduped
/// against `seen` across the whole push.
fn collect_tree(
    store: &ObjectStore,
    sha: &str,
    seen: &mut HashSet<String>,
    plan: &mut PushPlan,
) -> Result<(), PlanError> {
    if !seen.insert(sha.to_string()) {
        return Ok(());
    }
    let raw = store.read(sha).map_err(read_error)?;
    if raw.kind != ObjectKind::Tree {
        return Err(PlanError::Corrupt(sha.to_string()));
    }
    let entries =
        super::object::parse_tree(&raw.content).map_err(|_| PlanError::Corrupt(sha.to_string()))?;
    for entry in &entries {
        if entry.is_gitlink() {
            return Err(PlanError::Gitlink(format!("{sha}:{}", entry.name)));
        }
        if entry.is_tree() {
            collect_tree(store, &entry.sha, seen, plan)?;
        } else {
            collect_blob(store, &entry.sha, seen, plan)?;
        }
    }
    plan.trees.push(PlannedTree {
        sha: sha.to_string(),
        entries,
    });
    Ok(())
}

fn collect_blob(
    store: &ObjectStore,
    sha: &str,
    seen: &mut HashSet<String>,
    plan: &mut PushPlan,
) -> Result<(), PlanError> {
    if !seen.insert(sha.to_string()) {
        return Ok(());
    }
    let raw = store.read(sha).map_err(read_error)?;
    if raw.kind != ObjectKind::Blob {
        return Err(PlanError::Corrupt(sha.to_string()));
    }
    plan.blobs.push(PlannedBlob {
        sha: sha.to_string(),
        content: raw.content,
    });
    Ok(())
}

fn read_error(err: ReadError) -> PlanError {
    match err {
        ReadError::NotFound(sha) => PlanError::Unreadable(format!(
            "object {sha} is not a loose object and is not present in any opened pack index"
        )),
        ReadError::Corrupt(sha) => PlanError::Corrupt(sha),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_fixture::Fixture;

    fn store(fx: &Fixture) -> ObjectStore<'static> {
        // Leaked so the store can outlive the function body without borrowing `fx`'s tempdir
        // path across calls in these tests; each test owns exactly one `Fixture`.
        let git_dir: &'static std::path::Path = Box::leak(fx.git_dir().into_boxed_path());
        ObjectStore::open(git_dir).expect("open object store")
    }

    /// Plan against a remote that has exactly the commits in `remote_has`.
    fn plan_with(
        fx: &Fixture,
        head: &str,
        remote_sha: Option<&str>,
        force: bool,
        remote_has: &[&str],
    ) -> Result<PushPlan, PlanError> {
        plan_push(&store(fx), head, remote_sha, force, &mut |sha: &str| {
            Ok(remote_has.contains(&sha))
        })
    }

    fn plan(
        fx: &Fixture,
        head: &str,
        remote_sha: Option<&str>,
        force: bool,
    ) -> Result<PushPlan, PlanError> {
        plan_with(fx, head, remote_sha, force, &[])
    }

    #[test]
    fn plans_a_two_commit_history_from_scratch() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit("first");
        fx.write_file("a.txt", b"two");
        let c2 = fx.commit("second");

        let plan = plan(&fx, &c2, None, false).unwrap();
        assert!(plan.fast_forward);
        assert_eq!(plan.commits.len(), 2);
        assert_eq!(plan.commits[0].sha, c1);
        assert_eq!(plan.commits[1].sha, c2);
        assert!(plan.trees.len() >= 2);
        assert_eq!(plan.blobs.len(), 2);
    }

    #[test]
    fn a_fast_forward_excludes_the_boundary_commit() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit("first");
        fx.write_file("a.txt", b"two");
        let c2 = fx.commit("second");

        let plan = plan(&fx, &c2, Some(&c1), false).unwrap();
        assert!(plan.fast_forward);
        assert_eq!(plan.commits.len(), 1);
        assert_eq!(plan.commits[0].sha, c2);
    }

    #[test]
    fn a_diverged_history_is_refused_without_force() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit("first");
        fx.write_file("a.txt", b"two");
        let _c2 = fx.commit("second");
        fx.write_file("a.txt", b"three-unrelated");
        let unrelated = fx.commit_root("unrelated");
        let _ = c1;

        let err = plan(&fx, &unrelated, Some("0".repeat(40).as_str()), false).unwrap_err();
        assert_eq!(err, PlanError::NotFastForward);
    }

    #[test]
    fn force_replans_the_whole_local_history() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit("first");

        let fake_remote = "1".repeat(40);
        let plan = plan(&fx, &c1, Some(&fake_remote), true).unwrap();
        assert!(!plan.fast_forward);
        assert_eq!(plan.commits.len(), 1);
        assert_eq!(plan.commits[0].sha, c1);
    }

    #[test]
    fn a_gitlink_is_refused() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        fx.write_gitlink("sub", &"a".repeat(40));
        let c1 = fx.commit("with a submodule");

        let err = plan(&fx, &c1, None, false).unwrap_err();
        assert!(matches!(err, PlanError::Gitlink(_)), "{err:?}");
    }

    #[test]
    fn a_signed_commit_is_refused() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit_signed("signed");

        let err = plan(&fx, &c1, None, false).unwrap_err();
        assert_eq!(err, PlanError::Signed(c1));
    }

    #[test]
    fn an_object_absent_from_every_store_is_refused_by_name() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let _c1 = fx.commit("first");
        let nonexistent = "f".repeat(40);

        let err = plan(&fx, &nonexistent, None, false).unwrap_err();
        match err {
            PlanError::Unreadable(message) => assert!(message.contains(&nonexistent), "{message}"),
            other => panic!("expected PlanError::Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn the_walk_stops_at_a_signed_commit_the_remote_already_has() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        fx.write_file("b.txt", b"unchanged");
        let _c1 = fx.commit("first");
        let merged = fx.commit_signed("squash-merged on GitHub");
        fx.write_file("a.txt", b"two");
        let c3 = fx.commit("new work");

        let plan = plan_with(&fx, &c3, None, false, &[&merged]).unwrap();
        assert_eq!(plan.commits.len(), 1);
        assert_eq!(plan.commits[0].sha, c3);
        assert_eq!(plan.trees.len(), 1);
        assert_eq!(
            plan.blobs.len(),
            1,
            "only the changed file; b.txt is under the remote's commit"
        );
        assert_eq!(plan.blobs[0].content, b"two");
    }

    #[test]
    fn a_commit_already_on_the_remote_needs_no_upload() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit_signed("signed, on GitHub");

        let plan = plan_with(&fx, &c1, None, false, &[&c1]).unwrap();
        assert_eq!(plan.object_count(), 0);
    }

    #[test]
    fn a_parent_missing_locally_but_on_the_remote_bounds_the_walk() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit("first");
        fx.write_file("a.txt", b"two");
        let c2 = fx.commit("second");
        // A shallow clone's edge: the parent is known by sha only.
        std::fs::remove_file(fx.git_dir().join("objects").join(&c1[..2]).join(&c1[2..])).unwrap();

        let plan = plan_with(&fx, &c2, Some(&c1), false, &[]).unwrap();
        assert!(plan.fast_forward);
        assert_eq!(plan.commits.len(), 1);
        assert_eq!(plan.commits[0].sha, c2);
    }

    #[test]
    fn a_non_fast_forward_is_refused_before_the_remote_is_asked() {
        let fx = Fixture::new();
        fx.write_file("a.txt", b"one");
        let c1 = fx.commit("first");

        let err = plan_push(
            &store(&fx),
            &c1,
            Some(&"0".repeat(40)),
            false,
            &mut |sha: &str| -> Result<bool, PlanError> { panic!("asked about {sha}") },
        )
        .unwrap_err();
        assert_eq!(err, PlanError::NotFastForward);
    }
}
