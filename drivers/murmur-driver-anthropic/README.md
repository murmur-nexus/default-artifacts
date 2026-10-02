# murmur-driver-anthropic

Anthropic Messages API inference driver for Murmur agent capsules.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the Anthropic
Messages API, including SSE streaming, extended-thinking blocks, prompt-cache
breakpoints, and model-family handling (Claude 3.x vs Claude 4+ naming and
parameter rules).

The driver reads no provider key and sends no authentication header. Murmur's
runtime authenticates each call with the header declared under
`upstream_auth:` in [murmur.yaml](./murmur.yaml), the full manifest; the
capsule author still supplies the key as `inference.api_key`.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable,
resolved from the capsule manifest's `inference.endpoint`. That field is
required, and the driver carries no provider URL of its own to fall back on: a
value that never arrives fails the call with `driver: missing
MURMUR_INFERENCE_ENDPOINT`, and one that arrives empty or whitespace-only with
`driver: MURMUR_INFERENCE_ENDPOINT is set but empty`. Neither reaches the
network. A usable value is trimmed and used exactly as given.

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
`config:` on this driver's entry in the capsule's `artifacts:` list, which the
runtime lowers to compact JSON and delivers to this artifact alone as
`MURMUR_ARTIFACT_CONFIG`:

| Key | Accepted | Default | Effect |
|---|---|---|---|
| `prompt_cache` | `enabled` / `disabled` (case-insensitive) or `true` / `false` | `enabled` | `disabled` or `false` emits no marker anywhere |
| `prompt_cache_ttl` | `5m` / `1h` (case-insensitive) | `5m` | `1h` puts the 1-hour TTL on every marker |

```yaml
artifacts:
  - name: murmur-driver-anthropic
    runtime: driver
    config:
      prompt_cache: enabled
      prompt_cache_ttl: 1h
```

Neither key errors or warns. Any other value — an unrecognised string, a number,
a missing key, an entry with no `config:` block at all, or config that is not
valid JSON — falls back to the default. Only the exact value `disabled` or
`false` turns caching off, so a typo cannot silently disable it.

These two keys are the only ones read from `MURMUR_ARTIFACT_CONFIG`; everything
else this driver takes — extended thinking, `anthropic-beta` features, `params`
— is read from `inference.driver.config`. Caching belongs on the artifact
because a `cache_control` breakpoint is an Anthropic construct: the other
drivers have nowhere to put one, since OpenAI and DeepSeek cache a prefix
automatically with no marker in the request.

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

## Stop reasons

| Anthropic `stop_reason` | murmur `stop_reason` |
|---|---|
| `end_turn` | `end_turn` |
| `stop_sequence` | `end_turn` |
| `tool_use` | `tool_call` |
| `max_tokens` | `max_tokens` |
| `refusal` | `error`, with `Anthropic response refused` |

Any other value, `pause_turn` included, is refused with
`driver: unsupported Anthropic stop_reason '<value>'`. So is `""` in a JSON
body; in a stream an empty `stop_reason` is not recorded, so the stream ends
with no stop reason (see below).

### Failed turns

A turn the driver cannot report as finished is returned as
`{"stop_reason":"error","error":"<message>"}`, so the task ends failed with
cause `driver_error`. The message says which of three kinds of failure it was:

| Kind | `error` message | Example |
|---|---|---|
| Translation failure: the response cannot be read as a finished Messages API turn | starts with `driver: ` | `driver: malformed Anthropic stream: tool_use block 1 has no id` |
| Provider error | starts with `HTTP <status>: `, or with `Anthropic error: <type>: <message>` for an error object in a 2xx JSON body or an SSE `event: error` | `Anthropic error: overloaded_error: Overloaded` |
| Refusal | exactly `Anthropic response refused` | `stop_reason: "refusal"` |

In `Anthropic error: <type>: <message>`, a part the error object does not carry
as a string reads `unknown`. Reading a stream stops at its `event: error`, so no
later text is shown.

A failed task status and a `task_failed` trace line with cause `driver_error`
need a murmur runtime released after v0.4.0. On v0.4.0 and earlier the runtime
records the error but still reports the task as `ok` and exits 0.

The translation failures:

| Response | Refused with |
|---|---|
| JSON body whose `stop_reason` is absent, `null` or not a string, such as a chat-completions body | `driver: Anthropic response has no stop_reason` |
| Body that holds lines but no SSE `event:` line, such as a chat-completions stream | `driver: response is not an Anthropic event stream` |
| SSE stream that ends before any `message_delta` carries a non-empty `stop_reason`: a dropped connection, or an empty body | `driver: Anthropic stream ended with no stop_reason` |
| JSON body missing a field that changes what the turn means | `driver: malformed Anthropic response: <what is missing>` |
| SSE event missing a field that changes what the turn means | `driver: malformed Anthropic stream: <what is missing>` |
| Streamed tool input that does not parse, on a turn stopped by `max_tokens` | `driver: Anthropic turn stopped at the inference.max_tokens output cap; tool call '<name>' was cut off mid-input and cannot be run — raise the cap and re-run` |

A JSON body must carry a `content` array. Each block must carry a string `type`;
a `text` block its `text`; a `tool_use` block its `id`, `name` and an object
`input`; a `thinking` block its `thinking`.

Every line of a stream must be UTF-8, as a JSON body must be. In a stream, the
driver reads the data of `message_start`,
`content_block_start`, `content_block_delta` and `message_delta`, and that data
must be JSON. A block event must carry an integer `index`; a block start its
block `type`, and a `tool_use` start its `id` and `name`; a delta its `type`,
for a block that started, of the matching kind, with a string payload. Streamed
tool input must parse to a JSON object.

No message includes any part of the response body or of a tool call's input.
Text already streamed through `murmur:stream/events` before a failure has been
shown and is not withdrawn; it is just not recorded as a finished reply.

### Not refused

- `content: []` with `stop_reason: "end_turn"`: a successful empty turn.
- A tool call with no arguments: `input: {}`, or a streamed `tool_use` block
  with no `input_json_delta` or only empty ones.
- A `thinking` block with no `signature`. It is kept with `signature: ""`, and
  never replayed on the next request. `thinking` blocks are kept on the JSON
  fallback as well as on the SSE path.
- A `redacted_thinking` block, and any block type the driver does not know:
  dropped.
- An SSE event name or delta type the driver does not know, such as
  `citations_delta`: ignored.
- `ping`, `content_block_stop` and `message_stop` data that is not JSON: never
  read.
- Absent or malformed `usage`: the turn reports no usage.
- A stream whose `message_delta` carried a stop reason but that ended before
  `message_stop`: the reply is already complete at that point.

## Prompt cache key

Murmur puts a `prompt_cache_key` on every driver request. This driver drops it:
the Messages API rejects a body carrying any field it does not define, and it
defines no cache-key field. The value reaches the provider body at no nesting
level. It is unrelated to the `cache_control` markers above, which are how this
driver caches a prefix.
