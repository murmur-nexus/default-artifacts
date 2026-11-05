//! The operation handlers, as the native 0.6.0 tool ran them.
//!
//! Each handler reads its inputs from the call's arguments, runs git through [`Git::git`] — one
//! `murmur:process` `run` per command — and builds the result envelope from git's output. git
//! sees `repo`, `dest`, `dest_paths`, `paths` and `path` exactly as the call gave them, and runs
//! in the work folder, so a relative value is relative to it.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::env::{log_write, Git, Log};
use crate::out::{err, err_result, fail_msg, ok_simple, ok_with, strip_fatal};
use crate::paths;
use crate::resolve;

/// Run the operation `op` names.
pub(crate) fn dispatch(g: &Git, op: &Value, log: &Log) -> Value {
    let operation = op.get("operation").and_then(|v| v.as_str()).unwrap_or("");

    match operation {
        // ── WORKING TREE ──────────────────────────────────────────────────────
        "status" => op_status(g, op, log),
        "add" => op_add(g, op, log),
        "diff" => op_diff(g, op, log),
        "restore" => op_restore(g, op, log),
        "stash" => op_stash(g, op, log),
        // ── HISTORY ──────────────────────────────────────────────────────────
        "log" => op_log(g, op, log),
        "show" => op_show(g, op, log),
        // ── SYMBOL HISTORY ────────────────────────────────────────────────────
        "symbol_history" => op_symbol_history(g, op, log),
        // ── COMMITS ──────────────────────────────────────────────────────────
        "commit" => op_commit(g, op, log),
        "cherry_pick" => op_cherry_pick(g, op, log),
        // ── BRANCHES ─────────────────────────────────────────────────────────
        "branch" => op_branch(g, op, log),
        "checkout" => op_checkout(g, op, log),
        "switch" => op_switch(g, op, log),
        // ── RESET ────────────────────────────────────────────────────────────
        "reset" => op_reset(g, op, log),
        // ── REMOTE ───────────────────────────────────────────────────────────
        "fetch" => op_fetch(g, op, log),
        "pull" => op_pull(g, op, log),
        "push" => op_push(g, op, log),
        "clone" => op_clone(g, op, log),
        "remote" => op_remote(g, op, log),
        // ── TAGS / MERGE / WORKTREES ──────────────────────────────────────────
        "tag" => op_tag(g, op, log),
        "merge" => op_merge(g, op, log),
        "worktree" => op_worktree(g, op, log),
        // create_worktree predates the worktree/add operation and takes the same inputs.
        "create_worktree" => op_create_worktree(g, op, log),
        // list_files is deliberately not an operation; use `status` instead.
        other => fail_msg(format!("unknown operation: {other}")),
    }
}

// ── Repo root resolution ──────────────────────────────────────────────────────

/// Resolve the git repository root using a three-step fallback chain:
///   1. Explicit `repo` field in the operation — used verbatim.
///   2. Auto-discover via `git rev-parse --show-toplevel`, run in the work folder.
///   3. Last resort: `"."` (the work folder).
fn resolve_repo(g: &Git, op: &Value) -> String {
    if let Some(r) = op.get("repo").and_then(|v| v.as_str()) {
        r.to_string()
    } else {
        let discovered = g.git(&["rev-parse", "--show-toplevel"]);
        Some(discovered)
            .filter(|o| o.success)
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".to_string())
    }
}

// ── WORKING TREE operations ───────────────────────────────────────────────────

pub(crate) fn op_status(g: &Git, op: &Value, log: &Log) -> Value {
    // Accept `path` as a legacy alias for `repo` — the original binary used `path` for
    // status, and existing test fixtures send `path`. If both are absent, auto-discover.
    let repo = if let Some(r) = op
        .get("repo")
        .or_else(|| op.get("path"))
        .and_then(|v| v.as_str())
    {
        r.to_string()
    } else {
        resolve_repo(g, op)
    };

    log_write(log, &format!("status: repo={repo:?}\n"));

    let out = g.git(&["-C", &repo, "status", "--porcelain=v1"]);
    log_write(
        log,
        &format!("stdout: {}\nstderr: {}\n", out.stdout, out.stderr),
    );

    if !out.success {
        return fail_msg(format!(
            "git status failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let entries: Vec<Value> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            // Porcelain v1 format: XY SP path  (XY is always exactly 2 chars)
            let xy = if line.len() >= 2 { &line[..2] } else { "??" };
            let path = if line.len() > 3 {
                line[3..].to_string()
            } else {
                String::new()
            };
            let x = xy.chars().next().unwrap_or(' ');
            let y = xy.chars().nth(1).unwrap_or(' ');
            // Map porcelain codes to human-readable status words.
            // Staged code (x) takes priority over unstaged (y).
            let code = if x != ' ' && x != '?' && x != '!' {
                x
            } else {
                y
            };
            let status = match (x, y, code) {
                ('?', '?', _) => "untracked",
                ('!', '!', _) => "ignored",
                (_, _, 'M') => "modified",
                (_, _, 'A') => "added",
                (_, _, 'D') => "deleted",
                (_, _, 'R') => "renamed",
                (_, _, 'C') => "copied",
                (_, _, 'U') => "unmerged",
                _ => "unknown",
            };
            json!({ "path": path, "status": status, "status_code": xy })
        })
        .collect();

    let message = if entries.is_empty() {
        "working tree clean".to_string()
    } else {
        format!("{} changed file(s)", entries.len())
    };

    ok_with(message, json!({ "entries": entries }))
}

pub(crate) fn op_add(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let paths: Vec<String> = match op.get("paths") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => return fail_msg("missing required field: paths (must be an array of strings)"),
    };

    if paths.is_empty() {
        return fail_msg("paths must not be empty");
    }

    log_write(log, &format!("add: repo={repo:?} paths={paths:?}\n"));

    let mut args: Vec<&str> = vec!["-C", &repo, "add", "--"];
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    args.extend_from_slice(&path_refs);

    let out = g.git(&args);
    if !out.success {
        return fail_msg(format!(
            "git add failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(
        format!("staged {} path(s)", paths.len()),
        json!({ "staged": paths }),
    )
}

pub(crate) fn op_diff(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let staged = op.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
    let path = op.get("path").and_then(|v| v.as_str());

    log_write(
        log,
        &format!("diff: repo={repo:?} staged={staged} path={path:?}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "diff"];
    if staged {
        args.push("--staged");
    }
    if let Some(p) = path {
        args.push("--");
        args.push(p);
    }

    let out = g.git(&args);
    if !out.success {
        return fail_msg(format!(
            "git diff failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with("diff complete", json!({ "diff": out.stdout }))
}

pub(crate) fn op_restore(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let paths: Vec<String> = match op.get("dest_paths") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => return fail_msg("missing required field: dest_paths (must be an array of strings)"),
    };

    if paths.is_empty() {
        return fail_msg("dest_paths must not be empty");
    }

    let staged = op.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("restore: repo={repo:?} staged={staged} dest_paths={paths:?}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "restore"];
    if staged {
        args.push("--staged");
    }
    args.push("--");
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    args.extend_from_slice(&path_refs);

    let out = g.git(&args);
    if !out.success {
        return fail_msg(format!(
            "git restore failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(
        format!("restored {} path(s)", paths.len()),
        json!({ "restored": paths }),
    )
}

pub(crate) fn op_stash(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    match op.get("subcommand").and_then(|v| v.as_str()) {
        Some("push") => stash_push(g, &repo, op, log),
        Some("pop") => stash_pop(g, &repo, op, log),
        Some("list") => stash_list(g, &repo, log),
        Some(other) => fail_msg(format!(
            "unknown stash subcommand: {other}; expected push|pop|list"
        )),
        None => fail_msg("missing required field: subcommand (push|pop|list)"),
    }
}

fn stash_push(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let message = op.get("message").and_then(|v| v.as_str());

    let mut args: Vec<&str> = vec!["-C", repo, "stash", "push"];
    if let Some(m) = message {
        args.push("-m");
        args.push(m);
    }

    log_write(
        log,
        &format!("stash push: repo={repo:?} message={message:?}\n"),
    );

    let out = g.git(&args);

    // "No local changes to save" appears on stdout or stderr depending on git version;
    // check both. Git may exit 1 in this case, so we check before the success guard.
    if out.stdout.contains("No local changes to save")
        || out.stderr.contains("No local changes to save")
    {
        return err_result(
            err::NOTHING_TO_STASH,
            "nothing to stash: working tree is clean",
        );
    }

    if !out.success {
        return fail_msg(format!(
            "git stash push failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    // A fresh push always lands at stash@{0}.
    ok_with("stash pushed", json!({ "stash_ref": "stash@{0}" }))
}

fn stash_pop(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let index = op.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
    let stash_ref = format!("stash@{{{index}}}");

    log_write(log, &format!("stash pop: repo={repo:?} index={index}\n"));

    let out = g.git(&["-C", repo, "stash", "pop", &stash_ref]);
    if !out.success {
        return fail_msg(format!(
            "git stash pop failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_simple("stash popped")
}

fn stash_list(g: &Git, repo: &str, log: &Log) -> Value {
    log_write(log, &format!("stash list: repo={repo:?}\n"));

    // %gd = reflog selector (stash@{N}), %gs = reflog subject (WIP on branch: ...)
    let out = g.git(&["-C", repo, "stash", "list", "--format=%gd|%gs"]);
    if !out.success {
        return fail_msg(format!(
            "git stash list failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let entries: Vec<Value> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (ref_part, subject) = line.split_once('|')?;
            let index = ref_part
                .trim_start_matches("stash@{")
                .trim_end_matches('}')
                .parse::<u64>()
                .unwrap_or(0);
            let branch = parse_stash_branch(subject);
            Some(json!({
                "index": index,
                "message": subject,
                "branch": branch,
            }))
        })
        .collect();

    ok_with(
        format!("{} stash entry/entries", entries.len()),
        json!({ "entries": entries }),
    )
}

fn parse_stash_branch(subject: &str) -> String {
    // Subjects look like "WIP on main: abc123 msg" or "On feature/foo: msg"
    if let Some(rest) = subject.strip_prefix("WIP on ") {
        rest.split(':').next().unwrap_or("").to_string()
    } else if let Some(rest) = subject.strip_prefix("On ") {
        rest.split(':').next().unwrap_or("").to_string()
    } else {
        String::new()
    }
}

// ── HISTORY operations ────────────────────────────────────────────────────────

pub(crate) fn op_log(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let n = op.get("n").and_then(|v| v.as_u64()).unwrap_or(20);
    let author = op.get("author").and_then(|v| v.as_str()).map(String::from);
    let since = op.get("since").and_then(|v| v.as_str()).map(String::from);
    let path = op.get("path").and_then(|v| v.as_str()).map(String::from);

    let n_str = n.to_string();
    let author_arg = author.as_deref().map(|a| format!("--author={a}"));
    let since_arg = since.as_deref().map(|s| format!("--since={s}"));

    let mut args: Vec<&str> = vec![
        "-C",
        &repo,
        "log",
        "-n",
        &n_str,
        "--format=%H|%h|%an|%aI|%s",
    ];

    if let Some(ref a) = author_arg {
        args.push(a);
    }
    if let Some(ref s) = since_arg {
        args.push(s);
    }
    if let Some(ref p) = path {
        args.push("--");
        args.push(p);
    }

    log_write(log, &format!("log: repo={repo:?} n={n}\n"));

    let out = g.git(&args);
    if !out.success {
        return fail_msg(format!(
            "git log failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let commits: Vec<Value> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut parts = line.splitn(5, '|');
            let hash = parts.next()?;
            let short_hash = parts.next()?;
            let author = parts.next()?;
            let date_iso = parts.next()?;
            let subject = parts.next().unwrap_or("");
            Some(json!({
                "hash": hash,
                "short_hash": short_hash,
                "author": author,
                "date_iso": date_iso,
                "subject": subject,
            }))
        })
        .collect();

    ok_with(
        format!("{} commit(s)", commits.len()),
        json!({ "commits": commits }),
    )
}

pub(crate) fn op_show(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let ref_str = op
        .get("ref")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD")
        .to_string();

    log_write(log, &format!("show: repo={repo:?} ref={ref_str:?}\n"));

    // 1. Commit metadata — two separate calls per spec.
    let meta_out = g.git(&[
        "-C",
        &repo,
        "show",
        "--no-patch",
        "--format=%H|%an|%aI|%s|%b",
        &ref_str,
    ]);

    if !meta_out.success {
        let msg = meta_out.stderr.to_lowercase();
        if msg.contains("bad object")
            || msg.contains("unknown revision")
            || msg.contains("not a valid object")
        {
            return err_result(err::NOT_FOUND, format!("ref '{ref_str}' does not resolve"));
        }
        return fail_msg(format!("git show failed: {}", meta_out.stderr));
    }

    // %b can span multiple lines; the first format line contains fields 0–4 split on '|',
    // and any additional lines from %b follow after.
    let mut meta_lines = meta_out.stdout.lines();
    let first_line = meta_lines.next().unwrap_or("");
    let rest_body_lines: Vec<&str> = meta_lines.collect();

    let commit = {
        let mut parts = first_line.splitn(5, '|');
        let hash = parts.next().unwrap_or("").to_string();
        let author = parts.next().unwrap_or("").to_string();
        let date_iso = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        let body_first = parts.next().unwrap_or("").to_string();
        let body = if rest_body_lines.is_empty() {
            body_first
        } else {
            format!("{}\n{}", body_first, rest_body_lines.join("\n"))
        };
        json!({
            "hash": hash,
            "author": author,
            "date_iso": date_iso,
            "subject": subject,
            "body": body,
        })
    };

    // 2. Full patch output.
    let diff_out = g.git(&["-C", &repo, "show", "--stat", "-p", &ref_str]);
    let diff = if diff_out.success {
        diff_out.stdout
    } else {
        String::new()
    };

    ok_with(
        format!("show {ref_str}"),
        json!({ "commit": commit, "diff": diff }),
    )
}

// ── SYMBOL HISTORY operations ─────────────────────────────────────────────────

/// Answer "what commit(s) last touched this symbol's interface?" without the
/// caller ever tracking a line number by hand.
///
/// Resolves `symbol_id` against `tool-code-graph`'s indexed `symbols`
/// table (via [`resolve::graph_db_path`] and [`resolve::resolve_symbol_location`]) to get the symbol's *current*
/// file path and 1-based line range, then runs `git log -L<start>,<end>:<file>`
/// and returns the most recent `n` commits (default 1) that touched any line in
/// that range. Keeping the query symbol-identity-based — not line-based like raw
/// `git blame` — is the whole point: an unrelated edit shifting lines above the
/// symbol re-indexes into a new range and this still finds the symbol's own last
/// real edit.
pub(crate) fn op_symbol_history(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let symbol_id = match op.get("symbol_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return fail_msg("missing required field: symbol_id"),
    };

    // Default 1 ("the last commit"). A zero or non-integer n falls back to 1 —
    // requesting zero commits is meaningless, not an error worth surfacing.
    let n = op
        .get("n")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .unwrap_or(1);

    log_write(
        log,
        &format!("symbol_history: repo={repo:?} symbol_id={symbol_id:?} n={n}\n"),
    );

    let located = resolve::graph_db_path(g, &repo)
        .and_then(|open| resolve::resolve_symbol_location(g, &open, &symbol_id));
    let loc = match located {
        Ok(loc) => loc,
        Err(resolve::ResolveError::OutsideRoots) => {
            return err_result(
                err::OUTSIDE_ROOTS,
                format!(
                    "symbol_history: cannot read {repo}/.murmur/code-graph.db: it is outside \
                     the work folder and every host folder this session opens to tool-git"
                ),
            );
        }
        Err(resolve::ResolveError::ReservedName { suggestion }) => {
            return err_result(
                err::RESERVED_NAME,
                format!(
                    "symbol_history: 'repo' starts with murmur-host, where this tool's host \
                     folders are opened; name the host path absolutely: '{suggestion}'"
                ),
            );
        }
        Err(resolve::ResolveError::NotIndexed) => {
            return err_result(
                err::NOT_INDEXED,
                "repo has not been indexed: no .murmur/code-graph.db found. \
                 Run index_repository (tool-code-graph) first to build the \
                 symbol graph this operation resolves against.",
            );
        }
        Err(resolve::ResolveError::NotFound) => {
            return err_result(
                err::NOT_FOUND,
                format!("symbol_history: symbol not found: {symbol_id}"),
            );
        }
        Err(resolve::ResolveError::Internal(e)) => {
            return fail_msg(format!("symbol_history: failed to read code-graph db: {e}"));
        }
    };

    // A symbol whose file has no committed content at HEAD (brand-new, untracked,
    // or staged-only) has no history yet: report an empty commit list rather than
    // letting `git log -L` fail with "there is no path ... in the commit". This is
    // distinct from not_found (scenario 4) and not_indexed (scenario 5): the
    // symbol resolved fine, it just isn't committed. It is also distinct from a
    // stale index (scenario 8) — there the file IS in HEAD, so this guard passes
    // and the real "file ... has only N lines" error surfaces below.
    let in_head = g.git(&["-C", &repo, "cat-file", "-e", &format!("HEAD:{}", loc.file)]);
    if !in_head.success {
        return ok_with(
            format!("0 commit(s) for {symbol_id}"),
            json!({
                "symbol_id": symbol_id,
                "file": loc.file,
                "start_line": loc.start_line,
                "end_line": loc.end_line,
                "commits": [],
            }),
        );
    }

    let n_str = n.to_string();
    // git log -L<start>,<end>:<file> — the -L range is against the file's state in
    // HEAD, exactly the range code-graph just indexed.
    let range = format!("-L{},{}:{}", loc.start_line, loc.end_line, loc.file);
    let out = g.git(&[
        "-C",
        &repo,
        "log",
        "-n",
        &n_str,
        "--format=%H|%h|%an|%aI|%s",
        &range,
    ]);

    if !out.success {
        // Surface git's own stderr (e.g. "file ... has only N lines" when the
        // index is stale). Not a panic, not a silently-empty result.
        return fail_msg(format!(
            "symbol_history: git log -L failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    // Each matched commit's block starts with the --format line, then a blank
    // line and the diff hunk body. Keep only lines that split cleanly into the
    // five |-delimited fields (same heuristic op_log inherits); the diff body
    // has no such shape and is naturally skipped.
    let commits: Vec<Value> = out
        .stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, '|');
            let hash = parts.next()?;
            let short_hash = parts.next()?;
            let author = parts.next()?;
            let date_iso = parts.next()?;
            let subject = parts.next()?;
            Some(json!({
                "hash": hash,
                "short_hash": short_hash,
                "author": author,
                "date_iso": date_iso,
                "subject": subject,
            }))
        })
        .collect();

    ok_with(
        format!("{} commit(s) for {symbol_id}", commits.len()),
        json!({
            "symbol_id": symbol_id,
            "file": loc.file,
            "start_line": loc.start_line,
            "end_line": loc.end_line,
            "commits": commits,
        }),
    )
}

// ── COMMITS operations ────────────────────────────────────────────────────────

pub(crate) fn op_commit(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let message = match op.get("message").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => return fail_msg("missing required field: message"),
    };

    let allow_empty = op
        .get("allow_empty")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    log_write(
        log,
        &format!("commit: repo={repo:?} allow_empty={allow_empty}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "commit", "-m", &message];
    if allow_empty {
        args.push("--allow-empty");
    }

    let out = g.git(&args);

    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("nothing to commit") {
            return err_result(
                err::NOTHING_TO_COMMIT,
                "nothing to commit; use allow_empty=true to create an empty commit",
            );
        }
        return fail_msg(format!(
            "git commit failed: {}",
            if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            }
        ));
    }

    let head = g.git(&["-C", &repo, "rev-parse", "HEAD"]);
    let hash = head.stdout.trim().to_string();
    let short_head = g.git(&["-C", &repo, "rev-parse", "--short", "HEAD"]);
    let short_hash = short_head.stdout.trim().to_string();
    let log_out = g.git(&["-C", &repo, "log", "-1", "--format=%s", "HEAD"]);
    let subject = log_out.stdout.trim().to_string();

    ok_with(
        "commit created",
        json!({ "hash": hash, "short_hash": short_hash, "subject": subject }),
    )
}

pub(crate) fn op_cherry_pick(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let ref_str = match op.get("ref").and_then(|v| v.as_str()) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => return fail_msg("missing required field: ref"),
    };

    log_write(
        log,
        &format!("cherry_pick: repo={repo:?} ref={ref_str:?}\n"),
    );

    let out = g.git(&["-C", &repo, "cherry-pick", &ref_str]);

    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("CONFLICT") || combined.contains("after resolving the conflicts") {
            return err_result(
                err::CONFLICT,
                format!(
                    "cherry-pick conflict on {ref_str}; \
                     resolve conflicts and commit, or run cherry-pick --abort"
                ),
            );
        }
        return fail_msg(format!(
            "git cherry-pick failed: {}",
            if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            }
        ));
    }

    let head = g.git(&["-C", &repo, "rev-parse", "HEAD"]);
    let hash = head.stdout.trim().to_string();
    let short_head = g.git(&["-C", &repo, "rev-parse", "--short", "HEAD"]);
    let short_hash = short_head.stdout.trim().to_string();
    let log_out = g.git(&["-C", &repo, "log", "-1", "--format=%s", "HEAD"]);
    let subject = log_out.stdout.trim().to_string();

    ok_with(
        format!("cherry-picked {ref_str}"),
        json!({ "hash": hash, "short_hash": short_hash, "subject": subject }),
    )
}

// ── BRANCHES operations ───────────────────────────────────────────────────────

pub(crate) fn op_branch(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    match op.get("subcommand").and_then(|v| v.as_str()) {
        Some("list") => branch_list(g, &repo, log),
        Some("create") => branch_create(g, &repo, op, log),
        Some("delete") => branch_delete(g, &repo, op, log),
        Some(other) => fail_msg(format!(
            "unknown branch subcommand: {other}; expected list|create|delete"
        )),
        None => fail_msg("missing required field: subcommand (list|create|delete)"),
    }
}

fn branch_list(g: &Git, repo: &str, log: &Log) -> Value {
    log_write(log, &format!("branch list: repo={repo:?}\n"));

    let out = g.git(&[
        "-C",
        repo,
        "branch",
        "-vv",
        "--format=%(refname:short)|%(HEAD)|%(upstream:short)",
    ]);
    if !out.success {
        return fail_msg(format!(
            "git branch failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let branches: Vec<Value> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, '|');
            let name = parts.next().unwrap_or("").to_string();
            let head_marker = parts.next().unwrap_or("");
            let upstream = parts.next().unwrap_or("").to_string();
            let current = head_marker == "*";
            let upstream_val = if upstream.is_empty() {
                Value::Null
            } else {
                Value::String(upstream)
            };
            json!({ "name": name, "current": current, "upstream": upstream_val })
        })
        .collect();

    ok_with(
        format!("{} branch(es)", branches.len()),
        json!({ "branches": branches }),
    )
}

fn branch_create(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let name = match op.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return fail_msg("missing required field: name"),
    };
    let from = op.get("from").and_then(|v| v.as_str()).map(String::from);

    log_write(
        log,
        &format!("branch create: repo={repo:?} name={name:?} from={from:?}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", repo, "branch", &name];
    if let Some(ref f) = from {
        args.push(f);
    }

    let out = g.git(&args);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("already exists") {
            return err_result(
                err::ALREADY_EXISTS,
                format!("branch '{name}' already exists"),
            );
        }
        return fail_msg(format!(
            "git branch create failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(format!("branch '{name}' created"), json!({ "name": name }))
}

fn branch_delete(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let name = match op.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return fail_msg("missing required field: name"),
    };
    let force = op.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("branch delete: repo={repo:?} name={name:?} force={force}\n"),
    );

    let flag = if force { "-D" } else { "-d" };
    let out = g.git(&["-C", repo, "branch", flag, &name]);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("not found") {
            return err_result(err::NOT_FOUND, format!("branch '{name}' not found"));
        }
        if combined.contains("not fully merged") {
            return err_result(
                err::NOT_MERGED,
                format!("branch '{name}' is not fully merged; use force=true to delete"),
            );
        }
        return fail_msg(format!(
            "git branch delete failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(format!("branch '{name}' deleted"), json!({ "name": name }))
}

pub(crate) fn op_checkout(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let ref_str = match op.get("ref").and_then(|v| v.as_str()) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => return fail_msg("missing required field: ref"),
    };
    let create = op.get("create").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("checkout: repo={repo:?} ref={ref_str:?} create={create}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "checkout"];
    if create {
        args.push("-b");
    }
    args.push(&ref_str);

    let out = g.git(&args);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("Your local changes") || combined.contains("would be overwritten") {
            return err_result(
                err::DIRTY_WORKING_TREE,
                "checkout aborted: uncommitted changes in working tree would be overwritten",
            );
        }
        if combined.contains("already exists") {
            return err_result(
                err::ALREADY_EXISTS,
                format!("branch '{ref_str}' already exists"),
            );
        }
        if combined.contains("did not match") || combined.contains("invalid reference") {
            return err_result(err::NOT_FOUND, format!("ref '{ref_str}' not found"));
        }
        return fail_msg(format!(
            "git checkout failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let branch_out = g.git(&["-C", &repo, "branch", "--show-current"]);
    let branch = branch_out.stdout.trim().to_string();
    let detached = branch.is_empty();

    ok_with(
        format!("checked out {ref_str}"),
        json!({ "branch": branch, "detached": detached }),
    )
}

pub(crate) fn op_switch(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let branch = match op.get("branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => return fail_msg("missing required field: branch"),
    };
    let create = op.get("create").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("switch: repo={repo:?} branch={branch:?} create={create}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "switch"];
    if create {
        args.push("-c");
    }
    args.push(&branch);

    let out = g.git(&args);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("Your local changes") || combined.contains("would be overwritten") {
            return err_result(
                err::DIRTY_WORKING_TREE,
                "switch aborted: uncommitted changes in working tree would be overwritten",
            );
        }
        if combined.contains("already exists") {
            return err_result(
                err::ALREADY_EXISTS,
                format!("branch '{branch}' already exists"),
            );
        }
        if combined.contains("invalid reference") || combined.contains("does not exist") {
            return err_result(err::NOT_FOUND, format!("branch '{branch}' not found"));
        }
        return fail_msg(format!(
            "git switch failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let branch_out = g.git(&["-C", &repo, "branch", "--show-current"]);
    let current = branch_out.stdout.trim().to_string();

    ok_with(
        format!("switched to {branch}"),
        json!({ "branch": current }),
    )
}

// ── RESET operations ──────────────────────────────────────────────────────────

pub(crate) fn op_reset(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let mode = match op.get("mode").and_then(|v| v.as_str()) {
        Some(m @ ("soft" | "mixed" | "hard")) => m.to_string(),
        Some(m) => return fail_msg(format!("invalid mode '{m}'; expected soft|mixed|hard")),
        None => return fail_msg("missing required field: mode (soft|mixed|hard)"),
    };
    let ref_str = op
        .get("ref")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD")
        .to_string();
    let mode_flag = format!("--{mode}");

    log_write(
        log,
        &format!("reset: repo={repo:?} mode={mode} ref={ref_str:?}\n"),
    );

    let out = g.git(&["-C", &repo, "reset", &mode_flag, &ref_str]);
    if !out.success {
        return fail_msg(format!(
            "git reset failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let head = g.git(&["-C", &repo, "rev-parse", "HEAD"]);
    let resolved = head.stdout.trim().to_string();

    ok_with(
        format!("reset {mode} to {ref_str}"),
        json!({ "ref": resolved }),
    )
}

// ── REMOTE operations ────────────────────────────────────────────────────────

pub(crate) fn op_fetch(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let remote = op
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin");
    let branch = op.get("branch").and_then(|v| v.as_str());

    log_write(
        log,
        &format!("fetch: repo={repo:?} remote={remote:?} branch={branch:?}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "fetch", remote];
    if let Some(b) = branch {
        args.push(b);
    }

    let out = g.git(&args);

    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("does not appear to be a git repository")
            || combined.contains("Could not read from remote")
            || combined.contains("No such remote")
            || combined.contains("unknown remote")
        {
            return err_result(
                err::REMOTE_NOT_FOUND,
                format!("remote '{remote}' not found"),
            );
        }
        return fail_msg(format!(
            "git fetch failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    // git fetch reports ref updates to stderr
    let updated_refs = parse_fetch_updated_refs(&out.stderr);
    let msg = if updated_refs.is_empty() {
        "already up to date".to_string()
    } else {
        format!("{} ref(s) updated", updated_refs.len())
    };

    ok_with(
        msg,
        json!({ "remote": remote, "updated_refs": updated_refs }),
    )
}

fn parse_fetch_updated_refs(stderr: &str) -> Vec<String> {
    // Lines like "   abc1234..def5678  main -> origin/main" signal an updated ref.
    // Only include lines containing the hash..hash pattern (fast-forward updates).
    let mut refs = Vec::new();
    for line in stderr.lines() {
        if line.contains("..") {
            if let Some(arrow) = line.find("->") {
                let right = line[arrow + 2..].trim();
                if !right.is_empty() {
                    refs.push(right.to_string());
                }
            }
        }
    }
    refs
}

pub(crate) fn op_pull(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let remote = op
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin");
    let branch = op.get("branch").and_then(|v| v.as_str());
    let rebase = op.get("rebase").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("pull: repo={repo:?} remote={remote:?} branch={branch:?} rebase={rebase}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "pull"];
    if rebase {
        args.push("--rebase");
    }
    args.push(remote);
    if let Some(b) = branch {
        args.push(b);
    }

    let out = g.git(&args);

    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("no tracking information")
            || combined.contains("There is no tracking information")
            || combined.contains("no configured pull-branch")
        {
            return err_result(
                err::NO_TRACKING_BRANCH,
                "no tracking branch configured; specify a branch explicitly",
            );
        }
        if combined.contains("CONFLICT") || combined.contains("Automatic merge failed") {
            return err_result(
                err::CONFLICT,
                "pull produced merge conflicts; resolve and commit, or run git merge --abort",
            );
        }
        return fail_msg(format!(
            "git pull failed: {}",
            if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            }
        ));
    }

    let combined = format!("{}\n{}", out.stdout, out.stderr);
    let fast_forward = combined.contains("Fast-forward");

    let commits_pulled = if combined.contains("Already up to date") {
        0i64
    } else {
        let log_out = g.git(&["-C", &repo, "log", "ORIG_HEAD..HEAD", "--oneline"]);
        if log_out.success {
            log_out.stdout.lines().filter(|l| !l.is_empty()).count() as i64
        } else {
            0
        }
    };

    let result_branch = if let Some(b) = branch {
        b.to_string()
    } else {
        g.git(&["-C", &repo, "branch", "--show-current"])
            .stdout
            .trim()
            .to_string()
    };

    ok_with(
        format!("pulled from {remote}"),
        json!({
            "remote": remote,
            "branch": result_branch,
            "fast_forward": fast_forward,
            "commits_pulled": commits_pulled,
        }),
    )
}

pub(crate) fn op_push(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let remote = op
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin");
    let branch = op.get("branch").and_then(|v| v.as_str());
    let force = op.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let set_upstream = op
        .get("set_upstream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    log_write(
        log,
        &format!(
            "push: repo={repo:?} remote={remote:?} branch={branch:?} force={force} set_upstream={set_upstream}\n"
        ),
    );

    let mut args: Vec<&str> = vec!["-C", &repo, "push", "--porcelain"];
    if force {
        args.push("--force");
    }
    if set_upstream {
        args.push("-u");
    }
    args.push(remote);
    if let Some(b) = branch {
        args.push(b);
    }

    let out = g.git(&args);

    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("non-fast-forward") || combined.contains("[rejected]") {
            return err_result(
                err::NON_FAST_FORWARD,
                "push rejected: remote has commits not in local; use force=true to override",
            );
        }
        if combined.contains("no upstream branch") || combined.contains("has no upstream") {
            return err_result(
                err::NO_UPSTREAM,
                "no upstream branch configured; use set_upstream=true",
            );
        }
        return fail_msg(format!(
            "git push failed: {}",
            if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            }
        ));
    }

    let push_branch = if let Some(b) = branch {
        b.to_string()
    } else {
        g.git(&["-C", &repo, "branch", "--show-current"])
            .stdout
            .trim()
            .to_string()
    };

    // Parse full remote ref from porcelain stdout; fall back to constructing it.
    let remote_ref = parse_push_remote_ref(&out.stdout, &push_branch);

    ok_with(
        format!("pushed to {remote}/{push_branch}"),
        json!({ "remote": remote, "branch": push_branch, "remote_ref": remote_ref }),
    )
}

/// Parse the remote ref (right side of `from:to`) from `git push --porcelain` stdout.
/// Porcelain output format per line: `<flag>\t<from>:<to>[\t<summary>]`
fn parse_push_remote_ref(stdout: &str, fallback_branch: &str) -> String {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("To ") || trimmed == "Done" || trimmed.is_empty() {
            continue;
        }
        // Split by tab; field[1] is "from:to"
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() >= 2 {
            let from_to = parts[1];
            if let Some(colon) = from_to.rfind(':') {
                let to = &from_to[colon + 1..];
                if to.starts_with("refs/") {
                    return to.to_string();
                }
            }
        }
    }
    if fallback_branch.is_empty() {
        String::new()
    } else {
        format!("refs/heads/{fallback_branch}")
    }
}

pub(crate) fn op_clone(g: &Git, op: &Value, log: &Log) -> Value {
    let url = match op.get("url").and_then(|v| v.as_str()) {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => return fail_msg("missing required field: url"),
    };
    let dest = match op.get("dest").and_then(|v| v.as_str()) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => return fail_msg("missing required field: dest"),
    };
    let branch = op.get("branch").and_then(|v| v.as_str());
    let depth = op.get("depth").and_then(|v| v.as_u64());

    log_write(
        log,
        &format!("clone: url={url:?} dest={dest:?} branch={branch:?} depth={depth:?}\n"),
    );

    if paths::dest_exists(g, &dest) {
        return err_result(
            err::DEST_EXISTS,
            format!("destination '{dest}' already exists"),
        );
    }

    let depth_str = depth.map(|d| d.to_string());
    let mut args: Vec<&str> = vec!["clone"];
    if let Some(b) = branch {
        args.push("--branch");
        args.push(b);
    }
    if let Some(ref d) = depth_str {
        args.push("--depth");
        args.push(d);
    }
    args.push(&url);
    args.push(&dest);

    let out = g.git(&args);

    if !out.success {
        // An absolute `dest` no held host folder covers skips the check above; git's own check
        // refuses a non-empty folder there with the same answer.
        if out
            .stderr
            .contains("already exists and is not an empty directory")
        {
            return err_result(
                err::DEST_EXISTS,
                format!("destination '{dest}' already exists"),
            );
        }
        return err_result(
            err::CLONE_FAILED,
            format!(
                "clone failed: {}",
                if out.stderr.is_empty() {
                    "unknown error"
                } else {
                    &out.stderr
                }
            ),
        );
    }

    let head_out = g.git(&["-C", &dest, "symbolic-ref", "--short", "HEAD"]);
    let default_branch = head_out.stdout.trim().to_string();

    ok_with(
        format!("cloned to {dest}"),
        json!({ "dest": dest, "default_branch": default_branch }),
    )
}

pub(crate) fn op_remote(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    match op.get("subcommand").and_then(|v| v.as_str()) {
        Some("list") => remote_list(g, &repo, log),
        Some("add") => remote_add(g, &repo, op, log),
        Some("remove") => remote_remove(g, &repo, op, log),
        Some(other) => fail_msg(format!(
            "unknown remote subcommand: {other}; expected list|add|remove"
        )),
        None => fail_msg("missing required field: subcommand (list|add|remove)"),
    }
}

fn remote_list(g: &Git, repo: &str, log: &Log) -> Value {
    log_write(log, &format!("remote list: repo={repo:?}\n"));

    // `git remote -v` emits two lines per remote: one for fetch, one for push.
    // Format: "<name>\t<url> (fetch|push)"
    let out = g.git(&["-C", repo, "remote", "-v"]);
    if !out.success {
        return fail_msg(format!(
            "git remote -v failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let mut fetch_urls: HashMap<String, String> = HashMap::new();
    let mut push_urls: HashMap<String, String> = HashMap::new();

    for line in out.stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        if let Some(url) = rest.strip_suffix(" (fetch)") {
            fetch_urls.insert(name.to_string(), url.to_string());
        } else if let Some(url) = rest.strip_suffix(" (push)") {
            push_urls.insert(name.to_string(), url.to_string());
        }
    }

    let mut names: Vec<&str> = fetch_urls.keys().map(String::as_str).collect();
    names.sort_unstable();

    let remotes: Vec<Value> = names
        .iter()
        .map(|name| {
            let fetch_url = fetch_urls.get(*name).cloned().unwrap_or_default();
            let push_url = push_urls
                .get(*name)
                .cloned()
                .unwrap_or_else(|| fetch_url.clone());
            json!({ "name": *name, "fetch_url": fetch_url, "push_url": push_url })
        })
        .collect();

    ok_with(
        format!("{} remote(s)", remotes.len()),
        json!({ "remotes": remotes }),
    )
}

fn remote_add(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let name = match op.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return fail_msg("missing required field: name"),
    };
    let url = match op.get("url").and_then(|v| v.as_str()) {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => return fail_msg("missing required field: url"),
    };

    log_write(
        log,
        &format!("remote add: repo={repo:?} name={name:?} url={url:?}\n"),
    );

    let out = g.git(&["-C", repo, "remote", "add", &name, &url]);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("already exists") {
            return err_result(
                err::ALREADY_EXISTS,
                format!("remote '{name}' already exists"),
            );
        }
        return fail_msg(format!(
            "git remote add failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(
        format!("remote '{name}' added"),
        json!({ "name": name, "url": url }),
    )
}

fn remote_remove(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let name = match op.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return fail_msg("missing required field: name"),
    };

    log_write(
        log,
        &format!("remote remove: repo={repo:?} name={name:?}\n"),
    );

    let out = g.git(&["-C", repo, "remote", "remove", &name]);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("No such remote") || combined.contains("does not exist") {
            return err_result(err::NOT_FOUND, format!("remote '{name}' not found"));
        }
        return fail_msg(format!(
            "git remote remove failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(format!("remote '{name}' removed"), json!({ "name": name }))
}

// ── TAG operations ────────────────────────────────────────────────────────────

pub(crate) fn op_tag(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    match op.get("subcommand").and_then(|v| v.as_str()) {
        Some("list") => tag_list(g, &repo, log),
        Some("create") => tag_create(g, &repo, op, log),
        Some(other) => fail_msg(format!(
            "unknown tag subcommand: {other}; expected list|create"
        )),
        None => fail_msg("missing required field: subcommand (list|create)"),
    }
}

fn tag_list(g: &Git, repo: &str, log: &Log) -> Value {
    log_write(log, &format!("tag list: repo={repo:?}\n"));

    let out = g.git(&[
        "-C",
        repo,
        "tag",
        "-l",
        "--format=%(refname:short)|%(objectname:short)",
    ]);
    if !out.success {
        return fail_msg(format!(
            "git tag list failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let tags: Vec<Value> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (name, ref_hash) = line.split_once('|')?;
            Some(json!({ "name": name, "ref": ref_hash }))
        })
        .collect();

    ok_with(format!("{} tag(s)", tags.len()), json!({ "tags": tags }))
}

fn tag_create(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let name = match op.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return fail_msg("missing required field: name"),
    };
    let ref_str = op
        .get("ref")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD")
        .to_string();
    let message = op.get("message").and_then(|v| v.as_str());

    log_write(
        log,
        &format!("tag create: repo={repo:?} name={name:?} ref={ref_str:?} message={message:?}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", repo, "tag"];
    if let Some(m) = message {
        args.push("-a");
        args.push("-m");
        args.push(m);
    }
    args.push(&name);
    args.push(&ref_str);

    let out = g.git(&args);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("already exists") {
            return err_result(err::ALREADY_EXISTS, format!("tag '{name}' already exists"));
        }
        return fail_msg(format!(
            "git tag create failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    // Resolve the short hash the tag points to
    let rev_out = g.git(&["-C", repo, "rev-parse", "--short", &name]);
    let resolved_ref = rev_out.stdout.trim().to_string();

    ok_with(
        format!("tag '{name}' created"),
        json!({ "name": name, "ref": resolved_ref }),
    )
}

// ── MERGE operations ──────────────────────────────────────────────────────────

pub(crate) fn op_merge(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    let branch = match op.get("branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => return fail_msg("missing required field: branch"),
    };
    let message = op.get("message").and_then(|v| v.as_str());
    let ff_only = op.get("ff_only").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("merge: repo={repo:?} branch={branch:?} ff_only={ff_only}\n"),
    );

    // Record HEAD before merge to detect whether a new commit was created.
    let head_before = g.git(&["-C", &repo, "rev-parse", "HEAD"]);
    let head_before_hash = head_before.stdout.trim().to_string();

    let mut args: Vec<&str> = vec!["-C", &repo, "merge"];
    if ff_only {
        args.push("--ff-only");
    }
    if let Some(m) = message {
        args.push("-m");
        args.push(m);
    } else {
        // Suppress interactive editor in non-TTY environments.
        args.push("--no-edit");
    }
    args.push(&branch);

    let out = g.git(&args);
    let combined = format!("{}\n{}", out.stdout, out.stderr);

    if !out.success {
        if combined.contains("CONFLICT")
            || combined.contains("Automatic merge failed")
            || combined.contains("fix conflicts")
        {
            return err_result(
                err::CONFLICT,
                "merge conflict: resolve conflicts and commit, or run git merge --abort",
            );
        }
        if combined.contains("Not possible to fast-forward")
            || combined.contains("cannot fast-forward")
            || combined.contains("fast-forward is not possible")
        {
            return err_result(
                err::FF_ONLY_FAILED,
                format!("cannot fast-forward: '{branch}' has diverged from the current branch"),
            );
        }
        return fail_msg(format!(
            "git merge failed: {}",
            if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            }
        ));
    }

    let fast_forward = combined.contains("Fast-forward");
    let head_after = g.git(&["-C", &repo, "rev-parse", "HEAD"]);
    let head_after_hash = head_after.stdout.trim().to_string();

    // merged_commit is null for fast-forwards (no new merge commit) or no-ops.
    let merged_commit = if fast_forward || head_after_hash == head_before_hash {
        Value::Null
    } else {
        Value::String(head_after_hash)
    };

    ok_with(
        format!("merged '{branch}'"),
        json!({ "merged_commit": merged_commit, "fast_forward": fast_forward }),
    )
}

// ── WORKTREE operations ───────────────────────────────────────────────────────

pub(crate) fn op_worktree(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);

    match op.get("subcommand").and_then(|v| v.as_str()) {
        Some("add") => worktree_add(g, &repo, op, log),
        Some("list") => worktree_list(g, &repo, log),
        Some("remove") => worktree_remove(g, &repo, op, log),
        Some(other) => fail_msg(format!(
            "unknown worktree subcommand: {other}; expected add|list|remove"
        )),
        None => fail_msg("missing required field: subcommand (add|list|remove)"),
    }
}

fn worktree_add(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let dest = match op.get("dest").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return fail_msg("missing required field: dest"),
    };
    let branch = match op.get("branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => return fail_msg("missing required field: branch"),
    };

    log_write(
        log,
        &format!("worktree add: repo={repo:?} dest={dest:?} branch={branch:?}\n"),
    );

    let out = g.git(&["-C", repo, "worktree", "add", &dest, &branch]);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("is already checked out")
            || combined.contains("already checked out")
            || combined.contains("already used by worktree")
        {
            return err_result(
                err::BRANCH_CONFLICT,
                format!("branch '{branch}' is already checked out"),
            );
        }
        return fail_msg(strip_fatal(&out.stderr));
    }

    ok_with(
        format!("worktree created at {dest} on branch {branch}"),
        json!({ "path": dest, "branch": branch }),
    )
}

fn worktree_list(g: &Git, repo: &str, log: &Log) -> Value {
    log_write(log, &format!("worktree list: repo={repo:?}\n"));

    let out = g.git(&["-C", repo, "worktree", "list", "--porcelain"]);
    if !out.success {
        return fail_msg(format!(
            "git worktree list failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    let mut worktrees: Vec<Value> = Vec::new();
    let mut wt_path = String::new();
    let mut head = String::new();
    let mut branch = String::new();
    let mut bare = false;

    for line in out.stdout.lines() {
        if line.is_empty() {
            if !wt_path.is_empty() {
                worktrees.push(json!({
                    "path": wt_path,
                    "branch": branch,
                    "head": head,
                    "bare": bare,
                }));
                wt_path.clear();
                head.clear();
                branch.clear();
                bare = false;
            }
        } else if let Some(p) = line.strip_prefix("worktree ") {
            wt_path = p.to_string();
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.trim_start_matches("refs/heads/").to_string();
        } else if line == "bare" {
            bare = true;
        }
        // "detached" case: branch field stays empty
    }
    // Handle last block (no trailing blank line)
    if !wt_path.is_empty() {
        worktrees.push(json!({
            "path": wt_path,
            "branch": branch,
            "head": head,
            "bare": bare,
        }));
    }

    ok_with(
        format!("{} worktree(s)", worktrees.len()),
        json!({ "worktrees": worktrees }),
    )
}

fn worktree_remove(g: &Git, repo: &str, op: &Value, log: &Log) -> Value {
    let dest = match op.get("dest").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return fail_msg("missing required field: dest"),
    };
    let force = op.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    log_write(
        log,
        &format!("worktree remove: repo={repo:?} dest={dest:?} force={force}\n"),
    );

    let mut args: Vec<&str> = vec!["-C", repo, "worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&dest);

    let out = g.git(&args);
    if !out.success {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if combined.contains("is not a worktree") || combined.contains("not a working tree") {
            return err_result(err::NOT_FOUND, format!("'{dest}' is not a worktree"));
        }
        if combined.contains("contains modified or untracked files") || combined.contains("unclean")
        {
            return err_result(
                err::DIRTY,
                format!("worktree '{dest}' has uncommitted changes; use force=true to remove"),
            );
        }
        return fail_msg(format!(
            "git worktree remove failed: {}",
            if out.stderr.is_empty() {
                "unknown error"
            } else {
                &out.stderr
            }
        ));
    }

    ok_with(format!("worktree removed: {dest}"), json!({ "path": dest }))
}

/// `create_worktree`: the operation `worktree` / `add` runs, under its older name. The op
/// carries `dest` and `branch` at the top level, which `worktree_add` reads.
pub(crate) fn op_create_worktree(g: &Git, op: &Value, log: &Log) -> Value {
    let repo = resolve_repo(g, op);
    worktree_add(g, &repo, op, log)
}
