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
| `tools/murmur-tool-request-input/`, `murmur-tool-create/`, `murmur-tool-editor/`, `murmur-tool-corpus/` | WASM (`wasm32-wasip2`) | `.wasm` + `murmur.yaml` → `.mur.zip` |
| `tools/murmur-tool-git/`, `murmur-tool-registry-search/`, `murmur-tool-code-graph/`, `murmur-tool-test-report/`, `murmur-tool-code-coverage/` | Native binary | `bin/<name>` + `murmur.yaml` → `.mur.zip` |
| `skills/` | Docs only | `skill.md` + `murmur.yaml` → `.mur.zip` |

These tools are native because they need capabilities a `wasm32-wasip2` guest doesn't
have: `murmur-tool-git` spawns the system `git` binary; `murmur-tool-registry-search`
needs raw sockets and a native TLS stack; `murmur-tool-code-graph`,
`murmur-tool-test-report`, and `murmur-tool-code-coverage` link C sources (bundled
SQLite, tree-sitter) that don't cross-compile. They are excluded from the workspace
wasm build and built by `build.yml`'s `build-native` matrix instead.

Nothing lists them by name to make that happen. `scripts/classify-crates.sh` reads
each crate's own manifest and classifies every `[workspace] members` entry:

| Class | Rule | Wasm build |
|---|---|---|
| `native-artifact` | `murmur.yaml` says `implementation: native` | excluded |
| `internal-bin` | no `murmur.yaml`, and Cargo.toml declares a `[[bin]]` — a host-side helper, e.g. `murmur-tool-git-validate` | excluded |
| `wasm-artifact` | `murmur.yaml` says `implementation: wasm`, or omits the key (drivers and hooks) | built |
| `internal-lib` | no `murmur.yaml`, library only — linked into wasm artifacts, e.g. `murmur-test-parse`, `murmur-hook-transcript` | built |

A member matching none of the four is a hard error, not a guess.

## Building locally

These commands are for development only — CI handles all building and packaging when you push a release tag.

```bash
# All WASM artifacts — the same command CI runs
./scripts/build-wasm.sh

# A single artifact
cargo build -p murmur-driver-anthropic --target wasm32-wasip2 --release
```

`build-wasm.sh` is the one definition of "build everything that targets wasm": it
derives the `--exclude` flags from the classification above and prints which rule
excluded each crate. Extra arguments are passed through to `cargo build`. Running
`cargo build --workspace --target wasm32-wasip2 --release` by hand instead fails
deep inside a C build (`tree-sitter` needs `clang`, `libsqlite3-sys` needs a C
toolchain) with an error that names the C compiler rather than the missing
exclusions.

Output lands in `target/wasm32-wasip2/release/<crate_name>.wasm`.

Note: a bare `cargo build --workspace` on the host (no `--target`) fails at link
time on the WASM `cdylib` tools — build native tools with `-p <name>` instead.

### Validating a built component

Every WASM artifact must be a well-formed component whose world-level
imports/exports match its category (hooks export `murmur:hook/lifecycle`;
drivers and wasm tools export `murmur:tool/run`), at the exact interface version
declared by the vendored WIT it is built against — `wit/hook/deps/murmur-hook/lifecycle.wit`
for hooks, `wit/guest/deps/murmur-tool/tool.wit` for tools. A component left
unrebuilt across a WIT version bump exports the old version and is rejected here
rather than failing to link at `mur run`.

The script also checks a hook's `murmur:*` imports against an allowlist: the
four interfaces `world hook` declares — `murmur:runtime/inference`,
`murmur:runtime/tokens`, `murmur:task-io/read` and `murmur:conversation/read` —
plus the type-only `murmur:hook/lifecycle` instance the first and last of those
pull in. An import declared in `world hook` but never called is not emitted into
the built component, so most hooks carry far fewer than four;
`murmur-hook-memory` is the one that carries them all. An import outside the
world's own set fails here with `unexpected import '<id>'`, which is the case
the allowlist exists to catch — the host would refuse to link it at `mur run`.

Run bare, the script validates every `.wasm` in the build output and exits
non-zero if any failed — this is exactly what CI runs:

```bash
./scripts/validate-component.sh
```

Given a path, it validates that one artifact:

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

A new artifact is a four-file change, enforced by CI:

1. `artifacts.toml` — add an `[[artifact]]` entry (name, path, version).
2. Root `Cargo.toml` — add the crate to `[workspace] members` (if it is a crate).
3. `.github/workflows/build.yml` — add it to the matching matrix
   (`build-wasm`, `build-native`, or `build-skills`).
4. `scripts/validate-component.sh` — add the name to the category map (WASM
   components only). The script refuses to skip a name it does not recognise and
   exits `2`, so leaving this out fails CI rather than silently validating
   nothing. A name matching `murmur-hook-*` or `murmur-driver-*` is already
   covered by its wildcard; a `murmur-tool-*` WASM component must be listed
   explicitly, because most tools are native binaries the script skips.

To start the artifact directory itself, `murmur-tool-create` scaffolds one for a
`native` tool, a `wasm` tool, or a `hook` — emitting a `murmur.yaml` that already
carries the `runtime:` / `implementation:` split and the `requires_files:` entry
this checklist and `scripts/check-build-coverage.sh` depend on. See
[tools/murmur-tool-create/README.md](./tools/murmur-tool-create/README.md) for
what each arm generates.

A native tool needs no further change: `implementation: native` in its
`murmur.yaml` is what excludes it from the wasm build, via
`scripts/classify-crates.sh`. `scripts/check-build-coverage.sh` (run by CI) fails
if an artifact in `artifacts.toml` is not built by exactly one `build.yml` matrix,
or is built by a matrix that disagrees with its `implementation:` — so a native
tool added to `build-wasm`, or one whose `implementation:` changes without its
matrix entry moving, fails CI rather than a release.

> **Note — the crates under `libs/`.** They are *not* artifacts and must never
> be added to `artifacts.toml` or a `build.yml` matrix — they are shared,
> unpublished libraries, not shippable components. Each is compiled into the
> artifacts that depend on it, so changing one changes every dependent
> component's binary.

| Library | Holds | Used by |
|---|---|---|
| `libs/murmur-test-parse` | The four test-runner output parsers plus format auto-detection | `murmur-hook-regression-verifier`, `murmur-tool-test-report` |
| `libs/murmur-hook-transcript` | The host's tool-result envelope marker and the readers that turn a lifecycle `message`'s `content` into driver-safe text | `murmur-hook-compact`, `murmur-hook-memory` |

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

## Published hooks vs. the WIT contract

Every published `.mur.zip` embeds the version of `murmur:hook/lifecycle` its
component was built against. That embedded version is the string the host
resolves an instantiation against, and it is a separate axis from the
artifact's own release version in `artifacts.toml` — a hook at release `0.4.0`
can, and does, export interface `@0.8.0`.

All eight `hooks/*` artifacts export `murmur:hook/lifecycle@0.8.0`, matching
this repo's `wit/hook/` mirror. Murmur's host accepts that one version and keeps
no fallback: `LIFECYCLE_IFACE` in `crates/capsule-runtime/src/hooks.rs` names a
single instance, and a component exporting any other version is rejected at
instantiation.

That is why a `murmur:hook` bump is never a partial change. Every hook is
rebuilt against the new mirror and every hook's `version` in `artifacts.toml`
moves, including hooks whose Rust source did not change: a rebuilt component
exports a different interface version, so its bytes differ and the registry
cannot serve the new component at the old artifact version.

To confirm what a published binary actually exports:

```bash
mur install -g murmur-hook-compact
unzip -o -d /tmp/hook-check \
  ~/.murmur/artifacts/murmur-hook-compact/<version>/murmur-hook-compact-<version>.mur.zip
strings /tmp/hook-check/murmur_hook_compact.wasm \
  | grep -o 'murmur:hook/lifecycle@[0-9.]*' | sort -u
```

`wasm-tools component wit <file>.wasm` prints the full interface if you need
to confirm a specific record or variant case rather than the version alone. For
a locally built component, `./scripts/validate-component.sh` checks the same
thing across every artifact at once, reading the expected version out of
`wit/hook/deps/murmur-hook/lifecycle.wit` rather than a value written down here.

## Where a hook's logic lives

A hook crate is three layers:

- a `cfg`-independent module at the crate root holding the pure decision logic —
  plain mirrors of the WIT records and the functions that act on them;
- a `#[cfg(target_arch = "wasm32")]` adapter that converts a WIT record into
  those mirrors, calls the logic, and converts the result back into a
  `HookOutput` — and holds no branch, threshold or string the runtime acts on;
- a `#[cfg(test)] mod tests` at the crate root, which therefore runs on the host.

Code behind `#[cfg(target_arch = "wasm32")]` does not exist for the host target.
A crate written entirely behind that gate compiles to nothing when cargo builds
it for the host, so `cargo test --workspace` reports a green run having executed
none of its lines — and nothing in CI distinguishes "0 tests passed" from
"20 tests passed". Splitting the logic out of the gate is what makes the crate
testable at all; keeping the adapter free of decisions is what keeps the tests
worth reading.

`murmur-hook-compact`, `murmur-hook-memory` and `murmur-hook-regression-verifier`
are the reference implementations. `murmur-tool-create`'s `hook` arm scaffolds
this shape, so a new hook starts from it rather than needing to know it: the
generated crate ships a `logic` module, a converting adapter and one passing
test, and deleting that test is a deliberate edit visible in a diff.

## Running tests

```bash
# Whole workspace
cargo test --workspace

# One crate
cargo test -p murmur-tool-create

# Show test stdout
cargo test --workspace -- --nocapture
```

Cargo prints one result line per test binary, not a workspace-wide total.

`tools/murmur-tool-corpus/tests/mur_run_state.rs` is `#[ignore]`d, because it
launches the `mur` runtime as a subprocess to prove the corpus reaches a durable
store under a real capsule. Point `MUR_BIN` at a `mur` binary — or put one on
`PATH` — and ask for it by name:

```bash
MUR_BIN=/path/to/mur cargo test -p murmur-tool-corpus --test mur_run_state -- --ignored
```

A run that finds no `mur` fails rather than skipping. The `corpus-state` workflow
runs it against a `mur` built from murmur's default branch.

`tools/murmur-tool-create/tests/mur_manifest_shape.rs` is `#[ignore]`d for the same
reason: it builds and publishes an unedited scaffold through a real `mur` to prove
the generated `murmur.yaml` classifies and packs the way its author asked for. It
resolves `mur` the same way, and fails rather than skipping when it finds none:

```bash
MUR_BIN=/path/to/mur cargo test -p murmur-tool-create --test mur_manifest_shape -- --ignored
```

`tools/murmur-tool-create/tests/scaffolded_hook_tests.rs` is `#[ignore]`d because
it runs a child `cargo test` inside a freshly scaffolded hook, which needs a
cargo registry able to resolve that crate's `wit-bindgen` and `tempfile` pins. It
asserts the child reports `1 passed` rather than `0 passed` — the assertion that
a scaffolded hook is testable the moment it is generated:

```bash
cargo test -p murmur-tool-create --test scaffolded_hook_tests -- --ignored
```
