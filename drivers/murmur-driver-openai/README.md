# murmur-driver-openai

OpenAI inference driver for Murmur agent capsules — Chat Completions, with the
Responses API for `gpt-5` and later models.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the OpenAI API,
including SSE streaming. Optional stateful continuation via
`previous_response_id` is gated behind an explicit `inference.driver.config`
store grant.

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. See [murmur.yaml](./murmur.yaml) for the full manifest.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | Chat Completions | Responses |
|---|---|---|
| `input_tokens` | `usage.prompt_tokens` | `usage.input_tokens` |
| `output_tokens` | `usage.completion_tokens` | `usage.output_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` | `usage.input_tokens_details.cached_tokens` |
| `cache_write_tokens` | not reported by OpenAI — always absent | not reported by OpenAI — always absent |

Each member is independently optional. A count the provider did not report is
omitted rather than sent as `0`; a reported `0` is kept as `0`. When no member
survives, the response carries no `usage` key at all. An error response carries
no `usage`.

Chat Completions requests are sent with
`stream_options: {"include_usage": true}`, without which the provider streams no
counts. The Responses surface reports usage on `response.completed` and needs no
opt-in.

## Prompt cache key

Murmur puts a `prompt_cache_key` on every driver request — one value per task,
constant across its turns. This driver copies it verbatim into the provider body
under the same name, on both surfaces and on the continuation path, so a task's
turns route to the machine holding the previous turn's cache entry.

| Value Murmur sends | Body |
|---|---|
| non-blank string | `prompt_cache_key`, trimmed |
| absent, `null`, empty, whitespace-only, or non-string | no `prompt_cache_key` member |

A `prompt_cache_key` set by the capsule author under `params` is overridden by
the value Murmur supplies.
