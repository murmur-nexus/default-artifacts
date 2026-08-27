# murmur-driver-deepseek

DeepSeek inference driver for Murmur agent capsules — `deepseek-v4-flash` and
`deepseek-v4-pro`, with thinking mode.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the DeepSeek API,
including SSE streaming.

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. See [murmur.yaml](./murmur.yaml) for the full manifest.

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
