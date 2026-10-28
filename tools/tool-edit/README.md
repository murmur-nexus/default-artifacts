# tool-edit

Replaces text in a file in the work folder: one or more exact `{old, new}` replacements, all or nothing.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). The model calls it `tool-edit`. Its file logic lives
in the shared, unpublished `libs/murmur-file-tools` library, which
`tool-read`, `tool-write`, `tool-edit` and `tool-find` all call.

## Definition

The model is sent this description and input schema:

```yaml
description: |
  Replaces text in a file. Edits apply in order, each to the result of the one before; each old must match exactly once. All or nothing: if one fails, the file is unchanged and the error names its position, from 1. path is relative to the work folder; an absolute path is refused.
input_schema:
  type: object
  properties:
    path:
      type: string
      format: murmur-destination
    edits:
      type: array
      items:
        type: object
        properties:
          old:
            type: string
          new:
            type: string
        required: [old, new]
  required: [path, edits]
```

```json
{"path": "src/app.py", "edits": [
  {"old": "retries = 3", "new": "retries = 5"},
  {"old": "def run(", "new": "def run_once("}
]}
```

## Editing

- The pairs apply in order, each to the text the one before produced, so a
  later `old` may match text an earlier `new` inserted.
- Each `old` must occur exactly once in the text it applies to. Overlapping
  occurrences count: `aa` occurs twice in `aaa`.
- `old` must be a non-empty string; `new` is a string and may be empty, which
  deletes the matched text.
- All or nothing. The file is read once and written once, only after every pair
  matched. If any pair fails, the file is left byte-identical.

A successful edit returns `applied <M> edit(s) to <path>` with summary
`<M> edits`, no `data`, and `state_effect: mutate`.

## Refused calls

A call is checked in this order, and the first failure is returned. A refused
call touches no file and declares no `state_effect`.

1. The input is a JSON object (or a JSON string encoding one).
2. Every key is one of `path` or `edits`. Any other key, whatever its value, is
   refused with `error_kind: "unknown_field"`, naming the keys sorted and
   quoted and the fields this tool takes:

   ```text
   unknown field "new_string", "old_string": tool-edit takes path, edits
   ```

3. The required fields are present. A field whose value is `null` counts as
   absent at every level, so murmur's own required-field check, which only
   tests presence, lets it through and the tool refuses it.
4. Every path is relative to the work folder (see below).

Every refusal is also the result's `summary`, which is what a host shows for
the call.

Every input is validated before the file is read, so a malformed `edits` fails
the same way whether or not the file exists. Positions count from 1 and are
written `edit N of M`:

| Failure | `error_kind` | Message |
|---|---|---|
| `path` absent, `null` or `""` | — | `missing required field: path` |
| `path` absolute | `absolute_path` | `'path' must be relative to the capsule workdir; got '/app/x.py' (did you mean 'x.py'?)` |
| `edits` absent or `null` | — | `missing required field: edits` |
| `edits` not an array | — | `edits must be an array of {old, new} objects` |
| `edits` is `[]` | — | `edits is empty: it takes at least one {old, new} object` |
| An element not an object | — | `edit 1 of 1 must be an object with old and new` |
| An element carries another key | `unknown_field` | `unknown field "path" in edit 2 of 2: an edit takes old, new` |
| `old` absent, `null` or not a string | — | `edit 1 of 1: missing required field: old` |
| `old` is `""` | — | `edit 2 of 2: old is empty; it must name the text to replace` |
| `new` absent, `null` or not a string | — | `edit 1 of 1: missing required field: new` |
| `old` occurs 0 times | `string_not_found` | `edit 2 of 3: old occurs 0 times in src/app.py; the file is unchanged` |
| `old` occurs 2 or more times | `multiple_matches` | `edit 2 of 2: old occurs 2 times in src/app.py; the file is unchanged` |
| The file does not exist | `not_found` | `src/app.py: <OS error>` |

## Absolute paths are refused

`path` is relative to the work folder, the capsule workdir. A value beginning with
`/` is refused with `error_kind: "absolute_path"` before any filesystem call:

```text
'path' must be relative to the capsule workdir; got '/app/x.py' (did you mean 'x.py'?)
```

The message names the field and suggests the value's final component; a value
with no final component (`/`, or one ending in `..`) gets no suggestion. A tool
is dispatched with one WASI preopen mapped to the workdir, so an absolute value
cannot escape it — `/app/x.txt` would resolve to `<workdir>/app/x.txt`, which
nothing outside the capsule would find where the call named it. Refusing is the
only way that contract is observable.

## How `read_only` is enforced

`path` is declared `format: murmur-destination`. murmur lowers that annotation
to the input location `path`, and when a capsule grants
`capabilities.filesystem.read_only`, a tool-edit call whose `path` lands under a
read-only entry is refused before dispatch, naming the declared location:

```text
Refused by the capsule manifest: 'src/spec.txt' is under the read-only path
'src' declared in capabilities.filesystem.read_only. Identified as a write by
destination 'path' declared by the tool's input schema.
```

`tool-write`'s and `tool-edit`'s `path` are the only declared destinations among
the file tools; `tool-read` and `tool-find` declare `murmur-destinations: []`.

An `edits` element holds no path key, and the call holds no content key beside
`path`, so murmur's key-name rules add nothing to the declared destination.

## Replaces tool-editor

`tool-edit` is one of the four flat tools that replace `tool-editor`, whose 0.12.0
calls held each operation in its own nested object. There is no alias: the
nested shape is refused as an unknown field, and so is every earlier shape.

| tool-editor 0.12.0 | `tool-edit` 0.1.0 |
|---|---|
| `{"edit": {"path": P, "edits": [{"old": O, "new": N}, …]}}` | `{"path": P, "edits": [{"old": O, "new": N}, …]}` |

`edits` keeps tool-editor 0.12.0's rules exactly: in order, each `old` once,
overlaps counted, all or nothing, and every `edit N of M` message unchanged.
Messages that named `edit.path` or `edit.edits` now name `path` and `edits`.
`hook-diff-summary` 0.13.0 snapshots `tool-edit` calls by their top-level
`path`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
