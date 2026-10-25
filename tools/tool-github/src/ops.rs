//! One call, end to end: validate the configuration, the input and the gateway address, send
//! the request(s) one operation needs, and map the reply to a tool result.
//!
//! Every check that can fail runs before the first request an operation would send from a
//! refused call, so a refused `push`, `pr_create` or `pr_merge` sends nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::{self, Config, MergeMethod};
use crate::git::object::ObjectKind;
use crate::git::pack::OpenError as PackOpenError;
use crate::git::rebuild;
use crate::git::store::{ObjectStore, ReadError};
use crate::git::walk::{self, PlanError};
use crate::git::write;
use crate::github;
use crate::input::{
    self, FetchInput, Input, PrCreateInput, PrListInput, PrMergeInput, PrViewInput, PushInput,
};
use crate::trailer;

/// The `error_kind` values a failure carries.
pub mod kind {
    pub const INVALID_INPUT: &str = "invalid_input";
    pub const CONFIG_INVALID: &str = "config_invalid";
    pub const GATEWAY_MISSING: &str = "gateway_missing";
    pub const OPERATION_NOT_ALLOWED: &str = "operation_not_allowed";
    pub const CONFLICT: &str = "conflict";
    pub const REPO_NOT_ALLOWED: &str = "repo_not_allowed";
    pub const HEAD_MOVED: &str = "head_moved";
    pub const UPSTREAM_UNAUTHORIZED: &str = "upstream_unauthorized";
    pub const UPSTREAM_FORBIDDEN: &str = "upstream_forbidden";
    pub const NOT_FOUND: &str = "not_found";
    pub const NOT_MERGEABLE: &str = "not_mergeable";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const REPO_UNREADABLE: &str = "repo_unreadable";
    pub const REPO_UNWRITABLE: &str = "repo_unwritable";
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    pub const TRANSPORT_ERROR: &str = "transport_error";
    pub const RESPONSE_INVALID: &str = "response_invalid";
}

/// Sends one request, headers included. The wasm adapter implements it over `wasi:http`; host
/// tests implement it with a recording fake — which is what lets a host-level test assert on
/// the standard headers below, not just the wasm-component-level ones.
pub trait Transport {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<HttpReply, String>;
}

/// Standard headers on every request this tool sends, whatever the operation. `body_len` is
/// the request body's byte length, so a `Content-Length` is always sent rather than the guest
/// http implementation falling back to `Transfer-Encoding: chunked` for a length it does not
/// otherwise know up front.
pub fn standard_headers(body_len: Option<usize>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("user-agent".to_string(), "tool-github".to_string()),
        (
            "accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        ("x-github-api-version".to_string(), "2022-11-28".to_string()),
    ];
    if let Some(len) = body_len {
        headers.push(("content-type".to_string(), "application/json".to_string()));
        headers.push(("content-length".to_string(), len.to_string()));
    }
    headers
}

/// What came back: the status, the `Location` header (set on a redirect), and the body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpReply {
    pub status: u16,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// Mirrors the WIT `status` enum without depending on the generated bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpStatus {
    Passed,
    Failed,
    Error,
}

/// The tool result, field for field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: OpStatus,
    pub summary: String,
    pub data: String,
    pub metadata: Vec<(String, String)>,
}

/// Run one call. `gateway_endpoint` is `MURMUR_GATEWAY_ENDPOINT` and `config_json` is
/// `MURMUR_ARTIFACT_CONFIG`, each `None` when the runtime did not set it. `repo_root` resolves
/// a `push` input's `repo_path`.
pub fn run(
    gateway_endpoint: Option<&str>,
    config_json: Option<&str>,
    data: &str,
    transport: &mut dyn Transport,
    repo_root: &Path,
) -> Response {
    let config = match config_json {
        Some(text) => match config::parse_config(text) {
            Ok(config) => config,
            Err(message) => return failure(kind::CONFIG_INVALID, message, Map::new()),
        },
        None => {
            return failure(
                kind::CONFIG_INVALID,
                "this tool requires a config: block naming the one repo it may touch \
                 (config_version: 1, repo: \"owner/name\")"
                    .to_string(),
                Map::new(),
            )
        }
    };

    let op = match input::parse_input(data) {
        Ok(op) => op,
        Err(message) => return failure(kind::INVALID_INPUT, message, Map::new()),
    };

    if !matches!(gateway_endpoint.map(str::trim), Some(endpoint) if endpoint.starts_with("http://") || endpoint.starts_with("https://"))
    {
        return failure(kind::GATEWAY_MISSING, gateway_missing_message(), Map::new());
    }

    match run_inner(&config, op, transport, repo_root) {
        Ok(response) => response,
        Err(response) => response,
    }
}

fn run_inner(
    config: &Config,
    op: Input,
    transport: &mut dyn Transport,
    repo_root: &Path,
) -> Result<Response, Response> {
    match op {
        Input::Push(input) => push(config, input, transport, repo_root),
        Input::Fetch(input) => fetch(config, input, transport, repo_root),
        Input::PrCreate(input) => pr_create(config, input, transport),
        Input::PrView(input) => pr_view(config, input, transport),
        Input::PrList(input) => pr_list(config, input, transport),
        Input::PrMerge(input) => pr_merge(config, input, transport),
    }
}

/// Send one request and return its parsed JSON body, or a ready-made failure [`Response`].
fn send(
    transport: &mut dyn Transport,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), Response> {
    let body_text = body.map(|value| value.to_string());
    let headers = standard_headers(body_text.as_ref().map(String::len));
    let reply = transport
        .request(method, path, &headers, body)
        .map_err(|e| {
            failure(
                kind::TRANSPORT_ERROR,
                format!("the request to the credential gateway failed: {e}"),
                Map::new(),
            )
        })?;

    if (300..400).contains(&reply.status) {
        let location = reply
            .location
            .clone()
            .unwrap_or_else(|| "<no Location header>".to_string());
        return Err(failure(
            kind::UPSTREAM_ERROR,
            format!(
                "GitHub answered HTTP {} (redirect to {location}); this tool does not follow \
                 redirects from the upstream",
                reply.status
            ),
            Map::new(),
        ));
    }

    let parsed: Value = if reply.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&reply.body).map_err(|e| {
            failure(
                kind::RESPONSE_INVALID,
                format!("GitHub's response is not valid JSON: {e}"),
                Map::new(),
            )
        })?
    };
    Ok((reply.status, parsed))
}

fn upstream_error(status: u16, body: &Value) -> Response {
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("<no message>");
    let mut extra = Map::new();
    extra.insert("http_status".to_string(), json!(status));
    failure(
        upstream_kind(status, message),
        format!("GitHub answered HTTP {status}: {message}"),
        extra,
    )
}

/// GitHub answers both its primary and its secondary rate limit with a 403 (sometimes a 429)
/// whose message names the limit; any other 403 is a permission refusal.
fn upstream_kind(status: u16, message: &str) -> &'static str {
    match status {
        401 => kind::UPSTREAM_UNAUTHORIZED,
        429 => kind::RATE_LIMITED,
        403 if message.to_ascii_lowercase().contains("rate limit") => kind::RATE_LIMITED,
        403 => kind::UPSTREAM_FORBIDDEN,
        404 => kind::NOT_FOUND,
        _ => kind::UPSTREAM_ERROR,
    }
}

// ── push ──────────────────────────────────────────────────────────────────────

fn push(
    config: &Config,
    input: PushInput,
    transport: &mut dyn Transport,
    repo_root: &Path,
) -> Result<Response, Response> {
    let git_dir = local_git_dir(repo_root, &input.repo_path)?;
    // Opened once, here, before any request: this is the one place the push's pack set is read
    // from disk, and every object the walk below needs is served from this same store.
    let store = ObjectStore::open(&git_dir).map_err(store_open_error_response)?;

    let (status, repo_body) = send(transport, "GET", &github::get_repo_path(&config.repo), None)?;
    if status != 200 {
        return Err(upstream_error(status, &repo_body));
    }
    let default_branch = github::default_branch(&repo_body)
        .map_err(|e| failure(kind::RESPONSE_INVALID, e, Map::new()))?;

    if input.remote_branch == default_branch && !config.allow_push_default_branch {
        return Err(failure(
            kind::OPERATION_NOT_ALLOWED,
            format!(
                "'{}' is this repo's default branch; set allow_push_default_branch: true on \
                 this tool's config: to push directly to it",
                input.remote_branch
            ),
            Map::new(),
        ));
    }

    if input.force && !config.allow_force_push {
        return Err(failure(
            kind::OPERATION_NOT_ALLOWED,
            "the input asked to force-push, but this tool's config: does not set \
             allow_force_push: true"
                .to_string(),
            Map::new(),
        ));
    }
    let effective_force = input.force && config.allow_force_push;

    let ref_path = github::get_ref_path(&config.repo, &input.remote_branch);
    let (status, ref_body) = send(transport, "GET", &ref_path, None)?;
    let remote_sha = match status {
        200 => Some(
            github::ref_sha(&ref_body)
                .map_err(|e| failure(kind::RESPONSE_INVALID, e, Map::new()))?,
        ),
        404 => None,
        other => return Err(upstream_error(other, &ref_body)),
    };
    let created = remote_sha.is_none();

    let mut on_remote = |sha: &str| -> Result<bool, Response> {
        let (status, body) = send(
            transport,
            "GET",
            &github::get_commit_path(&config.repo, sha),
            None,
        )?;
        match status {
            200 => Ok(true),
            404 => Ok(false),
            other => Err(upstream_error(other, &body)),
        }
    };
    let plan = walk::plan_push(
        &store,
        &input.head_sha,
        remote_sha.as_deref(),
        effective_force,
        &mut on_remote,
    )?;

    for blob in &plan.blobs {
        let body = github::create_blob_body(&blob.content);
        let (status, response) = send(
            transport,
            "POST",
            &github::create_blob_path(&config.repo),
            Some(&body),
        )?;
        if status != 201 {
            return Err(upstream_error(status, &response));
        }
        verify_sha(&response, &blob.sha)?;
    }
    for tree in &plan.trees {
        let body = github::create_tree_body(&tree.entries);
        let (status, response) = send(
            transport,
            "POST",
            &github::create_tree_path(&config.repo),
            Some(&body),
        )?;
        if status != 201 {
            return Err(upstream_error(status, &response));
        }
        verify_sha(&response, &tree.sha)?;
    }
    for commit in &plan.commits {
        let body = github::create_commit_body(&commit.commit);
        let (status, response) = send(
            transport,
            "POST",
            &github::create_commit_path(&config.repo),
            Some(&body),
        )?;
        if status != 201 {
            return Err(upstream_error(status, &response));
        }
        verify_sha(&response, &commit.sha)?;
    }

    let (status, response) = if created {
        let body = github::create_ref_body(&input.remote_branch, &input.head_sha);
        send(
            transport,
            "POST",
            &github::create_ref_path(&config.repo),
            Some(&body),
        )?
    } else {
        let body = github::update_ref_body(&input.head_sha, effective_force);
        send(
            transport,
            "PATCH",
            &github::update_ref_path(&config.repo, &input.remote_branch),
            Some(&body),
        )?
    };
    if status == 409 || status == 422 {
        return Err(failure(
            kind::CONFLICT,
            "GitHub refused the ref update as a non-fast-forward; the remote branch moved"
                .to_string(),
            Map::new(),
        ));
    }
    if !(200..300).contains(&status) {
        return Err(upstream_error(status, &response));
    }

    let mut data = Map::new();
    data.insert("ok".to_string(), json!(true));
    data.insert("remote_branch".to_string(), json!(input.remote_branch));
    data.insert("head_sha".to_string(), json!(input.head_sha));
    data.insert("created".to_string(), json!(created));
    data.insert("objects_uploaded".to_string(), json!(plan.object_count()));
    Ok(Response {
        status: OpStatus::Passed,
        summary: format!(
            "pushed {} object(s) to {}:{}",
            plan.object_count(),
            config.repo,
            input.remote_branch
        ),
        data: Value::Object(data).to_string(),
        metadata: vec![
            ("state_effect".to_string(), "mutate".to_string()),
            (
                "resource_id".to_string(),
                format!("github:{}:branch:{}", config.repo, input.remote_branch),
            ),
        ],
    })
}

fn verify_sha(response: &Value, expected: &str) -> Result<(), Response> {
    let actual = github::created_sha(response)
        .map_err(|e| failure(kind::RESPONSE_INVALID, e, Map::new()))?;
    if actual != expected {
        return Err(failure(
            kind::RESPONSE_INVALID,
            format!(
                "GitHub computed sha {actual} for an object this tool sent as {expected}; \
                 the remote object store would not byte-reproduce the local history"
            ),
            Map::new(),
        ));
    }
    Ok(())
}

/// Map a failure to open the pack set (before any GitHub request was sent) to a `repo_unreadable`
/// response, naming the specific file or size at fault.
fn store_open_error_response(error: PackOpenError) -> Response {
    let message = match error {
        PackOpenError::Alternates(path) => format!(
            "{} is present; this tool does not resolve objects through git alternates",
            path.display()
        ),
        PackOpenError::Unmatched(path) => format!(
            "{} has no matching pack index/data file; a packfile's .pack and .idx must both \
             be present",
            path.display()
        ),
        PackOpenError::Corrupt(path) => {
            format!(
                "{} could not be read as a well-formed pack index or pack data file",
                path.display()
            )
        }
        PackOpenError::TooLarge { observed, ceiling } => format!(
            "the pack files under objects/pack total {observed} bytes, exceeding this tool's \
             {ceiling}-byte ceiling; refusing to read them into memory"
        ),
    };
    failure(kind::REPO_UNREADABLE, message, Map::new())
}

impl From<PlanError> for Response {
    fn from(error: PlanError) -> Self {
        match error {
            PlanError::Unreadable(message) => failure(kind::REPO_UNREADABLE, message, Map::new()),
            PlanError::Signed(sha) => failure(
                kind::INVALID_INPUT,
                format!(
                    "commit {sha} carries a gpgsig header (a GPG or SSH signature) and is not on \
                     GitHub yet; this tool cannot upload a signed commit through GitHub's API, so \
                     the push is refused"
                ),
                Map::new(),
            ),
            PlanError::Gitlink(name) => failure(
                kind::INVALID_INPUT,
                format!("tree entry {name} is a gitlink (a submodule); pushing a gitlink is not supported"),
                Map::new(),
            ),
            PlanError::Corrupt(sha) => failure(
                kind::REPO_UNREADABLE,
                format!("object {sha} could not be read as a well-formed git object"),
                Map::new(),
            ),
            PlanError::NotFastForward => failure(
                kind::CONFLICT,
                "the local range does not build on the remote branch's current tip; pass force: \
                 true (and set allow_force_push: true in config:) to overwrite it"
                    .to_string(),
                Map::new(),
            ),
        }
    }
}

/// The git directory of the repository at `repo_path` (a worktree's `.git`, or a bare repo
/// itself), refused as `repo_unreadable` when it has no objects directory.
fn local_git_dir(repo_root: &Path, repo_path: &str) -> Result<PathBuf, Response> {
    let git_dir = resolve_git_dir(&repo_root.join(repo_path));
    if !git_dir.join("objects").is_dir() {
        return Err(failure(
            kind::REPO_UNREADABLE,
            format!(
                "{} is not a readable git repository: it has no objects directory",
                git_dir.display()
            ),
            Map::new(),
        ));
    }
    Ok(git_dir)
}

fn resolve_git_dir(repo_path: &Path) -> PathBuf {
    let dot_git = repo_path.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        repo_path.to_path_buf()
    }
}

// ── fetch ─────────────────────────────────────────────────────────────────────

/// Most commits one fetch rebuilds before refusing: a bound on the requests and memory a single
/// call spends, far above what a branch gains between two fetches.
const MAX_FETCH_COMMITS: usize = 10_000;

/// Objects a fetch has rebuilt and verified, in the order they are written: every tree and blob
/// before the commits, and the remote-tracking ref only after all of them.
struct Fetched<'a> {
    config: &'a Config,
    store: ObjectStore<'a>,
    seen: HashSet<String>,
    contents: Vec<(ObjectKind, String, Vec<u8>)>,
    commits: Vec<(String, Vec<u8>)>,
}

impl Fetched<'_> {
    /// Whether the local repository already holds `sha`. A held commit is taken to have its
    /// whole history, as git itself assumes.
    fn is_local(&self, sha: &str) -> Result<bool, Response> {
        match self.store.read(sha) {
            Ok(_) => Ok(true),
            Err(ReadError::NotFound(_)) => Ok(false),
            Err(ReadError::Corrupt(sha)) => Err(failure(
                kind::REPO_UNREADABLE,
                format!("object {sha} could not be read as a well-formed git object"),
                Map::new(),
            )),
        }
    }

    /// Every commit reachable from `tip` that the local repository lacks, rebuilt from GitHub's
    /// GraphQL history 100 commits at a time.
    fn commits_from(
        &mut self,
        tip: &str,
        transport: &mut dyn Transport,
    ) -> Result<Vec<String>, Response> {
        let mut trees = Vec::new();
        let mut frontier = vec![tip.to_string()];
        while let Some(sha) = frontier.pop() {
            if self.seen.contains(&sha) || self.is_local(&sha)? {
                continue;
            }
            let body = github::commit_history_body(&self.config.repo, &sha);
            let (status, response) = send(transport, "POST", github::GRAPHQL_PATH, Some(&body))?;
            if status != 200 {
                return Err(upstream_error(status, &response));
            }
            if let Some(message) = github::graphql_errors(&response) {
                return Err(failure(
                    kind::UPSTREAM_ERROR,
                    format!("GitHub's GraphQL API answered: {message}"),
                    Map::new(),
                ));
            }
            let history = github::parse_commit_history(&response).map_err(response_invalid)?;
            if !history.iter().any(|commit| commit.sha == sha) {
                return Err(response_invalid(format!(
                    "GitHub's history for {sha} does not include it"
                )));
            }
            for commit in history {
                if self.seen.contains(&commit.sha) || self.is_local(&commit.sha)? {
                    continue;
                }
                let content = rebuild::rebuild_commit(&commit).map_err(response_invalid)?;
                self.seen.insert(commit.sha.clone());
                frontier.extend(commit.parents.iter().cloned());
                trees.push(commit.tree.clone());
                self.commits.push((commit.sha, content));
                if self.commits.len() > MAX_FETCH_COMMITS {
                    return Err(failure(
                        kind::OPERATION_NOT_ALLOWED,
                        format!(
                            "the branch is more than {MAX_FETCH_COMMITS} commits ahead of this \
                             repository; clone it outside the capsule instead"
                        ),
                        Map::new(),
                    ));
                }
            }
        }
        Ok(trees)
    }

    /// `sha` and everything under it that the local repository lacks. Gitlinks name commits in
    /// another repository and are not followed.
    fn tree(&mut self, sha: &str, transport: &mut dyn Transport) -> Result<(), Response> {
        if !self.seen.insert(sha.to_string()) || self.is_local(sha)? {
            return Ok(());
        }
        let path = github::get_tree_path(&self.config.repo, sha);
        let (status, response) = send(transport, "GET", &path, None)?;
        if status != 200 {
            return Err(upstream_error(status, &response));
        }
        let entries = github::parse_tree_response(&response).map_err(response_invalid)?;
        let content = rebuild::encode_tree(&entries).map_err(response_invalid)?;
        let content =
            rebuild::verified(ObjectKind::Tree, sha, content).map_err(response_invalid)?;
        for entry in &entries {
            if entry.is_tree() {
                self.tree(&entry.sha, transport)?;
            } else if !entry.is_gitlink() {
                self.blob(&entry.sha, transport)?;
            }
        }
        self.contents
            .push((ObjectKind::Tree, sha.to_string(), content));
        Ok(())
    }

    fn blob(&mut self, sha: &str, transport: &mut dyn Transport) -> Result<(), Response> {
        if !self.seen.insert(sha.to_string()) || self.is_local(sha)? {
            return Ok(());
        }
        let path = github::get_blob_path(&self.config.repo, sha);
        let (status, response) = send(transport, "GET", &path, None)?;
        if status != 200 {
            return Err(upstream_error(status, &response));
        }
        let content = github::parse_blob_response(&response).map_err(response_invalid)?;
        let content =
            rebuild::verified(ObjectKind::Blob, sha, content).map_err(response_invalid)?;
        self.contents
            .push((ObjectKind::Blob, sha.to_string(), content));
        Ok(())
    }
}

fn fetch(
    config: &Config,
    input: FetchInput,
    transport: &mut dyn Transport,
    repo_root: &Path,
) -> Result<Response, Response> {
    let git_dir = local_git_dir(repo_root, &input.repo_path)?;
    let store = ObjectStore::open(&git_dir).map_err(store_open_error_response)?;

    let branch = match input.branch {
        Some(branch) => branch,
        None => {
            let (status, repo_body) =
                send(transport, "GET", &github::get_repo_path(&config.repo), None)?;
            if status != 200 {
                return Err(upstream_error(status, &repo_body));
            }
            let branch = github::default_branch(&repo_body).map_err(response_invalid)?;
            if !input::is_valid_ref_name(&branch) {
                return Err(response_invalid(format!(
                    "GitHub's default branch {branch:?} is not a valid ref name"
                )));
            }
            branch
        }
    };

    let (status, ref_body) = send(
        transport,
        "GET",
        &github::get_ref_path(&config.repo, &branch),
        None,
    )?;
    let tip = match status {
        200 => github::ref_sha(&ref_body).map_err(response_invalid)?,
        404 => {
            return Err(failure(
                kind::NOT_FOUND,
                format!("{} has no branch '{branch}'", config.repo),
                Map::new(),
            ))
        }
        other => return Err(upstream_error(other, &ref_body)),
    };

    let mut fetched = Fetched {
        config,
        store,
        seen: HashSet::new(),
        contents: Vec::new(),
        commits: Vec::new(),
    };
    for tree in fetched.commits_from(&tip, transport)? {
        fetched.tree(&tree, transport)?;
    }

    let objects = fetched.contents.len() + fetched.commits.len();
    let written = fetched
        .contents
        .iter()
        .map(|(kind, sha, content)| (*kind, sha, content))
        .chain(
            fetched
                .commits
                .iter()
                .map(|(sha, content)| (ObjectKind::Commit, sha, content)),
        );
    for (object_kind, sha, content) in written {
        write::write_loose(&git_dir, object_kind, sha, content).map_err(|e| {
            failure(
                kind::REPO_UNWRITABLE,
                format!(
                    "could not write {} {sha} into {}: {e}",
                    object_kind.as_str(),
                    git_dir.display()
                ),
                Map::new(),
            )
        })?;
    }

    let remote_ref = format!("refs/remotes/{}/{branch}", input.remote);
    let previous = write::read_ref(&git_dir, &remote_ref);
    if previous.as_deref() != Some(tip.as_str()) {
        write::write_ref(&git_dir, &remote_ref, &tip).map_err(|e| {
            failure(
                kind::REPO_UNWRITABLE,
                format!(
                    "could not update {remote_ref} in {}: {e}",
                    git_dir.display()
                ),
                Map::new(),
            )
        })?;
    }

    let mut data = Map::new();
    data.insert("ok".to_string(), json!(true));
    data.insert("branch".to_string(), json!(branch));
    data.insert("remote_ref".to_string(), json!(remote_ref));
    data.insert("head_sha".to_string(), json!(tip));
    data.insert("previous_sha".to_string(), json!(previous));
    data.insert("objects_written".to_string(), json!(objects));
    Ok(Response {
        status: OpStatus::Passed,
        summary: if previous.as_deref() == Some(tip.as_str()) {
            format!("{remote_ref} is already at {}", short(&tip))
        } else {
            format!(
                "fetched {objects} object(s); {remote_ref} is now at {}",
                short(&tip)
            )
        },
        data: Value::Object(data).to_string(),
        metadata: vec![
            ("state_effect".to_string(), "read".to_string()),
            (
                "resource_id".to_string(),
                format!("github:{}:branch:{branch}", config.repo),
            ),
        ],
    })
}

fn response_invalid(message: String) -> Response {
    failure(kind::RESPONSE_INVALID, message, Map::new())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

// ── pull requests ────────────────────────────────────────────────────────────

fn repo_owner(repo: &str) -> &str {
    repo.split('/').next().unwrap_or_default()
}

fn pr_create(
    config: &Config,
    input: PrCreateInput,
    transport: &mut dyn Transport,
) -> Result<Response, Response> {
    if let Some((owner, _)) = input.head.split_once(':') {
        if owner != repo_owner(&config.repo) {
            return Err(failure(
                kind::REPO_NOT_ALLOWED,
                format!(
                    "head '{}' names a fork outside the pinned repo {}; this tool's entry may \
                     only touch {}",
                    input.head, config.repo, config.repo
                ),
                Map::new(),
            ));
        }
    }

    let body = crate::footer::apply(input.body.as_deref(), config.pr_footer.as_deref());

    let request_body = github::create_pr_body(
        &input.title,
        body.as_deref(),
        &input.head,
        &input.base,
        input.draft,
    );
    let (status, response) = send(
        transport,
        "POST",
        &github::create_pr_path(&config.repo),
        Some(&request_body),
    )?;
    if status != 201 {
        return Err(upstream_error(status, &response));
    }
    let pr = github::parse_pull_request(&response)
        .map_err(|e| failure(kind::RESPONSE_INVALID, e, Map::new()))?;

    let mut data = Map::new();
    data.insert("ok".to_string(), json!(true));
    data.insert("number".to_string(), json!(pr.number));
    data.insert("url".to_string(), json!(pr.url));
    data.insert("head_sha".to_string(), json!(pr.head_sha));
    Ok(Response {
        status: OpStatus::Passed,
        summary: format!("opened PR #{} on {}", pr.number, config.repo),
        data: Value::Object(data).to_string(),
        metadata: vec![
            ("state_effect".to_string(), "mutate".to_string()),
            (
                "resource_id".to_string(),
                format!("github:{}:pr:{}", config.repo, pr.number),
            ),
        ],
    })
}

fn pr_view(
    config: &Config,
    input: PrViewInput,
    transport: &mut dyn Transport,
) -> Result<Response, Response> {
    let (status, response) = send(
        transport,
        "GET",
        &github::get_pr_path(&config.repo, input.number),
        None,
    )?;
    if status != 200 {
        return Err(upstream_error(status, &response));
    }
    let pr = github::parse_pull_request(&response)
        .map_err(|e| failure(kind::RESPONSE_INVALID, e, Map::new()))?;

    let (status, reviews_body) = send(
        transport,
        "GET",
        &github::reviews_path(&config.repo, input.number),
        None,
    )?;
    if status != 200 {
        return Err(upstream_error(status, &reviews_body));
    }
    let reviews: Vec<Value> = reviews_body
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|review| {
            json!({
                "user": review.get("user").and_then(|u| u.get("login")).cloned().unwrap_or(Value::Null),
                "state": review.get("state").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    let (status, checks_body) = send(
        transport,
        "GET",
        &github::check_runs_path(&config.repo, &pr.head_sha),
        None,
    )?;
    if status != 200 {
        return Err(upstream_error(status, &checks_body));
    }
    let checks: Vec<Value> = checks_body
        .get("check_runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|check| {
            json!({
                "name": check.get("name").cloned().unwrap_or(Value::Null),
                "status": check.get("status").cloned().unwrap_or(Value::Null),
                "conclusion": check.get("conclusion").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    let mut data = Map::new();
    data.insert("ok".to_string(), json!(true));
    data.insert("number".to_string(), json!(pr.number));
    data.insert("state".to_string(), json!(pr.state));
    data.insert("mergeable".to_string(), json!(pr.mergeable));
    data.insert("head_sha".to_string(), json!(pr.head_sha));
    data.insert("checks".to_string(), json!(checks));
    data.insert("reviews".to_string(), json!(reviews));
    Ok(Response {
        status: OpStatus::Passed,
        summary: format!("PR #{} is {}", pr.number, pr.state),
        data: Value::Object(data).to_string(),
        metadata: vec![
            ("state_effect".to_string(), "read".to_string()),
            (
                "resource_id".to_string(),
                format!("github:{}:pr:{}", config.repo, pr.number),
            ),
        ],
    })
}

fn pr_list(
    config: &Config,
    input: PrListInput,
    transport: &mut dyn Transport,
) -> Result<Response, Response> {
    let path = github::list_prs_path(
        &config.repo,
        input.state.as_deref(),
        input.head.as_deref(),
        input.base.as_deref(),
    );
    let (status, response) = send(transport, "GET", &path, None)?;
    if status != 200 {
        return Err(upstream_error(status, &response));
    }
    let prs: Vec<Value> = response
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| github::parse_pull_request(item).ok())
        .map(|pr| {
            json!({
                "number": pr.number,
                "state": pr.state,
                "title": pr.title,
                "head_ref": pr.head_ref,
                "base_ref": pr.base_ref,
            })
        })
        .collect();

    let mut data = Map::new();
    data.insert("ok".to_string(), json!(true));
    data.insert("prs".to_string(), json!(prs));
    Ok(Response {
        status: OpStatus::Passed,
        summary: format!("{} pull request(s) on {}", prs.len(), config.repo),
        data: Value::Object(data).to_string(),
        metadata: vec![
            ("state_effect".to_string(), "read".to_string()),
            (
                "resource_id".to_string(),
                format!("github:{}:pulls", config.repo),
            ),
        ],
    })
}

fn pr_merge(
    config: &Config,
    input: PrMergeInput,
    transport: &mut dyn Transport,
) -> Result<Response, Response> {
    if !config.allow_merge {
        return Err(failure(
            kind::OPERATION_NOT_ALLOWED,
            "this tool's config: does not set allow_merge: true".to_string(),
            Map::new(),
        ));
    }
    let method = match &input.method {
        None => config.merge_methods.first().copied().ok_or_else(|| {
            failure(
                kind::OPERATION_NOT_ALLOWED,
                "this tool's config: lists no merge_methods".to_string(),
                Map::new(),
            )
        })?,
        Some(name) => MergeMethod::parse(name).ok_or_else(|| {
            failure(
                kind::INVALID_INPUT,
                format!("\"method\" must be one of merge, squash, rebase, got {name:?}"),
                Map::new(),
            )
        })?,
    };
    if !config.merge_methods.contains(&method) {
        return Err(failure(
            kind::OPERATION_NOT_ALLOWED,
            format!(
                "'{}' is not in this tool's config: merge_methods",
                method.as_str()
            ),
            Map::new(),
        ));
    }

    let (status, pr_body) = send(
        transport,
        "GET",
        &github::get_pr_path(&config.repo, input.number),
        None,
    )?;
    if status != 200 {
        return Err(upstream_error(status, &pr_body));
    }
    let pr = github::parse_pull_request(&pr_body)
        .map_err(|e| failure(kind::RESPONSE_INVALID, e, Map::new()))?;

    let composed = trailer::compose(
        method,
        &pr.title,
        &pr.body,
        input.message.as_deref(),
        config.merge_trailer.as_deref(),
    );

    let merge_body = github::merge_pr_body(
        method.as_str(),
        &input.expected_head_sha,
        None,
        composed.commit_message.as_deref(),
    );
    let (status, response) = send(
        transport,
        "PUT",
        &github::merge_pr_path(&config.repo, input.number),
        Some(&merge_body),
    )?;
    if status == 409 {
        return Err(failure(
            kind::HEAD_MOVED,
            "the pull request's head moved since it was read; refresh and retry".to_string(),
            Map::new(),
        ));
    }
    if status == 405 {
        return Err(failure(
            kind::NOT_MERGEABLE,
            "GitHub reports this pull request is not mergeable".to_string(),
            Map::new(),
        ));
    }
    if status != 200 {
        return Err(upstream_error(status, &response));
    }

    let merged = response
        .get("merged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let merge_sha = response
        .get("sha")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut data = Map::new();
    data.insert("ok".to_string(), json!(true));
    data.insert("merged".to_string(), json!(merged));
    data.insert("merge_sha".to_string(), json!(merge_sha));
    data.insert("trailer".to_string(), json!(composed.trailer));
    Ok(Response {
        status: OpStatus::Passed,
        summary: format!(
            "merged PR #{} on {} via {}",
            pr.number,
            config.repo,
            method.as_str()
        ),
        data: Value::Object(data).to_string(),
        metadata: vec![
            ("state_effect".to_string(), "mutate".to_string()),
            (
                "resource_id".to_string(),
                format!("github:{}:pr:{}", config.repo, pr.number),
            ),
        ],
    })
}

fn gateway_missing_message() -> String {
    format!(
        "this tool's entry binds no credential gateway, so there is nowhere to send the \
         request; add gateway: {{endpoint: {}, api_key: {}}} to the tool-github entry \
         in the capsule manifest and store the token with mur config set -g {} <token>",
        crate::SUGGESTED_ENDPOINT,
        crate::SUGGESTED_KEY_REFERENCE,
        crate::SUGGESTED_CREDENTIAL
    )
}

/// A failure: `data` is `{"ok":false,"error_kind":…,"message":…}` plus the kind's extra
/// fields, `summary` is the message, and nothing is written.
fn failure(error_kind: &str, message: impl Into<String>, extra: Map<String, Value>) -> Response {
    let message = message.into();
    let mut data = Map::new();
    data.insert("ok".to_string(), json!(false));
    data.insert("error_kind".to_string(), json!(error_kind));
    data.insert("message".to_string(), json!(message));
    data.extend(extra);
    let status = if error_kind == kind::INVALID_INPUT {
        OpStatus::Failed
    } else {
        OpStatus::Error
    };
    Response {
        status,
        summary: message,
        data: Value::Object(data).to_string(),
        metadata: Vec::new(),
    }
}
