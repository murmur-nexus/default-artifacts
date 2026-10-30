# driver-baseten

Baseten Model APIs inference driver for Murmur agent capsules —
`zai-org/GLM-5.3` on the Chat Completions surface.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and Baseten's
OpenAI-compatible Chat Completions API, including SSE streaming, a full
tool-calling round trip, reasoning sent back on later turns unless a capsule
turns it off, and Baseten's session affinity.

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
`<endpoint>/chat/completions`, with any trailing `/` on the endpoint dropped
first.

## Using it from a capsule manifest

```yaml
artifacts:
  - name: murmur/driver-baseten
    version: "0.4.0"
    runtime: driver
    gateway:
      endpoint: https://inference.baseten.co/v1
      api_key: ${BASETEN_API_KEY}

inference:
  transport: http
  model: zai-org/GLM-5.3
  driver:
    artifact: murmur/driver-baseten
    config:
      reasoning_effort: max
```

A capsule running GLM 5.3 on `driver-zai` moves to Baseten by changing this
entry, `inference.model` and `inference.driver.artifact`. Nothing else in the
manifest changes.

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — (must be `zai-org/GLM-5.3`) |
| `gateway.endpoint` | yes | — |
| `gateway.api_key` | yes, unless the entry declares `keyless: true` | — without either, the launch fails with `E-CAP-018` |
| `inference.max_tokens` | no | from the host; ceiling `262144` |
| `inference.driver.config` | no | `{}` — see below |

## Model scope

`zai-org/GLM-5.3` is the only model this driver accepts. The id is matched
exactly, with no case folding and no prefix match, so `zai-org/glm-5.3`,
`glm-5.3`, and Baseten's own `zai-org/GLM-5.3-Fast` and `zai-org/GLM-5.3-Flash`
are all refused. The check runs before a request body is built, so an
unsupported model costs no HTTP call.

| Model | `max_output_tokens` | `context_window` | `max_input_tokens` |
|---|---|---|---|
| `zai-org/GLM-5.3` | `262144` | `1048576` | `none` |

| Limit | Source, read 2026-10-24 |
|---|---|
| `max_output_tokens: 262144` | the Model APIs overview's supported-models table gives a max output of `262k`, and the Chat Completions reference gives `max_tokens` a `maximum: 262144` |
| `context_window: 1048576` | the Model APIs overview's supported-models table gives a context of `1048k` |
| `max_input_tokens`: none | Baseten documents no input ceiling separate from the window. When input plus `max_tokens` exceeds the window, it truncates `max_tokens` rather than refusing the request |

The limits live in one table, `SUPPORTED_MODELS` in `src/lib.rs`, and
`murmur.yaml` declares the same numbers under `models:`. A test fails if the two
disagree, or if the row above loses either number.

murmur compacts against one input budget per call:
`min(max_input_tokens, context_window − output cap)`, which here is
`context_window − output cap`. `inference.compaction.threshold` and
`context.seed_budget` are fractions of that budget. With the default cap of
`262144`, the budget is `1048576 − 262144 = 786432` tokens. An
`inference.max_tokens` the capsule sets changes the `window − output cap` term.

Whether the runtime reads that block depends on the murmur release:

| murmur | What it does with `models:` | What the capsule sets |
|---|---|---|
| v0.7.0 and earlier | ignores it; an unknown manifest key is a `W-SEC-019` warning, never a refusal | `inference.max_tokens` and `context.max_tokens`; left unset, the runtime's own defaults apply, not `zai-org/GLM-5.3`'s limits |
| the first release after v0.7.0 | defaults the output cap and context window from it when the capsule leaves them unset, reads `max_input_tokens` as the input ceiling, measures compaction and the seed budget against the input budget, and caps a hook's `run-inference` (e.g. `hook-compact`) at its `max_output_tokens` | nothing, unless it wants tighter limits than the table |

## Config keys

`inference.driver.config` accepts exactly two keys. Every other key,
`thinking`, `clear_thinking` and `chat_template_args` included, is refused by
name on the first inference call, before any HTTP request is dispatched.

| Key | Required | Default | Accepted values | Provider body field |
|---|---|---|---|---|
| `reasoning_effort` | no | `high` | `low`, `high`, `max`, trimmed and matched case-insensitively | top-level `reasoning_effort` |
| `replay_reasoning` | no | `true` | `true`, `false`, as a JSON boolean only | `chat_template_args.clear_thinking`, sent as the opposite value |

`replay_reasoning` decides whether earlier turns' reasoning goes back to
Baseten; [Reasoning across turns](#reasoning-across-turns) gives what each
setting sends and costs. A string such as `"false"`, a number or `null` is
refused rather than read as a boolean.

## How Baseten expresses effort

| Fact, from Baseten's reasoning guide | What the driver does |
|---|---|
| GLM 5.3 takes `reasoning_effort` of `none`, `low`, `high` or `max`, and defaults to `high` | accepts `low`, `high` and `max`, sent lowercased and top-level on the body |
| "`none` runs at `low`" on the GLM 5.3 models | refuses `none` with its own message, naming the model and pointing at `low`, before any HTTP call. A capsule asking for no reasoning would otherwise get `low` reasoning and pay for it |
| Reasoning is on by default for GLM 5.3. `chat_template_args.enable_thinking: false` is accepted, "but responses can still include `reasoning_content`" | never sends `enable_thinking`: there is no off switch to offer. The driver's `chat_template_args` holds only `clear_thinking`, from `replay_reasoning`, and `chat_template_args` stays refused as a config key |
| Baseten's gateway accepts `minimal`, `medium` and `xhigh` too, but the guide lists none of them for GLM 5.3 | refuses each by name, with the rejected value quoted back alongside the accepted set. It never maps one onto another tier |

**`high` is what an operator gets by saying nothing.** That is Baseten's own
default for GLM 5.3, kept rather than substituted. It is stamped explicitly on
every request, including the unconfigured one, so the effective tier is visible
in a recorded request and cannot drift when Baseten changes its default. A
capsule that wants the deepest reasoning sets `max`.

## Reasoning across turns

| Step | What happens |
|---|---|
| Response | the provider's `reasoning_content`, `delta.reasoning_content` on a stream, becomes a `{"type":"thinking","text":…}` content block |
| Storage | the runtime stores and replays whatever content blocks a driver emits without inspecting them |
| Next request | with `replay_reasoning: true`, the driver reads the turn's first non-empty `thinking` block back and sends it byte for byte as `reasoning_content` on that assistant message; with `false`, it sends none |

`replay_reasoning` sets which of these a request carries:

| `replay_reasoning` | Earlier assistant messages carry | Stamp on every request | Prompt-token cost of earlier turns |
|---|---|---|---|
| `true` (default) | their `reasoning_content`, byte for byte, beside `content` and `tool_calls` | `"chat_template_args": {"clear_thinking": false}` | each earlier turn's reasoning is sent again as prompt tokens on every later request until a compaction removes it. It is billed as input, at Baseten's cached rate where the replica still holds that prefix |
| `false` | no `reasoning_content`, on any assistant message | `"chat_template_args": {"clear_thinking": true}` | the reasoning is not sent, so earlier turns cost only their visible text, tool calls and tool results |

With `false`, the current tool loop's reasoning is dropped too: the turns
since the last user message go back without `reasoning_content`, like every
other assistant message. Under both settings the model reasons on the current
turn, and that reasoning comes back as a `thinking` block and reaches the
trace. A recorded request body always shows the setting in
`chat_template_args.clear_thinking`. An `inference` trace event's
`driver_config` holds `replay_reasoning` only when the capsule writes the key,
so an unset key, which means `true`, does not appear there.

The stamp sits in `chat_template_args` because Baseten's request object is
`additionalProperties: false`, which leaves that open member as the only place
for it, and the GLM-5.3 chat template reads a `clear_thinking` argument that
defaults to `false` and, when `true`, renders each earlier turn's reasoning
empty. `{"clear_thinking": false}` is the template's own default, so a
replaying request reaches the model as it would without the member.

On a replaying request, `reasoning_content` goes back on every assistant
message, on a turn that ended in tool calls and on a plain-text turn alike. A
tool-call turn can carry text beside its calls; the driver keeps it as a
`text` block and sends it back as the assistant message's `content`, under
both settings. `content` is `null` only on a tool-call turn with no text. The
replayed bytes are the ones the model generated, so the prompt prefix stays
equal to what the replica already holds in its cache.

Baseten documents `reasoning_content` on prior assistant messages: its
reasoning guide's "Beam conversation history" sends it back and answers a Beam
history without it with HTTP 400. **Confirmed for `zai-org/GLM-5.3` by a live
call on 2026-10-27:** a capsule run sent a tool-call turn back with its
`reasoning_content`, streamed, and Baseten answered HTTP 200, as it did for the
next request carrying two replayed turns. The task ended `end_turn`.

A turn on which the model does not reason carries no `reasoning_content`, so
nothing is replayed for it. A committed compaction replaces the live context
with a summary that crosses the hook boundary as a single `text` block, so
historical assistant messages carry no `thinking` block afterwards. That is the
ordinary post-compaction state, not an error: the `reasoning_content` member is
left out, never sent as an empty string.

## Streaming

Every request is sent with `stream: true` and
`stream_options: {"include_usage": true}`, stamped last so they override
anything from `params`. Baseten's `continuous_usage_stats` defaults to `true`,
so running usage can arrive on every chunk as well as on the final one; the
driver merges each report member by member, and the last one wins.

SSE lines are parsed incrementally: `delta.reasoning_content` goes to
`emit_thinking_chunk`, `delta.content` to `emit_chunk`, and `delta.tool_calls`
is accumulated by `index` into id, name and argument fragments. A tool call can
arrive in fragments or whole in one chunk, and the driver reads either. A
non-streaming JSON fallback is selected by sniffing the first non-whitespace
byte of the response body for `{` or `[`.

A stream counts as complete only when it ends with `data: [DONE]`. Baseten says
to "treat an explicit error event or a missing stream-end marker as a failed
request", and that "a timeout or an unavailable model can end the stream early
without an error event". So:

| Stream | Result |
|---|---|
| ends without `data: [DONE]`, whatever it carried before, a `finish_reason` included | `driver: Baseten stream ended before data: [DONE]` |
| ends with `data: [DONE]`, but no chunk carried a non-empty `finish_reason` | `driver: Baseten stream ended with no finish_reason` |
| a chunk carries a non-null top-level `error` | `driver: Baseten stream error: <error>`, the member serialised compactly |

The `[DONE]` check runs first. On both paths the response `content` is ordered:

1. the `thinking` block, when the reasoning is non-empty
2. the `text` block, when the text is non-empty
3. the `tool_call` blocks, in `index` order

An empty `tool_calls: []` counts as no calls. Baseten delivers reasoning on its
own delta field, so there is no inline reasoning-tag scanner anywhere in this
crate.

## Tool-call progress

While the model is still writing a tool call, the driver reports it to the host
through `murmur:stream/events@0.1.0`:

| Call | Sent on | Carries |
|---|---|---|
| `tool-call-started` | the first `delta.tool_calls[]` fragment by which the call's `id` and `function.name` have both arrived | `tool_calls[].id` and the function name, which the returned `tool_call` carries too |
| `tool-call-input-bytes` | each fragment that grows the call's `function.arguments` | the call's argument bytes so far |

The size is a running UTF-8 byte total of the call's argument text. It only
rises, and its last value is the byte length of the text parsed into the
returned `input`. No argument text is ever forwarded. On `transport: http`, the
host writes these as `tool-call-started` and `tool-call-progress` frames ahead
of the call's `artifact`.

A call whose `id` has not arrived yet is not reported. Once it arrives, the
start and the size so far go out together. A call that never gets an id is not
reported at all, and is still returned.

The driver imports `murmur:stream/events@0.1.0`, which murmur serves from
v0.6.0.

## Stop reasons

| Baseten `finish_reason` | Murmur `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `tool_calls` | `tool_call` |
| `length` | `max_tokens` |
| anything else | `driver: unsupported Baseten finish_reason '<reason>'` |

A turn that never says why it stopped is refused rather than read as
`end_turn`. A `finish_reason` that is absent, `null`, not a string or empty
counts as none:

| Response | Refused with |
|---|---|
| JSON body whose first choice has no `finish_reason` | `driver: Baseten response has no finish_reason` |
| SSE stream that sent `data: [DONE]` with no `finish_reason` before it | `driver: Baseten stream ended with no finish_reason` |

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE path and the JSON fallback. Murmur records the members on the
`inference` trace event.

| `usage` member | Baseten field |
|---|---|
| `input_tokens` | `usage.prompt_tokens` − `usage.prompt_tokens_details.cached_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `thinking_tokens` | `usage.completion_tokens_details.reasoning_tokens` |
| `cache_write_tokens` | never sent |

Baseten's `prompt_tokens` includes the cached tokens, while the usage contract
puts `cached_tokens` beside `input_tokens`. The driver subtracts once, after
merging every streamed chunk, so `input_tokens + cached_tokens` is Baseten's
`prompt_tokens`. Baseten's own example for `zai-org/GLM-5.3`, 14,820 input of
which 9,200 cached, reports as `input_tokens: 5620` and `cached_tokens: 9200`.
`thinking_tokens` is part of `output_tokens`, as Baseten counts it. Caching is
automatic and Baseten reports no cache-write count, so `cache_write_tokens` is
always absent.

`completion_tokens_details.accepted_prediction_tokens` and
`rejected_prediction_tokens` are speculative-decoding statistics, not billing
counters, and `total_tokens` is the sum of the other two; the driver reads none
of them.

| Report | Result |
|---|---|
| no `cached_tokens`, or one that is not a non-negative integer | subtracted as `0`, and left out of `usage` |
| `cached_tokens` greater than `prompt_tokens` | `input_tokens` left out; the other members are still reported |
| a reported `0` | kept as `0` |
| no member survives | no `usage` key at all |

## Session affinity

Baseten routes requests that carry the same `x-session-affinity` header to the
same replica, where the shared prompt prefix is most likely still in the KV
cache. Cached input is billed at a fraction of uncached input, so a task whose
turns stay on one replica costs less.

Murmur puts a reserved top-level `prompt_cache_key` on every driver request,
fixed for every turn of a task and unchanged by compaction. The driver sends the
lowercase hex SHA-256 of that key, trimmed, as `x-session-affinity`:

| `prompt_cache_key` | `x-session-affinity` |
|---|---|
| a non-blank string | 64 lowercase hex characters, the same on every turn of the task |
| absent, `null`, not a string, empty or whitespace-only | header not sent |

murmur sends the key on every agent-loop turn. A hook's `run-inference` call,
such as `hook-compact`'s summary, carries none, because it shares no prompt
prefix with the agent loop, so it goes out without the header.

Baseten asks for "an opaque value, such as a UUID, or hash an internal
conversation ID before sending it". The key opens with the capsule's name and
version, so it is hashed rather than sent. The raw key never leaves the guest:
it reaches neither a header nor the request body, where Baseten's schema
defines no member for it. A key that is unusable is never an error.

## Compaction with `hook-compact`

`hook-compact`'s summary call has no tools and reasons like every request. At
`reasoning_effort: high` or `max` a summary call can take longer than the
30-second default hook deadline. The task then fails with `E-RUN-040` "hook
'hook-compact' on-compaction exceeded its 30s execution deadline". A capsule
that compacts on `zai-org/GLM-5.3` raises the deadline:

```yaml
capabilities:
  limits:
    deadline_seconds: 300
```

Set `inference.max_tokens` and `inference.compaction.max_tokens` too. Left
unset, a murmur that reads `models:` takes both caps from it, so every agent
turn and every summary call asks for `262144` tokens. Baseten notes that higher
`max_tokens` values slightly deprioritize request scheduling, and the agent cap
also shrinks the compaction budget above. `16384` is ample for a coding turn,
and `4096` for a summary. A capsule that also sets `context.max_tokens` keeps it
above both, or a murmur that measures the input budget refuses the launch with
`E-RUN-055`.

## Transient failures

A failed call's result can carry one `transient` tuple in its `metadata`, which
tells murmur whether to make the call again. murmur reads it from v0.8.0, the
first release after v0.7.0. An earlier murmur ignores it and fails the call as
before. Before a retry, murmur discards everything the failed attempt streamed.

The driver marks `"true"` only after Baseten answered `2xx`:

| Failure | Message |
|---|---|
| the SSE stream ended without `data: [DONE]`; an empty body counts | `driver: Baseten stream ended before data: [DONE]` |
| the SSE stream sent `data: [DONE]` before any chunk carried a non-empty `finish_reason` | `driver: Baseten stream ended with no finish_reason` |
| a stream chunk carried a non-null top-level `error` | `driver: Baseten stream error: <error>` |
| a read of the response body failed, on the SSE path or the JSON fallback | `driver: failed to read response stream: <error>` |

Baseten documents no error vocabulary for a failure inside a `2xx` stream, and
says to treat an explicit error event as a failed request and retry it as a new
request, so every `error` chunk is marked.

Baseten names `429`, `500`, `502`, `503` and `529` as statuses to retry with
backoff. murmur retries `408`, `429` and every `5xx` but `501` and `505` by
itself, so the driver adds nothing to them. It marks no status `"false"`: a
Baseten `429` is a requests- or tokens-per-minute limit or a capacity shortage,
which clears, and an account with no credit is answered `402`, which murmur
never retries.

Every other failure carries no `transient` tuple:

| Failure | Why |
|---|---|
| any status `>= 400`, and a connection failure before the response head | murmur decides these itself, from the status or the connection error |
| a read that fails while reading a `>= 400` body | the status decides |
| a `finish_reason` other than `stop`, `tool_calls` or `length` | the same request fails the same way |
| a JSON body with no `finish_reason` or no `choices[0]` | the response is malformed |
| a tool call index past the limit, or tool arguments that are not JSON | the response is malformed |
| every refusal under [Errors](#errors) | no request was sent |

Baseten's delay hints arrive as HTTP `Retry-After` headers, which murmur reads
itself. The driver sets no delay and never repeats a request.

## Errors

Every refusal opens `driver: `, and nothing panics. None of these reaches the
network: the config check runs before the endpoint is read, and the rest before
any request is sent.

| Check | Refusal |
|---|---|
| `inference.driver.config` not JSON | `driver: inference.driver.config is not valid JSON: <serde error>` |
| `inference.driver.config` not an object | `driver: inference.driver.config must be a JSON object` |
| an unknown config key | `driver: unrecognised inference.driver.config key(s): <keys, sorted>. Accepted keys: reasoning_effort, replay_reasoning` |
| `reasoning_effort: none` | `driver: inference.driver.config 'reasoning_effort' cannot be "none" on driver-baseten: Baseten runs none at low on zai-org/GLM-5.3, which always reasons. Set it to "low".` |
| any other `reasoning_effort` string | `driver: inference.driver.config 'reasoning_effort' must be one of low, high, max, got "<raw>"` |
| a non-string `reasoning_effort` | `driver: inference.driver.config 'reasoning_effort' must be a string, got <json>` |
| a `replay_reasoning` that is not a JSON boolean, `"false"` and `null` included | `driver: inference.driver.config 'replay_reasoning' must be a boolean, got <json>` |
| `inference.model` | `driver: model '<m>' is not supported by driver-baseten. Supported models: zai-org/GLM-5.3.` |
| `inference.max_tokens` | `driver: inference.max_tokens <n> exceeds the Baseten max_tokens ceiling of 262144 for zai-org/GLM-5.3` |
| an `image` block in any message | `driver: driver-baseten sends text only and refuses an image block` |
| a `tool` message with no `tool_call_id` | `driver: tool message is missing required field 'tool_call_id'` |
| an unknown message role | `driver: unsupported message role '<role>'` |

Baseten lists vision for GLM 5.3, but this driver sends text only.

These come from the response instead:

| Response | Error |
|---|---|
| a streamed chunk numbering a tool call 1024 or more | `driver: tool call index <n> is past the limit of 1024 per turn`, before anything is stored for it or the call is reported. A chunk whose call has no `index` is read as index 0 |
| tool-call arguments that are not JSON, on either path | `driver: failed to parse Baseten tool call arguments JSON: <error>` |
| a JSON body with no first choice | `driver: Baseten response missing choices[0]` |
| the stream and stop-reason failures above | their messages under [Streaming](#streaming) and [Stop reasons](#stop-reasons) |

The `run` wrapper turns an error into a `ToolResult` with `status: error`, the
message as its summary, and `{"stop_reason":"error","error":…}` as its data. A
provider status `>= 400` produces the same shape carrying `HTTP <status>: <body>`.
The result's `metadata` is empty, except for the `transient` tuple described
under [Transient failures](#transient-failures).
