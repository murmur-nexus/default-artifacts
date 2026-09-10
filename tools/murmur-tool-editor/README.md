# murmur-tool-editor

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
| `find_in_files` | `pattern`, `dir` | `recursive`, `context_lines` | — |

The two writing operations name their target `dest_path`; the two reading
operations take `path` and `dir`. The split is not cosmetic — see below.

## Absolute paths are refused

`path`, `dest_path` and `dir` are workdir-relative by contract, and a value
beginning with `/` is refused before the operation touches the filesystem:

```json
{
  "ok": false,
  "error_kind": "absolute_path",
  "message": "'dest_path' must be relative to the capsule workdir; got '/app/results.txt' (did you mean 'results.txt'?)"
}
```

The message names the property the call actually wrote — `path` for `read_file`,
`dest_path` for `write_file` and `replace_in_file`, `dir` for `find_in_files` —
and suggests the value's final component. A value with no final component (`/`,
or one ending in `..`) gets no suggestion, so the message ends at the offending
value:

```text
'dir' must be relative to the capsule workdir; got '/'
```

Refusing rather than resolving is the whole point. A tool is dispatched with one
WASI preopen mapped to the capsule workdir, so an absolute value resolves
*inside* that preopen: `write_file` with `dest_path: /app/results.txt` used to
create `<workdir>/app/`, write the file there, and report `ok: true` with a byte
count, and a later `read_file` of the same value returned that content — every
operation agreeing with every other, while anything outside the capsule looking
where the manifest said the file would be found nothing. A refused call creates
no directory, writes nothing, and declares no `state_effect`, so it is not
recorded as a mutation.

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

## Breaking change in 0.3.0

`write_file` and `replace_in_file` no longer accept `path`. Sent the old way the
call returns `ok: false` with `missing required field: dest_path` and writes
nothing; there is no fallback, because a silent one would leave the call judged
by key name again. Update any capsule manifest, system prompt, fixture or saved
conversation that calls either operation to use `dest_path`. `read_file`
(`path`) and `find_in_files` (`dir`) are unchanged.

See [murmur.yaml](./murmur.yaml) for the full manifest and per-operation
input/output schemas.
