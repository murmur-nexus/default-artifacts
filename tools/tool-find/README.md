# tool-find

Searches the contents of files in the work folder for a regular expression, within fixed limits.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). The model calls it `tool-find`. Its file logic lives
in the shared, unpublished `libs/murmur-file-tools` library, which
`tool-read`, `tool-write`, `tool-edit` and `tool-find` all call.

## Definition

The model is sent this description and input schema:

```yaml
description: |
  Searches file contents for a regex, skipping hidden directories, target, node_modules and binary files. Stops at 200 matches, 5000 files walked, 32 MiB read or a 500 KB result, returning the matches so far with a capped note naming the limit. dir is relative to the work folder, or absolute inside a host folder the session opened; match paths are relative to dir.
input_schema:
  type: object
  properties:
    pattern:
      type: string
    dir:
      type: string
    glob:
      type: string
      description: "*.py or src/**/*.rs"
    recursive:
      type: boolean
    context_lines:
      type: integer
      description: max 20
  required: [pattern]
  murmur-destinations: []
```

A successful call declares `state_effect: read`.

## Searching

`tool-find` matches `pattern`, a regular expression, against each line of the
files it reads, and returns one `{path, line, text}` per matching line in
`data.matches`. `path` is relative to `dir`, `/`-separated. `context_lines`
(0 to 20, default 0) adds `context_before` and `context_after` arrays to each
match; with 0 neither key is present.

`dir` is optional. Omitted, `null`, `""`, `.`, `./`, or any value that
normalizes to the workdir root, such as `src/..`, searches the whole workdir.
`.` segments are dropped and each `..` removes the segment before it; a `..`
that climbs above the workdir is refused with `error_kind: "outside_workdir"`.
An absolute `dir` searches a host folder (see [Host folders](#host-folders)).

Below the directory it searches, the walk skips:

| Skipped | Rule |
|---|---|
| Hidden directories | any name starting with `.`, such as `.git` and `.venv` |
| Build and dependency directories | `target`, `node_modules` |
| Binary files | a NUL byte in the first 8 KiB, or content that is not UTF-8 |

Hidden files, such as `.env`, are searched. A `dir` that names a skipped
directory, such as `.git` or `target/debug`, searches it. `recursive: false`
(default `true`) reads only the files directly in `dir`.

`glob` is optional and filters which files are read. A glob without `/` is
matched against the file name, so `*.py` matches a Python file at any depth. A
glob with `/` is matched against the path relative to `dir`, the same `path` a
match reports.

| Glob syntax | Matches |
|---|---|
| `*` | any characters within one path segment |
| `?` | one character other than `/` |
| `**/` | zero or more whole segments: `src/**/*.rs` matches `src/a.rs` and `src/x/y/a.rs` |
| `/**` at the end | everything below: `lib/**` |
| any other character | itself |

Files the glob excludes are never read, but they count toward the limit of
files walked.

## Limits

A search stops at the first limit it reaches:

| Limit | Value |
|---|---|
| Matches | 200 |
| Files walked | 5000 |
| Bytes read | 32 MiB |
| Result size | 500 KB |

A search that reaches a limit still succeeds. It returns the matches found so
far, in walk order (sorted by name within each directory), with a `capped` note
in `data`, and its summary ends in `(capped)`:

```json
{
  "capped": "Search stopped at the limit of 200 matches after reading 1 file.",
  "matches": [
    { "path": "big.py", "line": 1, "text": "def f1(): pass" },
    { "path": "big.py", "line": 2, "text": "def f2(): pass" }
  ]
}
```

A search that reaches no limit carries no `capped` key. The result's
`truncated` flag is set exactly when `capped` is present.

## Refused calls

Under murmur, a call missing a top-level `required` field (`pattern`) never
reaches the tool: murmur refuses it before dispatch, records a
`tool_input_refused` event, and tells the model which field is missing. A call
that gets past that check is checked by the tool in this order, and the first
failure is returned. A refused call touches no file and declares no
`state_effect`.

1. The input is a JSON object (or a JSON string encoding one).
2. Every key is one of `pattern`, `dir`, `glob`, `recursive` or `context_lines`. Any other key, whatever its value, is
   refused with `error_kind: "unknown_field"`, naming the keys sorted and
   quoted and the fields this tool takes:

   ```text
   unknown field "find": tool-find takes pattern, dir, glob, recursive, context_lines
   ```

3. The required fields are present. A field whose value is `null` counts as
   absent at every level, so murmur's own required-field check, which only
   tests presence, lets it through and the tool refuses it.
4. `dir` resolves: relative to the work folder, or absolute inside a host folder
   the session opened (see below). `glob` is relative.

Every refusal is also the result's `summary`, which is what a host shows for
the call.

| Failure | `error_kind` | Message |
|---|---|---|
| `pattern` absent, `null` or `""` | — | `missing required field: pattern` |
| `dir` not a string | — | `dir must be a string` |
| `glob` not a string | — | `glob must be a string` |
| `dir` absolute, no host folder opened | `absolute_path` | `'dir' must be relative to the work folder: this session opens no host folder to tool-find; got '/app/src' (did you mean 'src'?)` |
| `dir` absolute, outside every host folder | `absolute_path` | `'dir' is outside every host folder tool-find may open: /home/op/.cargo (read); got '/etc/hosts'. A relative path opens in the work folder` |
| `dir` relative, starting with `murmur-host`, while a host folder is opened | `reserved_name` | `'dir' starts with murmur-host, where this tool's host folders are opened; name the host path absolutely: '/home/op/x'` |
| `glob` absolute | `absolute_path` | `'glob' is matched against paths below 'dir' and must be relative; got '/src/*.rs' (did you mean '*.rs'?). Name a host folder in 'dir'` |
| `context_lines` below 0 or above 20 | `context_too_large` | `context_lines must be between 0 and 20, got 21` |
| `dir` climbs above the workdir | `outside_workdir` | `'dir' resolves outside the capsule workdir: '../..'` |
| `pattern` is not a valid regex | `invalid_pattern` | `invalid regex '(': <regex error>` |
| `glob` cannot be compiled | `invalid_pattern` | `invalid glob '<glob>': <regex error>` |
| The first match alone exceeds 500 KB | `result_size_exceeded` | `a single match exceeds the find result size limit of 512000 bytes (500 KB).` |

## Host folders

`dir` is relative to the work folder. An absolute `dir` opens only inside a host
folder the session opened for the tool:

| Session | Host folders |
|---|---|
| `mur run --unconfined` | The whole host, `/`, read and write |
| Contained, with `host_paths` in `~/.murmur/config.yaml` | Each entry's folder, with the entry's `read` or `read-write` access |
| Contained, no `host_paths` | None |

murmur hands each folder to the tool as a WASI preopen named `murmur-host`
followed by the folder's absolute path, `murmur-host` alone for `/`. A `mur`
older than `e86e0b42` hands none, and every absolute `dir` is refused. The
tool lists the folders with `wasi:filesystem/preopens.get-directories`, resolves
the value's `.` and `..` segments lexically, and opens it through the
longest folder that covers it.
A folder covers whole components only: `/home/op/.cargo` covers
`/home/op/.cargo/bin/x` but not `/home/op/.cargo2`.

| `dir` | Result |
|---|---|
| Relative | Opens in the work folder, as always |
| Relative, first segment `murmur-host`, while a host folder is opened | Refused with `reserved_name`, naming the absolute path to use |
| Absolute, inside a host folder | Opens there |
| Absolute, no host folder opened | Refused with `absolute_path`, suggesting the value's final component |
| Absolute, outside every host folder | Refused with `absolute_path`, listing the host folders and their access in preopen order |

```text
'dir' is outside every host folder tool-find may open: /home/op/.cargo (read), /data (read-write); got '/etc/hosts'. A relative path opens in the work folder
```

A value with no final component (`/`, or one ending in `..`) gets no
suggestion. With no host folder opened, `murmur-host/…` is an ordinary
work-folder path.

A `host_paths` entry that is a symlink is opened at its target, so a path is
covered only when spelled through the target. An absolute symlink inside a
folder is not followed: opening one fails with `Operation not permitted`. Every
message names `dir` as the call wrote it.

An absolute `dir` is searched with the same rules as a relative one, and match
`path`s stay relative to it. `glob` is always matched below `dir`, so a value
beginning with `/` is refused with `absolute_path` whether or not a host folder
is opened:

```text
'glob' is matched against paths below 'dir' and must be relative; got '/src/*.rs' (did you mean '*.rs'?). Name a host folder in 'dir'
```

While a host folder is opened, a search of the work folder's root never reads
or descends into a top-level entry named `murmur-host`: that name is where the
host folders are opened.

## How `read_only` is enforced

The schema declares `murmur-destinations: []`: tool-find writes nothing named by
or derived from its input. No property carries `format: murmur-destination`,
so a `capabilities.filesystem.read_only` grant never refuses a tool-find call,
and murmur raises no `W-SEC-018` for it. Its properties pair no path key with a content key, so murmur's key-name rules read it as a read too. The `path` key in each match is an output; the runtime scans tool input only.

## Replaces tool-editor

`tool-find` is one of the four flat tools that replace `tool-editor`, whose 0.12.0
calls held each operation in its own nested object. There is no alias. A
nested call carries none of the top-level `required` fields, so under murmur it
is refused before dispatch for the missing field and the tool never runs.
Outside that check, or when the call also carries the required fields, the
tool refuses the nested key as an unknown field, and every earlier shape the same way.

| tool-editor 0.12.0 | `tool-find` 0.1.0 |
|---|---|
| `{"find": {"pattern": R, "dir": D, …}}` | `{"pattern": R, "dir": D, …}` |

Messages that named `find.pattern`, `find.dir` or `find.glob` now name
`pattern`, `dir` and `glob`. Every result, note, limit and error kind is
otherwise tool-editor 0.12.0's `find`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
