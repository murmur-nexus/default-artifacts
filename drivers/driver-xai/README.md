# driver-xai

xAI inference driver for Murmur agent capsules — `grok-4.7`, `grok-4.6` and
`grok-build-0.1` on xAI's Responses API, billed per token on an xAI API key.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and xAI's Responses
API, including SSE streaming, a full tool-calling round trip, and the model's
encrypted reasoning carried across turns with response storage at xAI turned
off.

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
`<endpoint>/responses`.

The Grok Build CLI on a SuperGrok subscription is a different product, served
by a process driver. This driver is the API-key case.

## Using it from a capsule manifest

```yaml
artifacts:
  - name: murmur/driver-xai
    version: "0.1.0"
    runtime: driver
    gateway:
      endpoint: https://api.x.ai/v1
      api_key: ${XAI_API_KEY}

inference:
  transport: http
  model: grok-4.7
  driver:
    artifact: murmur/driver-xai
    config:
      reasoning_effort: medium
```

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — (one of the ids in [Model scope](#model-scope)) |
| `gateway.endpoint` | yes | — |
| `gateway.api_key` | yes | — without it, the launch fails with `E-CAP-018` |
| `inference.max_tokens` | no | `128000`, the model's `max_output_tokens` from `models:`; that number is also the ceiling |
| `context.max_tokens` | no | the model's `context_window` from `models:` |
| `inference.driver.config` | no | `{}` — see [Config keys](#config-keys) |

A capsule needs neither `inference.max_tokens` nor `context.max_tokens`:
murmur takes both from the driver's `models:` table.

## Model scope

The driver accepts three models, each from its xAI model page
(`https://docs.x.ai/developers/models/<id>.md`, read 2026-11-12 UTC):

| Model | Why | `reasoning_effort` |
|---|---|---|
| `grok-4.7` | xAI's frontier model for coding, agents and knowledge work. Function calling: Yes. Streams reasoning summaries, and always returns encrypted reasoning | `low`, `medium`, `high`, `xhigh`; default `high` |
| `grok-4.6` | Same price, window and effort set as 4.7. Function calling: Yes. xAI documents no reasoning summaries for it | `low`, `medium`, `high`, `xhigh`; default `high` |
| `grok-build-0.1` | xAI's coding model, at half the price. Function calling: Yes. Reasoning: Yes, but its page lists no effort values | refused |

The model is checked by exact string before a request body is built, so an
unsupported model costs no HTTP call. Every other id is refused with
`driver: model '<id>' is not supported by driver-xai. Supported models:
grok-4.7, grok-4.6, grok-build-0.1.`

`SUPPORTED_MODELS` in `src/lib.rs` is the one place the crate branches on a
model id. Each entry holds the id, its `ModelLimits` and its `EffortRule`:
`Effort { values, default }` or `NoEffort`. Adding a model means one entry
there, one entry under `models:` in `murmur.yaml`, and one row in the table
below.

What each entry does that only a real call can settle:

| Fact | `grok-4.7` | `grok-4.6` | `grok-build-0.1` |
|---|---|---|---|
| Reasoning summaries stream | documented | pending live check | pending live check |
| Encrypted reasoning returned | documented (always) | pending live check | pending live check |
| Replayed reasoning accepted with `store: false` | pending live check | pending live check | pending live check |
| A cap above 128,000 accepted | pending live check | pending live check | pending live check |
| The usage arithmetic below | pending live check | pending live check | pending live check |
| `cached_tokens` reported on a repeated prefix | pending live check | pending live check | pending live check |

## Model limits

| Model | max_output_tokens | context_window | max_input_tokens | Source |
|---|---|---|---|---|
| `grok-4.7` | 128000 | 500000 | none | Read 2026-11-12 UTC. Window: <https://docs.x.ai/developers/models/grok-4.7.md>, "Context window: 500,000 tokens". Output: <https://docs.x.ai/developers/grok-4-7.md>, "Output limit: No text output limit"; the cap is the Responses default, see below |
| `grok-4.6` | 128000 | 500000 | none | Read 2026-11-12 UTC. Window: <https://docs.x.ai/developers/models/grok-4.6.md>, "Context window: 500,000 tokens". Output: no ceiling documented; the cap is the Responses default, see below |
| `grok-build-0.1` | 128000 | 256000 | none | Read 2026-11-12 UTC. Window: <https://docs.x.ai/developers/models/grok-build-0.1.md>, "Context window: 256,000 tokens". Output: no ceiling documented; the cap is the Responses default, see below |

The cap is xAI's own default, not the window. The Responses reference
(<https://docs.x.ai/developers/rest-api-reference/inference/responses>, read
2026-11-12 UTC; the parameter text is on the rendered page, not its `.md`)
says `max_output_tokens` "Only applies to visible output tokens (i.e. does not
apply to tokens used for reasoning or function calls). Defaults to 128,000 when
unset; set a larger value to allow longer generations." No model page documents
a ceiling. Declaring the window as `max_output_tokens` would leave murmur no
input budget (`window − cap = 0`), and murmur would refuse every capsule that
leaves `inference.max_tokens` unset with `E-RUN-055`. So the table declares
128,000, and the driver refuses a larger cap by name.

The limits live in one table, `SUPPORTED_MODELS` in `src/lib.rs`, and
`murmur.yaml` declares the same numbers under `models:`, the `none` as no
`max_input_tokens` key. A test fails if the two disagree, or if this table
disagrees with `murmur.yaml`. xAI documents no input ceiling separate from the
window, and no per-model image facts, so no entry declares `max_input_tokens`
or `vision`.

murmur compacts against one input budget per call:
`min(max_input_tokens, context_window − output cap)`, which with no
`context.max_tokens` and no `inference.max_tokens` is 372,000 on the 4.x
models and 128,000 on `grok-build-0.1`.

## Models this driver refuses

| Model | Why |
|---|---|
| `grok-4.5` | Its model page lists `xhigh`, while the reasoning guide says it treats `xhigh` as `high`; the docs disagree, so it waits for a live check |
| `grok-4.3` | An older 1M-window model, not picked for the first table |
| the `grok-4.20` variants | Older 1M-window models, not picked for the first table |
| `grok-4.20-multi-agent-0309` | The multi-agent model, where effort sets an agent count rather than reasoning depth; out of scope |
| `grok-code-fast-1`, `grok-code-fast`, `grok-code-fast-1-0825` | Aliases of `grok-build-0.1`. The driver matches exact ids, so a capsule names the model the table lists |
| any id differing in case or whitespace, such as `Grok-4.7` or `grok-4.7 ` | Exact match only |
| any non-xAI id, such as `gpt-5.6-luna` | Not an xAI model |

Each is refused on the first inference call with the message in
[Model scope](#model-scope). None has a `models:` entry, so on a murmur that
reads `models:` a capsule that sets neither `inference.max_tokens` nor
`context.max_tokens` is refused at launch with `E-RUN-052` first.

## Config keys

`inference.driver.config` accepts exactly two keys. Every other key is refused
by name on the first inference call, before any HTTP request is dispatched:
`driver: unrecognised inference.driver.config key(s): <keys>. Accepted keys:
reasoning_effort, response_format`.

| Key | Required | Accepted values | `grok-4.7`, `grok-4.6` | `grok-build-0.1` | Provider body field |
|---|---|---|---|---|---|
| `reasoning_effort` | no | a string, trimmed and lowercased | `low`, `medium`, `high`, `xhigh`; default `high` | refused | `reasoning.effort` |
| `response_format` | no | `{type: json_schema, json_schema: {name, schema, strict?}}` | absent unless set | absent unless set | `text.format` |

There is no `thinking`, `store` or `replay_reasoning` key:

- reasoning cannot be disabled on xAI's reasoning models (reasoning guide,
  <https://docs.x.ai/developers/model-capabilities/text/reasoning.md>);
- storage is always off (`store: false`);
- the encrypted reasoning always goes back (see
  [Preserved reasoning across turns](#preserved-reasoning-across-turns)).

### `reasoning_effort`

The effort in force, configured or the model's default, is sent as
`reasoning: {"effort": "<value>"}` on every request to a `grok-4.x` model, so a
recorded request always shows it. A value the model does not take is refused,
quoting the value as written:

```text
driver: inference.driver.config 'reasoning_effort' must be one of low, medium, high, xhigh for model 'grok-4.7', got "max"
```

On `grok-build-0.1`, whose page lists no effort values, any value is refused:

```text
driver: inference.driver.config 'reasoning_effort' is not accepted for model 'grok-build-0.1': xAI documents no reasoning effort for it. Remove the key.
```

A value that is not a string is refused with
`driver: inference.driver.config 'reasoning_effort' must be a string, got <value>`.

### `response_format`

Structured output, documented on Responses as `text.format`
(<https://docs.x.ai/developers/model-capabilities/text/structured-outputs.md>).
The key takes the spelling the other drivers here read, and the driver sends it
as `text: {"format": {"type": "json_schema", "name", "schema", "strict": true}}`.

```yaml
inference:
  driver:
    artifact: murmur/driver-xai
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

- `type` must be `json_schema`.
- `json_schema.name` must be a non-blank string; `text.format` carries one.
- `json_schema.schema` must be an object.
- `json_schema.strict` is absent or `true`, and is always sent as `true`.
  `strict: false` is refused rather than silently upgraded.
- Any other member of `response_format` or `json_schema` is refused by name,
  since `text.format` has no place for it.

The schema constrains the final text only. Reasoning becomes `thinking` blocks
exactly as it does without a schema. Tool arguments always conform to their
schema on xAI ("the `strict` flag is implicitly always `true`"), so no `strict`
is sent on tools.

## Fixed parameters

These `params` members are never sent (`PROVIDER_REFUSED_PARAMS`):

| Member | Why |
|---|---|
| `background` | A background response is stored at xAI and polled; this driver is stateless with `store: false` |
| `frequency_penalty`, `presence_penalty`, `stop` | The reasoning guide says they return an error on reasoning models, and every model here reasons |
| `max_completion_tokens`, `max_tokens` | The cap goes out once, as `max_output_tokens` |
| `previous_response_id` | Needs a stored response, and cannot be combined with `instructions` |
| `reasoning`, `reasoning_effort` | The effort comes from `reasoning_effort` in the config and the model's effort rule only |
| `search_parameters` | xAI's legacy server-side live search, a surface this driver declines |

A `params` member the driver already sets on the body is never overridden.
`stream` is forced to `true`.

## The output cap

The canonical envelope's `max_tokens` is written as `max_output_tokens`; no
`max_tokens` or `max_completion_tokens` member is emitted. A cap above the
model's `max_output_tokens` is refused before the request:

```text
driver: inference.max_tokens 128001 exceeds driver-xai's max_output_tokens ceiling of 128000 for model 'grok-4.7'
```

128,000 is xAI's default for `max_output_tokens`. It counts only visible text,
not reasoning or function-call tokens, and xAI documents no ceiling above it
("set a larger value to allow longer generations"). Whether xAI accepts a
larger value is pending live check; widening the ceiling is a later release.

## Preserved reasoning across turns

The driver is stateless. Every request carries:

- `store: false`, so nothing is kept at xAI (the default is `true`, with
  responses kept for 30 days);
- `include: ["reasoning.encrypted_content"]`, so each reasoning item comes back
  with its encrypted content;
- the whole history as `input`.

xAI's reasoning guide says grok-4.7 returns `encrypted_content` on every
reasoning item, and that the items should be passed back unchanged in the next
request's `input` so the model keeps its reasoning across turns. The driver
does that:

1. Each reasoning output item of a turn becomes one `thinking` block, in output
   order: `{"type": "thinking", "text": <summary>, "xai_reasoning": <the item
   exactly as xAI sent it>}`. The item is taken from
   `response.output_item.done`, or from the terminal event's `response.output`
   when no item arrived that way, or from `output` on a buffered body.
2. murmur stores the block as returned and sends it back to the same driver and
   model on later turns.
3. On each later request the driver puts the `xai_reasoning` value back into
   `input`, verbatim, before that turn's `message` and `function_call` items,
   when it is an object with `type: "reasoning"` and a non-empty string
   `encrypted_content`. Anything else is dropped without an error.

The block's `text` is the summary deltas streamed for that item, else the
item's `summary[].text` joined with a blank line, else `""`. A block with empty
text and encrypted content is still emitted and replayed. The `text` itself is
never sent back; the item carries its own summary.

A committed compaction replaces the history with a summary in a single `text`
block, so later requests carry no reasoning items. That is the ordinary state,
not an error. murmur also drops `thinking` blocks from a request served by a
different driver or model.

The driver never sends `previous_response_id`: it needs `store: true`, and xAI's
reference says `instructions` "cannot be used alongside
`previous_response_id`". Whether xAI accepts a replayed item, with its `id`,
when `store` is `false` is pending live check.

## Streaming

Requests always carry `stream: true`. xAI publishes no list of Responses event
names. The driver reads OpenAI's, which the Grok Build CLI 1.0.77 was seen
reading from xAI's `/v1/responses` against a mock (2026-11-12):

| Event | What the driver does |
|---|---|
| `response.output_text.delta` | `emit_chunk`, appended to the text |
| `response.reasoning_summary_text.delta`, `response.reasoning_text.delta` | `emit_thinking_chunk`, appended to the reasoning item at its `output_index`. xAI's own sample reads both |
| `response.output_item.added` (`function_call`) | the call's `call_id` and name |
| `response.function_call_arguments.delta` | appended to the call's arguments |
| `response.function_call_arguments.done` | replaces the arguments; authoritative |
| `response.output_item.done` (`reasoning`) | keeps the item verbatim; emits its summary when no delta streamed |
| `response.output_item.done` (`function_call`) | fills a missing call id, name or arguments |
| `response.completed`, `response.incomplete`, `response.failed` | status, `incomplete_details.reason`, `error`, usage, and the reasoning fallback |
| `error` | the provider failure |
| `data: [DONE]` | ends the stream |

Empty deltas are dropped. Nothing scans the text for reasoning tags. A body
whose first non-whitespace byte is `{` or `[` is read as a buffered JSON
response instead.

The assembled `content` is the `thinking` blocks in output order, then the
`text` block when non-empty, then the `tool_call` blocks. Text written beside
tool calls is kept, and goes back as a `message` item.

Whether the summaries stream on each entry, and whether one item ever streams
both event kinds, is pending live check.

## Tool-call progress

While the model is still writing a tool call, the driver reports it to the host
through `murmur:stream/events@0.1.0`:

| Call | Sent on | Carries |
|---|---|---|
| `tool-call-started` | the first event by which the call's `call_id` and name have both arrived | the `call_id` and name, which the returned `tool_call` carries too |
| `tool-call-input-bytes` | each event that grows the call's arguments | the call's argument bytes so far |

No argument text is ever forwarded. Calls are kept by `output_index`, which
counts every output item of the turn. A turn holds at most 1024 output items: an
event naming `output_index` 1024 or more for a function call fails the turn
with `driver: tool call index <n> is past the limit of 1024 per turn` before
anything is stored for it.

## Stop reasons

| xAI terminal status | Murmur `stop_reason` |
|---|---|
| `completed` with no function call | `end_turn` |
| `completed` with a function call | `tool_call` |
| `incomplete`, `incomplete_details.reason: max_output_tokens` | `max_tokens` |

Every other ending is a plain failure naming what arrived:

| Ending | Error message |
|---|---|
| `incomplete` with another reason or none | `driver: xAI Responses turn ended incomplete: <reason or no reason given>` |
| `failed` with no `error` | `driver: xAI Responses turn failed with no error` |
| an unknown status | `driver: unsupported xAI Responses status '<status>'` |
| function-call arguments that are not JSON on a completed turn | `driver: failed to parse xAI function_call arguments JSON for tool '<name>': <error>` |
| function-call arguments cut off by the cap | `driver: xAI Responses turn stopped at the inference.max_tokens output cap; tool call '<name>' was cut off mid-arguments and cannot be run — raise the cap and re-run` |
| a buffered body with no `status` | `driver: xAI Responses response has no status` |

The status is `response.status`, else the terminal event's name.

## Transient failures

murmur makes an inference call again when it fails transiently. It sees the
status and a connection that broke before the response head itself. What goes
wrong after a `2xx` head only the driver sees, so on those failures the driver
adds one reserved tuple to the result's `metadata`: `("transient", "true")`.

The driver marks these `"true"`, each only after xAI answered `2xx`:

| What happened | Error message |
|---|---|
| The SSE stream ended, or the body was empty, before any terminal event | `driver: xAI Responses stream ended with no terminal event` |
| A read of the response body failed | `driver: failed to read response stream: <error>` |
| An `error` event, or a terminal event's `error`, whose `code` or `type` is `overloaded`, `rate_limit_error`, `rate_limit_exceeded`, `server_error` or `service_unavailable` | `xAI error: <code-or-type>: <message>` |

xAI documents no in-stream error codes. That list is the OpenAI-compatible
vocabulary for server load, a rate limit and a server fault.

Everything else carries no `transient` tuple, and murmur alone decides whether
to retry it:

| Failure | Error message |
|---|---|
| Any status `>= 400`, a `429` included | `HTTP <status>: <body>` |
| A read of a `>= 400` body failed | `driver: failed to read response stream: <error>` |
| A connection failure before the response head | murmur's own |
| A provider error with any other code or type, or none | `xAI error: <code-or-type>: <message>`, with `unknown` for a missing part |
| A stream with a line that is not SSE, or a `data:` payload that is not JSON, that then ends with no terminal event | `driver: xAI Responses stream ended with no terminal event` |
| Any ending under [Stop reasons](#stop-reasons) that is not a stop reason | its own message |
| Any refusal before the request is sent | the refusal's own message |

murmur retries a `429` and a `5xx` from the status. xAI returns `429` when a
team exceeds its per-model rate limit
(<https://docs.x.ai/developers/rate-limits.md>, read 2026-11-12 UTC) and
documents no rate-limit headers, no `429` body and no quota body, so nothing is
marked permanent. A provider error outranks a missing terminal event. On a
retry murmur discards everything the failed attempt streamed; the driver never
repeats a request itself.

## Token usage

Every translated response carries an optional top-level `usage` object, read
from the terminal event's `response.usage` or a buffered body's `usage`. Murmur
records the members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `thinking_tokens`.

| `usage` member | xAI fields |
|---|---|
| `input_tokens` | `input_tokens − input_tokens_details.cached_tokens`; omitted when that would go below zero or `input_tokens` is unreported |
| `cached_tokens` | `input_tokens_details.cached_tokens` |
| `output_tokens` | `total_tokens − input_tokens` when both are reported and the difference is not negative, else `output_tokens` |
| `thinking_tokens` | `output_tokens_details.reasoning_tokens`, kept only when it is at most that `output_tokens` |

The completion is counted the way xAI bills it. The Responses reference's
example reads `input 32, cached 8, output 9, reasoning 110, total 151`
(spelled `prompt_tokens` and `completion_tokens` on that example): reasoning is
counted beside `output_tokens` there, while murmur's contract makes
`thinking_tokens` a subset of `output_tokens`. Taking the completion from
`total − input` is right either way:

| Reported (input / cached / output / reasoning / total) | Sent |
|---|---|
| 32 / 8 / 9 / 110 / 151 (Responses reference) | `input_tokens 24, cached_tokens 8, output_tokens 119, thinking_tokens 110` |
| 125 / 98 / 48 / 0 / 173 (caching guide) | `input_tokens 27, cached_tokens 98, output_tokens 48, thinking_tokens 0` |

Which way real responses count is pending live check. Each member is
independently optional and omitted, never zero-filled, when xAI did not report
it; a reported `0` is kept. `cost_in_usd_ticks`, xAI's own cost figure, is not
read or reported: murmur's contract has no cost member.

## Prompt cache key

Murmur puts a reserved top-level `prompt_cache_key` on every driver request,
constant for every turn of a task. The driver forwards it, trimmed, as the
body's `prompt_cache_key` when it is a non-blank string, and sends nothing
otherwise. xAI's reference says it is "Plumbed to x-grok-conv-id", and xAI's
caching guide
(<https://docs.x.ai/developers/advanced-api-usage/prompt-caching/usage-and-pricing.md>,
read 2026-11-12 UTC) recommends setting it when `cached_tokens` stays `0`.

## API surfaces this driver declines

| Surface | Why |
|---|---|
| Chat Completions | returns no reasoning content and has no field for the encrypted reasoning |
| The Anthropic-compatible surface | would mean an Anthropic-shaped body to reach the same models |
| Server-side tools (web search, X search, code interpreter, MCP), `search_parameters` | the capsule's own tools are the tool surface |
| `previous_response_id` | needs stored responses, and cannot be combined with `instructions` |
| Batch | not an interactive surface |
| `/v1/responses/compact` | murmur compacts with its own hook |
| Images | xAI documents no per-model image facts yet; an image block is refused |

## Errors

Every fallible function returns a message opening `driver: `, except the
provider's own errors, which open `HTTP <status>: ` or `xAI error: `. Nothing
panics. The `run` wrapper turns an error into a `ToolResult` with
`status: error`, the message as its summary, and
`{"stop_reason":"error","error":…}` as its data, plus the `transient` tuple
described under [Transient failures](#transient-failures) when the failure
carries one.

Refusals before any request is sent, in the order they are checked:

| Check | Message |
|---|---|
| Config is not JSON | `driver: inference.driver.config is not valid JSON: <error>` |
| Config is not an object | `driver: inference.driver.config must be a JSON object` |
| Unknown config key | `driver: unrecognised inference.driver.config key(s): <keys>. Accepted keys: reasoning_effort, response_format` |
| `reasoning_effort` not a string | `driver: inference.driver.config 'reasoning_effort' must be a string, got <value>` |
| `response_format` not an object | `driver: inference.driver.config 'response_format' must be a JSON object, got <value>` |
| `response_format.type` | `driver: inference.driver.config 'response_format.type' must be "json_schema", got <value>` |
| `response_format` extra member | `driver: inference.driver.config 'response_format' has unrecognised member(s): <members>. Accepted members: json_schema, type` |
| `response_format.json_schema` | `driver: inference.driver.config 'response_format.json_schema' must be an object` |
| `json_schema` extra member | `driver: inference.driver.config 'response_format.json_schema' has unrecognised member(s): <members>. Accepted members: name, schema, strict` |
| `json_schema.name` | `driver: inference.driver.config 'response_format.json_schema.name' is required and must be a non-blank string` |
| `json_schema.schema` | `driver: inference.driver.config 'response_format.json_schema.schema' is required and must be an object` |
| `json_schema.strict` not a boolean | `driver: inference.driver.config 'response_format.json_schema.strict' must be a boolean, got <value>` |
| `json_schema.strict: false` | `driver: inference.driver.config 'response_format.json_schema.strict' must be true — strict: false is refused rather than silently upgraded, because a schema the provider does not enforce is not the guarantee this key offers` |
| No endpoint | `driver: missing MURMUR_INFERENCE_ENDPOINT` |
| Blank endpoint | `driver: MURMUR_INFERENCE_ENDPOINT is set but empty` |
| No request data | `driver: missing tool-input.data` |
| Request data unreadable | `driver: failed to parse tool-input.data: <error>` |
| Model | `driver: model '<id>' is not supported by driver-xai. Supported models: grok-4.7, grok-4.6, grok-build-0.1.` |
| Cap | `driver: inference.max_tokens <n> exceeds driver-xai's max_output_tokens ceiling of <c> for model '<id>'` |
| Effort the model does not take | `driver: inference.driver.config 'reasoning_effort' must be one of low, medium, high, xhigh for model '<id>', got "<value>"` |
| Effort on `grok-build-0.1` | `driver: inference.driver.config 'reasoning_effort' is not accepted for model 'grok-build-0.1': xAI documents no reasoning effort for it. Remove the key.` |
| Image block, in any message | `driver: driver-xai sends text only and refuses an image block` |
| Tool turn without its call id | `driver: tool message is missing required field 'tool_call_id'` |
| Any other role | `driver: unsupported message role '<role>'` |
| Endpoint scheme | `driver: endpoint must start with http:// or https://: '<url>'` |
