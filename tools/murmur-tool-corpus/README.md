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

## Extending it

`murmur_tool_corpus::record::searchable_text(&Record) -> Vec<String>` is the seam
between retrieval versions: v1 term-matches over its output, and a future
embedding-based v2 consumes the identical output. Its ordering is a contract, not
an implementation detail.

See [murmur.yaml](./murmur.yaml) for the full manifest and per-operation
input/output schemas.
