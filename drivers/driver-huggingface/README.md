# driver-huggingface

Hugging Face Inference Providers inference driver for Murmur agent capsules —
open models hosted behind Hugging Face's router, each pinned to one backend,
on the Chat Completions surface.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the router's
OpenAI-compatible Chat Completions API, including SSE streaming, a full
tool-calling round trip, and reasoning sent back on later turns on the backend
that reads it.

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
  - name: murmur/driver-huggingface
    version: "0.1.0"
    runtime: driver
    gateway:
      endpoint: https://router.huggingface.co/v1
      api_key: ${HF_TOKEN}

inference:
  transport: http
  model: "Qwen/Qwen3.8-27B:cerebras"
  driver:
    artifact: murmur/driver-huggingface
    config:
      reasoning_effort: medium
```

`HF_TOKEN` is a fine-grained Hugging Face token with the "Make calls to
Inference Providers" permission. The router bills the calls to the token's
account; a free account needs purchased credits.

Quote `inference.model`: the id holds a `:`, and quoting keeps every YAML
reader from reading it as anything but one string.

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — (one of the ids under [Model scope](#model-scope)) |
| `gateway.endpoint` | yes | — |
| `gateway.api_key` | yes, unless the entry declares `keyless: true` | — without either, the launch fails with `E-CAP-018` |
| `inference.max_tokens` | no | the model's `max_output_tokens`, from `models:` |
| `context.max_tokens` | no | the model's `context_window`, from `models:` |
| `inference.driver.config` | no | `{}` — see [Config keys](#config-keys) |

## Model scope

The driver serves only the ids below. Each one pins a single backend as
`<org>/<model>:<provider>`. The id goes to the router verbatim as the body's
`model`, and the router reads the suffix as the backend to send it to.

| Model | `context_window` | `max_output_tokens` | `max_input_tokens` | Source, read 2026-11-09 |
|---|---|---|---|---|
| `openai/gpt-oss-120b:groq` | `131072` | `65536` | `none` | window: the router's `GET /v1/models`, provider `groq`, `context_length: 131072`, `status: live`, `supports_tools: true`; Groq's model page lists the same context window. Output: Groq's model page, "MAX OUTPUT TOKENS 65,536" |
| `Qwen/Qwen3.8-27B:cerebras` | `65536` | `32768` | `none` | window: the router's `GET /v1/models`, provider `cerebras`, `context_length: 65536`, `status: live`, `supports_tools: true`. Output: Cerebras's model page, "32K tokens (32,768) for Free Trial" |

| Source | URL |
|---|---|
| the router's model list (no token needed) | `https://router.huggingface.co/v1/models` |
| Groq's model page | `https://console.groq.com/docs/model/openai/gpt-oss-120b` |
| Cerebras's model page | `https://inference-docs.cerebras.ai/models/qwen-3.8-27b` |

Cerebras publishes two tiers for Qwen 3.8 27B: a context of 65,536 with an
output cap of 32,768 on its Free Trial, and 131,072 with 40,960 on its paid
tiers. The router reports 65,536, the Free Trial context, so the driver pairs it
with that tier's 32,768. Neither backend documents an input ceiling separate
from the window.

The id is matched exactly, with no trimming, no case folding and no prefix
match. Three kinds of id are refused, each with its own message under
[Errors](#errors), before a request body is built:

| Id | Why it is refused |
|---|---|
| a bare name, such as `openai/gpt-oss-120b` | it names no backend, so the router picks one. Context size, tool support and reasoning differ per backend, and a backend the router picks can change between runs |
| a name with a router policy, `:fastest`, `:cheapest` or `:preferred` | the policy picks the backend by throughput, price or the account's preference order at the time of the call, so two runs of one capsule can reach two backends |
| any other id, `openai/gpt-oss-120b:cerebras` included | the driver has no limits or reasoning facts for it |

The limits live in one table, `SUPPORTED_MODELS` in `src/lib.rs`, and
`murmur.yaml` declares the same numbers under `models:`. A test fails if the two
disagree, or if a row above loses either number.

murmur takes the output cap and the context window from `models:` when the
capsule leaves `inference.max_tokens` and `context.max_tokens` unset, and
compacts against one input budget per call,
`min(max_input_tokens, context_window − output cap)`, here
`context_window − output cap`:

| Model | Output cap left unset | Input budget |
|---|---|---|
| `openai/gpt-oss-120b:groq` | `65536` | `131072 − 65536 = 65536` |
| `Qwen/Qwen3.8-27B:cerebras` | `32768` | `65536 − 32768 = 32768` |

An `inference.max_tokens` the capsule sets changes the `window − output cap`
term: `8192` on `Qwen/Qwen3.8-27B:cerebras` gives a budget of `57344`.

## Config keys

`inference.driver.config` accepts two keys at most, and what each accepts
depends on the model. Every other key, `clear_thinking`, `include_reasoning`
and `reasoning_format` included, is refused by name on the first inference
call, before any HTTP request is dispatched.

| Model | `reasoning_effort` | `replay_reasoning` |
|---|---|---|
| `openai/gpt-oss-120b:groq` | `low`, `medium`, `high`; default `medium` | refused, `true` and `false` alike |
| `Qwen/Qwen3.8-27B:cerebras` | `none`, `low`, `medium`, `high`; default `high` | `true` or `false`; default `true` |

`reasoning_effort` is trimmed and matched case-insensitively, and sent
lowercased as the body's top-level `reasoning_effort`. It is stamped on every
request, the default included, so the effort in force shows in a recorded
request and cannot drift with the backend's own default. A value the model's
backend does not document is refused with the value quoted back as written,
beside the accepted set. It is never mapped onto another value.

`replay_reasoning` must be a JSON boolean: a string such as `"false"`, a number
or `null` is refused rather than read as one. It decides whether earlier
assistant messages carry their reasoning; see [Reasoning](#reasoning).

An `inference` trace event's `driver_config` holds only the keys the capsule
writes, so an unset key, which means the model's default, does not appear
there.

## Reasoning

| | `openai/gpt-oss-120b:groq` | `Qwen/Qwen3.8-27B:cerebras` |
|---|---|---|
| Field reasoning arrives in | `message.reasoning`, `delta.reasoning` on a stream | `message.reasoning`, `delta.reasoning` on a stream |
| Effort values, default | `low`, `medium`, `high`; `medium` | `none`, `low`, `medium`, `high`; `high` |
| Reasoning sent back on earlier turns | never; `replay_reasoning` is refused | as `reasoning` on each earlier assistant message, unless `replay_reasoning: false` |
| Source | Groq's reasoning guide | Cerebras's reasoning guide |
| Confirmed by a live call | not yet: docs, not yet confirmed live | not yet: docs, not yet confirmed live |

| Source | URL |
|---|---|
| Groq's reasoning guide | `https://console.groq.com/docs/reasoning` |
| Cerebras's reasoning guide | `https://inference-docs.cerebras.ai/capabilities/reasoning` |

Every fact in this section comes from the backends' own docs, read
2026-11-09. None has been confirmed through the router by a live call yet.

**`openai/gpt-oss-120b:groq`.** Groq's guide gives `reasoning_effort` values of
`low`, `medium` and `high` for GPT-OSS, and puts the reasoning in the
`reasoning` field of the assistant response by default. `include_reasoning`
defaults to `true`, and the driver never sends it or `reasoning_format`. The
guide documents no way to send reasoning back on an earlier assistant message,
so the driver never sends it and refuses a `replay_reasoning` key, whichever
value it holds.

**`Qwen/Qwen3.8-27B:cerebras`.** Cerebras's guide gives `reasoning_effort`
values of `none`, `low`, `medium` and `high`, with `high` as the default and
`none` turning reasoning off. Reasoning comes back in `message.reasoning`, and
in `delta.reasoning` on a stream. An earlier assistant message's `reasoning`
field is read back while `clear_thinking` is `false`, its default, so the
driver sends the reasoning back as `reasoning` and never sends `clear_thinking`.
The guide says not to send Qwen-native parameters such as `enable_thinking` or
`thinking_budget`, and the driver sends none.

| Step | What happens |
|---|---|
| Response | `<field>`, `delta.<field>` on a stream, becomes a `{"type":"thinking","text":…}` content block |
| Storage | the runtime stores and replays whatever content blocks a driver emits without inspecting them |
| Next request | where replay is on, the driver reads the turn's first non-empty `thinking` block back and sends it byte for byte as `<field>` on that assistant message |

`<field>` is `reasoning` for both models. With replay on, the reasoning goes
back on every assistant message, on a turn that ended in tool calls and on a
plain-text turn alike. Each earlier turn's reasoning is then sent again as
prompt tokens on every later request until a compaction removes it. With
`replay_reasoning: false`, no assistant message carries it, the current tool
loop's included. Under every setting the model reasons on the current turn
unless the effort is `none`, and that reasoning comes back as a `thinking`
block and reaches the trace.

A tool-call turn can carry text beside its calls; the driver sends it back as
the assistant message's `content`. `content` is `null` only on a tool-call turn
with no text. A turn on which the model did not reason, and every turn after a
committed compaction, carries no `thinking` block, so its `reasoning` member is
left out, never sent as an empty string.

## Streaming

Every request is sent with `stream: true` and
`stream_options: {"include_usage": true}`, stamped last so they override
anything from `params`. Usage can arrive on any chunk; the driver merges each
report member by member, and the last one wins.

SSE lines are parsed incrementally: `delta.<field>` goes to
`emit_thinking_chunk`, `delta.content` to `emit_chunk`, and `delta.tool_calls`
is accumulated by `index` into id, name and argument fragments. A tool call can
arrive in fragments or whole in one chunk, and the driver reads either. A
non-streaming JSON fallback, which reads `message.<field>`, is selected by
sniffing the first non-whitespace byte of the response body for `{` or `[`.

A stream counts as complete only when it ends with `data: [DONE]`, a last line
without a trailing newline included:

| Stream | Result |
|---|---|
| ends without `data: [DONE]`, whatever it carried before, a `finish_reason` included | `driver: Hugging Face stream ended before data: [DONE]` |
| ends with `data: [DONE]`, but no chunk carried a non-empty `finish_reason` | `driver: Hugging Face stream ended with no finish_reason` |
| a chunk carries a non-null top-level `error` | `driver: Hugging Face stream error: <error>`, the member serialised compactly |

The `[DONE]` check runs first. On both paths the response `content` is ordered:

1. the `thinking` block, when the reasoning is non-empty
2. the `text` block, when the text is non-empty
3. the `tool_call` blocks, in `index` order

An empty `tool_calls: []` counts as no calls. Both backends deliver reasoning
on its own delta field, so there is no inline reasoning-tag scanner anywhere in
this crate.

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

## Stop reasons

| `finish_reason` | Murmur `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `tool_calls` | `tool_call` |
| `length` | `max_tokens` |
| anything else | `driver: unsupported Hugging Face finish_reason '<reason>'` |

A turn that never says why it stopped is refused rather than read as
`end_turn`. A `finish_reason` that is absent, `null`, not a string or empty
counts as none:

| Response | Refused with |
|---|---|
| JSON body whose first choice has no `finish_reason` | `driver: Hugging Face response has no finish_reason` |
| SSE stream that sent `data: [DONE]` with no `finish_reason` before it | `driver: Hugging Face stream ended with no finish_reason` |

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE path and the JSON fallback. Murmur records the members on the
`inference` trace event.

| `usage` member | Field read |
|---|---|
| `input_tokens` | `usage.prompt_tokens` − `usage.prompt_tokens_details.cached_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `thinking_tokens` | `usage.completion_tokens_details.reasoning_tokens` |
| `cache_write_tokens` | never sent |

`prompt_tokens` includes the cached tokens, while the usage contract puts
`cached_tokens` beside `input_tokens`. The driver subtracts once, after merging
every streamed chunk, so `input_tokens + cached_tokens` is the backend's
`prompt_tokens`. `thinking_tokens` is part of `output_tokens`. Neither backend
reports a cache-write count, so `cache_write_tokens` is always absent.

| Model | What its backend documents |
|---|---|
| `openai/gpt-oss-120b:groq` | Groq's prompt-caching guide lists the model and reports `cached_tokens` inside `prompt_tokens`. Groq's Chat Completions docs show no `reasoning_tokens` count |
| `Qwen/Qwen3.8-27B:cerebras` | Cerebras's Chat Completions reference reports `usage.prompt_tokens_details.cached_tokens` and `usage.completion_tokens_details.reasoning_tokens` |

Whether the router passes each member through unchanged is not yet confirmed by
a live call. A member a backend does not report is left out, never sent as `0`:

| Report | Result |
|---|---|
| no `cached_tokens`, or one that is not a non-negative integer | subtracted as `0`, and left out of `usage` |
| `cached_tokens` greater than `prompt_tokens` | `input_tokens` left out; the other members are still reported |
| a reported `0` | kept as `0` |
| no member survives | no `usage` key at all |

## Prompt cache key

Murmur puts a reserved top-level `prompt_cache_key` on every agent-loop
request. The driver drops it: the router's Chat Completions spec documents no
cache-key field and no session-affinity header, and an undocumented member
could be refused or forwarded to a backend that reads it differently. Requests
carry `content-type` and `content-length` and no other header of the driver's
own. Both backends cache prompt prefixes on their own, with no key.

## Compaction with `hook-compact`

`hook-compact`'s summary call has no tools, and the driver builds it like any
other request: no `tools` member, and the capsule's `reasoning_effort` or the
model's default. murmur caps that call at the model's `max_output_tokens` unless
the capsule sets `inference.compaction.max_tokens`.

Set `inference.max_tokens` and `inference.compaction.max_tokens` when the input
budget above is too small for the task. On `Qwen/Qwen3.8-27B:cerebras` the
unset budget is `32768` tokens, so a long tool loop compacts early; `8192` for
agent turns and `4096` for a summary leave `57344`. A capsule that also sets
`context.max_tokens` keeps it above both, or murmur refuses the launch with
`E-RUN-055`.

A summary call at a high effort can take longer than the 30-second default hook
deadline, and the task then fails with `E-RUN-040`. A capsule that compacts
raises the deadline:

```yaml
capabilities:
  limits:
    deadline_seconds: 300
```

## Transient failures

A failed call's result can carry one `transient` tuple in its `metadata`, which
tells murmur whether to make the call again. Before a retry, murmur discards
everything the failed attempt streamed.

The driver marks `"true"` only after the router answered `2xx`:

| Failure | Message |
|---|---|
| the SSE stream ended without `data: [DONE]`; an empty body counts | `driver: Hugging Face stream ended before data: [DONE]` |
| the SSE stream sent `data: [DONE]` before any chunk carried a non-empty `finish_reason` | `driver: Hugging Face stream ended with no finish_reason` |
| a stream chunk carried an `error` naming a status murmur retries: `408`, `429`, or `500`–`599` except `501` and `505` | `driver: Hugging Face stream error: <error>` |
| a read of the response body failed, on the SSE path or the JSON fallback | `driver: failed to read response stream: <error>` |

The status of an `error` chunk is read from its `code`, `status` or
`status_code` member, in that order, as a JSON number or a string of digits.
The router documents no error vocabulary for a failure inside a `2xx` stream,
so the driver treats an error naming a retryable status as murmur treats that
status on a response. Any other `error` chunk carries no `transient` tuple, and
reads the same `driver: Hugging Face stream error: <error>`.

murmur retries `408`, `429` and every `5xx` but `501` and `505` by itself, so
the driver adds nothing to a response status: the router's `429` (a rate limit)
and its `5xx` (a backend that is down or overloaded) are retried by murmur, and
`402` (an account with no credits) never is.

Every other failure carries no `transient` tuple:

| Failure | Why |
|---|---|
| any status `>= 400`, and a connection failure before the response head | murmur decides these itself, from the status or the connection error |
| a read that fails while reading a `>= 400` body | the status decides |
| an `error` chunk naming no status, or one murmur does not retry | the same request fails the same way |
| a `finish_reason` other than `stop`, `tool_calls` or `length` | the same request fails the same way |
| a JSON body with no `finish_reason` or no `choices[0]` | the response is malformed |
| a tool call index past the limit, or tool arguments that are not JSON | the response is malformed |
| every refusal under [Errors](#errors) | no request was sent |

## Errors

Every refusal opens `driver: `, and nothing panics. None of these reaches the
network: the config check runs before the endpoint is read, and the rest before
any request is sent. The model check runs first among the request checks, then
the config against the model, then the output cap, then images.

| Check | Refusal |
|---|---|
| `inference.driver.config` not JSON | `driver: inference.driver.config is not valid JSON: <serde error>` |
| `inference.driver.config` not an object | `driver: inference.driver.config must be a JSON object` |
| an unknown config key | `driver: unrecognised inference.driver.config key(s): <keys, sorted>. Accepted keys: reasoning_effort, replay_reasoning` |
| a non-string `reasoning_effort` | `driver: inference.driver.config 'reasoning_effort' must be a string, got <json>` |
| a `replay_reasoning` that is not a JSON boolean, `"false"` and `null` included | `driver: inference.driver.config 'replay_reasoning' must be a boolean, got <json>` |
| `inference.model` with no `:` | `driver: model '<m>' names no backend. driver-huggingface serves only the <org>/<model>:<provider> ids it lists, so the router cannot switch backends between runs. Supported models: openai/gpt-oss-120b:groq, Qwen/Qwen3.8-27B:cerebras.` |
| `inference.model` ending `:fastest`, `:cheapest` or `:preferred` | `driver: model '<m>' asks the router to choose the backend (':<suffix>'). driver-huggingface pins one backend per model so a capsule's runs stay repeatable. Supported models: openai/gpt-oss-120b:groq, Qwen/Qwen3.8-27B:cerebras.` |
| any other unlisted `inference.model` | `driver: model '<m>' is not supported by driver-huggingface. Supported models: openai/gpt-oss-120b:groq, Qwen/Qwen3.8-27B:cerebras.` |
| a `reasoning_effort` the model's backend does not document | `driver: inference.driver.config 'reasoning_effort' must be one of <values> for <model>, got "<raw>"` |
| `replay_reasoning` on `openai/gpt-oss-120b:groq` | `driver: inference.driver.config 'replay_reasoning' is not accepted for openai/gpt-oss-120b:groq: Groq's reasoning guide documents no way to send reasoning back on an earlier assistant message, so driver-huggingface never sends it` |
| `inference.max_tokens` | `driver: inference.max_tokens <n> exceeds the max_tokens ceiling of <cap> for <model>` |
| an `image` block in any message | `driver: driver-huggingface sends text only and refuses an image block` |
| a `tool` message with no `tool_call_id` | `driver: tool message is missing required field 'tool_call_id'` |
| an unknown message role | `driver: unsupported message role '<role>'` |

`<values>` is `low, medium, high` for `openai/gpt-oss-120b:groq` and
`none, low, medium, high` for `Qwen/Qwen3.8-27B:cerebras`.

These come from the response instead:

| Response | Error |
|---|---|
| a streamed chunk numbering a tool call 1024 or more | `driver: tool call index <n> is past the limit of 1024 per turn`, before anything is stored for it or the call is reported. A chunk whose call has no `index` is read as index 0 |
| tool-call arguments that are not JSON, on either path | `driver: failed to parse Hugging Face tool call arguments JSON: <error>` |
| a JSON body with no first choice | `driver: Hugging Face response missing choices[0]` |
| the stream and stop-reason failures above | their messages under [Streaming](#streaming) and [Stop reasons](#stop-reasons) |

The `run` wrapper turns an error into a `ToolResult` with `status: error`, the
message as its summary, and `{"stop_reason":"error","error":…}` as its data. A
provider status `>= 400` produces the same shape carrying `HTTP <status>: <body>`.
The result's `metadata` is empty, except for the `transient` tuple described
under [Transient failures](#transient-failures).
