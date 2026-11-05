//! The ordered calls the parity suites run against [`super::fixture`]: every operation and
//! subcommand, and the failures `nothing_to_commit`, `non_fast_forward`, `dest_exists`, a merge
//! conflict, a missing required field and an unknown operation. Each call names the repository
//! `repo`, relative to the work folder.

use serde_json::{json, Value};

/// The symbol the fixture's code-graph database holds.
pub const SID: &str = "rust://fixture/#sym(->())";

pub struct Step {
    /// A short name, for messages.
    pub name: &'static str,
    pub data: Value,
}

pub fn steps() -> Vec<Step> {
    let s = |name, data| Step { name, data };
    vec![
        s("status", json!({"operation": "status", "repo": "repo"})),
        s("fetch", json!({"operation": "fetch", "repo": "repo"})),
        s("pull", json!({"operation": "pull", "repo": "repo"})),
        s("diff", json!({"operation": "diff", "repo": "repo"})),
        s(
            "add",
            json!({"operation": "add", "repo": "repo", "paths": ["NEW.md"]}),
        ),
        s(
            "diff staged",
            json!({"operation": "diff", "repo": "repo", "staged": true}),
        ),
        s(
            "restore",
            json!({"operation": "restore", "repo": "repo", "dest_paths": ["README.md"]}),
        ),
        s(
            "stash push",
            json!({"operation": "stash", "repo": "repo", "subcommand": "push", "message": "wip"}),
        ),
        s(
            "stash list",
            json!({"operation": "stash", "repo": "repo", "subcommand": "list"}),
        ),
        s(
            "stash pop",
            json!({"operation": "stash", "repo": "repo", "subcommand": "pop"}),
        ),
        s("log", json!({"operation": "log", "repo": "repo", "n": 5})),
        s("show", json!({"operation": "show", "repo": "repo"})),
        s(
            "symbol_history",
            json!({"operation": "symbol_history", "repo": "repo", "symbol_id": SID}),
        ),
        s(
            "add again",
            json!({"operation": "add", "repo": "repo", "paths": ["NEW.md"]}),
        ),
        s(
            "commit",
            json!({"operation": "commit", "repo": "repo", "message": "add new"}),
        ),
        s(
            "commit nothing",
            json!({"operation": "commit", "repo": "repo", "message": "again"}),
        ),
        s(
            "cherry_pick",
            json!({"operation": "cherry_pick", "repo": "repo", "ref": "topic"}),
        ),
        s(
            "branch list",
            json!({"operation": "branch", "repo": "repo", "subcommand": "list"}),
        ),
        s(
            "branch create",
            json!({"operation": "branch", "repo": "repo", "subcommand": "create", "name": "made"}),
        ),
        s(
            "branch delete",
            json!({"operation": "branch", "repo": "repo", "subcommand": "delete", "name": "made"}),
        ),
        s(
            "checkout",
            json!({"operation": "checkout", "repo": "repo", "ref": "feature"}),
        ),
        s(
            "switch",
            json!({"operation": "switch", "repo": "repo", "branch": "main"}),
        ),
        s(
            "reset",
            json!({"operation": "reset", "repo": "repo", "mode": "mixed", "ref": "HEAD"}),
        ),
        s("push", json!({"operation": "push", "repo": "repo"})),
        s(
            "push rejected",
            json!({"operation": "push", "repo": "repo", "branch": "side"}),
        ),
        s(
            "clone",
            json!({"operation": "clone", "url": "remote.git", "dest": "clone"}),
        ),
        s(
            "clone existing",
            json!({"operation": "clone", "url": "remote.git", "dest": "clone"}),
        ),
        s(
            "remote list",
            json!({"operation": "remote", "repo": "repo", "subcommand": "list"}),
        ),
        s(
            "remote add",
            json!({"operation": "remote", "repo": "repo", "subcommand": "add",
                   "name": "mirror", "url": "../remote.git"}),
        ),
        s(
            "remote remove",
            json!({"operation": "remote", "repo": "repo", "subcommand": "remove", "name": "mirror"}),
        ),
        s(
            "tag create",
            json!({"operation": "tag", "repo": "repo", "subcommand": "create",
                   "name": "v1", "message": "release v1"}),
        ),
        s(
            "tag list",
            json!({"operation": "tag", "repo": "repo", "subcommand": "list"}),
        ),
        s(
            "merge",
            json!({"operation": "merge", "repo": "repo", "branch": "feature",
                   "message": "merge feature"}),
        ),
        s(
            "merge conflict",
            json!({"operation": "merge", "repo": "repo", "branch": "conflict"}),
        ),
        s(
            "reset hard",
            json!({"operation": "reset", "repo": "repo", "mode": "hard", "ref": "HEAD"}),
        ),
        s(
            "worktree add",
            json!({"operation": "worktree", "repo": "repo", "subcommand": "add",
                   "dest": "wt", "branch": "topic"}),
        ),
        s(
            "worktree list",
            json!({"operation": "worktree", "repo": "repo", "subcommand": "list"}),
        ),
        s(
            "worktree remove",
            json!({"operation": "worktree", "repo": "repo", "subcommand": "remove", "dest": "wt"}),
        ),
        s(
            "create_worktree",
            json!({"operation": "create_worktree", "repo": "repo", "dest": "wt2", "branch": "side"}),
        ),
        s(
            "missing field",
            json!({"operation": "commit", "repo": "repo"}),
        ),
        s("unknown operation", json!({"operation": "bogus"})),
    ]
}
