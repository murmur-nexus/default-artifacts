# murmur-driver-anthropic

Anthropic Messages API inference driver for Murmur agent capsules.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the Anthropic
Messages API, including SSE streaming, extended-thinking blocks, and
model-family handling (Claude 3.x vs Claude 4+ naming and parameter rules).

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. See [murmur.yaml](./murmur.yaml) for the full manifest.

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
level.
