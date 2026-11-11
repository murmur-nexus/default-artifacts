# tool-git

## What it is

`tool-git` gives a Murmur capsule the common git operations as typed tool calls, returning
structured JSON in place of raw `git` output. It is a WASM component (`wasm32-wasip2`, exporting
`murmur:tool/run@0.2.0`), one zip for every platform.

Every git command it runs goes through `murmur:process` `run`: the program `git`, the
operation's arguments, the work folder as its working directory, and no extra environment. So the
capsule must list `git` in `capabilities.shell.allow` (see
[What the capsule must grant](#what-the-capsule-must-grant)), and murmur puts each git command to
the capsule's decision point exactly as it puts a shell `git` command: the
`capabilities.filesystem.read_only` check and every `on-shell` deny hook see its argv. For
operations outside its scope, `bash` in the shell allowlist serves as the escape hatch.

```bash
mur install -g murmur/tool-git@0.8.0
```

The tool imports `murmur:process/process@0.1.0`, so it needs a `mur` whose `mur version` reports
WIT level 4 or higher: the murmur release holding #390 (`f9d4a1f8`). No murmur release holds it
yet; murmur `main` from `f9d4a1f8` on does. An older `mur` installs the zip, then refuses the
capsule at launch:

```text
error[E-WIT-LEVEL-001]: artifact 'local/tool-git@0.7.0' does not fit WIT level 3, the level this mur serves: it imports murmur:process/process@0.1.0, and level 3 has no version of murmur:process
```

Supported operations: `status`, `add`, `diff`, `restore`, `stash`, `log`, `show`,
`symbol_history`, `commit`, `cherry_pick`, `branch`, `checkout`, `switch`, `reset`, `fetch`,
`pull`, `push`, `clone`, `remote`, `tag`, `merge`, `worktree`, `create_worktree` (the same as
`worktree`/`add`).

Every operation accepts an optional `repo`: the repository root, relative to the work folder or
absolute. If omitted, the tool asks git for the repository the work folder is in.

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

## How paths are found

git sees `repo`, `dest`, `dest_paths`, `paths` and `path` exactly as the call gives them, and runs
in the work folder. So a relative value is relative to the work folder, and an operation that
passes `-C <repo>` (all but `clone`) makes git read `dest`, `dest_paths`, `paths` and `path`
relative to the repository. Where git may reach is the capsule's containment, not the tool's.

The tool itself opens a file in three places.

**`symbol_history`'s database.** It reads tool-code-graph's `.murmur/code-graph.db` below the
repository root `R` (the `repo` value, or the discovered root), trying in order:

1. `R` relative and not climbing out of the work folder: `<R>/.murmur/code-graph.db` in the work
   folder. A relative `R` whose first segment is `murmur-host` is refused while the session opens
   a host folder to the tool (`reserved_name`), because that is where host folders are opened.
2. Otherwise git places it: `git -C <R> rev-parse --show-toplevel --absolute-git-dir
   --show-prefix`, then `git --git-dir=<G> --work-tree=<T> rev-parse --path-format=relative
   --show-toplevel`, run in the work folder. A worktree that does not lie above the work folder
   opens there, below `R`'s place in it.
3. Otherwise the database through the host folder the session opens to the tool that covers it,
   the longest one winning.
4. Otherwise the call fails: `symbol_history: cannot read <repo>/.murmur/code-graph.db: it is
   outside the work folder and every host folder this session opens to tool-git`.

The database is opened read-only as `file:<path>?immutable=1`: the tool never writes it, never
creates a lock or journal file beside it, and reads it in a read-only host folder. The SQLite built
into the component has no WAL support, so a database in WAL mode fails with `symbol_history: failed
to read code-graph db: file is not a database`. tool-code-graph never turns WAL on.

**`clone`'s destination.** A `dest` that already exists — a file, or a folder empty or not — is
refused before git runs: `destination '<dest>' already exists`. The tool checks a relative `dest`
in the work folder and an absolute one through the host folder covering it. An absolute `dest` no
host folder covers is left to git, which refuses a non-empty folder with the same answer but clones
into an existing **empty** folder.

**`log_path`.** Each operation appends a line to the call's `log_path`, best effort: a relative
path opens in the work folder, an absolute one through the host folder covering it, and any other,
or any write error, is skipped without failing the call.

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

A destination value is checked relative to the work folder, while `restore` hands `dest_paths` to
git relative to the repository. Under `read_only: [repo/src]`, `dest_paths: ["repo/src/lib.rs"]` is
refused and `dest_paths: ["src/a.rs"]` is not.

Each git command is then put to the same grant on its argv. In a `mur run` at `scoped` with
`read_only: [repo/src]`, that check refused none of `status`, `log`, `add`, `commit`, `checkout`,
`switch` and `reset --hard`: their argv names no path under `repo/src`. The calls the grant refused
were the three whose declared destination lies under it: `restore` of `repo/src/lib.rs`,
`worktree`/`add` at `repo/src/wt` and `clone` into `repo/src/c`.

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

## Breaking in 0.7.0

0.7.0 replaces the native binary with a WASM component. Operation names, inputs and every result a
capsule saw from 0.6.0 are unchanged, except:

- **One zip for every platform.** It needs a `mur` at WIT level 4 or later, one that links
  `murmur:process@0.1.0`. There are no per-platform native zips and no `bin/tool-git`.
- **The decision point judges every git command.** `on-shell` deny hooks (such as
  `hook-command-guard`) and the `capabilities.filesystem.read_only` check now see each git command
  the tool runs. A rule refusing `git push --force` refuses `push` with `force: true`: the call
  returns `tool-git: murmur refused to run git: on-shell: <the hook's reason>` and runs no further
  git command. Any rule murmur refuses on reads the same way: `tool-git: murmur refused to run git:
  <rule>: <reason>`.
- **git's output is capped.** git's stdout and stderr are each cut at 1 MiB per command. The
  tool keeps the complete lines before the cut, sets the result's `truncated` flag, and ends the
  summary with `; git output cut at 1 MiB`.
- **`symbol_history` reads only where the component can.** The database must be inside the work
  folder or a host folder the session opens to the tool; anywhere else it is refused by name (see
  [How paths are found](#how-paths-are-found)). A WAL-mode database cannot be read.
- **A `capabilities:` block on the tool's entry now narrows it,** as for any WASM tool. 0.6.0 ran
  with the capsule-wide grants and ignored such a block (`W-SEC-008`).

Two smaller differences follow from the component not knowing the work folder's absolute path: an
absolute `clone` `dest` that is an existing empty folder outside every host folder is cloned into,
and a git command murmur cannot start reads `failed to execute git: <murmur's text>` where 0.6.0
said `git not found in PATH`. A git command that runs past the call's deadline fails the call with
`git <subcommand> timed out`.

## Breaking change in 0.2.0

Two operation families moved to their own destination properties:

| Operation | No longer accepts | Now takes |
|---|---|---|
| `restore` | `paths` | `dest_paths` |
| `worktree`/`add`, `worktree`/`remove`, `create_worktree` | `path` | `dest` |

Sent the old way, `restore` returns an error with `missing required field:
dest_paths (must be an array of strings)` and restores nothing; the three
worktree operations return an error with `missing required field: dest` and
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
  - name: murmur/tool-git
    version: "0.8.0"

capabilities:
  shell:
    allow:
      - git    # tool-git runs every git command through murmur:process
      - bash   # fallback for operations outside tool-git's scope
```

When `tool-git` is declared as an artifact, the capsule runtime makes the tool available as a
structured JSON tool call.

## What the capsule must grant

murmur runs each git command at the session's containment tier, in its cgroup and network
namespace, with the sanitized shell environment.

| Need | Capsule grant |
|---|---|
| Run `git` | `git` in `capabilities.shell.allow`. Without it murmur refuses every git command the tool asks for, at `scoped` and at `sealed`, and `status` returns `tool-git: murmur refused to run git: capabilities.shell.allow: binary 'git' is not in capabilities.shell.allow` |
| Read and write the repository | The repository inside the work folder; under `scoped` and `sealed` git reaches nothing outside it |
| `fetch`, `pull`, `push`, `clone` with a remote that is a local path | `sh` in `capabilities.shell.allow`: git starts `git-upload-pack` and `git-receive-pack` through `/bin/sh`. Without it they fail with `cannot exec 'git-upload-pack '<remote>'': Permission denied`, at `scoped` and at `sealed`, as they did for 0.6.0 |
| `fetch`, `pull`, `push`, `clone` over the network | The remote's host in `capabilities.network.allow`, and a `capabilities.shell.interpreter_runtime` grant for `git` naming the directory its remote helpers live in (`/usr/lib/git-core` on Debian and Ubuntu); without it `git` cannot start `git-remote-https` |

Every operation was run in a real `mur run` at `scoped` and at `sealed` with these grants:

```yaml
capabilities:
  containment: scoped          # and sealed
  shell:
    allow:
      - git
      - sh
    interpreter_runtime:
      - binary: git
        dirs:
          - path: /usr/lib/git-core
            list_dir: false
```

Under a real `mur run`, each operation gives the same result as the frozen results recorded from
tool-git 0.6.0 (`tests/fixtures/native-0.6.0-results.json`).

## Building

```bash
./scripts/wasi-sdk.sh        # once: the pinned WASI C toolchain that compiles SQLite
cargo build -p tool-git --target wasm32-wasip2 --release
./scripts/package-artifact.sh tools/tool-git dist/
```

The component is `target/wasm32-wasip2/release/tool_git.wasm`; the zip is
`dist/tool-git-0.8.0.mur.zip`.

## Tests

```bash
cargo test -p tool-git
```

| Suite | What it runs |
|---|---|
| `git_tool`, `git_tool_integration`, `symbol_history` | The behaviour suites (`tests/cases/`), through the compiled component in Wasmtime with a test host for `murmur:process` `run` |
| `git_tool_core`, `git_tool_integration_core`, `symbol_history_core` | The same suites through the portable core on the host, also asserting each `error_kind` |
| `wasm_component`, `rules_core` | Refusals, `failed`, time-outs, the 1 MiB caps, the database lookup, `clone`'s destination and `log_path`, through the component and through the core |
| `parity` | Every operation over a fixture work folder, through the component and the core, against the results and git argv recorded from tool-git 0.6.0 |
| `manifest_declares_destinations` | The `dest`/`dest_paths` annotations in `murmur.yaml` |

One suite is ignored by default:

```bash
# Real `mur run` at scoped and sealed through a fake provider on loopback, against the recorded
# 0.6.0 results; and what an on-shell hook and a read_only grant refuse.
MUR_BIN=<mur at WIT level 4> cargo test -p tool-git --test mur_run_parity -- --ignored --nocapture
```
