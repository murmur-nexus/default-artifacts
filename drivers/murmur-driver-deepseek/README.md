# murmur-driver-deepseek

DeepSeek inference driver for Murmur agent capsules — `deepseek-v4-flash` and
`deepseek-v4-pro`, with thinking mode.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the DeepSeek API,
including SSE streaming.

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

## Stop reasons

| DeepSeek `finish_reason` | Murmur `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `tool_calls` | `tool_call` |
| `length` | `max_tokens` |
| anything else | an error naming the unmapped reason |

A turn that never says why it stopped is refused rather than read as
`end_turn`. A `finish_reason` that is absent, `null`, not a string or empty
counts as none:

| Response | Refused with |
|---|---|
| JSON body whose first choice has no `finish_reason` | `driver: DeepSeek response has no finish_reason` |
| SSE stream in which no chunk carries a `finish_reason`, such as a connection dropped before the final chunk | `driver: DeepSeek stream ended with no finish_reason` |

The refusal is returned as `{"stop_reason":"error","error":"<message>"}`, so the
task ends failed. Text already streamed before the stream ended is not
withdrawn; it is just not recorded as a finished reply.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | DeepSeek field |
|---|---|
| `input_tokens` | `usage.prompt_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens`, else `usage.prompt_cache_hit_tokens` |
| `cache_write_tokens` | not reported by DeepSeek — always absent |

`usage.prompt_cache_miss_tokens` counts input that missed the cache, not input
written into it, and is never mapped anywhere.

Each member is independently optional. A count the provider did not report is
omitted rather than sent as `0`; a reported `0` is kept as `0`. When no member
survives, the response carries no `usage` key at all.

Requests are sent with `stream_options: {"include_usage": true}`, without which
DeepSeek streams no counts.

## Prompt cache key

Murmur puts a `prompt_cache_key` on every driver request. This driver drops it:
DeepSeek's context cache is automatic and its API defines no cache-key field.
The value reaches the provider body at no nesting level.
