# tool-read

Reads a file in the work folder, within fixed line and size limits.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). The model calls it `tool-read`. Its file logic lives
in the shared, unpublished `libs/murmur-file-tools` library, which
`tool-read`, `tool-write`, `tool-edit` and `tool-find` all call.

## Definition

The model is sent this description and input schema:

```yaml
description: |
  Reads a file. path is relative to the work folder; an absolute path is refused.
  Returns content and total_lines: at most 2000 lines and 100 KB, lines cut to 2000 characters. A cut read adds a truncated note naming the limit and the start_line that continues it. A range over 2000 lines is refused.
input_schema:
  type: object
  properties:
    path:
      type: string
    start_line:
      type: integer
      description: 1-based
    end_line:
      type: integer
  required: [path]
  murmur-destinations: []
```

`start_line` and `end_line` are 1-based and inclusive. A successful call
declares `state_effect: read`.

## Reading

Every read, with or without a range, is held to three limits:

| Limit | Value |
|---|---|
| Lines per read | 2000 |
| Content per read | 100 KB (102,400 bytes) |
| Characters per line | 2000 |

A line longer than 2000 characters comes back as its first 2000 characters. A
read with no range starts at line 1, and an omitted `end_line` reads to the end
of the file or to the first limit reached. An `end_line` past the end clamps to
the file's line count.

| Read | `data` keys | `message` |
|---|---|---|
| No range, file within every limit | `content` (the file verbatim), `total_lines` | `read <path>` |
| With a range, within every limit | `content`, `total_lines`, `start_line`, `end_line` | `read <path> lines <s>-<e>` |
| Cut by a limit, with or without a range | `content`, `total_lines`, `start_line`, `end_line`, `truncated` | `read <path> lines <s>-<e>` |

Every response but the verbatim one returns the lines joined with `\n`, with no
trailing newline, and `start_line`/`end_line` as resolved. A cut read still
succeeds: its summary ends in `, truncated)` and the result's `truncated` flag
is set. The `truncated` note names the limit that stopped the read and the
`start_line` that continues it. A 5000-line file read with no range returns:

```json
{
  "content": "line 1\nline 2\n...\nline 2000",
  "end_line": 2000,
  "start_line": 1,
  "total_lines": 5000,
  "truncated": "Read stopped at the limit of 2000 lines. Lines 1-2000 of 5000 are returned; start_line 2001 continues the read."
}
```

A read that cuts long lines says so in a second sentence, after the first when
both apply: `Lines longer than 2000 characters are cut to their first 2000
characters: 1 in this read, the first at line 10.`

## Refused calls

Under murmur, a call missing a top-level `required` field (`path`) never
reaches the tool: murmur refuses it before dispatch, records a
`tool_input_refused` event, and tells the model which field is missing. A call
that gets past that check is checked by the tool in this order, and the first
failure is returned. A refused call touches no file and declares no
`state_effect`.

1. The input is a JSON object (or a JSON string encoding one).
2. Every key is one of `path`, `start_line` or `end_line`. Any other key, whatever its value, is
   refused with `error_kind: "unknown_field"`, naming the keys sorted and
   quoted and the fields this tool takes:

   ```text
   unknown field "content": tool-read takes path, start_line, end_line
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
| `path` absolute | `absolute_path` | `'path' must be relative to the capsule workdir; got '/etc/hosts' (did you mean 'hosts'?)` |
| `start_line` below 1 | `invalid_range` | `start_line must be >= 1, got 0` |
| `start_line` past the end | `invalid_range` | `start_line 99 is beyond end of file (4 lines)` |
| `start_line` after `end_line` | `invalid_range` | `inverted range: start_line 5 is greater than end_line 2` |
| An explicit range over 2000 lines | `range_too_large` | `requested range of 2500 lines exceeds the maximum of 2000; narrow start_line/end_line` |
| The file does not exist | `not_found` | `src/app.py: <OS error>` |

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

The schema declares `murmur-destinations: []`: tool-read writes nothing named by
or derived from its input. No property carries `format: murmur-destination`,
so a `capabilities.filesystem.read_only` grant never refuses a tool-read call,
and murmur raises no `W-SEC-018` for it. Its properties pair no path key with a content key, so murmur's key-name rules read it as a read too.

## Replaces tool-editor

`tool-read` is one of the four flat tools that replace `tool-editor`, whose 0.12.0
calls held each operation in its own nested object. There is no alias. A
nested call carries none of the top-level `required` fields, so under murmur it
is refused before dispatch for the missing field and the tool never runs.
Outside that check, or when the call also carries the required fields, the
tool refuses the nested key as an unknown field, and every earlier shape the same way.

| tool-editor 0.12.0 | `tool-read` 0.1.0 |
|---|---|
| `{"read": {"path": P}}` | `{"path": P}` |
| `{"read": {"path": P, "start_line": S, "end_line": E}}` | `{"path": P, "start_line": S, "end_line": E}` |

Messages name the bare field: `missing required field: path`, `'path' must be
relative …`. Every result, note, limit and error kind is otherwise
tool-editor 0.12.0's `read`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
