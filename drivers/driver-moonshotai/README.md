# driver-moonshotai

Moonshot AI inference driver for Murmur agent capsules — `kimi-k3` on the Chat
Completions surface.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and Moonshot's
OpenAI-shaped Chat Completions API, including SSE streaming, a full tool-calling
round trip and preserved reasoning across turns.

The driver reads no provider key and sends no authentication header. Murmur's
runtime authenticates each call with the header declared under
`upstream_auth:` in [murmur.yaml](./murmur.yaml), the full manifest; the
capsule author supplies the key as `gateway.api_key` on the driver's
`artifacts:` entry.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable.
The runtime sets it to its own loopback gateway plus the path of the entry's
`gateway.endpoint`, and readdresses each request to the real upstream. The
driver carries no provider URL of its own to fall back on: a value that never
arrives fails the call with `driver: missing MURMUR_INFERENCE_ENDPOINT`, and one
that arrives empty or whitespace-only with `driver: MURMUR_INFERENCE_ENDPOINT is
set but empty`. Neither reaches the network. Requests go to
`<endpoint>/chat/completions`.

## Using it from a capsule manifest

```yaml
artifacts:
  - name: murmur/driver-moonshotai
    version: "0.11.0"
    runtime: driver
    gateway:
      endpoint: https://api.moonshot.ai/v1
      api_key: ${MOONSHOT_API_KEY}

inference:
  transport: http
  model: kimi-k3
  driver:
    artifact: murmur/driver-moonshotai
```

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — (must be `kimi-k3`) |
| `gateway.endpoint` | yes | — |
| `gateway.api_key` | yes, unless the entry declares `keyless: true` | — without either, the launch fails with `E-CAP-018` |
| `inference.max_tokens` | no | `1048576`, from `models:` — see [Model limits](#model-limits); ceiling `1048576` |
| `context.max_tokens` | no | `1048576`, from `models:` |
| `inference.driver.config` | no | `{}` — see below |

## Model scope

`kimi-k3` is the only model this driver accepts, and the check runs before a
request body is built, so an unsupported model costs no HTTP call. Moonshot also
ships `kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed`, `kimi-k2.7-code` and
`kimi-k2.6`; none of them are implemented here. Adding one means one entry in
`SUPPORTED_MODELS`, with its limits, one entry under `models:` in
`murmur.yaml`, and one row in the table below — nothing else in the crate
branches on the model name.

## Model limits

| Model | max_output_tokens | context_window | Source |
|---|---|---|---|
| `kimi-k3` | 1048576 | 1048576 | Read 2026-10-23. Output: <https://platform.kimi.ai/docs/api/chat>, `max_completion_tokens`: "for Kimi K3-series models (kimi-k3.1-pro, kimi-k3.1-pro-highspeed, kimi-k3) it defaults to 131072 and can be set up to 1048576". Window: <https://platform.kimi.ai/docs/pricing/chat>, "All K3 series models support a 1,048,576-token context window." The input alone "must be less than the model context window size" |

The limits live in one table, `SUPPORTED_MODELS` in `src/lib.rs`, and
`murmur.yaml` declares the same numbers under `models:`. A test fails if the two
disagree, or if this table disagrees with `murmur.yaml`.

`max_output_tokens` equals the whole window, and that is safe as the default
cap: when `prompt_tokens` plus `max_completion_tokens` exceeds the window,
Moonshot lowers the generation limit for that request to fit the remaining
context and serves it.

Compaction fires when occupancy crosses `inference.compaction.threshold`
(default `0.98`) of the window. Moonshot refuses a prompt that alone fills the
window, so a session that would reach that limit before compaction fires sets
`inference.compaction.threshold` or `context.max_tokens` lower.

Whether the runtime reads `models:` depends on the murmur release:

| murmur | What it does with `models:` | What the capsule sets |
|---|---|---|
| v0.7.0 and earlier | ignores it; an unknown manifest key is a `W-SEC-019` warning, never a refusal | `inference.max_tokens` and `context.max_tokens`; left unset, the runtime's own defaults apply, not `kimi-k3`'s limits |
| the first release after v0.7.0 | defaults the output cap and context window from it when the capsule leaves them unset, and caps a hook's `run-inference` (e.g. `hook-compact`) at its `max_output_tokens` | nothing, unless it wants tighter limits than the table |

A capsule value always wins: the runtime never clamps or raises
`inference.max_tokens` or `context.max_tokens` to the table.

## Models without declared limits

None. `SUPPORTED_MODELS` and `models:` hold the same ids, so every model this
driver accepts has declared limits.

Any other id, such as `kimi-k3.1-pro`, has no entry. The runtime checks for one at
launch, before the driver sees a request:

| Capsule sets | Result |
|---|---|
| neither `inference.max_tokens` nor `context.max_tokens` | the launch is refused with `E-RUN-052` |
| only `inference.max_tokens` | it launches with `W-RUN-009`, and compaction does not run |
| both | it launches |

In each case that launches, the driver then refuses the first inference call:
`driver: model '<m>' is not supported by driver-moonshotai. Supported models: kimi-k3.`

## Config keys

`inference.driver.config` accepts exactly two keys. Every other key is refused
by name on the first inference call, before any HTTP request is dispatched.

| Key | Required | Default | Accepted values | Validated or forwarded | Provider body field |
|---|---|---|---|---|---|
| `reasoning_effort` | no | `max` | `low`, `high`, `max` — trimmed, matched case-insensitively | validated | top-level `reasoning_effort` |
| `response_format` | no | absent | an object with `type: json_schema` and `json_schema.schema` present | validated, then forwarded | `response_format` |

### `reasoning_effort`

Moonshot's own vocabulary, sent lowercased and top-level on the body — not
nested under a `reasoning` object. `medium` is OpenAI's spelling and fails here,
with the rejected value quoted back alongside the accepted set. There is no
murmur-side normalisation across providers' effort dials; each driver owns its
provider's vocabulary.

**`max` is the expensive end, and it is what an operator gets by saying
nothing.** That is Moonshot's own default, kept rather than substituted with a
cheaper tier so this driver does not quietly disagree with the provider about
what an unconfigured request means. It is stamped explicitly on every request,
including the unconfigured one, so the effective value is visible in a recorded
request and cannot drift when Moonshot changes its default. A capsule that wants
to spend less writes `reasoning_effort: low`.

### `response_format`

Structured output. The value is an object forwarded to the provider body under
the same name, after a small amount of validation: it must be an object, `type`
must be `"json_schema"`, and `json_schema.schema` must be present. `strict`
defaults to `true` when absent and is normalised onto the body so the effective
value shows up in a recorded request. `strict: false` is refused by name rather
than silently upgraded — a schema the provider does not enforce is not the
feature this key offers.

```yaml
inference:
  driver:
    artifact: murmur/driver-moonshotai
    config:
      response_format:
        type: json_schema
        json_schema:
          name: plan
          schema:
            type: object
            properties:
              steps: { type: array }
```

The schema constrains the final `content` only. `reasoning_content` is never
parsed as the structured payload: it becomes a `thinking` block exactly as it
does without a schema, and the JSON payload lands in the `text` block.

Structured output is a config key rather than an inference parameter because
the host pins the canonical envelope's `params` to `{}` and nothing writes into
it, so a capsule manifest has no way to reach the provider body through `params`.

### Why there is no `thinking` key

`kimi-k3` always reasons and Moonshot exposes no switch to turn that off, so a
`thinking` key here would be a setting with nothing behind it. It is refused
with its own message — distinct from the generic unrecognised-key text — naming
the key and saying why. `thinking: disabled` is refused too: a key that cannot
be honoured is refused whatever its value.

This is a deliberate divergence from `driver-anthropic` and
`driver-deepseek`, both of which read a real `thinking` toggle. If
`kimi-k2.6` — the one Moonshot model with a genuine thinking/non-thinking
toggle — is ever added to this driver, the key becomes meaningful and **must
then be spelled `thinking`**, matching those two drivers. Do not re-decide the
spelling at that point.

## Fixed parameters

Moonshot fixes `temperature=1.0`, `top_p=0.95`, `n=1`, `presence_penalty=0` and
`frequency_penalty=0` server-side. This driver omits all five from every
request rather than forwarding them to be ignored. They are one constant
consulted by the one `params` pass-through loop.

## The output cap

The canonical envelope's `max_tokens` is written to the body as
`max_completion_tokens`, which is what Moonshot's Chat Completions surface
spells it; no `max_tokens` member is emitted. The ceiling is the model's
`max_output_tokens` in `SUPPORTED_MODELS`, `1048576` for `kimi-k3`, and a larger
`inference.max_tokens` is refused pre-flight, naming the field and the ceiling,
rather than clamped silently or spent on a provider 400. Moonshot's own default
is `131072`, but the runtime always sends a cap — the capsule's
`inference.max_tokens`, or else the `max_output_tokens` under `models:` — so the
driver never substitutes one.

## Preserved reasoning across turns

Moonshot asks for the assistant message to come back complete on multi-turn
conversations and tool calls, which means the turn's `reasoning_content` has to
survive the round trip. It does so entirely inside this driver: the driver emits
the provider's `reasoning_content` as a `{"type":"thinking","text":…}` content
block, the runtime stores and replays whatever content blocks a driver emits
without inspecting them, and on the next call the driver reads that block back
and reattaches it as `reasoning_content` on the outgoing assistant message. No
runtime concept, no interface change and no other artifact is involved.

A committed compaction replaces the live context with a summary that crosses the
hook boundary as a single `text` block, so historical assistant messages carry
no `thinking` block afterwards. That is the ordinary post-compaction state, not
an error: the `reasoning_content` member is omitted entirely — never sent as an
empty string — and the request goes out normally.

## Streaming

Requests are always sent with `stream: true` and
`stream_options: {"include_usage": true}`, stamped together and overriding
anything from `params`. SSE lines are parsed incrementally:
`delta.reasoning_content` goes to `emit_thinking_chunk`, `delta.content` to
`emit_chunk`, and `delta.tool_calls` is accumulated by `index` into id, name and
argument fragments, reporting each call's start and size to the host as it grows (see
[Tool-call progress](#tool-call-progress)). `data: [DONE]` ends the stream.

A non-streaming JSON fallback is selected by sniffing the first non-whitespace
byte of the response body for `{` or `[`, which is where scripted test servers
and any provider that ignores `stream: true` land.

The assembled response's `content` puts the `thinking` block first, then either
the `tool_call` blocks or the `text` block. Thinking-first is what makes the
tool-call round trip work and what the UI renders separately.

Moonshot delivers reasoning on its own delta field, so there is no inline
reasoning-tag scanner anywhere in this crate.

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
as before.

The driver imports `murmur:stream/events@0.1.0`, which murmur serves from v0.6.0; an older murmur
cannot load it. `mur` 0.7.0 or later refuses a release of this driver built before that
interface (0.6.0 and earlier) at launch with `E-WIT-LEVEL-001`: it imports
`murmur:text/chunks@0.1.0`, and WIT level 1 has no version of `murmur:text`.

## Stop reasons

| Moonshot `finish_reason` | Murmur `stop_reason` |
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
| JSON body whose first choice has no `finish_reason` | `driver: Moonshot response has no finish_reason` |
| SSE stream in which no chunk carries a `finish_reason`, such as a connection dropped before the final chunk | `driver: Moonshot stream ended with no finish_reason` |

The refusal is returned as `{"stop_reason":"error","error":"<message>"}`, so the
task ends failed. Text already streamed before the stream ended is not
withdrawn; it is just not recorded as a finished reply.

A failed task status and a `task_failed` trace line with cause `driver_error`
need a murmur runtime released after v0.4.0. On v0.4.0 and earlier the runtime
records the error but still reports the task as `ok` and exits 0.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | Moonshot field |
|---|---|
| `input_tokens` | `usage.prompt_tokens` − `cached_tokens` − `cache_write_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `cache_write_tokens` | `usage.prompt_tokens_details.cache_write_tokens` |

`input_tokens` is the request tokens neither read from nor written to the cache;
`cached_tokens` and `cache_write_tokens` sit beside it, so
`input_tokens + cached_tokens + cache_write_tokens` is Moonshot's total request
count. Moonshot reports that total with both cache counts inside it, so the
driver subtracts them once, after merging every streamed chunk; an unreported
cache count is subtracted as `0`. A report whose cache counts exceed its total is
inconsistent: `input_tokens` is omitted rather than invented, and the other
members are still reported. The top-level `usage.cached_tokens` some responses
also carry is not read.

Each member is independently optional. A count the provider did not report is
omitted rather than sent as `0`; a reported `0` is kept as `0`. When no member
survives, the response carries no `usage` key at all. Streaming usage arrives
across more than one chunk and is merged; the usage-bearing final chunk has an
empty `choices` array, so it is read before any `choices`-shaped early return.

## Prompt cache key

Murmur puts a reserved top-level `prompt_cache_key` on every driver request.
This driver drops it: Moonshot's context caching is automatic and its Chat
Completions API defines no cache-key field. The field is not declared on the
request struct at all, so serde discards it and it reaches the provider body at
no nesting level.

## API surfaces this driver declines

Moonshot exposes three compatible surfaces. Only Chat Completions is
implemented, and there is no surface router — one surface needs none.

| Surface | Status | Why |
|---|---|---|
| Chat Completions | implemented | `reasoning_content` is specified against it |
| Responses | declined | duplicates every feature Chat Completions carries, and is not the surface `reasoning_content` is specified against |
| Messages | declined | Anthropic-shaped; using it would mean translating into an Anthropic body to reach an OpenAI-shaped provider |

## Errors

Every fallible function returns a message opening `driver: `, and nothing
panics. The `run` wrapper turns an error into a `ToolResult` with
`status: error`, the message as its summary, and
`{"stop_reason":"error","error":…}` as its data. A provider status `>= 400`
produces the same shape carrying `HTTP <status>: <body>`.

A turn holds at most 1024 tool calls: a streamed chunk numbers each call with an
`index`, and one naming index 1024 or more fails the turn with
`driver: tool call index <n> is past the limit of 1024 per turn` before anything
is stored for it or the call is reported. A chunk whose call has no `index` is
read as index 0.
