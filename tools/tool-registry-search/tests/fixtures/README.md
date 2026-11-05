# Test fixtures

No file here is edited by hand.

## `artifacts-index-schema1.json`

The schema 1 index the repository kept at tag `v0.24.0`, verbatim:

```bash
git show v0.24.0:artifacts-index.json > tools/tool-registry-search/tests/fixtures/artifacts-index-schema1.json
```

## `artifacts-index.json`

A schema 2 index for two releases, written by `mur index` from murmur commit
`57012159bc0b8d981f58105181197f8d4d184eb3`, a murmur `main` commit between v0.6.0 and v0.7.0
(`mur version` prints `0.6.0`, WIT level 1). Release `v0.25.0` holds
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

## `native-0.5.0-results.json`

What the native 0.5.0 build answered for 14 inputs, as the runtime turned each answer into a
`ToolResult`: `status` (`passed`, `failed`, anything else `error`), `summary`, and `data`, the
printed `data` value, or `null`. `error_kind`, `message`, `ok` and the top-level mirror of `data`
never reached the model, so they are not recorded. The `native_0_5_0_results` test in
`../search_index.rs` runs each input through the component from the crate directory and requires
an equal `status`, `summary` and `data`.

One case differs by design: the missing file's summary ends in ` (os error 2)` here, and WASI
numbers the same error 44. The test compares that summary with the trailing ` (os error N)` cut.

Recorded from commit `95e17e1` (`ci: bump version to v0.36.0`), the last commit with the native
build, on linux-x86_64, with `jq` 1.7. From the repository root, with `$S` an empty scratch
directory under `/space`:

```bash
git worktree add --detach "$S/native-0.5.0" 95e17e1
(cd "$S/native-0.5.0" && cargo build --release -p tool-registry-search --target-dir "$S/native-target")
cd tools/tool-registry-search
bash "$S/record-native.sh" "$S/native-target/release/tool-registry-search" tests/fixtures/native-0.5.0-results.json
git worktree remove "$S/native-0.5.0"
```

`$S/record-native.sh`, run from `tools/tool-registry-search`:

```bash
#!/usr/bin/env bash
# record-native.sh <native-binary> <out.json> — run from tools/tool-registry-search.
set -euo pipefail
bin="$1"; out="$2"
fx=tests/fixtures/artifacts-index.json
cases=()
for q in tool-editor murmur/tool-editor TOOL-EDITOR tool; do
  cases+=("$(jq -cn --arg q "$q" --arg i "$fx" '{query:$q,index:$i}')")
done
cases+=("$(jq -cn --arg i "$fx" '{query:"tool",index:$i,limit:1}')")
for q in Messages tool-git skill-create-manifest murmur/ zzz; do
  cases+=("$(jq -cn --arg q "$q" --arg i "$fx" '{query:$q,index:$i}')")
done
cases+=('{"query":"git","index":"tests/fixtures/artifacts-index-schema1.json"}')
cases+=('{"query":"x","index":"no/such/artifacts-index.json"}')
cases+=('{"query":"editor","index":"local"}')
cases+=('{"query":"x","registry":"local"}')
for input in "${cases[@]}"; do
  jq -cn --argjson d "$input" '{data:$d, log_path:null}' | "$bin" \
    | jq -c --argjson input "$input" '{
        input: $input,
        status: (if .status == "passed" or .status == "failed" then .status else "error" end),
        summary: .summary,
        data: (if .data == null then null else .data end)
      }'
done | jq -s '.' > "$out"
```
