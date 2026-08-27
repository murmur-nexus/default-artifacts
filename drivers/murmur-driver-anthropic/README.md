# murmur-driver-anthropic

Anthropic Messages API inference driver for Murmur agent capsules.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the Anthropic
Messages API, including SSE streaming, extended-thinking blocks, prompt-cache
breakpoints, and model-family handling (Claude 3.x vs Claude 4+ naming and
parameter rules).

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. See [murmur.yaml](./murmur.yaml) for the full manifest.

## Prompt caching

Anthropic caches a prompt prefix only where the request carries a
`cache_control` marker. The driver places three, in the order Anthropic renders
a prompt:

| # | Marker position | Emitted when |
|---|---|---|
| 1 | the last tool definition | `tools` is non-empty |
| 2 | the system prompt block | the system prompt is non-blank |
| 3 | the last cacheable content block of the conversation | at least one cacheable block exists |

Marker 2 covers the tool inventory and the system prompt together, because tools
render ahead of the system prompt. Marker 3 sits at the end of the settled
conversation, so the next turn reads everything before it and writes only what
the last turn appended.

Marker 3 searches backwards and crosses message boundaries: a `thinking` block
cannot carry a marker, and a message can translate to an empty content array.
When no cacheable block exists anywhere, marker 3 is left out and the other two
still stand.

Caching is on by default and needs no capsule change. Configure it under
`inference.driver.config` in the capsule's `murmur.yaml`:

| Key | Accepted | Default | Effect |
|---|---|---|---|
| `prompt_cache` | `enabled` / `disabled` (case-insensitive) or `true` / `false` | `enabled` | `disabled` or `false` emits no marker anywhere |
| `prompt_cache_ttl` | `5m` / `1h` (case-insensitive) | `5m` | `1h` puts the 1-hour TTL on every marker |

```yaml
inference:
  driver:
    config:
      prompt_cache: enabled
      prompt_cache_ttl: 1h
```

Neither key errors or warns. Any other value — an unrecognised string, a number,
a missing key, or driver config that is not valid JSON — falls back to the
default. Only the exact value `disabled` or `false` turns caching off, so a typo
cannot silently disable it.

**The `system` field changes shape with caching on.** It is sent as a
one-element array of text blocks so a marker has a block to sit on:

```json
"system": [
  {"type": "text", "text": "You are helpful", "cache_control": {"type": "ephemeral"}}
]
```

With `prompt_cache: disabled` it is a bare JSON string, which is the escape
hatch for an Anthropic-compatible gateway that rejects `cache_control`, and for
a capsule that supplies its own marker through `inference.driver.config.params`
— the driver never inspects or reconciles `params`.

Behaviour worth knowing:

| Situation | What happens |
|---|---|
| Prefix shorter than the model's minimum cacheable length | The marker is silently a no-op; `cache_write_tokens` stays `0`. The driver applies no length heuristic of its own. |
| Every marker in one request | Carries the same TTL, so a longer-TTL entry can never follow a shorter-TTL one. |
| A turn that appends more than 20 content blocks | Anthropic looks back at most 20 blocks for a prior entry, so the next request rewrites the conversation instead of reading it. |
| The first turn after a compaction | Marker 3 misses and writes a fresh entry; markers 1 and 2 still hit, because compaction changes neither the tools nor the system prompt. |

A cache read costs about 0.1x the base input price; a cache write costs 1.25x at
the 5-minute TTL and 2x at the 1-hour TTL. Two requests over the same prefix
break even at `5m`, three at `1h` — which is why `5m` is the default. Reading an
entry refreshes its timer at no cost.

Prompt caching is generally available, so the driver adds no `anthropic-beta`
header for it.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | Anthropic Messages field |
|---|---|
| `input_tokens` | `usage.input_tokens` |
| `output_tokens` | `usage.output_tokens` |
| `cached_tokens` | `usage.cache_read_input_tokens` |
| `cache_write_tokens` | `usage.cache_creation_input_tokens` |

While streaming, the counts are seeded from `message_start` and each member a
later `message_delta` reports replaces the seeded value — so the cumulative
`output_tokens` wins over the placeholder `message_start` carries.

Each member is independently optional. A count the provider did not report is
omitted rather than sent as `0`; a reported `0` is kept as `0`. When no member
survives, the response carries no `usage` key at all.

## Prompt cache key

Murmur puts a `prompt_cache_key` on every driver request. This driver drops it:
the Messages API rejects a body carrying any field it does not define, and it
defines no cache-key field. The value reaches the provider body at no nesting
level. It is unrelated to the `cache_control` markers above, which are how this
driver caches a prefix.
