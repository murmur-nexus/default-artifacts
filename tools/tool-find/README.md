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
  Searches file contents for a regex, skipping hidden directories, target, node_modules and binary files. Stops at 200 matches, 5000 files walked, 32 MiB read or a 500 KB result, returning the matches so far with a capped note naming the limit. dir is relative to the work folder, and match paths to dir.
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

A call is checked in this order, and the first failure is returned. A refused
call touches no file and declares no `state_effect`.

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
4. Every path is relative to the work folder (see below).

Every refusal is also the result's `summary`, which is what a host shows for
the call.

| Failure | `error_kind` | Message |
|---|---|---|
| `pattern` absent, `null` or `""` | — | `missing required field: pattern` |
| `dir` not a string | — | `dir must be a string` |
| `glob` not a string | — | `glob must be a string` |
| `dir` absolute | `absolute_path` | `'dir' must be relative to the capsule workdir; got '/app' (did you mean 'app'?)` |
| `glob` absolute | `absolute_path` | `'glob' must be relative to the capsule workdir; got '/src/*.rs' (did you mean '*.rs'?)` |
| `context_lines` below 0 or above 20 | `context_too_large` | `context_lines must be between 0 and 20, got 21` |
| `dir` climbs above the workdir | `outside_workdir` | `'dir' resolves outside the capsule workdir: '../..'` |
| `pattern` is not a valid regex | `invalid_pattern` | `invalid regex '(': <regex error>` |
| `glob` cannot be compiled | `invalid_pattern` | `invalid glob '<glob>': <regex error>` |
| The first match alone exceeds 500 KB | `result_size_exceeded` | `a single match exceeds the find result size limit of 512000 bytes (500 KB).` |

## Absolute paths are refused

`dir` and `glob` are relative to the work folder, the capsule workdir. A value beginning with
`/` is refused with `error_kind: "absolute_path"` before any filesystem call:

```text
'dir' must be relative to the capsule workdir; got '/'
```

The message names the field and suggests the value's final component; a value
with no final component (`/`, or one ending in `..`) gets no suggestion. A tool
is dispatched with one WASI preopen mapped to the workdir, so an absolute value
cannot escape it — `/app/x.txt` would resolve to `<workdir>/app/x.txt`, which
nothing outside the capsule would find where the call named it. Refusing is the
only way that contract is observable.

## How `read_only` is enforced

The schema declares `murmur-destinations: []`: tool-find writes nothing named by
or derived from its input. No property carries `format: murmur-destination`,
so a `capabilities.filesystem.read_only` grant never refuses a tool-find call,
and murmur raises no `W-SEC-018` for it. Its properties pair no path key with a content key, so murmur's key-name rules read it as a read too. The `path` key in each match is an output; the runtime scans tool input only.

## Replaces tool-editor

`tool-find` is one of the four flat tools that replace `tool-editor`, whose 0.12.0
calls held each operation in its own nested object. There is no alias: the
nested shape is refused as an unknown field, and so is every earlier shape.

| tool-editor 0.12.0 | `tool-find` 0.1.0 |
|---|---|
| `{"find": {"pattern": R, "dir": D, …}}` | `{"pattern": R, "dir": D, …}` |

Messages that named `find.pattern`, `find.dir` or `find.glob` now name
`pattern`, `dir` and `glob`. Every result, note, limit and error kind is
otherwise tool-editor 0.12.0's `find`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
