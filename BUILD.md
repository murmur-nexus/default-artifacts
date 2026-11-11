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
| wasi-sdk | `34.0` | `wasi-sdk.toml`; installed by `./scripts/wasi-sdk.sh` (`build-wasm.sh` and `local-install.sh` run it; `ci.yml` caches it) |
| `mur` with the `index` subcommand | murmur at `MURMUR_REV` | `env:` in `.github/workflows/build.yml`; `ci.yml`'s `build-isolation` and `wit-sync` jobs read it through `./scripts/murmur-pin.sh` |

wasi-sdk is the C compiler and wasi-libc sysroot for C sources compiled into a `wasm32-wasip2`
component, currently rusqlite's bundled SQLite in `tool-test-report`, `tool-code-coverage` and
`tool-code-graph`, and tree-sitter with its Rust and Python grammars in `tool-code-graph`;
`.cargo/config.toml` points `CC_wasm32_wasip2` and `AR_wasm32_wasip2` at the `.wasi-sdk` link the
script makes in the repository root. The script installs it once into
`${WASI_SDK_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/murmur-default-artifacts}`, shared by every
checkout and worktree; set `WASI_SDK_CACHE` to put that ~630 MB elsewhere.

Build `mur` from a murmur checkout, from inside it so this repository's toolchain file is not
picked up, and point `MUR_BIN` at it. Every script below reads `MUR_BIN`, else `mur` on `PATH`,
and refuses one without `index`:

```bash
git -C ../murmur checkout "$(./scripts/murmur-pin.sh)"
(cd ../murmur && cargo build -p murmur-cli)
export MUR_BIN=$PWD/../murmur/target/debug/mur
```

## Layout

An artifact is a directory `<category>/<name>/`, where the category is `drivers`, `hooks`, `tools`
or `skills`, named after the artifact. A driver, hook or tool holds a `murmur.yaml`, the only place
its name and version are written. A skill is `skills/<name>/SKILL.md`, a standard Agent Skills
folder whose front block holds its name, description and `metadata.version`, the only place its
name and version are written; it holds no `murmur.yaml`, and `mur build` generates the zip's from
the front block. Every artifact installs as `murmur/<name>`. `./scripts/list-artifacts.sh` lists
them, and refuses a directory that differs from its `name:` or a name with a `murmur-` prefix.

| Kind | Declared by | Payload | Built by |
|---|---|---|---|
| wasm | `implementation: wasm`, or none (drivers, hooks) | `<name with - as _>.wasm` | `./scripts/build-wasm.sh` |
| skill | `skills/<name>/SKILL.md` front block | `SKILL.md` and the files it names | nothing to compile |

`./scripts/check-skill-folders.sh` holds every skill folder to the standard: `SKILL.md` spelled
exactly so, `name` equal to the folder's name, a `description` of at most 1024 characters, a
semver `metadata.version`, no `murmur.yaml`, no symlink, and every other file named in the body.
`list-artifacts.sh` and `package-artifact.sh` run the same checks, and
`./scripts/test-check-skill-folders.sh` tests them. Because the folders follow the standard,
`npx skills add murmur-nexus/default-artifacts` installs them into any agent that reads it;
`npx skills` reads the default branch. [skills/README.md](./skills/README.md) holds each skill's
release history, outside every skill folder, so neither `npx skills` nor `mur build` packs it.

Every tool is a `wasm32-wasip2` component, packed as one zip for every platform. A tool runs a
program through `murmur:process`, under `capabilities.shell.allow`, as `tool-git` runs `git`,
`tool-browser` runs Chrome and `tool-computer` runs Xvfb and speaks the X protocol to it. It reaches
the network through `wasi:http`, under `capabilities.network.allow`; the credential gateway attaches a
third-party API's key to those requests. C sources compile into the component with the pinned
wasi-sdk (see Prerequisites): rusqlite's bundled SQLite and tree-sitter with its grammars do so in
`tool-test-report`, `tool-code-coverage` and `tool-code-graph`.

`libs/` holds shared crates compiled into the artifacts that use them; they have no `murmur.yaml`
and are never published. `libs/` also holds `murmur-fake-computer`, a test component the drivers'
computer-tool suites pack and nothing publishes. `./scripts/classify-crates.sh` classifies every
workspace member from its own manifest, and is what excludes internal `[[bin]]` crates from the wasm
build.

The tools that run programs through `murmur:process` share two of them:

| Crate | What it holds | Used as |
|---|---|---|
| `libs/murmur-process-host` | The `Processes`, `Conn` and `Clock` traits a tool's call logic is written against, and `is_not_found` for murmur's text for a program missing from `PATH` | a dependency. Each tool still implements the traits over its own `wit_bindgen` import |
| `libs/murmur-process-test-host` | The Wasmtime test host: it runs a tool's compiled component with a `murmur:process` host that starts real programs | a dev-dependency only. It is empty on `wasm32`, so the workspace wasm build compiles it without Wasmtime |

A shared crate whose artifacts each need only part of it puts each part behind a cargo feature, so
an artifact compiles only what it calls. `libs/murmur-file-tools` has one feature per file tool,
`read`, `write`, `edit` and `find`, all on by default so its own tests cover every tool. Each tool
crate turns the defaults off and enables only its own feature:

```toml
murmur-file-tools = { path = "../../libs/murmur-file-tools", default-features = false, features = ["find"] }
```

`regex` and `walkdir` link only into `tool-find`, and the image-header code only into `tool-read`.
A tool crate that drops its feature does not compile: its module is absent from the library.

## Build one artifact

```bash
cargo build -p tool-read --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/tool_read.wasm
./scripts/package-artifact.sh tools/tool-read release/         # → release/tool-read-0.4.0.mur.zip
./scripts/local-install.sh tool-read /path/to/capsule-folder
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

1. Create `<category>/<name>/murmur.yaml`. `tool-create` scaffolds a wasm tool or a hook with the
   `runtime:`, `implementation:` and `requires_files:` this layout needs — see
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

An HTTP driver also declares a top-level `models:` block in its `murmur.yaml`: one entry per
exact model id, holding `max_output_tokens` and `context_window`, and `max_input_tokens` when the
provider documents an input ceiling below the window. Each is an integer from 1 to 4294967295, and
`max_input_tokens` is below `context_window`. The README's "Model limits" table carries all three,
with `none` for a model that declares no `max_input_tokens`. A crate test fails until that block
agrees with the driver's README and with any `SUPPORTED_MODELS` or `MODEL_ROUTES` table in
`src/lib.rs`. To add a model to a driver:

| Driver | Add |
|---|---|
| `driver-anthropic` | a `models:` entry and a row in the README's "Model limits" table |
| `driver-baseten` | a `SUPPORTED_MODELS` entry with its limits and a `models:` entry; its README states the limits under "Model scope" |
| `driver-openai` | a `MODEL_ROUTES` row with its endpoint, a `models:` entry, a "Model limits" row and an "Endpoints" row |
| `driver-deepseek` | the id in `SUPPORTED_MODELS`, a `models:` entry and a "Model limits" row |
| `driver-huggingface` | a `SUPPORTED_MODELS` entry with its limits, reasoning field, effort values and replay support, and a quoted `models:` entry; its README states the limits under "Model scope" |
| `driver-moonshotai` | a `SUPPORTED_MODELS` entry with its limits and its `ThinkingControl`, a `models:` entry and a "Model limits" row |
| `driver-zai` | a `SUPPORTED_MODELS` entry with its limits and a `models:` entry; its README states the limits under "Model scope" |

Then bump the driver's minor version.

An HTTP driver tells murmur whether a failed call may be made again through the reserved
`transient` key of its `tool-result.metadata`. murmur classifies a status and a connection failure
before the response head itself, so the driver marks only what its gateway cannot see. Build every
failure as a `murmur_driver_stream::Failure` and have `run` return `failure.metadata()` beside the
error payload; never write the tuple by hand.

| Failure | Constructor | Metadata |
|---|---|---|
| after a `2xx` head: the stream ended before the provider's stop reason, a body read failed (`murmur_driver_stream::read_failure`), or the provider reported an overload, server, rate-limit or lost-connection error in the stream | `Failure::transient` | `("transient", "true")` |
| a `429` whose body names an exhausted quota, balance or spend cap | `Failure::permanent` | `("transient", "false")` |
| everything else: other statuses, refusals, malformed streams, pre-flight refusals | `Failure::plain` | none |

The driver's README lists each provider error it marks under `## Transient failures`.

## Release

1. Bump `version:` in the `murmur.yaml` of every artifact whose payload changed, or
   `metadata.version` in a skill's `SKILL.md` — a minor bump below 1.0, `0.5.0` → `0.6.0`. `./scripts/check-version-drift.sh` names any you missed; CI runs
   it on every push.
2. Bump `[workspace.package] version` in `Cargo.toml` to the next minor. It names the release
   only: every crate pins `version = "0.0.0"`, so neither `Cargo.lock` nor any artifact's bytes
   change with it.
3. When murmur has moved and this repository should follow it, run
   `./scripts/murmur-pin.sh --set <commit>` (see [WIT mirror](#wit-mirror)). It rewrites the
   `MURMUR_REV` line in `build.yml`, re-mirrors the WIT and lists the artifacts that need a bump.
   Bump those, and commit the `MURMUR_REV` line, the mirror and the bumps together.
   `MURMUR_REV` must reach a murmur containing `35653488` before a release packs the skills: an
   earlier `mur` cannot build a `SKILL.md` folder, and `package-artifact.sh` refuses a zip whose
   packed name or version differs from the front block.
4. Merge, then tag the merge commit `v<workspace version>` and push the tag.
5. The `build` workflow runs. `verify` checks that the tag is `v` + the workspace version and the
   newest `v*` tag, and that no artifact drifted. `release` builds `mur` at `MURMUR_REV` and the
   wasm artifacts, validates them, packs every version the latest release's index does not
   already list with
   `./scripts/package-release.sh --previous`, and writes `artifacts-index.json` with
   `./scripts/index-release.sh`, merged with the latest release's index. A version an earlier
   release published stays on that release with its entry carried forward: repacked by a newer
   `mur`, its zip would record a different WIT level in `murmur-build.json` and change bytes. Then
   `./scripts/check-release-installs.sh` installs every new version through `mur` against a
   loopback copy of the release, and only then is anything uploaded.
6. Afterwards, `mur install -g murmur/<name>@<version>` resolves from the new release.

**Docs pin the current version.** READMEs, skills and this page copy artifact versions into
install lines and example manifests. `./scripts/check-readme-pins.sh` holds every copy to the
artifact's version, and CI runs it on every push. `skills/skill-create-manifest/SKILL.md`
pins artifacts too, and it is that skill's payload: bumping an artifact it pins means updating the
pin, which changes the skill, so the skill needs a bump as well unless it has already moved since
the last tag.

The check scans every `*.md` outside `.nexus/`, tracked or untracked but not ignored, and counts
three forms as a pin:

| Form | Example | Reported on |
|---|---|---|
| inline | `mur install -g murmur/<name>@<x.y.z>` | that line |
| manifest entry | a YAML mapping holding `name: murmur/<name>` and `version: <x.y.z>` at the same indent | the `version:` line |
| zip name | `release/<name>-<x.y.z>.mur.zip`, when `<name>` is an artifact here; other names are ignored | that line |

An inline or manifest pin naming no artifact of this repository fails too. Prose version floors
such as ``needs `mur` 0.7.0 or later``, WIT package names (`murmur:driver/process@0.3.0`) and
entries without a `version:` are not pins. Run `./scripts/check-readme-pins.sh <file.md>` to check
one file.

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
| `own tree` | a crate's `murmur.yaml`, `Cargo.toml` and `src/`; a skill's whole folder, every file `mur build` packs (all but dot-names and `*.mur.zip`) |
| `path deps` | each `libs/*` it links, transitively |
| `wit mirror` | the `wit/` subtree its bindgen `path:` names |
| `workspace` | `[workspace.package]` minus its version (with it, for a crate that inherits `version.workspace = true`), the `[workspace.dependencies]` it declares, its resolved third-party versions in `Cargo.lock`, `rust-toolchain.toml`, and `wasi-sdk.toml` for a wasm artifact linking `libsqlite3-sys`, `tree-sitter`, `tree-sitter-rust` or `tree-sitter-python` (the crates in `WASM_C_CRATES`) |

## WIT mirror

`wit/guest/`, `wit/hook/` and `wit/process-driver/` are a byte-identical copy of
`murmur/crates/capsule-runtime/wit/` at `MURMUR_REV` in `.github/workflows/build.yml`, the commit
the release builds `mur` from. No other workflow or script names a murmur commit. The mirror and
the pin move together:

```bash
./scripts/murmur-pin.sh                    # print the pinned commit
git ls-remote https://github.com/murmur-nexus/murmur.git refs/tags/v<x.y.z>
./scripts/murmur-pin.sh --set <commit>     # move the pin and the mirror to <commit>
```

`--set` takes a commit, or an annotated tag's object name and writes the commit it points at. It
refuses uncommitted changes under `wit/` or to `build.yml`. It rewrites the `MURMUR_REV` line,
replaces the three subtrees with murmur's at that commit and stages them, then runs
`./scripts/check-wit-sync.sh` and `./scripts/check-version-drift.sh`. Every artifact rebuilt
against a new interface version ships different bytes, so bump each one it lists.

`./scripts/check-wit-sync.sh` reads murmur's WIT at the pinned commit, whatever the murmur
checkout has checked out. It looks for murmur beside the main checkout, from a worktree too, or
at `MURMUR_DIR`. Either must be a git checkout that holds the commit; the script never fetches.

## Tests

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
```

CI runs both, formatting first. The workspace uses rustfmt's defaults (there is no `rustfmt.toml`);
run `cargo fmt --all` before committing.

The suites below are `#[ignore]`d because they need something the default run does not have. The
thirteen that drive a real `mur` read it from `MUR_BIN`, else `PATH`, and fail rather than skip without
one:

```bash
MUR_BIN=$MUR_BIN cargo test -p tool-corpus --test mur_run_state -- --ignored         # 4 tests
MUR_BIN=$MUR_BIN cargo test -p tool-tavily --test mur_run_gateway -- --ignored       # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-github --test mur_run_gateway -- --ignored       # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-transit-board-relay --test mur_run_gateway -- --ignored  # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-create --test mur_manifest_shape -- --ignored    # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-test-report --test mur_run_stable_id -- --ignored  # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-code-coverage --test mur_run_localize -- --ignored  # 2 tests
MUR_BIN=$MUR_BIN cargo test -p tool-code-graph --test mur_run_code_graph -- --ignored  # 2 tests
MUR_BIN=$MUR_BIN TOOL_BROWSER_TEST_CHROME=<chrome-headless-shell> cargo test -p tool-browser --test mur_run_browser -- --ignored  # 2 tests; fetches about 128 MB; its mur must carry murmur b0d4c42e
MUR_BIN=$MUR_BIN cargo test -p tool-computer --test mur_run_computer -- --ignored    # 3 tests; its mur must be WIT level 4; needs Xvfb, xkbcomp, xmessage
MUR_BIN=$MUR_BIN cargo test -p tool-git --test mur_run_parity -- --ignored --nocapture  # 2 tests; its mur must be WIT level 4
MUR_BIN=$MUR_BIN cargo test -p driver-anthropic --test mur_run_computer_tool -- --ignored  # 3 tests
MUR_BIN=$MUR_BIN cargo test -p driver-openai --test mur_run_computer_tool -- --ignored     # 4 tests
cargo test -p tool-create --test scaffolded_hook_tests -- --ignored  # needs a cargo registry
```

| Suite | Proves |
|---|---|
| `mur_run_state` | the corpus reaches a durable store under a real `mur run`; the `corpus-state` workflow runs it nightly against murmur's default branch |
| `mur_run_gateway` (tool-tavily) | the runtime attaches the Tavily key to the tool's request and it appears nowhere in the session |
| `mur_run_gateway` (tool-github) | the runtime attaches the GitHub token to the tool's requests and it appears nowhere in the session |
| `mur_run_gateway` (tool-transit-board-relay) | the runtime readdresses the relay's one request to a fake `nexus transit serve` at `http://127.0.0.1:<port>` with the stored key as `Authorization: Bearer <key>`, exactly once; the envelope carries the manifest's `name` as `capsule`, the trace's session id as `session` and the entry's `config:` as `config`; serve's answer reaches the model's next request unchanged inside the `<untrusted-content>` fence, and the key appears nowhere in the session. Without `gateway:` the model gets the relay's no-gateway line and serve receives nothing |
| `mur_manifest_shape` | an unedited scaffold packs and publishes as the kind its author asked for |
| `mur_run_stable_id` | under a real `mur run`, a code-graph database written into the work folder between two calls of one session gives the next call its `stable_id`, and one in a `read` host path resolves through an absolute `repo_path` with no lock file beside it |
| `mur_run_localize` | under a real `mur run`, scores one call of a session writes into a work-folder code-graph database are reset and rewritten by the next, leaving no lock or journal file beside it, and an absolute `repo_path` in a `read-write` host path writes the scores into that host database |
| `mur_run_code_graph` | under a real `mur run`, a repository in the work folder is indexed, edited on the host between calls, re-indexed (one changed file) and its `impact_analysis` names the new caller, with no lock or journal left; a repository in a `read-write` host path is indexed through an absolute `repo_path` onto the host |
| `mur_run_computer` | in a `sealed` capsule the tool starts Xvfb and `xmessage` as `murmur:process` session processes, speaks X over a `connect` to the display's port, clicks the real button, and the screenshots before and after differ and reach the next model request as images. The trace records `process_start` for `display` and `launch-0` and `process_exit` for `display` with `reason: session_end`, and no Xvfb or xmessage runs after `mur run` exits. The shipped manifest on `scoped` is refused with `E-CAP-003`; a test-only copy without `provider_tool:` runs the same flow on `scoped`, granting Xvfb `/usr/share/X11/xkb` through `interpreter_runtime`. A `confirm`-listed click is held. Its `mur` must report WIT level 4 or higher |
| `mur_run_computer_tool` (driver-anthropic) | in a `sealed` capsule a tool declaring `provider_tool: {kind: computer}` goes to `claude-sonnet-4-6` as `computer_20251124` with its beta header and to `claude-sonnet-5-5` as the toolset; Claude's click reaches the tool scaled to the display, a toolset batch runs in order, and the next request replays the calls in provider form. Without the block the tool stays a function. Its `mur` must carry murmur `a9ea4726` |
| `mur_run_computer_tool` (driver-openai) | in a `sealed` capsule the same tool goes to `gpt-5.6-luna` as `{"type": "computer"}`, a `computer_call` runs as an ordered group answered by one `computer_call_output`, a flagged call nobody answers under `mur run --task` times out unrun, and a served capsule runs it only after a person's yes over the door and never after a no. Without the block the tool stays a function. Its `mur` must carry murmur `a9ea4726` |
| `scaffolded_hook_tests` | a freshly scaffolded hook reports `1 passed`, not `0 passed` |
| `mur_run_parity` | every tool-git operation, scripted over a fixture work folder, gives the same results and trace statuses at `scoped` and at `sealed` as the frozen fixture recorded from tool-git 0.6.0; the trace holds one `process_start`/`process_exit` pair per git command. A `hook-command-guard` rule refuses `push` with `force: true`, and a `read_only` grant refuses what the README says. Its `mur` must report WIT level 4 or higher |
| `mur_run_browser` | `mur install` of tool-browser fetches the `chrome-headless-shell` pinned for `chromium` (network, about 128 MB once per run) into the scratch `HOME`'s program store, and a second `mur install` finds it already in the store. A capsule with the capabilities of tool-browser's README manifest, copied as is but for the tier, grants only `chromium` and no host path. It runs once with no `chromium` on `PATH`, where `--explain-scope`, `mur doctor` and `session_start.effective_grants.pinned_programs` name the pinned copy, and once with `TOOL_BROWSER_TEST_CHROME` as the host's `chromium`, which they name instead. Each time it opens a work-folder page and, where the host reaches `example.com:443`, `https://example.com/` on `scoped` and again on `sealed`, with Chrome a `murmur:process` session process. Chrome's needs come from tool-browser's `programs:` block: `mur run --explain-scope` lists them under `declared needs:`, and `session_start.effective_grants.declared_needs` lists them with the browser's folder and `libsoftokn3.so` granted and none missing. It reads the page's console error through murmur's `<untrusted-content>` fence, and its screenshot reaches the next model request as an image. The trace records `process_start` for `browser` and `process_exit` with `reason: session_end`, and nothing naming `.tool-browser/` runs after `mur run` exits. With the store entry removed and no `chromium` on `PATH`, the call answers the not-installed line naming `mur install`, and `mur doctor` warns `W-RUN-012`. A capsule without the grant gets the single no-grant line. Without `storage.googleapis.com:443` the pinned phases print one skip line and only the host phase and the ungranted session run. On `scoped`, a copy of the tool whose `programs:` withholds NSS's modules and pins no download answers the https page with `tool-browser: chromium exited with 133 during Page.navigate: …` and the NSS hint, the trace records `process_exit` with `exit_code: 133`, and `browser.log` holds `FATAL:crypto/nss_util.cc`. Without `example.com:443` the https steps print one skip line. Its `mur` must carry murmur `b0d4c42e`, the first that reads `programs.<program>.download` |

`mur_run_browser` needs `TOOL_BROWSER_TEST_CHROME` set to a `chrome-headless-shell`, because murmur's
`scoped` and `sealed` tiers stop full Chrome (see [tool-browser's README](./tools/tool-browser/README.md#which-browser-runs-on-which-tier)).
The test links it as `chromium` in its own directory, which must lie outside the scratch `HOME` it
gives `mur`, for the host phase; the pinned phase removes every directory holding a `chromium`
from `PATH`. It packs the workspace `driver-openai` and
`tool-browser` with `scripts/package-artifact.sh`, so a level-4 `mur` needs neither a released
driver zip nor a manifest with `vision:` stripped.

The default run of `tool-browser` (its `browser` suite) runs the compiled component in Wasmtime,
with the test host for `murmur:process` in `libs/murmur-process-test-host`, which starts the browser
outside any capsule. It needs Chrome or
Chromium on Linux: one of `chromium`, `chromium-browser`, `google-chrome` or `chrome` on `PATH`, a
Playwright Chromium under `~/.cache/ms-playwright`, or `TOOL_BROWSER_TEST_CHROME`. Without one it
fails, naming what to install. `TOOL_BROWSER_KEEP_LOGS=<dir>` copies each test's `browser.log` and `state.json` there.

The default run of `tool-computer` (its `display` suite) runs the compiled component in Wasmtime the
same way, on the same test host, which starts the real Xvfb and `xmessage` outside any capsule; the
suite clicks `xmessage`'s button through the component. It needs `Xvfb` (`xvfb`), `xkbcomp`
(`x11-xkb-utils`) and `xmessage` (`x11-utils`) on Linux, and fails without them, naming the package
to install. `TOOL_COMPUTER_KEEP_LOGS=<dir>` copies each test's logs and `state.json` there.
