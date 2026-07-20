# Build and Release Guide

This repository contains the default Murmur artifacts: inference drivers, hooks, tools, and skills. Each artifact packages to a standalone `.mur.zip` published to the Murmur artifact registry.

## Prerequisites

- Rust toolchain — pinned by `rust-toolchain.toml` at the repo root (exact `channel`,
  plus `targets = ["wasm32-wasip2"]`); `rustup` reads this file automatically, so no
  manual `rustup target add` is needed.
- `wasm-tools` — for validating built components locally. CI pins an exact version;
  install the same one (check the `Install wasm-tools` step in `.github/workflows/ci.yml`).
- `zip` (macOS/Linux standard)

## Artifact types

| Directory | Runtime | Output |
|---|---|---|
| `drivers/` | WASM (`wasm32-wasip2`) | `.wasm` + `murmur.yaml` → `.mur.zip` |
| `hooks/` | WASM (`wasm32-wasip2`) | `.wasm` + `murmur.yaml` → `.mur.zip` |
| `tools/murmur-tool-request-input/`, `murmur-tool-create/`, `murmur-tool-editor/` | WASM (`wasm32-wasip2`) | `.wasm` + `murmur.yaml` → `.mur.zip` |
| `tools/murmur-tool-git/`, `murmur-tool-registry-search/`, `murmur-tool-code-graph/`, `murmur-tool-test-report/`, `murmur-tool-code-coverage/` | Native binary | `bin/<name>` + `murmur.yaml` → `.mur.zip` |
| `skills/` | Docs only | `skill.md` + `murmur.yaml` → `.mur.zip` |

Five tools are native because they need capabilities a `wasm32-wasip2` guest doesn't
have: `murmur-tool-git` spawns the system `git` binary; `murmur-tool-registry-search`
needs raw sockets and a native TLS stack; `murmur-tool-code-graph`,
`murmur-tool-test-report`, and `murmur-tool-code-coverage` link C sources (bundled
SQLite, tree-sitter) that don't cross-compile. All five are excluded from the
workspace wasm build in `ci.yml` and built by `build.yml`'s `build-native` matrix
instead.

## Building locally

These commands are for development only — CI handles all building and packaging when you push a release tag.

```bash
# All WASM artifacts
cargo build --workspace --target wasm32-wasip2 --release

# A single artifact
cargo build -p murmur-driver-anthropic --target wasm32-wasip2 --release
```

Output lands in `target/wasm32-wasip2/release/<crate_name>.wasm`.

Note: a bare `cargo build --workspace` on the host (no `--target`) fails at link
time on the WASM `cdylib` tools — build native tools with `-p <name>` instead.

### Validating a built component

Every WASM artifact must be a well-formed component whose world-level
imports/exports match its category (hooks export `murmur:hook/lifecycle`;
drivers and wasm tools export `murmur:tool/run`). CI enforces this on every
push/PR; to check locally the same way:

```bash
./scripts/validate-component.sh target/wasm32-wasip2/release/murmur_hook_debug.wasm
```

### Native tools

Each native tool has a `package.sh` that builds, stages, and zips the artifact:

```bash
cd tools/murmur-tool-git        # or any other native tool
./package.sh                    # auto-detect platform, build and zip
./package.sh darwin-aarch64     # explicit platform
```

Output: `tools/<name>/<name>-<version>-<platform>.mur.zip` (gitignored).

## Adding a new artifact

A new artifact is a three-file change, enforced by CI:

1. `artifacts.toml` — add an `[[artifact]]` entry (name, path, version).
2. Root `Cargo.toml` — add the crate to `[workspace] members` (if it is a crate).
3. `.github/workflows/build.yml` — add it to the matching matrix
   (`build-wasm`, `build-native`, or `build-skills`).

A native tool must additionally be added to the `Cargo build (wasm)` exclude
list in `ci.yml`. `scripts/check-build-coverage.sh` (run by CI) fails if an
artifact in `artifacts.toml` is not built by exactly one `build.yml` matrix.

## Version management

All artifact versions are controlled from a single file: **`artifacts.toml`** at the repo root. After editing it, propagate versions to every surface:

```bash
./scripts/apply-versions.sh
```

This updates `[workspace.package] version` in the root `Cargo.toml`, the
`version:` field in each artifact's `murmur.yaml`, the `VERSION=` variable in
each native tool's `package.sh`, and regenerates `artifacts-index.json`.
CI rejects any push where these surfaces are out of sync with `artifacts.toml` —
never bump a version by hand in an individual file.

## Releasing a new version

1. Edit `artifacts.toml` — bump `workspace_version` and each artifact `version`.
2. Run `./scripts/apply-versions.sh`.
3. Commit, push a branch, open a PR, merge to `main`.
4. Tag the merge commit and push the tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

CI picks up the `v*` tag, re-verifies version sync, builds every artifact in
`artifacts.toml`, packages each into a `.mur.zip` (native tools produce one zip
per platform), and creates a GitHub Release with all zips attached.

## WIT sync

The artifact-facing WIT lives as a file mirror under `wit/guest/` and
`wit/hook/`, vendored from `murmur/crates/capsule-runtime/wit/`. The `wit-sync`
CI job checks the mirror is byte-identical to the murmur commit pinned in
`.github/workflows/ci.yml`.

**The mirror and the pin must always move together.** To update after a WIT
change in murmur: check out murmur beside this repo (`../murmur`), copy its
`wit/{guest,hook}` over this repo's `wit/`, run `./scripts/check-wit-sync.sh`
until it exits `0`, then set the `ref:` in `ci.yml`'s `wit-sync` job to that
same murmur commit — both changes in one commit.

## Running tests

```bash
cargo test --workspace
```
