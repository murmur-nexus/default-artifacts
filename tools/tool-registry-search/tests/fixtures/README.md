# Test fixtures

No file here is edited by hand.

## `artifacts-index-schema1.json`

The schema 1 index the repository kept at tag `v0.24.0`, verbatim:

```bash
git show v0.24.0:artifacts-index.json > tools/tool-registry-search/tests/fixtures/artifacts-index-schema1.json
```

## `artifacts-index.json`

A schema 2 index for two releases. Release `v0.25.0` holds `tool-editor` 0.6.0,
`driver-anthropic` 0.10.0, `skill-create-manifest` 0.1.0 and `tool-git` 0.2.0 with a payload for
each of two platforms, `linux-x86_64` and `darwin-aarch64`. Release `v0.26.0` holds `tool-editor`
0.7.0, and its index is merged over the first. These are fixture stand-ins, not published versions:
`tool-editor` 0.7.0 is the 0.6.0 component under a copy of its `murmur.yaml`, and both `tool-git`
payloads are one linux-x86_64 binary. The file is frozen.

Written by `mur index` from murmur commit `57012159bc0b8d981f58105181197f8d4d184eb3`.

## `native-0.5.0-results.json`

What tool-registry-search 0.5.0 answered for 14 inputs, as the runtime turned each answer into a
`ToolResult`: `status` (`passed`, `failed`, anything else `error`), `summary`, and `data`, the
printed `data` value, or `null`. `error_kind`, `message`, `ok` and the top-level mirror of `data`
never reached the model, so they are not recorded. `../search_index.rs` runs each input through the
component from the crate directory and requires an equal `status`, `summary` and `data`.

One case differs by design: the missing file's summary ends in ` (os error 2)` here, and WASI
numbers the same error 44. The test compares that summary with the trailing ` (os error N)` cut.

Recorded from tool-registry-search 0.5.0, built from commit `95e17e1`.
