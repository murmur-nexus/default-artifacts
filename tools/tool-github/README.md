# tool-github

Pushes a local commit range from a capsule's workdir to one GitHub repo, and the remote branch
lands on the identical SHA. It also fetches a branch back, and opens, inspects, lists and merges
pull requests on that repo. The operator pins the repo and binds the GitHub token on the tool's
entry, and the runtime attaches the token to every request. The tool never holds, reads or builds
the token, and it never spawns `git`: it reads `.git` from the workdir and talks to GitHub's API.

WASM component (`wasm32-wasip2`), exporting `murmur:tool/run`. It needs a murmur runtime that
supports per-entry credential gateways: the first murmur release after v0.3.0 (murmur #191).

## Install

```bash
mur install -g murmur/tool-github@0.9.0
```

## Binding the token

Bind the token on the tool's own entry in the capsule manifest, as `gateway.api_key`, and pin the
repo in `config:`:

```yaml
artifacts:
  - name: murmur/tool-github
    version: 0.9.0
    runtime: tool
    gateway:
      endpoint: https://api.github.com
      api_key: ${GITHUB_TOKEN}
    capabilities:
      network:
        allow: []
    config:
      config_version: 1
      repo: owner/name
```

Store the token with:

```bash
mur config set -g credentials.GITHUB_TOKEN <token>
```

The token needs these permissions on the pinned repo:

| Token | Permissions |
|---|---|
| Fine-grained (recommended), limited to the pinned repo | Contents: read and write (`push`, `fetch`); Pull requests: read and write (`pr_*`); Metadata: read |
| Classic | `repo` |

An entry with no `gateway:` sends nothing. Every call returns `gateway_missing`:

```text
this tool's entry binds no credential gateway, so there is nowhere to send the request; add
gateway: {endpoint: https://api.github.com, api_key: ${GITHUB_TOKEN}} to the tool-github entry
in the capsule manifest and store the token with mur config set -g credentials.GITHUB_TOKEN <token>
```

## Who holds the token

The tool never holds, reads or builds the token. Its bundled `murmur.yaml` declares only how
GitHub takes a token:

```yaml
upstream_auth:
  header: Authorization
  value: "Bearer {key}"
```

The runtime gives the tool one address, `MURMUR_GATEWAY_ENDPOINT`, and that variable carries no
token. The tool sends plain HTTP to that address. The runtime removes any `Authorization` header,
attaches exactly one rendered from the token, and forwards the request over TLS to
`gateway.endpoint`.

| Where a token might be put | Result |
|---|---|
| A literal token in `config:` | Refused, as is any `config:` key whose name contains `key`, `token`, `secret`, `password`, `authoriz`, `bearer` or `credential` (`config_invalid`) |
| An environment variable the tool reads | Not supported: the tool reads only `MURMUR_ARTIFACT_CONFIG` and `MURMUR_GATEWAY_ENDPOINT` |
| No `gateway:` on the entry | Every call returns `gateway_missing` and nothing is sent |

## Network

Do not list `api.github.com` in any `network.allow`. Gateway requests are not checked against the
allow-list, so such an entry grants only direct, tokenless reach to GitHub, and murmur warns
`W-SEC-025`. `allow: []` on the entry removes all direct egress, leaving the gateway as the tool's
only way out.

## Cost

murmur brokers the requests but **does not meter** them. Every launch warns `W-SEC-030`: the
requests count toward neither `inference.max_session_tokens` nor
`spend.machine_tokens_per_day`. GitHub's own API rate limit applies, and a refusal under it is
`rate_limited`. One push sends one request per uploaded object, plus one lookup per commit on the
walk back to history GitHub already has. One fetch sends one GraphQL request per 100 commits, and
one request per tree and blob the local repository lacks.

## Pinning the repo

`repo` in `config:` is required. Every request goes to that repo and no other. A `pr_create`
whose `head` names another owner's fork (`someone:branch`) is refused with `repo_not_allowed`
before anything is sent.

## Configuration

`config:` is required, because `repo` has no default. An entry with no `config:` block is refused
with `config_invalid`.

| Key | Type | Default | Effect |
|---|---|---|---|
| `config_version` | integer | required | Must be `1` |
| `repo` | string | required | The one repo this entry may touch, as `owner/name` |
| `allow_merge` | boolean | `false` | `pr_merge` is refused with `operation_not_allowed` unless `true` |
| `merge_methods` | list of `merge`, `squash`, `rebase` | `[]` | The methods `pr_merge` may use. The first is the default when the call names none |
| `allow_force_push` | boolean | `false` | A `push` with `force: true` is refused with `operation_not_allowed` unless `true` |
| `allow_push_default_branch` | boolean | `false` | A `push` to the repo's default branch is refused with `operation_not_allowed` unless `true` |
| `pr_footer` | string or `false` | unset | One line appended to every PR body `pr_create` sends. Unset or `false` sends the body untouched |
| `merge_trailer` | string or `false` | unset | One `Key: value` trailer line appended to every merge or squash commit message. Unset or `false` sends none |
| `commit_identity` | mapping `{name, email}` or `false` | unset | The author and committer of every `merge` or `squash` commit `pr_merge` makes, which the tool then builds itself (see [Commit identity](#commit-identity)). Unset or `false` leaves those commits to GitHub's merge endpoint |

`pr_footer` applies these rules:

| Body the agent gave | Body sent |
|---|---|
| none, or empty | exactly the line |
| a body whose last line is already the line | the body, unchanged |
| any other body | the body, a blank line, then the line |

A `draft: true` pull request gets the footer too. `pr_footer: true` is refused, and so are an
empty or blank string, a string with a line break, and any non-string value. Each refusal is
`config_invalid` and names `pr_footer`. Set the line to append instead, for example
`pr_footer: "Opened by our release bot"`.

`merge_trailer` is appended to the caller's `message` if it gave one. Otherwise it is appended to
the PR body for a squash, or to the PR title for a merge. It joins a trailing trailer block with
no blank line, it is the whole message when the base text is empty, and it is never duplicated.
A rebase never carries a message or a trailer, because GitHub's merge endpoint takes neither for
that method. `merge_trailer: true` is refused, and so are a string that is not one `Key: value`
line (the key letters, digits and `-`) and any non-string value. Each refusal is
`config_invalid` and names `merge_trailer`. For example:
`merge_trailer: "Co-authored-by: Release Bot <bot@example.com>"`.

`commit_identity` is a mapping with exactly two string keys, `name` and `email`, both stored
verbatim:

```yaml
commit_identity:
  name: Release Bot
  email: bot@example.com
```

Each of these is refused with `config_invalid`, with a message naming `commit_identity`:

- `commit_identity: true`, with a message showing the `{name: …, email: …}` mapping to set
- a value that is neither a mapping nor `false`
- a mapping missing `name` or `email`, or with any other key
- a `name` or `email` that is not a string
- a `name` that is blank, or contains `<`, `>`, a line break or NUL
- an `email` that contains whitespace, `<`, `>` or NUL, or has no `@` with at least one
  character on each side

`config_version` stays `1`. Nothing in a call can set or change the identity: an `author`,
`committer` or `commit_identity` input field is `invalid_input`.

## Push

`push` takes a commit SHA (`head_sha`), not a branch name, and points `remote_branch` at it. It
reads the repository at `repo_path` (relative to the workdir, `.` by default): `<repo_path>/.git`,
or `<repo_path>` itself when it is a bare repository.

- **It byte-reproduces each commit**, through GitHub's Git Data API, so the remote branch lands
  on the identical SHA. Every object GitHub creates is checked against the local SHA. A mismatch
  is `response_invalid`, and the branch is not moved.
- **It uploads only what GitHub lacks.** The walk back from `head_sha` stops at the first commit
  GitHub already has. Trees and blobs under that commit are not re-sent.
- **It reads loose objects and packs**, including delta-compressed objects whose base is in
  another pack.
- **Shallow clones work** (`git clone --depth 1`), provided GitHub already has the history behind
  the shallow boundary. A commit made on top of a shallow clone of a branch GitHub has pushes
  normally. Pushing a shallow clone's own tip to a repo that lacks its parent is
  `repo_unreadable`, and the message names the parent.
- **A partial clone** (`git clone --filter=…`) lacks blobs. Any object in neither the loose store
  nor a pack is `repo_unreadable`.
- **A pack set over 256 MiB** (`.pack` plus `.idx`, by file size) is refused with
  `repo_unreadable` before any byte is read. For scale, a `git clone --depth 1` of murmur is one
  pack of 7.6 MB (7,591,251 bytes, 2.8% of the ceiling), while its full clone's packs are 1.7 GiB.
  Use a shallow clone as the workspace.
- **Alternates** (`git clone --shared` or `--reference`) are refused with `repo_unreadable`, and
  so is a `.pack` with no matching `.idx`.

| Refusal | `error_kind` |
|---|---|
| A signed commit GitHub does not have yet (GitHub's API cannot create one) | `invalid_input` |
| A tree with a gitlink (a submodule) | `invalid_input` |
| A non-fast-forward push without `force: true` | `conflict` |
| `force: true` without `allow_force_push: true` | `operation_not_allowed` |
| The repo's default branch without `allow_push_default_branch: true` | `operation_not_allowed` |
| GitHub refuses the ref update because the branch moved meanwhile | `conflict` |

Every refusal in this section happens before the first upload.

## Fetch

`fetch` is `git fetch` for one branch, the repo's default branch when `branch` is omitted. It
writes every commit, tree and blob the local repository lacks as loose objects, verifying that
each hashes to the SHA GitHub named. A mismatch is `response_invalid`. It then points
`refs/remotes/<remote>/<branch>` (`remote` defaults to `origin`) at the branch tip, only after
every object is written. It never touches a local branch, the index or the work tree: merge or
reset onto the remote-tracking ref afterwards as usual.

A blob over about 6 MB exceeds the 8 MiB response size the tool reads. A branch more than 10,000
commits ahead of the local repository is refused with `operation_not_allowed`.

## Pull requests

| Op | Does |
|---|---|
| `pr_create` | Opens a pull request from `head` into `base`. `draft: true` opens it as a draft |
| `pr_view` | Reads one pull request with its mergeability, head SHA, check runs and reviews |
| `pr_list` | Lists pull requests, filtered by `state`, `head` and `base` |
| `pr_merge` | Merges one pull request, if `allow_merge` and `merge_methods` allow it |

`pr_merge` requires `expected_head_sha`, the PR head the caller last saw in `pr_view`. The tool
sends it to GitHub, and if the PR has moved since, GitHub refuses the merge and the tool returns
`head_moved`. A PR GitHub cannot merge is `not_mergeable`. With `commit_identity` set, a `merge`
or `squash` commit is built by the tool instead, as described in
[Commit identity](#commit-identity).

## Commit identity

With `commit_identity` set, a `pr_merge` using `merge` or `squash` no longer asks GitHub's merge
endpoint to make the commit. The tool builds the commit itself through GitHub's Git Data API:
its author and committer are the configured identity, its message carries the `merge_trailer`,
and its tree is the merge GitHub already computed for the pull request. The tool then
fast-forwards the base branch to that commit, and GitHub records the pull request as merged. A
`rebase`, and every merge when `commit_identity` is unset, goes to GitHub's merge endpoint with
the same requests and the same result as without the key.

| What GitHub records | `commit_identity` unset | `commit_identity` set |
|---|---|---|
| The `merge` or `squash` commit `pr_merge` makes | Made by GitHub's merge endpoint: GitHub picks the author (the merging account or the PR's author), GitHub is the committer and signs it | Author and committer are the configured identity. The commit is unsigned |
| The `merge_trailer` co-author line | On a GitHub-made merge or squash commit | On the tool-built commit |
| Commits from a `rebase` merge | GitHub's rebase: the authors are kept and GitHub is the committer | Unchanged: the identity does not apply |
| Commits `push` uploads | Byte-identical to the local commits, with whoever made them as author and committer | Unchanged: `push` never rewrites a commit |
| The pull request's opener (`pr_create`) | Token owner | Token owner |
| Who merged the pull request, and who pushed each ref update | Token owner | Token owner |

Why the identity covers exactly those commits:

- **GitHub's REST merge endpoint** takes only `commit_title`, `commit_message`, `sha` and
  `merge_method`, with no author or committer, so through it only the trailer can be set. It
  stays the path when `commit_identity` is unset.
- **GraphQL `mergePullRequest`'s `authorEmail`** picks among the merging account's own verified
  addresses. It cannot credit another account and sets neither a name nor a committer.
- **Rewriting commits on `push`** would change every SHA and break `push`'s promise that the
  remote branch lands on the identical local SHA. A push-time check refusing other authors'
  commits creates no commit and would refuse branches carrying other people's work.
- **A tool-built Git Data commit** (`POST /git/commits` accepts any author and committer),
  placed on the base by a fast-forward, is what the tool does. GitHub marks a pull request
  merged once its head is reachable from its base.

The requests, after the `allow_merge` and `merge_methods` gates:

| Step | `merge` | `squash` |
|---|---|---|
| 1 | `GET /repos/{repo}/pulls/{n}` | same |
| 2 | `GET /repos/{repo}/git/commits/{merge_commit_sha}`: GitHub's test merge, whose parents must be the base tip and the PR head | same |
| 3 | `POST /repos/{repo}/git/commits` `{message, tree, parents: [base tip, head], author, committer}` | same, with `parents: [base tip]` |
| 4 | `PATCH /repos/{repo}/git/refs/heads/{base}` `{sha, force: false}` | `GET /repos/{repo}/git/refs/heads/{head}`: must still be the PR head |
| 5 | | `PATCH /repos/{repo}/git/refs/heads/{head}` `{sha, force: true}`: the head branch now holds the squashed commit, so it stays reachable and GitHub shows the PR merged, not closed |
| 6 | | `PATCH /repos/{repo}/git/refs/heads/{base}` `{sha, force: false}` |

`author` and `committer` are both `{name, email}` with no `date`, so GitHub stamps the current
time. The message is the caller's `message` when it gave one. Otherwise a merge reads
`Merge pull request #{n} from {owner}/{head}`, a blank line and the PR title, and a squash reads
`{title} (#{n})`, a blank line and the PR body (left out when blank). The `merge_trailer` joins
it by the rules above.

| Failure | `error_kind` | Sent |
|---|---|---|
| The PR is not open | `not_mergeable` | 1 |
| The PR's head branch is not in the pinned repo | `repo_not_allowed` | 1 |
| The PR's head is not `expected_head_sha` | `head_moved` | 1 |
| GitHub is still computing mergeability (run `pr_merge` again in a few seconds) | `not_mergeable` | 1 |
| GitHub reports the PR not mergeable | `not_mergeable` | 1 |
| The PR has no `merge_commit_sha`, or a squash's PR has no `base.repo.default_branch` | `response_invalid` | 1 |
| A squash whose head branch is the repo's default branch | `operation_not_allowed` | 1 |
| GitHub's test merge is out of date: not exactly two parents, or the second is not the head (run `pr_merge` again) | `conflict` | 2 |
| The commit or ref read fails, or GitHub refuses to create the commit | by HTTP status | 2, 3 or 4 |
| A squash's head branch no longer holds the PR head | `head_moved` | 4 |
| GitHub refuses the head branch update; the base is not touched | by HTTP status | 5 |
| The base moved since GitHub computed the merge (HTTP 422, "not a fast forward"); nothing was merged | `conflict` | 4 or 6 |
| Any other refused base update, such as branch protection. The message ends `; a protected base branch refuses a commit this tool builds itself — unset commit_identity to let GitHub merge as the token owner` | by HTTP status, with `http_status` and `errors` | 4 or 6 |

No ref is written until the commit exists. A squash failure after step 5 adds `head_sha`, the
squashed commit the PR's head branch now holds, with the same change as before. A squash
`conflict` says so and says to retry with `expected_head_sha` set to it.

## Input

`{"op": …, …}`. A field set to `""`, `0`, `false` or `null` counts as absent, so a model that
fills every schema property on every call, as OpenAI's strict function calling does, sends the
same request as one that sends only the op's own fields. No field gives one of those values
a meaning of its own: every boolean defaults to `false`, an empty `branch` is the default
branch, an empty `method` is the first of `merge_methods`, and `number: 0` is refused as a
missing `number`. A field that does not belong to the op and holds any other value is
`invalid_input`.

| Op | Fields |
|---|---|
| `push` | `head_sha` (40 hex, required), `remote_branch` (required), `repo_path`, `force` |
| `fetch` | `repo_path`, `branch`, `remote` |
| `pr_create` | `title` (required), `head` (required), `base` (required), `body`, `draft` |
| `pr_view` | `number` (required) |
| `pr_list` | `state` (`open`, `closed` or `all`), `head`, `base` |
| `pr_merge` | `number` (required), `expected_head_sha` (40 hex, required), `method`, `message` |

## Output

A success returns `data` as compact JSON with `"ok": true`:

| Op | Fields |
|---|---|
| `push` | `remote_branch`, `head_sha`, `created` (the branch was new), `objects_uploaded` |
| `fetch` | `branch`, `remote_ref`, `head_sha`, `previous_sha` (the ref's earlier value, or `null`), `objects_written` |
| `pr_create` | `number`, `url`, `head_sha` |
| `pr_view` | `number`, `state`, `mergeable`, `head_sha`, `checks`, `reviews` |
| `pr_list` | `prs` |
| `pr_merge` | `merged`, `merge_sha`, `trailer` (the trailer line sent, or `null`), `identity` (`{name, email}`; present only when `commit_identity` is configured, and `null` for a rebase) |

## Errors

A failure returns `data` as compact JSON, `{"ok": false, "error_kind": …, "message": …}`.
`invalid_input` has `status: failed`; every other kind has `status: error`. A failure read from
GitHub's HTTP status (`upstream_*`, `not_found`, `rate_limited`) adds `http_status`, except
`fetch`'s missing-branch `not_found`.

When GitHub explains a refusal with an `errors` list, a failure that carries `http_status` also
carries that list, and `message` and the summary name each entry. A pull request whose `head`
GitHub cannot read reads `GitHub answered HTTP 422: Validation Failed (head: invalid)`, with
`"errors": [{"code": "invalid", "field": "head", "resource": "PullRequest"}]`. The list never
changes `error_kind`. The `conflict`, `head_moved` and `not_mergeable` failures carry no list.

A `squash` with `commit_identity` set that fails after it rewrote the PR's head branch adds
`head_sha`: the squashed commit the head branch now holds.

| Rule | Behaviour |
|---|---|
| Kept from an object entry | `resource`, `field`, `code` and `message`, each only when its value is a string. Every other key is dropped, including `value`, where GitHub can echo a submitted value |
| Kept from a string entry | The string |
| Dropped | An object with none of the four keys as a string, and any entry that is neither an object nor a string |
| Count | The first 10 kept entries. When more are kept, `errors_omitted` holds the count of the rest and the summary ends `; +<count> more` |
| Length | Each kept string is cut to 200 characters, with `…` appended when cut |
| How an entry reads | `<label>: <detail>`, or whichever of the two exists. The label is `field`, else `resource`. The detail is `code`, followed by ` (<message>)` when there is a `message`; when the code is `custom` or absent, the detail is `message` |
| No list | When `errors` is absent, empty, not a list, or has no kept entry, `message` reads `GitHub answered HTTP <status>: <GitHub's message>` and `data` has no `errors` or `errors_omitted` key |

| `error_kind` | When |
|---|---|
| `invalid_input` | The input breaks a rule above, or the push meets a signed commit GitHub lacks or a gitlink |
| `config_invalid` | `config:` is missing or breaks a rule above |
| `gateway_missing` | The entry has no `gateway:` |
| `repo_not_allowed` | `pr_create`'s `head` names a fork outside the pinned repo, or a `commit_identity` merge's head branch is outside it |
| `repo_unreadable` | The local repository has no objects directory, uses alternates, has an unpaired pack, a pack set over 256 MiB, or lacks or cannot decode an object the operation needs |
| `repo_unwritable` | `fetch` could not write an object or update the remote-tracking ref |
| `operation_not_allowed` | A `config:` gate refuses the call: merging, a merge method, force-pushing, pushing the default branch, or a fetch over 10,000 commits |
| `conflict` | A non-fast-forward push without `force`, GitHub refused the ref update, or a `commit_identity` merge found GitHub's test merge out of date or the base moved |
| `not_mergeable` | GitHub answered the merge with HTTP 405, or a `commit_identity` merge found the PR not open or not (yet) mergeable |
| `head_moved` | GitHub answered the merge with HTTP 409, or a `commit_identity` merge found the PR head or head branch no longer at `expected_head_sha` |
| `upstream_unauthorized` | HTTP 401 |
| `upstream_forbidden` | HTTP 403 that is not a rate limit |
| `not_found` | HTTP 404 where the operation needed the resource, or `fetch` of a branch the repo lacks |
| `rate_limited` | HTTP 429, or a 403 whose message names a rate limit. The tool never sleeps or retries |
| `upstream_error` | Any other non-2xx, or any 3xx: the tool never follows a redirect |
| `transport_error` | The request could not be sent, including when the runtime denies it |
| `response_invalid` | A body that is not JSON or lacks a field, or an object whose SHA differs from the one expected |

Configuration, input and gateway are checked, in that order, before any request.

## Changes from the predecessor tool 0.2.0

This tool is ported from a predecessor GitHub tool at version 0.2.0. The ops, their input fields, the output fields, the
`config:` key names, the `error_kind` values and the gateway contract are unchanged, so a capsule
moves by changing the artifact name.

| Change | Predecessor 0.2.0 | tool-github |
|---|---|---|
| PR footer | `pr_footer` was a boolean, on by default: every PR body got a fixed product footer | `pr_footer` is the line to append. Unset or `false` sends the body untouched. `true` is `config_invalid` |
| Merge trailer | `merge_trailer` was a boolean, on by default: every merge and squash got a fixed product `Co-authored-by:` trailer, except a squash of a PR the product's own bot account opened | `merge_trailer` is the trailer line to append. Unset or `false` sends none. `true`, or a line that is not `Key: value`, is `config_invalid`. Who opened the PR does not matter |
| Names | The description, error messages and the versioned `User-Agent` named the predecessor product | Every name is `tool-github`. The `gateway_missing` message names "the tool-github entry", and the `User-Agent` is `tool-github` with no version |
| Unknown `op` message | Listed `push, pr_create, pr_view, pr_list, pr_merge` | Also lists `fetch` |
| Empty input fields | Every field outside the op's own list was `invalid_input`, empty or not, so a model that sends every schema property on every call could not make a single call. An empty `branch` was `invalid_input`, `number: 0` was sent to GitHub, and `body: ""` or `message: ""` was sent as an empty string | A field set to `""`, `0`, `false` or `null` counts as absent, for every op. An empty `branch` is the default branch, `number: 0` is `invalid_input` ("\"number\" is required"), and an empty `body` or `message` is not sent. A foreign field with any other value is still `invalid_input` |

Not changed:

- `config_version` stays `1`. A config that sets only `config_version`, `repo`, `allow_merge` and
  `merge_methods` parses unchanged. A config that relied on the old default footer or trailer
  gets neither until it sets the line.
- The pack budget stays 256 MiB.
- Every refusal keeps its kind and fires at the same point, before the first upload: alternates,
  an unmatched pack, an oversized pack set, a signed commit GitHub lacks, a gitlink, a
  non-fast-forward without `force`, `force` without `allow_force_push`, the default branch
  without `allow_push_default_branch`, a fork `head`, the merge gates, `head_moved` (409),
  `not_mergeable` (405), and the HTTP status map.

## Development

| Item | Location |
|---|---|
| Transport | `wasm_tool` in `src/lib.rs`: blocking requests over `wasi:http`, and the crate's only environment reads |
| Everything else | Host-testable modules that take their inputs as parameters: `config`, `input`, `ops`, `github`, `footer`, `trailer`, and the object store and push planner under `git/` |

| Suite | Proves |
|---|---|
| unit tests (`src/`) | config, input, footer and trailer rules, the object reader and push planner, and source invariants: no credential read, no GitHub host outside one constant, no predecessor-product name |
| `tests/ops_scenarios.rs` | every op and `error_kind` against a recording fake, including a fake GitHub that recomputes real git ids |
| `tests/fetch.rs` | fetch writes verified loose objects and the remote-tracking ref |
| `tests/pack_reading.rs` | push reads checked-in, `git`-built packs, including a cross-pack `REF_DELTA` |
| `tests/shallow_clone.rs` | a commit on a `git clone --depth 1` lands the identical SHA, as from a full clone; a shallow tip whose parent GitHub lacks is `repo_unreadable`. Builds its repositories with the system `git` at test time |
| `tests/wasm_component.rs` | the compiled component under Wasmtime: headers, no credential header, `gateway_missing`, the footer, packs read through WASI, and a `commit_identity` squash's six requests |
| `tests/mur_run_gateway.rs` (`#[ignore]`d) | under a real `mur run`, the runtime attaches the token to the tool's requests and it appears nowhere in the session |

```bash
cargo test -p tool-github
MUR_BIN=/path/to/mur cargo test -p tool-github --test mur_run_gateway -- --ignored
```
