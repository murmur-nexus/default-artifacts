# driver-deepseek

DeepSeek inference driver for Murmur agent capsules — `deepseek-v4-flash` and
`deepseek-v4-pro`, with thinking mode.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the DeepSeek API,
including SSE streaming.

The driver reads no provider key and sends no authentication header. Murmur's
runtime authenticates each call with the header declared under
`upstream_auth:` in [murmur.yaml](./murmur.yaml), the full manifest; the
capsule author supplies the key as `gateway.api_key` on the driver's
`artifacts:` entry, as a reference such as `${DEEPSEEK_API_KEY}`. An entry that
sets neither `api_key` nor `keyless: true` fails the launch with
`E-CAP-018`.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable.
The runtime sets it to its own loopback gateway plus the path of the entry's
`gateway.endpoint`, and readdresses each request to the real upstream. The
driver carries no provider URL of its own to fall back on: a value that never
arrives fails the call with `driver: missing MURMUR_INFERENCE_ENDPOINT`, and one
that arrives empty or whitespace-only with `driver: MURMUR_INFERENCE_ENDPOINT is
set but empty`. Neither reaches the network. Requests go to
`<endpoint>/chat/completions`.

## Model scope

The driver accepts exactly `deepseek-v4-flash` and `deepseek-v4-pro`
(`SUPPORTED_MODELS` in `src/lib.rs`). `deepseek-chat` and `deepseek-reasoner`
are refused as deprecated, and every other id as unsupported, before a request
is built. DeepSeek now names its flash model `deepseek-flash`; it still accepts
`deepseek-v4-flash`, a retired name whose requests DeepSeek serves with
DeepSeek-V4.1-Flash. The driver does not accept `deepseek-flash`.

## Model limits

| Model | max_output_tokens | context_window | Source |
|---|---|---|---|
| `deepseek-v4-flash` | 393216 | 1048576 | Read 2026-10-23. <https://api-docs.deepseek.com/quick_start/pricing> gives "CONTEXT LENGTH 1M" and "MAX OUTPUT MAXIMUM: 384K", and says `deepseek-v4-flash` requests "are served by the DeepSeek-V4.1-Flash-1017 model". Exact integers: <https://api-docs.deepseek.com/api/create-chat-completion>, `max_tokens`: "The value must be between 1 and 384K (393216)"; <https://api-docs.deepseek.com/api/list-models>, example response for DeepSeek-V4.1-Flash: `"context_window": 1048576`, `"max_output_tokens": 393216`. The window counts input and output together |
| `deepseek-v4-pro` | 393216 | 1048576 | Read 2026-10-23. <https://api-docs.deepseek.com/quick_start/pricing> gives "CONTEXT LENGTH 1M" and "MAX OUTPUT MAXIMUM: 384K". Exact integers: <https://api-docs.deepseek.com/api/create-chat-completion>, `max_tokens`: "The value must be between 1 and 384K (393216)"; <https://api-docs.deepseek.com/api/list-models>, example response for `deepseek-v4-pro`: `"context_window": 1048576`, `"max_output_tokens": 393216`. The window counts input and output together |

`murmur.yaml` declares the same numbers under `models:`, and a test fails if the
two disagree or if `models:` holds any id other than those in `SUPPORTED_MODELS`.
The driver itself keeps no limits and checks no ceiling.

DeepSeek counts output against the window: "The total length of input tokens and
generated tokens is limited by the model's context length." With the default cap
of `393216`, the input that still leaves room for a full-length reply is
`1048576 − 393216 = 655360` tokens. Compaction fires when occupancy crosses
`inference.compaction.threshold` (default `0.98`) of the window, so at the
default a long session can reach DeepSeek's limit first. A capsule that runs
that long sets `inference.compaction.threshold` or `context.max_tokens` lower.

Whether the runtime reads `models:` depends on the murmur release:

| murmur | What it does with `models:` | What the capsule sets |
|---|---|---|
| v0.7.0 and earlier | ignores it; an unknown manifest key is a `W-SEC-019` warning, never a refusal | `inference.max_tokens` and `context.max_tokens`; left unset, the runtime's own defaults apply, not the models' limits |
| the first release after v0.7.0 | defaults the output cap and context window from it when the capsule leaves them unset, and caps a hook's `run-inference` (e.g. `hook-compact`) at its `max_output_tokens` | nothing, unless it wants tighter limits than the table |

A capsule value always wins: the runtime never clamps or raises
`inference.max_tokens` or `context.max_tokens` to the table.

## Models without declared limits

None. `SUPPORTED_MODELS` and `models:` hold the same ids, so every model this
driver accepts has declared limits.

Any other id, such as `deepseek-flash`, has no entry. The runtime checks for one
at launch, before the driver sees a request:

| Capsule sets | Result |
|---|---|
| neither `inference.max_tokens` nor `context.max_tokens` | the launch is refused with `E-RUN-052` |
| only `inference.max_tokens` | it launches with `W-RUN-009`, and compaction does not run |
| both | it launches |

In each case that launches, the driver then refuses the first inference call as
an unsupported or deprecated model.

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

A failed task status and a `task_failed` trace line with cause `driver_error`
need a murmur runtime released after v0.4.0. On v0.4.0 and earlier the runtime
records the error but still reports the task as `ok` and exits 0.

## A tool call index past 1024 is refused

A turn holds at most 1024 tool calls: a streamed chunk numbers each call with an
`index`, and one naming index 1024 or more fails the turn with
`driver: tool call index <n> is past the limit of 1024 per turn` before anything
is stored for it or the call is reported. A chunk whose call has no `index` is
read as index 0.

## Tool-call progress

While the model is still writing a tool call, the driver reports it to the host through
`murmur:stream/events@0.1.0`:

| Call | Sent on | Carries |
|---|---|---|
| `tool-call-started` | the first `delta.tool_calls[]` fragment by which the call's `id` and `function.name` have both arrived | `tool_calls[].id` and the function name, which the returned `tool_call` carries too |
| `tool-call-input-bytes` | each fragment that grows the call's `function.arguments` | the call's argument bytes so far |

The size is a running UTF-8 byte total of the call's argument text. It only rises, and its last
value is the byte length of the text parsed into the returned `input`. No argument text is ever
forwarded: the host learns a call's id, name and size, and the arguments reach it only in the
returned tool call. On `transport: http`, the host writes these as `tool-call-started` and
`tool-call-progress` frames ahead of the call's `artifact`.

A call whose `id` the provider has not sent yet is not reported. Once it arrives, the start and the
size so far go out together. A call that never gets an id is not reported at all, and is returned
as before. `reasoning_content` and `content` stream exactly as before.

The driver imports `murmur:stream/events@0.1.0`, which murmur serves from v0.6.0; an older murmur
cannot load it. `mur` 0.7.0 or later refuses a release of this driver built before that
interface (0.7.0 and earlier) at launch with `E-WIT-LEVEL-001`: it imports
`murmur:text/chunks@0.1.0`, and WIT level 1 has no version of `murmur:text`.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | DeepSeek field |
|---|---|
| `input_tokens` | `usage.prompt_cache_miss_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_cache_hit_tokens` |
| `cache_write_tokens` | not reported by DeepSeek — always absent |

`input_tokens` is the request tokens not served from the cache, and
`cached_tokens` sits beside it, so `input_tokens + cached_tokens` is DeepSeek's
total request count: DeepSeek documents `usage.prompt_tokens` as the sum of its
cache-hit and cache-miss counts, and the driver reports those two parts directly.
A cache miss is ordinary input, not a cache write.

Each member is independently optional. A count the provider did not report is
omitted rather than sent as `0`; a reported `0` is kept as `0`. When no member
survives, the response carries no `usage` key at all.

Requests are sent with `stream_options: {"include_usage": true}`, without which
DeepSeek streams no counts.

## Prompt cache key

Murmur puts a `prompt_cache_key` on every driver request. This driver drops it:
DeepSeek's context cache is automatic and its API defines no cache-key field.
The value reaches the provider body at no nesting level.
