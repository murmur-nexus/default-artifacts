# tool-write

Creates or replaces a file in the work folder, with any missing parent directories.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). The model calls it `tool-write`. Its file logic lives
in the shared, unpublished `libs/murmur-file-tools` library, which
`tool-read`, `tool-write`, `tool-edit` and `tool-find` all call.

## Definition

The model is sent this description and input schema:

```yaml
description: |
  Creates or replaces a file and any missing directories. path is relative to the work folder; an absolute path is refused.
input_schema:
  type: object
  properties:
    path:
      type: string
      format: murmur-destination
    content:
      type: string
  required: [path, content]
```

## Writing

`tool-write` creates or replaces the file at `path` with `content`, creating any
missing parent directories. It returns `wrote <path>` with summary
`<n> bytes written` and no `data`. A successful call declares
`state_effect: mutate`.

## Refused calls

Under murmur, a call missing a top-level `required` field (`path` and `content`) never
reaches the tool: murmur refuses it before dispatch, records a
`tool_input_refused` event, and tells the model which field is missing. A call
that gets past that check is checked by the tool in this order, and the first
failure is returned. A refused call touches no file and declares no
`state_effect`.

1. The input is a JSON object (or a JSON string encoding one).
2. Every key is one of `path` or `content`. Any other key, whatever its value, is
   refused with `error_kind: "unknown_field"`, naming the keys sorted and
   quoted and the fields this tool takes:

   ```text
   unknown field "write": tool-write takes path, content
   ```

3. The required fields are present. A field whose value is `null` counts as
   absent at every level, so murmur's own required-field check, which only
   tests presence, lets it through and the tool refuses it.
4. Every path is relative to the work folder (see below).

Every refusal is also the result's `summary`, which is what a host shows for
the call.

| Failure | `error_kind` | Message |
|---|---|---|
| `path` absent, `null` or `""` | — | `missing required field: path` |
| `path` absolute | `absolute_path` | `'path' must be relative to the capsule workdir; got '/app/results.txt' (did you mean 'results.txt'?)` |
| `content` absent, `null` or not a string | — | `missing required field: content` |
| A parent directory cannot be created | `permission_denied`, `io_error`, … | `failed to create directories for <path>: <OS error>` |

An absolute `path` is refused before `content` is read and before any directory
is created, so a refused write leaves no directory behind.

## Absolute paths are refused

`path` is relative to the work folder, the capsule workdir. A value beginning with
`/` is refused with `error_kind: "absolute_path"` before any filesystem call:

```text
'path' must be relative to the capsule workdir; got '/app/results.txt' (did you mean 'results.txt'?)
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
`capabilities.filesystem.read_only`, a tool-write call whose `path` lands under a
read-only entry is refused before dispatch, naming the declared location:

```text
Refused by the capsule manifest: 'src/spec.txt' is under the read-only path
'src' declared in capabilities.filesystem.read_only. Identified as a write by
destination 'path' declared by the tool's input schema.
```

`tool-write`'s and `tool-edit`'s `path` are the only declared destinations among
the file tools; `tool-read` and `tool-find` declare `murmur-destinations: []`.

## Replaces tool-editor

`tool-write` is one of the four flat tools that replace `tool-editor`, whose 0.12.0
calls held each operation in its own nested object. There is no alias. A
nested call carries none of the top-level `required` fields, so under murmur it
is refused before dispatch for the missing field and the tool never runs.
Outside that check, or when the call also carries the required fields, the
tool refuses the nested key as an unknown field, and every earlier shape the same way.

| tool-editor 0.12.0 | `tool-write` 0.1.0 |
|---|---|
| `{"write": {"path": P, "content": C}}` | `{"path": P, "content": C}` |

GLM 5.3 sometimes sent the mixed `{"path": P, "write": {…}, "content": C}`;
it is refused with `unknown field "write": tool-write takes path, content`.
`hook-diff-summary` 0.13.0 snapshots `tool-write` calls by their top-level
`path`; 0.12.0 and earlier find no write target in them.

See [murmur.yaml](./murmur.yaml) for the full manifest.
