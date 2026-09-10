# murmur-tool-git

## What it is

`murmur-tool-git` is a native binary tool artifact for Murmur capsules. It provides a structured JSON interface to the most common git operations, replacing the need to add `git` to `capabilities.shell.allow` in a capsule manifest. For operations outside its scope, `bash` in the shell allowlist serves as the escape hatch.

Supported operations: `status`, `add`, `diff`, `restore`, `stash`, `log`, `show`, `symbol_history`, `commit`, `cherry_pick`, `branch`, `checkout`, `switch`, `reset`, `fetch`, `pull`, `push`, `clone`, `remote`, `tag`, `merge`, `worktree`, `create_worktree` (compat alias).

Every operation accepts an optional `repo` field (absolute path to the repo root). If omitted, the tool auto-discovers the repo root from the current working directory.

## Input contract per operation

`repo` is optional everywhere and is omitted from this table. Only `dest` and
`dest_paths` are declared write destinations; see below for what that means.

| Operation | Subcommand | Required | Optional | Declared destination |
|---|---|---|---|---|
| `status` | | | `path` (legacy `repo` alias) | |
| `add` | | `paths[]` | | |
| `diff` | | | `staged`, `path` | |
| `restore` | | `dest_paths[]` | `staged` | `dest_paths[]` |
| `stash` | `push` | | `message` | |
| `stash` | `pop` | | `index` | |
| `stash` | `list` | | | |
| `log` | | | `n`, `author`, `since`, `path` | |
| `show` | | | `ref` | |
| `symbol_history` | | `symbol_id` | `n` | |
| `commit` | | `message` | `allow_empty` | |
| `cherry_pick` | | `ref` | | |
| `branch` | `list` | | | |
| `branch` | `create` | `name` | `from` | |
| `branch` | `delete` | `name` | `force` | |
| `checkout` | | `ref` | `create` | |
| `switch` | | `branch` | `create` | |
| `reset` | | `mode` | `ref` | |
| `fetch` | | | `remote`, `branch` | |
| `pull` | | | `remote`, `branch`, `rebase` | |
| `push` | | | `remote`, `branch`, `force`, `set_upstream` | |
| `clone` | | `url`, `dest` | `branch`, `depth` | `dest` |
| `remote` | `list` | | | |
| `remote` | `add` | `name`, `url` | | |
| `remote` | `remove` | `name` | | |
| `tag` | `list` | | | |
| `tag` | `create` | `name` | `ref`, `message` | |
| `merge` | | `branch` | `message`, `ff_only` | |
| `worktree` | `add` | `dest`, `branch` | | `dest` |
| `worktree` | `list` | | | |
| `worktree` | `remove` | `dest` | `force` | `dest` |
| `create_worktree` | | `dest`, `branch` | | `dest` |

## How `read_only` is enforced

Two locations in the input schema are declared write destinations. `dest` is
annotated on the property; `dest_paths` is annotated on its **items**, because a
`murmur-destination` on an array property would resolve to no string value at
all:

```yaml
    dest:
      type: string
      format: murmur-destination
    dest_paths:
      type: array
      items:
        type: string
        format: murmur-destination
```

When a capsule grants `capabilities.filesystem.read_only`, a call whose declared
destination falls under a read-only entry is refused before dispatch, and the
refusal names the declaration. The runtime renders the array location with `[]`:

```text
Refused by the capsule manifest: 'src/vendored' is under the read-only path
'src' declared in capabilities.filesystem.read_only. Identified as a write by
destination 'dest' declared by the tool's input schema.

Refused by the capsule manifest: 'src/keep.rs' is under the read-only path
'src' declared in capabilities.filesystem.read_only. Identified as a write by
destination 'dest_paths[]' declared by the tool's input schema.
```

### Why `repo`, `path` and `paths` are not annotated

An annotation lowers to a location in the input and carries no condition on a
sibling value, so there is no way to say "this is a destination only when
`operation` is `worktree`". A property shared with a read operation therefore
cannot be annotated without breaking that read under exactly the grant the tool
exists to respect.

| Property | Read operations that use it | Consequence of annotating it |
|---|---|---|
| `repo` | `log`, `diff`, `show`, `status` | Every git call becomes a write, so a capsule under `read_only` could no longer run `git log` |
| `path` | `diff` and `log` (pathspec filter), `status` (legacy `repo` alias) | `log`/`diff` filtered to a read-only path would refuse |
| `paths` | `add` | Staging a read-only file would refuse, though the bytes written are the index under `.git/` |

`repo` is an **operating context** rather than a destination: what a call writes
under it depends on the operation, not on the property's value. The current
annotation vocabulary — `murmur-destination` for a string, `murmur-opaque` for a
stored subtree — has no term for that, so `repo` stays undeclared and its calls
stay judged by key name. Note the consequence: because the tool now annotates
something, `W-SEC-018` no longer fires for it, so nothing warns about `repo`.

`status`, `worktree/list`, `worktree/add` and `worktree/remove` all emit a `path`
key in their results. Those are outputs, not inputs — the runtime scans tool
input only.

## Breaking change in 0.2.0

Two operation families moved to their own destination properties:

| Operation | No longer accepts | Now takes |
|---|---|---|
| `restore` | `paths` | `dest_paths` |
| `worktree`/`add`, `worktree`/`remove`, `create_worktree` | `path` | `dest` |

Sent the old way, `restore` returns `ok: false` with `missing required field:
dest_paths (must be an array of strings)` and restores nothing; the three
worktree operations return `ok: false` with `missing required field: dest` and
create or delete nothing. There is no fallback to the old spelling, because a
silent one would leave the call judged by key name again — the state this
version exists to leave.

`add` (`paths`), `diff` and `log` (`path`), `status` (`path` as a `repo` alias)
and `clone` (`dest`) are unchanged, as is every output key.

## Declaring it in a manifest

```yaml
name: my-capsule
version: 0.1.0

artifacts:
  - name: murmur-tool-git
    version: ">=0.2"

capabilities:
  shell:
    allow:
      - bash   # fallback for operations outside murmur-tool-git's scope
  # git is NOT listed here — murmur-tool-git handles all standard git operations.
  # Adding git to shell.allow would be redundant and would grant broader shell
  # access to arbitrary git subcommands; the artifact enforces the operation set.
```

When `murmur-tool-git` is declared as an artifact, the capsule runtime makes the tool available as a structured JSON tool call. The capsule does not need `git` in `shell.allow` for any operation the artifact covers.

## Building

```bash
cargo build -p murmur-tool-git --release
```

The binary is written to `target/release/murmur-tool-git`.

## Validation

The `validate/` directory contains a standalone binary harness that exercises every v1 operation against a real git repository. It creates a self-contained playground under the system temp directory, invokes `murmur-tool-git` directly (stdin JSON → stdout JSON), and cleans up completely on exit — including on failure and panic.

```bash
# build the tool first
cargo build -p murmur-tool-git --release

# run all validation (cleans up automatically)
cargo run -p murmur-tool-git-validate

# run one operation group only
cargo run -p murmur-tool-git-validate -- --op worktree

# keep the playground for inspection after a failure
cargo run -p murmur-tool-git-validate -- --keep
```

The playground is created in the system temp directory and removed on exit, including on failure and panic. If `--keep` is passed, the path is printed at the end:

```
Playground kept at: /tmp/murmur-tool-git-validate-1234567890/
```

The playground contains:
- `remote.git/` — bare repo acting as origin
- `repo/` — working repo pre-seeded with an initial commit, two feature branches, and an origin remote
- `worktrees/` — directory used by worktree add/remove tests
