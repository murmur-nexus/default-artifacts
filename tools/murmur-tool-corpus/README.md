# murmur-tool-corpus

Append-only record store for Murmur capsules — `append`, `get`, `read_recent`
and `search` over one JSON-lines file the capsule can trust rather than merely
ask an agent to respect.

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
- **Fail-closed configuration.** A type the operator never declared cannot be
  appended, a body failing its schema is not written, and a schema keyword this
  build does not implement is a hard configuration error rather than an ignored
  constraint.

## Where it lives

| Path | Written by | Purpose |
|---|---|---|
| `state/corpus.jsonl` | this tool, append-only | the records |
| `state/corpus.config.json` | the operator | declared types, their schemas, and the caps on `n` and `k` |

Both sit behind the capsule's `capabilities.state` grant, out of the agent's
reach. Without that grant every operation returns `state_unavailable` — the tool
never falls back to the workdir, because a corpus the agent can rewrite is worse
than no corpus.

> The Murmur runtime does not grant a `state/` preopen yet, so a capsule that
> installs this tool today gets `state_unavailable` from every call. The
> behaviour is proved against the compiled component by
> `tests/wasm_component.rs`; end-to-end use waits on the durable-state grant.

## Configuring it

`state/corpus.config.json` is written by the operator, not by the agent, and no
operation runs until it parses. Every field it accepts:

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

```json
{
  "config_version": 1,
  "read_recent": { "default": 10, "max": 50 },
  "search": { "default_k": 5, "max_k": 25 },
  "prefix_map": { "session-note": "snt" },
  "types": {
    "note": {
      "schema_version": 1,
      "schema": {
        "type": "object",
        "required": ["text"],
        "properties": {
          "text": { "type": "string" },
          "tags": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": false
      }
    }
  }
}
```

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

## Compared to `murmur-hook-memory-jsonl`

Both append JSON lines, and they are not alternatives:

| | `murmur-tool-corpus` | `murmur-hook-memory-jsonl` |
|---|---|---|
| Kind | tool the agent calls | hook fired by the turn lifecycle |
| Location | `state/`, behind `capabilities.state` | the capsule workdir |
| Record shape | operator-declared types, schema-checked | fixed turn / task-close records |
| Retrieval | `get`, capped `read_recent` and `search` | reloads prior turns into context |

Reach for the corpus when a record must survive an agent that can edit files;
reach for the memory log when a turn history should reload itself.

## Extending it

`murmur_tool_corpus::record::searchable_text(&Record) -> Vec<String>` is the seam
between retrieval versions: v1 term-matches over its output, and a future
embedding-based v2 consumes the identical output. Its ordering is a contract, not
an implementation detail.

See [murmur.yaml](./murmur.yaml) for the full manifest and per-operation
input/output schemas.
