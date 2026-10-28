# tool-editor

Structured file editing for Murmur capsules — read, write, edit and search
files without a shell capability grant.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`).

## Calls

A call is an object holding exactly one of four keys, `read`, `write`, `edit`
or `find`, whose value is an object of that operation's fields. Every file is
named `path`. All paths are relative to the capsule workdir (the component's CWD
at dispatch time); an absolute value is [refused](#absolute-paths-are-refused).

| Key | Required | Optional | Writes to |
|---|---|---|---|
| `read` | `path` | `start_line`, `end_line` | — |
| `write` | `path`, `content` | — | `write.path` |
| `edit` | `path`, `edits`: a non-empty array of `{old, new}`, both required | — | `edit.path` |
| `find` | `pattern` | `dir`, `glob`, `recursive`, `context_lines` | — |

```json
{"edit": {"path": "src/app.py", "edits": [
  {"old": "retries = 3", "new": "retries = 5"},
  {"old": "def run(", "new": "def run_once("}
]}}
```

A field whose value is `null` counts as absent. `state_effect` is `read` for a
successful `read` or `find` and `mutate` for a successful `write` or `edit`; a
failed call declares none.

### Refused calls

A call is checked in this order, and the first failure is returned. A refused
call touches no file.

| Check | Refusal | `error_kind` |
|---|---|---|
| Every top-level key is `read`, `write`, `edit` or `find` | `unknown field "dest_path", "operation": a call holds exactly one of read, write, edit, find` | `unknown_field` |
| Exactly one of them is non-null | `a call holds exactly one of read, write, edit, find; got none` (or `got read, write`) | — |
| Its value is an object | `write must be an object` | — |
| Every key of that object is one the operation takes | `unknown field "dest_path" in write: write takes path, content` | `unknown_field` |
| Every key of an `edits` element is `old` or `new` | `unknown field "old_string" in edit 1 of 2: an edit takes old, new` | `unknown_field` |
| A required field is present | `missing required field: edit.path` | — |

Unknown keys are listed sorted and quoted, with quotes, backslashes and control
characters escaped, so the message stays on one line. Every refusal is also the result's `summary`, which is what a
host shows for the call. The input names of 0.11.0 and earlier (`operation`,
`dest_path`, `old_string`, `new_string`) are refused this way; see
[Breaking change in 0.12.0](#breaking-change-in-0120).

## Reading with `read`

`start_line` and `end_line` are 1-based and inclusive, and both are optional.
Every read, with or without them, is held to three limits:

| Limit | Value |
|---|---|
| Lines per read | 2000 |
| Content per read | 100 KB (102,400 bytes) |
| Characters per line | 2000 |

A line longer than 2000 characters comes back as its first 2000 characters. A
read with no range starts at line 1, and an omitted `end_line` reads to the end
of the file or to the first limit reached. An explicit `start_line`/`end_line`
span over 2000 lines is refused with `error_kind: "range_too_large"`; a
`start_line` below 1 or past the end of the file, or an inverted range, is
refused with `error_kind: "invalid_range"`. An `end_line` past the end clamps
to the file's line count.

| Read | `data` keys | `message` |
|---|---|---|
| No range, file within every limit | `content` (the file verbatim), `total_lines` | `read <path>` |
| With a range, within every limit | `content`, `total_lines`, `start_line`, `end_line` | `read <path> lines <s>-<e>` |
| Cut by a limit, with or without a range | `content`, `total_lines`, `start_line`, `end_line`, `truncated` | `read <path> lines <s>-<e>` |

Every response but the verbatim one returns the lines joined with `\n`, with no
trailing newline, and `start_line`/`end_line` as resolved. A cut read still
succeeds: its summary ends in `, truncated)` and the result's `truncated` flag
is set. The `truncated` note names the limit that stopped the read and, when
the read stopped before the requested lines ran out, the `start_line` that
continues it. A 5000-line file read with no range returns:

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

## Writing with `write`

`write` creates or replaces the file at `path` with `content`, creating any
missing parent directories. It returns `wrote <path>` with summary
`<n> bytes written` and no `data`.

## Editing with `edit`

`edit` applies `edits` to the file at `path`:

- The pairs apply in order, each to the text the one before produced, so a
  later `old` may match text an earlier `new` inserted.
- Each `old` must occur exactly once in the text it applies to. Overlapping
  occurrences count: `aa` occurs twice in `aaa`.
- `old` must be a non-empty string; `new` is a string and may be empty, which
  deletes the matched text.
- All or nothing. The file is read once and written once, only after every pair
  matched. If any pair fails, the file is left byte-identical.

Every input is validated before the file is read, so an empty `edits`, an empty
`old` or an unknown key fails the same way whether or not the file exists.
Positions count from 1 and are written `edit N of M`:

| Failure | `error_kind` | Message |
|---|---|---|
| `old` occurs 0 times | `string_not_found` | `edit 2 of 3: old occurs 0 times in src/app.py; the file is unchanged` |
| `old` occurs 2 or more times | `multiple_matches` | `edit 2 of 2: old occurs 2 times in src/app.py; the file is unchanged` |
| `old` is `""` | — | `edit 2 of 2: old is empty; it must name the text to replace` |
| `edits` is `[]` | — | `edit.edits is empty: it takes at least one {old, new} object` |
| An element carries another key | `unknown_field` | `unknown field "path" in edit 2 of 2: an edit takes old, new` |
| The file does not exist | `not_found` | `src/app.py: <OS error>` |

A successful edit returns `applied <M> edit(s) to <path>` with summary
`<M> edits` and no `data`.

## Searching with `find`

`find` matches `pattern`, a regular expression, against each line of
the files it reads, and returns one `{path, line, text}` per matching line.
`path` is relative to `dir`, `/`-separated.

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
reads only the files directly in `dir`.

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
files walked. An absolute
`glob` is refused like an absolute `dir`.

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
`truncated` flag is set exactly when `capped` is present. The only refusal on
size is `error_kind: "result_size_exceeded"`, when the first match alone does
not fit in 500 KB.

## Absolute paths are refused

`read.path`, `write.path`, `edit.path`, `find.dir` and `find.glob` are
workdir-relative by contract, and a value beginning with `/` is refused before
the operation touches the filesystem:

```json
{
  "ok": false,
  "error_kind": "absolute_path",
  "message": "'write.path' must be relative to the capsule workdir; got '/app/results.txt' (did you mean 'results.txt'?)"
}
```

The message names the input the call wrote, as an input location, and suggests
the value's final component. A value with no final component (`/`, or one
ending in `..`) gets no suggestion, so the message ends at the offending value:

```text
'find.dir' must be relative to the capsule workdir; got '/'
```

Refusing rather than resolving is deliberate. A tool is dispatched with one WASI
preopen mapped to the capsule workdir, so an absolute value cannot escape it —
`/app/results.txt` resolves to `<workdir>/app/results.txt`. Left unchecked,
`write` would create that directory, write the file there, and report
`ok: true` with a byte count, and a later `read` of the same value would
return that content: every operation agreeing with every other, while anything
outside the capsule looking where the manifest said the file would be finds
nothing. A refused call creates no directory, writes nothing, and declares no
`state_effect`, so it is not recorded as a mutation.

## How `read_only` is enforced

`write.path` and `edit.path` are declared in the manifest with
`format: murmur-destination`:

```yaml
    write:
      type: object
      properties:
        path:
          type: string
          format: murmur-destination
```

murmur lowers that annotation to an *input location* — the chain of keys from
the root, `write.path` and `edit.path` — not to the property name `path`. When a
capsule grants `capabilities.filesystem.read_only`, a `write` or `edit` whose
path lands under a read-only entry is refused before dispatch, and the refusal
names the declared location:

```text
Refused by the capsule manifest: 'src/spec.txt' is under the read-only path
'src' declared in capabilities.filesystem.read_only. Identified as a write by
destination 'edit.path' declared by the tool's input schema.
```

`read.path` carries no annotation, so reading a read-only subtree still
succeeds; `find`'s `dir` and `glob` carry none either. The destination is marked
per location, which is why every operation can name its file `path`: annotating
the name would have made every read under the subtree a refusal.

murmur's key-name heuristic runs beside the declarations and reads none of the
reading calls as a write: `read` is `path` plus line numbers, `find` is
`pattern`/`dir`/`glob`, an `edit` object holds `path` and `edits` (not a content
key), and an `{old, new}` pair holds no path key. `write`'s `path` beside
`content` is a write by that heuristic too, but the declared destination is
checked first and is what the refusal names.

Because `read.path` is a path-shaped property with no annotation, a capsule
that declares `read_only` still sees `W-SEC-018` name `path` for this tool. The
warning is about `read.path`, which is a read by design.

`find` results carry a `path` key per match. That is an output, not an
input — the runtime scans tool input only.

## Breaking change in 0.12.0

0.12.0 breaks every existing call: a call now holds one of `read`, `write`, `edit` or `find` instead of an `operation`, every operation names its file `path` instead of `dest_path`, and `edit` takes a list of `{old, new}` pairs, each matching exactly once and all applied or none, in place of `replace_in_file`'s single `old_string`/`new_string`, which replaced every occurrence.

| 0.11.0 | 0.12.0 |
|---|---|
| `{"operation": "read_file", "path": P, ...}` | `{"read": {"path": P, ...}}` |
| `{"operation": "write_file", "dest_path": P, "content": C}` | `{"write": {"path": P, "content": C}}` |
| `{"operation": "replace_in_file", "dest_path": P, "old_string": O, "new_string": N}` | `{"edit": {"path": P, "edits": [{"old": O, "new": N}]}}` |
| `{"operation": "find_in_files", "pattern": R, ...}` | `{"find": {"pattern": R, ...}}` |

- There is no alias or fallback. `operation`, `dest_path`, `old_string`,
  `new_string` and every other unknown key are refused with
  `error_kind: unknown_field`, and the call touches no file.
- `replace_in_file` replaced every occurrence and returned `{count}`. An `edit`
  pair must match exactly once; two or more occurrences return
  `multiple_matches`. A caller that relied on replace-all must send one pair per
  occurrence with enough surrounding text to make each unique.
- An unknown input inside an operation used to be ignored; it is now refused.
- Messages that name an input name its location: `'read.path' must be relative
  …`, `missing required field: find.pattern`, `'find.dir' resolves outside the
  capsule workdir`.
- `hook-diff-summary` 0.12.0 reads `write.path` and `edit.path`; 0.11.0 and
  earlier find no write target in a 0.12.0 call. Upgrade both together.
- Update any capsule manifest, system prompt, fixture or saved conversation
  that calls the editor.

The sections below describe earlier versions in the input names of their time.

## Breaking change in 0.10.0

- A `read_file` with no range of a file over a limit now returns only the
  lines that fit, with `start_line`, `end_line` and a `truncated` note, where
  it used to return the whole file as `content` and `total_lines`.
- A `read_file` with only `start_line` whose remaining lines exceed 2000 now
  succeeds with the first 2000 of them, where it used to return
  `range_too_large`.
- A line longer than 2000 characters now comes back as its first 2000
  characters.

## Breaking change in 0.8.0

- The error kind `search_too_broad` is gone. A `find_in_files` call with
  `dir` set to `""`, `.` or `./` now searches the workdir root instead of being
  refused.
- `dir` is optional. Omitted, it searches the workdir root.
- A search over a limit returns `ok: true` with a `capped` note and the
  matches found so far, where it used to return `result_size_exceeded`. That
  error now fires only when a single match exceeds 500 KB. A caller that
  handled `search_too_broad` or `result_size_exceeded` must read `capped`
  instead.

## Breaking change in 0.5.0

`read_file` now returns `content` on every successful call, including a repeat
read of an unchanged file, and never returns a `cache_ref`. The
`MURMUR_TOOL_EDITOR_CACHE_DIR` environment override is no longer read, and the
tool no longer creates `.tool-editor-cache/` in the workdir; one left by
an earlier version is ignored and safe to delete. Any caller that reads
`cache_ref` must stop.

## Breaking change in 0.3.0

`write_file` and `replace_in_file` no longer accept `path`. Sent the old way the
call returns `ok: false` with `missing required field: dest_path` and writes
nothing; there is no fallback, because a silent one would leave the call judged
by key name again. Update any capsule manifest, system prompt, fixture or saved
conversation that calls either operation to use `dest_path`. `read_file`
(`path`) and `find_in_files` (`dir`) are unchanged.

See [murmur.yaml](./murmur.yaml) for the full manifest and its input schema.
