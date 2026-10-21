# driver-zai

Z.ai inference driver for Murmur agent capsules — `glm-5.3` on the Chat
Completions surface.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and Z.ai's
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
  - name: murmur/driver-zai
    version: "0.1.0"
    runtime: driver
    gateway:
      endpoint: https://api.z.ai/api/paas/v4
      api_key: ${ZAI_API_KEY}

inference:
  transport: http
  model: glm-5.3
  driver:
    artifact: murmur/driver-zai
    config:
      reasoning_effort: high
```

The Coding Plan base, `https://api.z.ai/api/coding/paas/v4`, works as
`gateway.endpoint` too.

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — (must be `glm-5.3`) |
| `gateway.endpoint` | yes | — |
| `gateway.api_key` | yes, unless the entry declares `keyless: true` | — without either, the launch fails with `E-CAP-018` |
| `inference.max_tokens` | no | from the host; ceiling `131072` |
| `inference.driver.config` | no | `{}` — see below |

## Model scope

`glm-5.3` is the only model this driver accepts. The id is matched exactly, with
no case folding and no prefix match, so `GLM-5.3`, `glm-5.3-flash` and `glm-5.2`
are all refused. The check runs before a request body is built, so an
unsupported model costs no HTTP call.

| Model | `max_output_tokens` | `context_window` |
|---|---|---|
| `glm-5.3` | `131072` | `1048576` |

| Limit | Source |
|---|---|
| `max_output_tokens: 131072` | Z.ai's GLM-5.3 guide gives a maximum output of 128K tokens, and Z.ai answers `max_tokens: 131073` with HTTP 400 code `1210`, "限制数值范围[1,131072]" |
| `context_window: 1048576` | the GLM-5.3 guide's "1M-token context window"; the model weights' `max_position_embeddings` is `1048576` |

The limits live in one table, `SUPPORTED_MODELS` in `src/lib.rs`, and
`murmur.yaml` declares the same numbers under `models:`. A test fails if the two
disagree. Whether the runtime reads that block depends on the murmur release:

| murmur | What it does with `models:` | What the capsule sets |
|---|---|---|
| v0.7.0 and earlier | ignores it; an unknown manifest key is a `W-SEC-019` warning, never a refusal | `inference.max_tokens` and `context.max_tokens`; left unset, the runtime's own defaults apply, not `glm-5.3`'s limits |
| the first release after v0.7.0 | defaults the output cap and context window from it when the capsule leaves them unset, and caps a hook's `run-inference` (e.g. `hook-compact`) at its `max_output_tokens` | nothing, unless it wants tighter limits than the table |

## Config keys

`inference.driver.config` accepts exactly two keys. Every other key is refused
by name on the first inference call, before any HTTP request is dispatched.
String values are trimmed and matched case-insensitively.

| Key | Required | Default | Accepted values | Provider body field |
|---|---|---|---|---|
| `reasoning_effort` | no | `max` | `low`, `high`, `max` | top-level `reasoning_effort` |
| `thinking` | no | `enabled` | `enabled` | `thinking.type`, always `enabled` |

## How Z.ai expresses effort

| Fact | What the driver does |
|---|---|
| `glm-5.3` takes `reasoning_effort` of `low`, `high` or `max`, and defaults to `max` | accepts those three, sent lowercased and top-level on the body |
| Z.ai answers `reasoning_effort: "medium"` on `glm-5.3` with HTTP 400 code `1210` ("please use low, high, or max"), on both the standard and the Coding Plan endpoint | refuses `medium` by name, with the accepted set, before any HTTP call. It never maps `medium` onto another tier |
| Z.ai's general reference lists `medium` for its text models, but only for GLM-5.2, where it maps to `high`. Its Deep Thinking guide says Coding Plan requests map `medium` to `high` | the nearest tier to `medium` is `high`; a capsule that wants it writes `high` |
| Amazon Bedrock lists `medium` and `xhigh` for GLM 5.3 | not applicable: that is Bedrock's API, not Z.ai's |

`xhigh`, `minimal`, `none` and every other value are refused the same way as
`medium`, with the rejected value quoted back alongside the accepted set.

**`max` is the expensive end, and it is what an operator gets by saying
nothing.** That is Z.ai's own default, kept rather than substituted with a
cheaper tier. It is stamped explicitly on every request, including the
unconfigured one, so the effective value is visible in a recorded request and
cannot drift when Z.ai changes its default.

## Why `thinking` accepts only `enabled`

`glm-5.3` always reasons. Z.ai requires `thinking.type` to be `enabled` and
answers `"thinking": {"type": "disabled"}` with HTTP 400 code `1210`, "This model
always engages in thinking and cannot be disabled". So:

| Value | Result |
|---|---|
| `enabled` | accepted; the body is the same as without the key |
| `disabled` | refused with its own message, naming the key, the value and `glm-5.3` |
| any other string | refused: `'thinking' must be "enabled"` |
| a non-string | refused: `'thinking' must be a string` |

The key is spelled `thinking`, matching `driver-anthropic` and `driver-deepseek`.

## Preserved reasoning across turns

Every request carries `"thinking": {"type": "enabled", "clear_thinking": false}`.
With `clear_thinking: false` Z.ai keeps earlier turns' reasoning in context, and
asks for each assistant turn's `reasoning_content` back complete, unmodified and
in its original order. The driver does that entirely by itself:

| Step | What happens |
|---|---|
| Response | the provider's `reasoning_content` becomes a `{"type":"thinking","text":…}` content block |
| Storage | the runtime stores and replays whatever content blocks a driver emits without inspecting them |
| Next request | the driver reads the turn's first non-empty `thinking` block back and sends it byte for byte as `reasoning_content` on that assistant message |

`reasoning_content` is restored on every assistant message, on a turn that ended
in tool calls and on a plain-text turn alike. A tool-call turn can carry text
beside its calls ("I'll check the weather for both cities."); the driver keeps
that text as a `text` block and sends it back as the assistant message's
`content`, so the turn goes back as Z.ai returned it. `content` is `null` only
on a tool-call turn with no text.

`glm-5.3` does not reason on every turn. On a trivial step, such as reading
one file, Z.ai sends no `reasoning_content` and reports `reasoning_tokens: 0`, so
that turn has no `thinking` block and nothing is replayed for it.

A committed compaction replaces the live context with a summary that crosses the
hook boundary as a single `text` block, so historical assistant messages carry
no `thinking` block afterwards. That is the ordinary post-compaction state, not
an error: the `reasoning_content` member is left out, never sent as an empty
string, and the request goes out normally.

## Streaming

Every request is sent with `stream: true` and `tool_stream: true`, stamped last
so they override anything from `params`. With `tool_stream` Z.ai streams each
tool call's arguments in fragments as the model writes them; without it each
call arrives whole in one chunk, and the driver reads either. No
`stream_options` is sent: Z.ai documents none, and puts `usage` on the chunk that
carries `finish_reason` regardless.

SSE lines are parsed incrementally: `delta.reasoning_content` goes to
`emit_thinking_chunk`, `delta.content` to `emit_chunk`, and `delta.tool_calls` is
accumulated by `index` into id, name and argument fragments. `data: [DONE]` ends
the stream. A non-streaming JSON fallback is selected by sniffing the first
non-whitespace byte of the response body for `{` or `[`.

On both paths the response `content` is ordered:

1. the `thinking` block, when the reasoning is non-empty
2. the `text` block, when the text is non-empty
3. the `tool_call` blocks, in `index` order

An empty `tool_calls: []` counts as no calls. Z.ai delivers reasoning on its own
delta field, so there is no inline reasoning-tag scanner anywhere in this crate.

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

| Z.ai `finish_reason` | Murmur `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `tool_calls` | `tool_call` |
| `length` | `max_tokens` |
| `sensitive`, `model_context_window_exceeded`, `network_error`, anything else | `driver: unsupported Z.ai finish_reason '<reason>'` |

A turn that never says why it stopped is refused rather than read as
`end_turn`. A `finish_reason` that is absent, `null`, not a string or empty
counts as none:

| Response | Refused with |
|---|---|
| JSON body whose first choice has no `finish_reason` | `driver: Z.ai response has no finish_reason` |
| SSE stream in which no chunk carries a `finish_reason` | `driver: Z.ai stream ended with no finish_reason` |

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE path and the JSON fallback. Murmur records the members on the
`inference` trace event.

| `usage` member | Z.ai field |
|---|---|
| `input_tokens` | `usage.prompt_tokens` − `usage.prompt_tokens_details.cached_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `thinking_tokens` | `usage.completion_tokens_details.reasoning_tokens` |
| `cache_write_tokens` | never sent |

Z.ai's `prompt_tokens` includes the cached tokens, while the usage contract puts
`cached_tokens` beside `input_tokens`. The driver subtracts once, after merging
every streamed chunk, so `input_tokens + cached_tokens` is Z.ai's
`prompt_tokens`. `thinking_tokens` is part of `output_tokens`, as Z.ai reports
it. Z.ai reports no cache-write count and bills no cache write, so
`cache_write_tokens` is always absent.

| Report | Result |
|---|---|
| no `cached_tokens`, or one that is not a non-negative integer | subtracted as `0`, and left out of `usage` |
| `cached_tokens` greater than `prompt_tokens` | `input_tokens` left out; the other members are still reported |
| a reported `0` | kept as `0` |
| no member survives | no `usage` key at all |

## Compaction with `hook-compact`

`hook-compact`'s summary call has no tools and runs with thinking on, like
every request. At `reasoning_effort: high` or `max` a summary call can take
longer than the 30-second default hook deadline. The task then fails with
`E-RUN-040` "hook 'hook-compact' on-compaction exceeded its 30s execution
deadline". A capsule that compacts on `glm-5.3` raises the deadline:

```yaml
capabilities:
  limits:
    deadline_seconds: 300
```

## Prompt cache key

Murmur puts a reserved top-level `prompt_cache_key` on every driver request.
This driver drops it: Z.ai's context caching is automatic and its Chat
Completions API defines no cache-key field. The field is not declared on the
request struct, so serde discards it and it reaches the provider body at no
nesting level.

## Errors

Every refusal opens `driver: `, and nothing panics. None of these reaches the
network: the config check runs before the endpoint is read, and the rest before
any request is sent.

| Check | Refusal |
|---|---|
| `inference.driver.config` | the config-key messages above |
| `inference.model` | `driver: model '<m>' is not supported by driver-zai. Supported models: glm-5.3.` |
| `inference.max_tokens` | `driver: inference.max_tokens <n> exceeds the Z.ai max_tokens ceiling of 131072 for glm-5.3` |
| an `image` block | `driver: model 'glm-5.3' is text-only, so driver-zai refuses an image block` |
| a `tool` message with no `tool_call_id` | `driver: tool message is missing required field 'tool_call_id'` |
| an unknown message role | `driver: unsupported message role '<role>'` |

The `run` wrapper turns an error into a `ToolResult` with `status: error`, the
message as its summary, and `{"stop_reason":"error","error":…}` as its data. A
provider status `>= 400` produces the same shape carrying `HTTP <status>: <body>`.
