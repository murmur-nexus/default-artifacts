# Build and Release Guide

This repository holds Murmur's default artifacts — inference drivers, hooks, tools and skills —
and publishes them as GitHub release assets. For the general flow of publishing an artifact
source, see murmur's [Publish your own artifact source](https://docs.murmur.nexus/how-to/publish-artifact-source/);
this page covers what is particular to this repository.

## Prerequisites

| Tool | Version | Where it is pinned |
|---|---|---|
| Rust | the `channel` in `rust-toolchain.toml`, with `wasm32-wasip2` | `rustup` reads the file at every cargo call; never `rustup target add` by hand |
| `wasm-tools` | `1.247.0` | the `Install wasm-tools` steps in `.github/workflows/` |
| `mur` with the `index` subcommand | murmur at `MURMUR_REV` | `env:` in `.github/workflows/build.yml`; `ci.yml`'s `build-isolation` job reads it from there |

Build `mur` from a murmur checkout, from inside it so this repository's toolchain file is not
picked up, and point `MUR_BIN` at it. Every script below reads `MUR_BIN`, else `mur` on `PATH`,
and refuses one without `index`:

```bash
git -C ../murmur checkout <MURMUR_REV>
(cd ../murmur && cargo build -p murmur-cli)
export MUR_BIN=$PWD/../murmur/target/debug/mur
```

## Layout

An artifact is a directory `<category>/<name>/` holding a `murmur.yaml`, where the category is
`drivers`, `hooks`, `tools` or `skills`. The directory is named after the artifact, and its
`murmur.yaml` is the only place its name and version are written. Every artifact installs as
`murmur/<name>`. `./scripts/list-artifacts.sh` lists them, and refuses a directory that differs
from its `name:` or a name with a `murmur-` prefix.

| Kind | Declared by | Payload | Built by |
|---|---|---|---|
| wasm | `implementation: wasm`, or none (drivers, hooks) | `<name with - as _>.wasm` | `./scripts/build-wasm.sh` |
| native | `implementation: native` | `bin/<name>`, one zip per platform | `./scripts/build-native.sh` (all) / `cargo build --release -p <name>` (one) |
| skill | `runtime: skill` | `skill.md` | nothing to compile |

A tool is native only when a `wasm32-wasip2` guest cannot do its job — spawning `git`, raw sockets
with a native TLS stack, or C sources that do not cross-compile. A tool that calls a third-party
API stays wasm: the credential gateway serves wasm stores alone.

`libs/` holds shared crates compiled into the artifacts that use them; they have no `murmur.yaml`
and are never published. `./scripts/classify-crates.sh` classifies every workspace member from its
own manifest, and is what excludes native and internal crates from the wasm build.

## Build one artifact

```bash
cargo build -p tool-editor --target wasm32-wasip2 --release   # wasm
cargo build --release -p tool-git                              # native
./scripts/validate-component.sh target/wasm32-wasip2/release/tool_editor.wasm
./scripts/package-artifact.sh tools/tool-editor release/       # → release/tool-editor-0.7.0.mur.zip
./scripts/local-install.sh tool-editor /path/to/capsule-folder
```

`validate-component.sh` checks a component's world-level imports and exports against its category
at the exact interface version in `wit/`; run bare, it validates every `.wasm` in the build output.
`local-install.sh` builds, packs and installs one artifact into a capsule, where it is
`local/<name>` — reference it that way in the capsule's `artifacts:`. The target must be a capsule
folder holding `murmur.yaml`; any other folder is refused before anything is built.

Cargo unifies dependency features across every crate one invocation builds, so the build scripts
give each artifact its own invocation. `cargo build -p <name>`, `local-install.sh`, the
`wasm_component` tests and the release therefore produce the same bytes for an artifact, and
nothing needs rebuilding between testing and packing. `./scripts/check-build-isolation.sh
[<name>...]` proves it: it runs the release's build, rebuilds each artifact alone, and fails on any
zip that differs. It needs `MUR_BIN`; run it after changing a build script or a dependency's
features. CI runs it on every push and pull request, in `ci.yml`'s `build-isolation` job, with a
`mur` built at `build.yml`'s `MURMUR_REV`.

## Add one

1. Create `<category>/<name>/murmur.yaml`. `tool-create` scaffolds a native tool, a wasm tool or
   a hook with the `runtime:`, `implementation:` and `requires_files:` this layout needs — see
   [tools/tool-create/README.md](./tools/tool-create/README.md).
2. Add the crate to `[workspace] members` in `Cargo.toml` and run `cargo update --workspace --offline`.
   Give it `version = "0.0.0"` in its own `[package]`, never `version.workspace = true`: rustc
   hashes a crate's version into its symbols, so an inherited version would change every
   artifact's bytes on each release.
3. A wasm tool also needs an arm in `scripts/validate-component.sh`'s category map; `hook-*` and
   `driver-*` names are covered by their wildcards. A process driver gets its own arm ahead of
   `driver-*`, because it exports a different interface under the same prefix.

Nothing else lists it: the release, CI and every script find it through `list-artifacts.sh` and
`classify-crates.sh`.

A hook keeps its decision logic outside the `cfg(target_arch = "wasm32")` gate, with a thin adapter
behind it and host tests beside it: code behind the gate does not exist for the host, so a crate
written entirely behind it runs zero tests and still reports green. `hooks/hook-compact` is the
reference, and `tool-create`'s hook arm scaffolds that shape.

For a tool's write destinations (`format: murmur-destination`) and a driver's `upstream_auth:`
block, see murmur's [manifest reference](https://docs.murmur.nexus/reference/manifest/).

## Release

1. Bump `version:` in the `murmur.yaml` of every artifact whose payload changed — a minor bump
   below 1.0, `0.5.0` → `0.6.0`. `./scripts/check-version-drift.sh` names any you missed; CI runs
   it on every push.
2. Bump `[workspace.package] version` in `Cargo.toml` to the next minor. It names the release
   only: every crate pins `version = "0.0.0"`, so neither `Cargo.lock` nor any artifact's bytes
   change with it.
3. Merge, then tag the merge commit `v<workspace version>` and push the tag.
4. The `build` workflow runs. `verify` checks that the tag is `v` + the workspace version and the
   newest `v*` tag, and that no artifact drifted. `native` runs `./scripts/build-native.sh` on each
   platform runner. `release` builds `mur` at `MURMUR_REV` and the wasm artifacts, validates them, packs
   every artifact with `./scripts/package-release.sh`, and writes `artifacts-index.json` with
   `./scripts/index-release.sh`, merged with the latest release's index. Then
   `./scripts/check-release-installs.sh` installs every new version through `mur` against a
   loopback copy of the release, and only then is anything uploaded.
5. Afterwards, `mur install -g murmur/<name>@<version>` resolves from the new release.

**A version is a promise about bytes.** `mur install` writes a payload's sha256 into the
consumer's `murmur.lock`, and a store that holds `<name>/<version>` never fetches it again.
Different bytes republished at a version someone has resolved do not upgrade them; they fail their
next resolve on the pin, and no fix exists after the tag. So every change to what an artifact ships
moves its version, in the same commit.

**The bytes gate** enforces that at release time. Every `name@version` in both the previous index
and the new one must carry identical assets — each file and its sha256. A mismatch deletes the new
index, names the artifact, the file and both hashes, and fails the release before upload; bump that
artifact's version. `mur build` dates every zip entry 1980-01-01, so a rebuild of unchanged inputs
packs identical bytes, and a re-run over one commit writes the same index.

`check-version-drift.sh` predicts the gate from the sources:

```bash
./scripts/check-version-drift.sh                  # against the last v* tag — what CI runs
./scripts/check-version-drift.sh --base v0.24.0   # against a specific release
./scripts/check-version-drift.sh --audit          # every tag, every version
```

| Class | Inputs |
|---|---|
| `own tree` | the artifact's `murmur.yaml`, `Cargo.toml`, `src/`, `skill.md` |
| `path deps` | each `libs/*` it links, transitively |
| `wit mirror` | the `wit/` subtree its bindgen `path:` names |
| `workspace` | `[workspace.package]` minus its version (with it, for a crate that inherits `version.workspace = true`), the `[workspace.dependencies]` it declares, its resolved third-party versions in `Cargo.lock`, `rust-toolchain.toml` |

## WIT mirror

`wit/guest/`, `wit/hook/` and `wit/process-driver/` are a byte-identical copy of
`murmur/crates/capsule-runtime/wit/`, pinned to a murmur commit by `ref:` in `ci.yml`'s `wit-sync`
job. The mirror and the pin move together. To re-pin after a WIT change in murmur: check murmur
out beside this repository, copy its `wit/{guest,hook,process-driver}` over this repository's
`wit/`, run `./scripts/check-wit-sync.sh` until it exits 0, set `ref:` to that commit, and commit
both. Every artifact rebuilt against a new interface version ships different bytes, so bump each
one. The script looks for murmur beside the main checkout, from a worktree too; to compare
against a murmur checkout elsewhere, set `MURMUR_DIR` to its root.

## Tests

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
```

CI runs both, formatting first. The workspace uses rustfmt's defaults (there is no `rustfmt.toml`);
run `cargo fmt --all` before committing.

Four suites are `#[ignore]`d because they need something the default run does not have. The three
that drive a real `mur` read it from `MUR_BIN`, else `PATH`, and fail rather than skip without one:

```bash
MUR_BIN=$MUR_BIN cargo test -p tool-corpus --test mur_run_state -- --ignored         # 4 tests
MUR_BIN=$MUR_BIN cargo test -p tool-tavily --test mur_run_gateway -- --ignored       # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-create --test mur_manifest_shape -- --ignored    # 3 tests
cargo test -p tool-create --test scaffolded_hook_tests -- --ignored  # needs a cargo registry
```

| Suite | Proves |
|---|---|
| `mur_run_state` | the corpus reaches a durable store under a real `mur run`; the `corpus-state` workflow runs it nightly against murmur's default branch |
| `mur_run_gateway` | the runtime attaches the Tavily key to the tool's request and it appears nowhere in the session |
| `mur_manifest_shape` | an unedited scaffold packs and publishes as the kind its author asked for |
| `scaffolded_hook_tests` | a freshly scaffolded hook reports `1 passed`, not `0 passed` |
