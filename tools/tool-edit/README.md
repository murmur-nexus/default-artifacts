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
  Replaces text in a file. Edits apply in order, each to the result of the one before; each old must match exactly once. All or nothing: if one fails, the file is unchanged and the error names its position, from 1. path is relative to the work folder, or absolute inside a host folder the session opened.
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

Under murmur, a call missing a top-level `required` field (`path` and `edits`) never
reaches the tool: murmur refuses it before dispatch, records a
`tool_input_refused` event, and tells the model which field is missing. A call
that gets past that check is checked by the tool in this order, and the first
failure is returned. A refused call touches no file and declares no
`state_effect`.

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
4. `path` resolves: relative to the work folder, or absolute inside a host folder
   the session opened (see below). A write into a `read` host folder is refused here.

Every refusal is also the result's `summary`, which is what a host shows for
the call.

Every input is validated before the file is read, so a malformed `edits` fails
the same way whether or not the file exists. Positions count from 1 and are
written `edit N of M`:

| Failure | `error_kind` | Message |
|---|---|---|
| `path` absent, `null` or `""` | — | `missing required field: path` |
| `path` absolute, no host folder opened | `absolute_path` | `'path' must be relative to the work folder: this session opens no host folder to tool-edit; got '/app/x.py' (did you mean 'x.py'?)` |
| `path` absolute, outside every host folder | `absolute_path` | `'path' is outside every host folder tool-edit may open: /home/op/.cargo (read); got '/etc/hosts'. A relative path opens in the work folder` |
| `path` relative, starting with `murmur-host`, while a host folder is opened | `reserved_name` | `'path' starts with murmur-host, where this tool's host folders are opened; name the host path absolutely: '/home/op/x'` |
| `path` inside a `read` host folder | `permission_denied` | `'path' '/home/op/.cargo/config.toml' is in /home/op/.cargo, which this session opens read-only` |
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

## Host folders

`path` is relative to the work folder. An absolute `path` opens only inside a host
folder the session opened for the tool:

| Session | Host folders |
|---|---|
| `mur run --unconfined` | The whole host, `/`, read and write |
| Contained, with `host_paths` in `~/.murmur/config.yaml` | Each entry's folder, with the entry's `read` or `read-write` access |
| Contained, no `host_paths` | None |

murmur hands each folder to the tool as a WASI preopen named `murmur-host`
followed by the folder's absolute path, `murmur-host` alone for `/`. murmur
does so from commit `e86e0b42` on, which no `mur` release up to v0.8.0
contains: under v0.8.0 or earlier the tool holds no host folder, and every
absolute `path` is refused. A tool entry that declares
`capabilities.filesystem.scope` holds none on any `mur`. The
tool lists the folders with `wasi:filesystem/preopens.get-directories`, resolves
the value's `.` and `..` segments lexically, and opens it through the
longest folder that covers it.
A folder covers whole components only: `/home/op/.cargo` covers
`/home/op/.cargo/bin/x` but not `/home/op/.cargo2`.

| `path` | Result |
|---|---|
| Relative | Opens in the work folder, as always |
| Relative, first segment `murmur-host`, while a host folder is opened | Refused with `reserved_name`, naming the absolute path to use |
| Absolute, inside a host folder | Opens there |
| Absolute, no host folder opened | Refused with `absolute_path`, suggesting the value's final component |
| Absolute, outside every host folder | Refused with `absolute_path`, listing the host folders and their access in preopen order |
| Absolute, inside a `read` folder | Refused with `permission_denied` before the file or its folders are touched |

```text
'path' is outside every host folder tool-edit may open: /home/op/.cargo (read), /data (read-write); got '/etc/hosts'. A relative path opens in the work folder
```

A value with no final component (`/`, or one ending in `..`) gets no
suggestion. With no host folder opened, `murmur-host/…` is an ordinary
work-folder path.

A `host_paths` entry that is a symlink is opened at its target, so a path is
covered only when spelled through the target. An absolute symlink inside a
folder is not followed: opening one fails with `Operation not permitted`. Every
message names `path` as the call wrote it.

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

murmur applies `read_only` to every spelling of `path`: relative, the work
folder's absolute host path, or a path through `murmur-host/`. A host folder
opened `read` is a separate fence, held by the tool itself (see
[Host folders](#host-folders)).

An `edits` element holds no path key, and the call holds no content key beside
`path`, so murmur's key-name rules add nothing to the declared destination.

## Replaces tool-editor

`tool-edit` is one of the four flat tools that replace `tool-editor`, whose 0.12.0
calls held each operation in its own nested object. There is no alias. A
nested call carries none of the top-level `required` fields, so under murmur it
is refused before dispatch for the missing field and the tool never runs.
Outside that check, or when the call also carries the required fields, the
tool refuses the nested key as an unknown field, and every earlier shape the same way.

| tool-editor 0.12.0 | `tool-edit` 0.1.0 |
|---|---|
| `{"edit": {"path": P, "edits": [{"old": O, "new": N}, …]}}` | `{"path": P, "edits": [{"old": O, "new": N}, …]}` |

`edits` keeps tool-editor 0.12.0's rules exactly: in order, each `old` once,
overlaps counted, all or nothing, and every `edit N of M` message unchanged.
Messages that named `edit.path` or `edit.edits` now name `path` and `edits`.
`hook-diff-summary` 0.13.0 snapshots `tool-edit` calls by their top-level
`path`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
