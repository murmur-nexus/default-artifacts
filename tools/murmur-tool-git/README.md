# murmur-tool-git

## What it is

`murmur-tool-git` is a native binary tool artifact for Murmur capsules. It provides a structured JSON interface to the most common git operations, replacing the need to add `git` to `capabilities.shell.allow` in a capsule manifest. For operations outside its scope, `bash` in the shell allowlist serves as the escape hatch.

Supported operations: `status`, `add`, `diff`, `restore`, `stash`, `log`, `show`, `commit`, `cherry_pick`, `branch`, `checkout`, `switch`, `reset`, `fetch`, `pull`, `push`, `clone`, `remote`, `tag`, `merge`, `worktree`, `create_worktree` (compat alias).

Every operation accepts an optional `repo` field (absolute path to the repo root). If omitted, the tool auto-discovers the repo root from the current working directory.

## Declaring it in a manifest

```yaml
name: my-capsule
version: 0.1.0

artifacts:
  - name: murmur-tool-git
    version: ">=0.1"

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
