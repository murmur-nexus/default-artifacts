# murmur-tool-corpus

Append-only record store for Murmur capsules — `append`, `get`, `read_recent`,
`search` and `verify` over one JSON-lines file the capsule can trust rather than
merely ask an agent to respect.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`, imports no `murmur:*` interface).

## What it guarantees

- **Append is the only write.** The corpus file is opened for append or
  read-only and never for anything else, so no code path can rewrite a byte
  already on disk.
- **The store assigns identity.** `id`, `created_at` and `schema_version` come
  from the store, never from the caller.
- **Retries are idempotent.** An `external_id` makes a repeated append return
  the first call's id with `deduped: true`, leaving the file byte-for-byte
  unchanged.
- **Deletion does not exist.** A record carrying `withdraws: <id>` is itself
  appended; the target drops out of `search` and `read_recent`, while `get`
  still resolves it with `body: null` and the withdrawal's id and timestamp.
- **No unbounded read.** `read_recent` and `search` are capped by operator
  config, and there is no operation that returns the whole corpus.
- **Retrieval order is fixed.** `read_recent` returns newest first; `search` returns
  highest score first, ties broken newest first. Both order by `(created_at, id)`
  descending, so two records minted in the same millisecond still come back in mint
  order and repeat runs over an unchanged corpus are byte-identical.
- **A search hit is an excerpt, not a record.** Each hit is
  `{id, type, created_at, score, excerpt}`. The excerpt is the first segment of the
  record's searchable text containing a query term, collapsed to one line and cut to
  120 characters on a character boundary. Scan the hits; call `get` on the two or
  three worth reading in full.
- **A bad line is skipped and reported, never fatal.** Every read — including the
  scan `append` runs for dedupe and withdrawal checks — steps over a line that does
  not parse. Any response built from such a scan carries `skipped_lines` and
  `skipped_line_count` and says so in its summary, so the damage reaches the agent's
  context and the trace on the very next call rather than going quiet.
- **`verify` names the damage, and nothing repairs it.** `verify` reports every
  unreadable line with its number, its parse error and a bounded preview. There is
  deliberately no `repair` verb: rewriting the file would be the only code path that
  opens the corpus for something other than append, and that invariant is worth more
  than the convenience. Repair is a human edit to `corpus.jsonl`, made with a
  `verify` report in front of you.
- **Fail-closed configuration.** A type the operator never declared cannot be
  appended, a body failing its schema is not written, and a schema keyword this
  build does not implement is a hard configuration error rather than an ignored
  constraint.

## Where it lives

| Path | Written by | Purpose |
|---|---|---|
| `state/corpus.jsonl` | this tool, append-only | the records |

It sits behind the capsule's `capabilities.state` grant, out of the agent's
reach. Without that grant every operation returns `state_unavailable` — the tool
never falls back to the workdir, because a corpus the agent can rewrite is worse
than no corpus. The configuration is not a file at all; see below.

The Murmur runtime mounts a granted store at the preopen `state`, so a capsule
whose manifest entry for this tool declares `capabilities.state` reaches the
corpus end to end, and one that does not gets `state_unavailable` from every
call. Both are proved against the compiled component by
`tests/wasm_component.rs`, and against a real `mur run` launch by the murmur
repository's `crates/murmur-cli/tests/corpus_state.rs`.

## Configuring it

The operator configuration is the `config:` block on this tool's entry in the
**capsule's** `murmur.yaml`. The runtime lowers it to compact JSON and delivers it
to this artifact alone as `MURMUR_ARTIFACT_CONFIG`; no capability declares it, and
`capabilities.env.allow` cannot substitute a host value for it. An entry with no
`config:` block gets `config_missing` from every operation but `verify`.

```yaml
artifacts:
  - name: murmur-tool-corpus
    runtime: tool
    capabilities:
      state: {}
    config:
      config_version: 1
      read_recent: { default: 10, max: 50 }
      search: { default_k: 5, max_k: 25 }
      prefix_map: { session-note: snt }
      types:
        note:
          schema_version: 1
          schema:
            type: object
            required: [text]
            properties:
              text: { type: string }
              tags: { type: array, items: { type: string } }
            additionalProperties: false
```

Keeping it there rather than in a file under `state/` puts every schema change in
the operator's own manifest, under whatever review that file already gets, and
leaves the agent no way to reach it. Every field the block accepts:

| Key | Required | Default | Meaning |
|---|---|---|---|
| `config_version` | yes | — | must be `1`; any other value is `config_invalid` |
| `types` | yes, non-empty | — | the declared record types, keyed by type tag |
| `types.<tag>.schema` | yes | — | JSON Schema (subset below) every body of this type must satisfy |
| `types.<tag>.schema_version` | no | `1` | stamped onto each record of the type |
| `read_recent.default` | no | `10` | `n` when the caller omits it |
| `read_recent.max` | no | `50` | ceiling `n` is clamped to |
| `search.default_k` | no | `5` | `k` when the caller omits it |
| `search.max_k` | no | `25` | ceiling `k` is clamped to |
| `prefix_map.<tag>` | no | derived | explicit id prefix for a type, `^[a-z][a-z0-9]{0,7}$` |

The runtime checks the block's shape and not its meaning — a mapping with string
keys whose compact JSON is at most 65536 bytes, or the launch is refused with
`error[E-CAP-010]` naming the entry. Which keys this tool needs, and what they
mean, is checked here and reported as `config_invalid`.

### Supported schema keywords

A keyword outside this list — `pattern`, `oneOf`, `$ref`, anything else — is
`config_invalid` naming the keyword, not an ignored constraint. In an
append-only log a record admitted by a silently dropped constraint is permanent.

| Supported | |
|---|---|
| keywords | `type`, `properties`, `required`, `items`, `enum`, `minLength`, `maxLength`, `minimum`, `maximum`, `minItems`, `maxItems`, `additionalProperties` (boolean form only) |
| `type` values | `object`, `array`, `string`, `number`, `integer`, `boolean`, `null` |

### Id prefixes

Record ids are `<prefix>_<uuid-v7>`. A type's prefix is the first three
`[a-z0-9]` characters of its tag unless `prefix_map` overrides it. The runtime
reserves `ses`, `tsk`, `ctx`, `req`, `dep`, `evt`, `msg`; a type deriving one of
those is `config_invalid` until `prefix_map` gives it another. Two types sharing
a prefix is allowed — the UUID keeps ids unique.

## Reading a `verify` report

```json
{"ok": true, "operation": "verify", "lines": 7, "records": 5, "bad_line_count": 2,
 "bad_lines": [
   {"line": 3, "error": "expected ident at line 1 column 2", "preview": "this line is not a record"},
   {"line": 7, "error": "missing field `body` at line 1 column 89", "preview": "{\"created_at\":\"2026-08-25T12:00:00.000Z\",\"id\":\"not_01a\", ...}"}
 ]}
```

`lines` counts every line in the file, `records` the ones that parse, and each
`bad_lines` entry gives the 1-based line number to open, the parse error, and a
one-line preview of at most 120 characters. `bad_lines` is capped at 100 entries
while `bad_line_count` stays the true total, so a badly damaged corpus cannot
flood an agent's context. The same cap and the same rule apply to the
`skipped_lines` / `skipped_line_count` pair the other four operations attach when
their scan stepped over something.

Fix a bad line by editing `state/corpus.jsonl` directly. Deleting the line loses
whatever it recorded; a line that is a mangled record is usually worth
reconstructing by hand. Either way the tool will not do it for you, on purpose.

## Compared to `murmur-hook-memory-jsonl`

Both append JSON lines, and they are not alternatives:

| | `murmur-tool-corpus` | `murmur-hook-memory-jsonl` |
|---|---|---|
| Kind | tool the agent calls | hook fired by the turn lifecycle |
| Location | `state/`, behind `capabilities.state` | the capsule workdir |
| Record shape | operator-declared types, schema-checked | fixed turn / task-close records |
| Retrieval | `get`, capped `read_recent`, excerpt `search` | reloads prior turns into context |

Reach for the corpus when a record must survive an agent that can edit files;
reach for the memory log when a turn history should reload itself.

## Extending it

`murmur_tool_corpus::record::searchable_text(&Record) -> Vec<String>` is the seam
between retrieval versions: v1 term-matches over its output *and* draws every
excerpt from it, and a future embedding-based v2 consumes the identical output.
Its ordering is a contract, not an implementation detail — it is what decides
which segment of a record an excerpt comes from.

See [murmur.yaml](./murmur.yaml) for the full manifest and per-operation
input/output schemas.
