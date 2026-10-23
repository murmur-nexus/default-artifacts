# tool-editor

Structured file editing for Murmur capsules — read, write, surgical patch, and
search operations (`read_file`, `write_file`, `replace_in_file`,
`find_in_files`) without requiring shell capability grants.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`).

## Operations

Every call takes an `operation` field. All paths are relative to the capsule
workdir (the component's CWD at dispatch time); an absolute value is
[refused](#absolute-paths-are-refused).

| Operation | Required | Optional | Writes to |
|---|---|---|---|
| `read_file` | `path` | `start_line`, `end_line` | — |
| `write_file` | `dest_path`, `content` | — | `dest_path` |
| `replace_in_file` | `dest_path`, `old_string`, `new_string` | — | `dest_path` |
| `find_in_files` | `pattern` | `dir`, `glob`, `recursive`, `context_lines` | — |

The two writing operations name their target `dest_path`; the two reading
operations take `path` and `dir`. The split is not cosmetic — see below.

## Searching with `find_in_files`

`find_in_files` matches `pattern`, a regular expression, against each line of
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

## A missing or unknown `operation`

A call whose `operation` the tool cannot dispatch returns `ok: false` with one of
two messages. Both list the operations it accepts, in the order above:

| `operation` sent | Message |
|---|---|
| absent, `null`, any other non-string, or `""` | `missing "operation": expected one of read_file, write_file, replace_in_file, find_in_files` |
| any other string not in the table | `unknown operation "X": expected one of read_file, write_file, replace_in_file, find_in_files` |

The unrecognised value is quoted with quotes, backslashes and control characters
escaped, so `"write\nfile"` appears as `"write\nfile"` and the message stays on
one line. The message is also the result's `summary`, which is what a host shows
for the call. Neither carries an `error_kind`. The call touches no file and
declares no `state_effect`. A caller can tell the two apart by their prefixes,
`missing "operation"` and `unknown operation "`.

Before 0.5.1 both cases returned `unknown operation: <value>`, which for a call
with no `operation` read `unknown operation: ` with nothing after the colon.

## Absolute paths are refused

`path`, `dest_path`, `dir` and `glob` are workdir-relative by contract, and a
value beginning with `/` is refused before the operation touches the filesystem:

```json
{
  "ok": false,
  "error_kind": "absolute_path",
  "message": "'dest_path' must be relative to the capsule workdir; got '/app/results.txt' (did you mean 'results.txt'?)"
}
```

The message names the property the call actually wrote — `path` for `read_file`,
`dest_path` for `write_file` and `replace_in_file`, `dir` or `glob` for
`find_in_files` — and suggests the value's final component. A value with no final component (`/`,
or one ending in `..`) gets no suggestion, so the message ends at the offending
value:

```text
'dir' must be relative to the capsule workdir; got '/'
```

Refusing rather than resolving is deliberate. A tool is dispatched with one WASI
preopen mapped to the capsule workdir, so an absolute value cannot escape it —
`/app/results.txt` resolves to `<workdir>/app/results.txt`. Left unchecked,
`write_file` would create that directory, write the file there, and report
`ok: true` with a byte count, and a later `read_file` of the same value would
return that content: every operation agreeing with every other, while anything
outside the capsule looking where the manifest said the file would be finds
nothing. A refused call creates no directory, writes nothing, and declares no
`state_effect`, so it is not recorded as a mutation.

## How `read_only` is enforced

`dest_path` is declared in the manifest with `format: murmur-destination`:

```yaml
    dest_path:
      type: string
      format: murmur-destination
```

That annotation tells the runtime which input is a filesystem write target. When
a capsule grants `capabilities.filesystem.read_only`, a `write_file` or
`replace_in_file` call landing under a read-only entry is refused before
dispatch, and the refusal names the declaration:

```text
Refused by the capsule manifest: 'src/notes.txt' is under the read-only path
'src' declared in capabilities.filesystem.read_only. Identified as a write by
destination 'dest_path' declared by the tool's input schema.
```

Without the annotation the runtime falls back to guessing write targets from
property names and warns with `W-SEC-018`. Because the destination has its own
property, `read_file` and `find_in_files` stay usable under a `read_only` grant:
their inputs are not annotated, so reading and searching a read-only subtree
still succeeds. Sharing one `path` property across all four operations would
have made every read under that subtree a refusal.

`find_in_files` results carry a `path` key per match. That is an output, not an
input — the runtime scans tool input only.

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

See [murmur.yaml](./murmur.yaml) for the full manifest and per-operation
input/output schemas.
