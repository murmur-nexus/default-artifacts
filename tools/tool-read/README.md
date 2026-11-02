# tool-read

Reads a file in the work folder or a host folder the session opened, within fixed line and size limits, or shows a
PNG, JPEG, GIF or WebP file to the model as an image.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). The model calls it `tool-read`. Its file logic lives
in the shared, unpublished `libs/murmur-file-tools` library, which
`tool-read`, `tool-write`, `tool-edit` and `tool-find` all call.

## Definition

The model is sent this description and input schema:

```yaml
description: |
  Reads a file. path is relative to the work folder, or absolute inside a host folder the session opened.
  Returns content and total_lines: at most 2000 lines and 100 KB, lines cut to 2000 characters. A cut read adds a truncated note naming the limit and the start_line that continues it. A range over 2000 lines is refused.
  A PNG, JPEG, GIF or WebP file is shown as an image, with its type, size and dimensions.
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

The four file tools' definitions are held to 500 cl100k tokens together
(`libs/murmur-file-tools/tests/definition_token_budget.rs`). The 0.3.0
sentence on host folders costs 24 characters and 4 tokens over 0.2.0's:

| | tool-read | Sum of the four |
|---|---|---|
| 0.2.0 | 573 chars, 141 tokens | 1955 chars, 465 tokens |
| 0.3.0 | 597 chars, 145 tokens | 2089 chars, 488 tokens |

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

## Images

A file whose leading bytes are a PNG, JPEG, GIF or WebP header is attached as an
image instead of being read as text. The bytes decide, not the name: PNG bytes
saved as `shot` or `photo.jpg` are attached as `image/png`. tool-read reads the
type and the width and height from the header alone, at most 32 bytes plus, for
a JPEG, the segment headers before its frame header. It never reads or decodes
the image data.

The call succeeds with no `data`. Its summary, the text the model sees, is one
line:

```text
image/png, 2048 bytes, 64x48 pixels
```

The result names the file in its `attachments`, with the type read from the
header and `path` exactly as the call gave it. `start_line` and `end_line` are
ignored on an image.

| File | Result |
|---|---|
| A PNG, JPEG, GIF or WebP header, whatever its name | Attached under the type its header declares |
| Named `.png`, `.jpg`, `.jpeg`, `.gif` or `.webp` in any case, but none of those headers | Refused with `not_an_image` |
| Text, a PDF or any other file | Read as text. A file that is not UTF-8 is refused with `io_error` |
| A missing file, a directory, an absolute path no host folder covers, a relative path outside the work folder | Refused exactly as for any other name |
| An image inside a host folder, named by its absolute path | Not attached: the summary says so, and `not_an_image` applies as above |

murmur attaches only a file under the work folder, so an image read by an
absolute path inside a host folder succeeds with no attachment, and its summary
says why:

```text
image/png, 2048 bytes, 64x48 pixels; not shown: only an image named by a path relative to the work folder is shown to the model
```

murmur then checks the attachment and puts the picture in front of the model.
tool-read applies no limit of its own. murmur refuses an image over
`context.images.max_bytes` (default `5Mi`), past `context.images.max_per_turn`
in one turn (default 5), reached through a symlink, outside the work folder, or
whose bytes do not match its type. The call's status does not change: murmur
adds a line after the summary instead:

```text
attachment 'logo.png' was not attached: <why> (<code>)
```

driver-anthropic and driver-openai send an accepted image to the model as an
image, and a harness driver shows it as an MCP image. A model that takes no
image input sees the driver's `[image not shown: …]` line in its place.

tool-read exports `murmur:tool/run@0.2.0` from 0.2.0 on, so it loads only on a `mur` at
WIT level 3, which `mur version` prints. driver-anthropic and driver-openai
declare which models take images with `models.<id>.vision`, which murmur reads
from `a9ea4726` on.

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
4. `path` resolves: relative to the work folder, or absolute inside a host folder
   the session opened (see below).

Every refusal is also the result's `summary`, which is what a host shows for
the call.

| Failure | `error_kind` | Message |
|---|---|---|
| `path` absent, `null` or `""` | — | `missing required field: path` |
| `path` absolute, no host folder opened | `absolute_path` | `'path' must be relative to the work folder: this session opens no host folder to tool-read; got '/etc/hosts' (did you mean 'hosts'?)` |
| `path` absolute, outside every host folder | `absolute_path` | `'path' is outside every host folder tool-read may open: /home/op/.cargo (read); got '/etc/hosts'. A relative path opens in the work folder` |
| `path` relative, starting with `murmur-host`, while a host folder is opened | `reserved_name` | `'path' starts with murmur-host, where this tool's host folders are opened; name the host path absolutely: '/home/op/x'` |
| `start_line` below 1 | `invalid_range` | `start_line must be >= 1, got 0` |
| `start_line` past the end | `invalid_range` | `start_line 99 is beyond end of file (4 lines)` |
| `start_line` after `end_line` | `invalid_range` | `inverted range: start_line 5 is greater than end_line 2` |
| An explicit range over 2000 lines | `range_too_large` | `requested range of 2500 lines exceeds the maximum of 2000; narrow start_line/end_line` |
| The file does not exist | `not_found` | `src/app.py: <OS error>` |
| Named as an image, but not a PNG, JPEG, GIF or WebP file | `not_an_image` | `'notes.png' is named as an image but is not a PNG, JPEG, GIF or WebP file` |
| The file is not UTF-8 and not an image | `io_error` | `blob.bin: stream did not contain valid UTF-8` |

## Host folders

`path` is relative to the work folder. An absolute `path` opens only inside a host
folder the session opened for the tool:

| Session | Host folders |
|---|---|
| `mur run --unconfined` | The whole host, `/`, read and write |
| Contained, with `host_paths` in `~/.murmur/config.yaml` | Each entry's folder, with the entry's `read` or `read-write` access |
| Contained, no `host_paths` | None |

murmur hands each folder to the tool as a WASI preopen named `murmur-host`
followed by the folder's absolute path, `murmur-host` alone for `/`. A `mur`
older than `e86e0b42` hands none, and every absolute `path` is refused. The
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

```text
'path' is outside every host folder tool-read may open: /home/op/.cargo (read), /data (read-write); got '/etc/hosts'. A relative path opens in the work folder
```

A value with no final component (`/`, or one ending in `..`) gets no
suggestion. With no host folder opened, `murmur-host/…` is an ordinary
work-folder path.

A `host_paths` entry that is a symlink is opened at its target, so a path is
covered only when spelled through the target. An absolute symlink inside a
folder is not followed: opening one fails with `Operation not permitted`. Every
message names `path` as the call wrote it.

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

Messages name the bare field: `missing required field: path`, `'path' is
outside every host folder …`. Every result, note, limit and error kind is otherwise
tool-editor 0.12.0's `read`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
