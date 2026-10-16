# Test fixtures

Neither file is edited by hand.

## `artifacts-index-schema1.json`

The schema 1 index the repository kept at tag `v0.24.0`, verbatim:

```bash
git show v0.24.0:artifacts-index.json > tools/tool-registry-search/tests/fixtures/artifacts-index-schema1.json
```

## `artifacts-index.json`

A schema 2 index for two releases, written by `mur index` from murmur commit
`57012159bc0b8d981f58105181197f8d4d184eb3` (`mur 0.6.0`, WIT level 1). Release `v0.25.0` holds
`tool-editor` 0.6.0, `driver-anthropic` 0.10.0, `skill-create-manifest` 0.1.0 and `tool-git`
0.2.0 for two platforms. Release `v0.26.0` holds `tool-editor` 0.7.0, and its index is merged
over the first.

These are fixture stand-ins, not published versions:

- `tool-editor` 0.7.0 is the 0.6.0 component, packed under a copy of its `murmur.yaml` with
  `version: 0.7.0`;
- the `darwin-aarch64` payload of `tool-git` is the linux-x86_64 binary, packed under the darwin
  tag.

From the repository root, with `$W` an empty scratch directory:

```bash
(cd ../murmur && cargo build -p murmur-cli --target-dir /tmp/mur-target)
export MUR=/tmp/mur-target/debug/mur MUR_BIN=/tmp/mur-target/debug/mur

cargo build -p tool-editor -p driver-anthropic --target wasm32-wasip2 --release
cargo build -p tool-git --release

mkdir -p "$W/dirA" "$W/dirB" "$W/scratch/tools"
./scripts/package-artifact.sh tools/tool-editor "$W/dirA"
./scripts/package-artifact.sh drivers/driver-anthropic "$W/dirA"
./scripts/package-artifact.sh skills/skill-create-manifest "$W/dirA"
./scripts/package-artifact.sh tools/tool-git "$W/dirA" --platform linux-x86_64
./scripts/package-artifact.sh tools/tool-git "$W/dirA" --platform darwin-aarch64 --payload target/release/tool-git
$MUR index "$W/dirA" --repo murmur-nexus/default-artifacts --release v0.25.0 -o "$W/dirA/artifacts-index.json"

cp -R tools/tool-editor "$W/scratch/tools/tool-editor"
sed -i 's/^version: 0.6.0$/version: 0.7.0/' "$W/scratch/tools/tool-editor/murmur.yaml"
./scripts/package-artifact.sh "$W/scratch/tools/tool-editor" "$W/dirB" --payload target/wasm32-wasip2/release/tool_editor.wasm
$MUR index "$W/dirB" --repo murmur-nexus/default-artifacts --release v0.26.0 \
    --merge "$W/dirA/artifacts-index.json" -o "$W/dirB/artifacts-index.json"

cp "$W/dirB/artifacts-index.json" tools/tool-registry-search/tests/fixtures/artifacts-index.json
```

`package-artifact.sh` packs the scratch copy of `tool-editor` as it is; it needs only the
directory to be named after the artifact.
