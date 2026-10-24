# driver-openai

OpenAI inference driver for Murmur agent capsules. It accepts the OpenAI models
listed in [Endpoints](#endpoints), sends each one to the Responses or the Chat
Completions API as that table says, and refuses every other model.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the OpenAI API,
including SSE streaming. Optional stateful continuation via
`previous_response_id` is gated behind an explicit `inference.driver.config`
store grant.

The driver reads no provider key and sends no authentication header. Murmur's
runtime authenticates each call with the header declared under
`upstream_auth:` in [murmur.yaml](./murmur.yaml), the full manifest; the
capsule author supplies the key as `gateway.api_key` on the driver's
`artifacts:` entry, as a reference such as `${OPENAI_API_KEY}`. An entry that
sets neither `api_key` nor `keyless: true` fails the launch with
`E-CAP-018`.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable.
The runtime sets it to its own loopback gateway plus the path of the entry's
`gateway.endpoint`, and readdresses each request to the real upstream. The
driver carries no provider URL of its own to fall back on: a value that never
arrives fails the call with `driver: missing MURMUR_INFERENCE_ENDPOINT`, and one
that arrives empty or whitespace-only with `driver: MURMUR_INFERENCE_ENDPOINT is
set but empty`. Neither reaches the network. Requests go to
`<endpoint>/responses` or `<endpoint>/chat/completions`, by model.

## Endpoints

`MODEL_ROUTES` in `src/lib.rs` is the one table the driver consults about a
model. It holds every model this driver accepts, in the order OpenAI's
[all-models page](https://developers.openai.com/api/docs/models/all) lists them,
with the endpoint the driver appends to `MURMUR_INFERENCE_ENDPOINT` for it. The
id is matched exactly: no trimming, no prefix match, and no mapping of a dated
snapshot to its model.

| Model | Endpoint |
|---|---|
| `gpt-6-astra` | `responses` |
| `gpt-6.1-sol` | `responses` |
| `gpt-6-luna` | `responses` |
| `gpt-5.6-cyber` | `responses` |
| `gpt-oss-120b` | `responses` |
| `gpt-oss-20b` | `responses` |
| `gpt-6-sol` | `responses` |
| `gpt-5.6-sol` | `responses` |
| `gpt-5.6-terra` | `responses` |
| `gpt-5.6-luna` | `responses` |
| `gpt-5.5` | `responses` |
| `gpt-5.5-pro` | `responses` |
| `gpt-5.4` | `responses` |
| `gpt-5.4-pro` | `responses` |
| `gpt-5.4-mini` | `responses` |
| `gpt-5.2` | `responses` |
| `gpt-5.2-pro` | `responses` |
| `chat-latest` | `chat/completions` |

Each endpoint is one the model's own page lists as "Supported" in its
`## Endpoints` table (read 2026-10-24; the [Model limits](#model-limits) Source
cells quote both rows):

| The model's page lists | Endpoint | Models |
|---|---|---|
| Responses only | `responses` | `gpt-5.6-cyber`, `gpt-oss-120b`, `gpt-oss-20b`, `gpt-5.5-pro`, `gpt-5.4-pro`, `gpt-5.2-pro` |
| both | `responses` for the `gpt-5.2`-and-later models, `chat/completions` for `chat-latest` | the rest |

A test fails if this table, `MODEL_ROUTES`, `murmur.yaml`'s `models:` and the
[Model limits](#model-limits) table do not hold the same ids, or if a row's
endpoint differs from the one `MODEL_ROUTES` gives it.

## Model limits

`murmur.yaml` declares limits under `models:` for every model in
[Endpoints](#endpoints), and for no other. A model is in that table when
OpenAI's all-models page lists it without marking it deprecated, it generates
text, and its own page lists the endpoint the driver sends it to.

| Model | max_output_tokens | context_window | max_input_tokens | Source |
|---|---|---|---|---|
| `gpt-6-astra` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-6-astra>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-6.1-sol` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-6.1-sol>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-6-luna` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-6-luna>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.6-cyber` | 128000 | 400000 | 272000 | <https://developers.openai.com/api/docs/models/gpt-5.6-cyber>, read 2026-10-24: "400,000 context window", "128,000 max output tokens", "Maximum input tokens: 272,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Not supported", Responses (`v1/responses`): "Supported" |
| `gpt-oss-120b` | 131072 | 131072 | none | <https://developers.openai.com/api/docs/models/gpt-oss-120b>, read 2026-10-24: "131,072 context window", "131,072 max output tokens"; Chat Completions (`v1/chat/completions`): "Not supported", Responses (`v1/responses`): "Supported" |
| `gpt-oss-20b` | 131072 | 131072 | none | <https://developers.openai.com/api/docs/models/gpt-oss-20b>, read 2026-10-24: "131,072 context window", "131,072 max output tokens"; Chat Completions (`v1/chat/completions`): "Not supported", Responses (`v1/responses`): "Supported" |
| `gpt-6-sol` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-6-sol>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.6-sol` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-5.6-sol>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.6-terra` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-5.6-terra>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.6-luna` | 128000 | 1050000 | 922000 | <https://developers.openai.com/api/docs/models/gpt-5.6-luna>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens", "Maximum input tokens: 922,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.5` | 128000 | 1050000 | none | <https://developers.openai.com/api/docs/models/gpt-5.5>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens"; Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.5-pro` | 128000 | 1050000 | none | <https://developers.openai.com/api/docs/models/gpt-5.5-pro>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens"; Chat Completions (`v1/chat/completions`): "Not supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.4` | 128000 | 1050000 | none | <https://developers.openai.com/api/docs/models/gpt-5.4>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens"; Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.4-pro` | 128000 | 1050000 | none | <https://developers.openai.com/api/docs/models/gpt-5.4-pro>, read 2026-10-24: "1,050,000 context window", "128,000 max output tokens"; Chat Completions (`v1/chat/completions`): "Not supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.4-mini` | 128000 | 400000 | 272000 | <https://developers.openai.com/api/docs/models/gpt-5.4-mini>, read 2026-10-24: "400,000 context window", "128,000 max output tokens", "Maximum input tokens: 272,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.2` | 128000 | 400000 | none | <https://developers.openai.com/api/docs/models/gpt-5.2>, read 2026-10-24: "400,000 context window", "128,000 max output tokens"; Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |
| `gpt-5.2-pro` | 128000 | 400000 | none | <https://developers.openai.com/api/docs/models/gpt-5.2-pro>, read 2026-10-24: "400,000 context window", "128,000 max output tokens"; Chat Completions (`v1/chat/completions`): "Not supported", Responses (`v1/responses`): "Supported" |
| `chat-latest` | 128000 | 400000 | 272000 | <https://developers.openai.com/api/docs/models/chat-latest>, read 2026-10-24: "400,000 context window", "128,000 max output tokens", "Maximum input tokens: 272,000" (input ceiling below the window); Chat Completions (`v1/chat/completions`): "Supported", Responses (`v1/responses`): "Supported" |

`murmur.yaml` declares the same ids and numbers under `models:`, each
`max_input_tokens` cell as that model's `max_input_tokens` and each `none` as
no such key, and a test fails if the two disagree or if either holds an id
outside `MODEL_ROUTES`.

OpenAI documents "Maximum input tokens" below the context window for several of
these models: `922000` against a `1050000` window for the `gpt-6` and
`gpt-5.6-sol`/`-terra`/`-luna` models, and `272000` against `400000` for
`gpt-5.6-cyber`, `gpt-5.4-mini` and `chat-latest`. A request whose input
exceeds that ceiling is refused even though the window has room.
`context_window` is still the documented window, and `max_input_tokens`
declares the ceiling. A model whose page gives no "Maximum input tokens",
`gpt-oss-120b` and `gpt-oss-20b` among them, declares none.

murmur compacts against one input budget per call:
`min(max_input_tokens, context_window − output cap)`.
`inference.compaction.threshold` and `context.seed_budget` are fractions of that
budget. With the default output cap, the model's `max_output_tokens`:

| Input budget | Basis | Models |
|---|---|---|
| 922000 | `max_input_tokens`; the ceiling equals `1050000 − 128000`, and a tie goes to the ceiling | each model whose `max_input_tokens` is 922000 |
| 272000 | `max_input_tokens`; the ceiling equals `400000 − 128000` | each model whose `max_input_tokens` is 272000 |
| 922000 | `1050000 − 128000` | `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro` |
| 272000 | `400000 − 128000` | `gpt-5.2`, `gpt-5.2-pro` |
| 0 | `131072 − 131072`; murmur refuses the launch with `E-RUN-055` | `gpt-oss-120b`, `gpt-oss-20b` |

With the default cap every declared ceiling equals `window − 128000`, so the
budget's value is the same with or without the ceiling; only its basis differs.
On `gpt-oss-120b` and `gpt-oss-20b` the default cap equals the whole window, so
a capsule on either sets an `inference.max_tokens` below `131072`; `8192` gives
an input budget of `131072 − 8192 = 122880`.
An `inference.max_tokens` the capsule sets changes the `window − output cap`
term, and the ceiling still bounds the budget: on `gpt-5.6-luna` an
`inference.max_tokens` of `8192` gives `min(922000, 1041808) = 922000`.

Whether the runtime reads `models:` depends on the murmur release:

| murmur | What it does with `models:` | What the capsule sets |
|---|---|---|
| v0.7.0 and earlier | ignores it; an unknown manifest key is a `W-SEC-019` warning, never a refusal | `inference.max_tokens` and `context.max_tokens`; left unset, the runtime's own defaults apply, not the model's limits |
| the first release after v0.7.0 | defaults the output cap and context window from it when the capsule leaves them unset, reads `max_input_tokens` as the input ceiling, measures compaction and the seed budget against the input budget, and caps a hook's `run-inference` (e.g. `hook-compact`) at its `max_output_tokens` | nothing, unless it wants tighter limits than the table |

A capsule value always wins: the runtime never clamps or raises
`inference.max_tokens` or `context.max_tokens` to the table.

## Models this driver refuses

`route` refuses every id outside `MODEL_ROUTES`, before a request body is built,
with one message naming the model and the accepted ones:

```text
driver: model 'o3-pro' is not supported by driver-openai. Supported models: gpt-6-astra, gpt-6.1-sol, gpt-6-luna, gpt-5.6-cyber, gpt-oss-120b, gpt-oss-20b, gpt-6-sol, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna, gpt-5.5, gpt-5.5-pro, gpt-5.4, gpt-5.4-pro, gpt-5.4-mini, gpt-5.2, gpt-5.2-pro, chat-latest.
```

| Refused | Why (read 2026-10-24) |
|---|---|
| `gpt-5`, `gpt-5-mini`, `gpt-5-nano`, `gpt-5-pro`, `o3`, `o3-pro` | the all-models page marks each "Deprecated.", and each model page reads "Status: Deprecated" |
| every other model the all-models page marks "Deprecated." or "Shut down.", such as `gpt-5.1`, `gpt-4.1`, `gpt-4o` or `o4-mini` | deprecated or shut down |
| dated snapshot ids, such as `gpt-5.2-2025-12-11` or `o3-2025-04-16` | only each model's own id is in the table; a snapshot is never mapped to its model |
| `gpt-rosalind-research` | it has no model page giving its limits or endpoints; the all-models page links it to pricing only, for "approved organizations" |
| audio, realtime, transcription, speech, image, embedding and moderation models, such as `gpt-audio-1.5` or `gpt-realtime-2.1` | they do not generate text on an endpoint this driver calls |
| any other id, including an OpenAI-compatible provider's ids such as `llama3.1:8b` | not on OpenAI's all-models page. Z.ai's GLM models have their own driver, `driver-zai` |

None of these has a `models:` entry. A murmur release after v0.7.0 checks for
one at launch, before the driver sees a request; on v0.7.0 and earlier every
row launches and the driver refuses the first inference call:

| Capsule sets | Result |
|---|---|
| neither `inference.max_tokens` nor `context.max_tokens` | the launch is refused with `E-RUN-052`, naming the driver and the model, before any HTTP call |
| only `inference.max_tokens` | it launches with `W-RUN-009`, and the driver refuses the first inference call with the message above |
| both | it launches, and the driver refuses the first inference call with the message above |

No refused model reaches OpenAI.

## Inference dials

`inference.driver.config` is this driver's operator surface. Four keys are read; any other key
fails the first inference call, before any request is dispatched.

```yaml
inference:
  driver:
    artifact: murmur/driver-openai
    config: { thinking: enabled, reasoning_effort: high }
```

| Key | Type | Accepted values | Default | Effect |
|---|---|---|---|---|
| `store` | boolean | `true` / `false` | `false` | Server-side response retention, and with it the `previous_response_id` continuation feature. |
| `thinking` | string | `enabled` / `disabled`, trimmed and case-insensitive | `disabled` | `enabled` asks the provider for a reasoning summary. |
| `reasoning_effort` | string | forwarded verbatim — the provider decides which tiers exist | absent | The quality-against-cost dial on a reasoning model. |
| `verbosity` | string | forwarded verbatim | absent | Output verbosity. |

`thinking` and `reasoning_effort` are spelled as `driver-anthropic` and
`driver-deepseek` spell them, so one concept keeps one name across the fleet.

### Where each dial lands

The endpoint a model is sent to (see [Endpoints](#endpoints)) decides which
dials it can carry. A dial the endpoint has no field for is dropped.

| Key | `responses` | `chat/completions` (`chat-latest`) |
|---|---|---|
| `thinking: enabled` | `reasoning.summary = "auto"` | dropped |
| `reasoning_effort` | `reasoning.effort` | dropped |
| `verbosity` | `text.verbosity` | dropped |
| `store` | `store` | not applicable |

A dial set here overwrites a same-named key supplied under `params`, and a `reasoning` object
supplied under `params` is replaced wholesale rather than merged.

Reasoning summaries arrive only on the Responses surface. `chat-latest` documents no reasoning,
so a capsule on it cannot display reasoning, whatever `thinking` is set to. The key is still
accepted there rather than rejected, so one manifest survives a model switch; it simply asks for
nothing.

### An unrecognised key is an error

A key this driver does not read fails the call with the offending keys and the accepted set:

```
driver: unrecognised inference.driver.config key(s): beta_features, thinking_budget_tokens. Accepted keys: reasoning_effort, store, thinking, verbosity
```

A value this driver defines the meaning of is checked too:

```
driver: inference.driver.config 'thinking' must be "enabled" or "disabled", got "on"
driver: inference.driver.config 'reasoning_effort' must be a string, got 3
```

`reasoning_effort: xhigh` is accepted and forwarded: only the provider knows which tiers exist,
so a tier OpenAI ships tomorrow needs no rebuild of this artifact.

`store` is the one carve-out. It keeps the lenient parse it has always had — `store: "true"` as
a string is read as opt-out with no error — so a capsule that relies on that behaviour is
unchanged.

### `inference.driver.config` is not a scratch block

Murmur delivers `inference.driver.config` to the driver, to every WASM tool and to every shell
tool in the session, so a key parked there for one of those reaches this driver too — and is now
rejected. A per-artifact setting belongs on that artifact's own `config:` block, which arrives as
`MURMUR_ARTIFACT_CONFIG`:

```yaml
artifacts:
  - name: my-tool
    config: { my_setting: value }
```

## Tool-call progress

While the model is still writing a tool call, the driver reports it to the host through
`murmur:stream/events@0.1.0`:

| Surface | `tool-call-started` sent on | `tool-call-input-bytes` sent on | Id |
|---|---|---|---|
| Responses | `response.output_item.added` with a `function_call` item | each `response.function_call_arguments.delta`; `.done` only when its arguments are longer than the deltas' total | the item's `call_id`, never its `fc_…` `id` |
| Chat Completions | the first `delta.tool_calls[]` fragment by which the call's `id` and `function.name` have both arrived | each fragment that grows the call's `function.arguments` | `tool_calls[].id` |

The id and name are the ones the returned `tool_call` block carries.

The size is a running UTF-8 byte total of the call's argument text. It only rises, and its last
value is the byte length of the text parsed into the returned `input`. No argument text is ever
forwarded: the host learns a call's id, name and size, and the arguments reach it only in the
returned tool call. On `transport: http`, the host writes these as `tool-call-started` and
`tool-call-progress` frames ahead of the call's `artifact`.

A Chat Completions call whose `id` the provider has not sent yet is not reported. Once it arrives,
the start and the size so far go out together. A call that never gets an id, or a Responses call
with an empty `call_id`, is not reported at all, and is returned as before.

The driver imports `murmur:stream/events@0.1.0`, which murmur serves from v0.6.0; an older murmur
cannot load it. `mur` 0.7.0 or later refuses a release of this driver built before that
interface (0.8.0 and earlier) at launch with `E-WIT-LEVEL-001`: it imports
`murmur:text/chunks@0.1.0`, and WIT level 1 has no version of `murmur:text`.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | Chat Completions | Responses |
|---|---|---|
| `input_tokens` | `usage.prompt_tokens` − `cached_tokens` − `cache_write_tokens` | `usage.input_tokens` − `cached_tokens` − `cache_write_tokens` |
| `output_tokens` | `usage.completion_tokens` | `usage.output_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` | `usage.input_tokens_details.cached_tokens` |
| `cache_write_tokens` | `usage.prompt_tokens_details.cache_write_tokens` | `usage.input_tokens_details.cache_write_tokens` |

`input_tokens` is the request tokens neither read from nor written to the cache;
`cached_tokens` and `cache_write_tokens` sit beside it, so
`input_tokens + cached_tokens + cache_write_tokens` is OpenAI's total request
count. OpenAI reports that total with both cache counts inside it, so the driver
subtracts them once, after merging every streamed event; an unreported cache
count is subtracted as `0`. A report whose cache counts exceed its total is
inconsistent: `input_tokens` is omitted rather than invented, and the other
members are still reported.

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

## Stop reasons on the Responses surface

The Responses API reports a turn the output cap cut short as
`status: "incomplete"` with `incomplete_details.reason: "max_output_tokens"`.
What the driver returns for that turn depends on what the cap landed in.

| Responses turn | `stop_reason` | Response carries |
|---|---|---|
| `completed`, no tool call | `end_turn` | `content`, `usage` |
| `completed`, tool call with parseable `arguments` | `tool_call` | `content`, `usage` |
| `incomplete` / `max_output_tokens`, no tool call | `max_tokens` | the partial `content`, `usage` |
| `incomplete` / `max_output_tokens`, tool call with parseable `arguments` | `max_tokens` | the tool call, `usage` |
| `incomplete` / `max_output_tokens`, tool call with truncated `arguments` | `error` | `error` only |
| `completed`, tool call with malformed `arguments` | `error` | `error` only |

A tool call whose `arguments` the cap cut in half cannot be run, so the turn is
an error rather than a capped turn — a capped turn is a fragment the runtime
records as a result, which would let an unrunnable tool call pass for an answer.
The message names the cap the capsule author set, not the JSON syntax that
truncation produced:

```
driver: OpenAI Responses turn stopped at the inference.max_tokens output cap; tool call 'delegate-task' was cut off mid-arguments and cannot be run — raise the cap and re-run
```

Raise `inference.max_tokens` in `murmur.yaml` and re-run. Both the streaming and
the non-streaming path emit this message byte-for-byte identically. When several
tool calls arrive in one turn, the message names the first whose `arguments`
fail to parse; a call whose name never arrived is named `'<unnamed>'`. The
truncated `arguments` are not echoed into the message, and the response carries
no `content` and no `usage`.

A tool call malformed for any reason other than the cap keeps reporting the
parse failure — `driver: failed to parse Responses function_call arguments
JSON: …` — so a genuine provider fault is not mistaken for a cap.

The Chat Completions surface maps `finish_reason: "length"` to the same
`max_tokens` stop reason, and hard-errors on unparseable tool-call arguments
with `driver: failed to parse OpenAI tool call arguments JSON: …`.

## A missing stop signal is refused

A turn that never says why it stopped is refused rather than read as finished.
On Chat Completions, a `finish_reason` that is absent, `null`, not a string or
empty counts as none. On Responses, a stream is finished only by a
`response.completed`, `response.incomplete` or `response.failed` event.

| Surface and path | Refused with |
|---|---|
| Chat Completions, JSON body whose first choice has no `finish_reason` | `driver: OpenAI response has no finish_reason` |
| Chat Completions, SSE stream in which no chunk carries a `finish_reason` | `driver: OpenAI stream ended with no finish_reason` |
| Responses, JSON body whose `status` is absent, `null` or not a string | `driver: OpenAI Responses response has no status` |
| Responses, SSE stream that ends with no terminal event | `driver: OpenAI Responses stream ended with no terminal event` |

A Responses `error` object is still reported first, as `OpenAI Responses error:
<message>`. The refusal is returned as `{"stop_reason":"error","error":"<message>"}`,
so the task ends failed. A connection dropped before the final chunk is the
common cause; text already streamed is not withdrawn, it is just not recorded as
a finished reply.

A stream that breaks off with a read error instead, such as a body cut short of
its declared `content-length`, fails the turn on either surface with
`driver: failed to read response stream: <error>`, returned the same way.

A failed task status and a `task_failed` trace line with cause `driver_error`
need a murmur runtime released after v0.4.0. On v0.4.0 and earlier the runtime
records the error but still reports the task as `ok` and exits 0.

## A tool call index past 1024 is refused

On Chat Completions, a turn holds at most 1024 tool calls: a streamed chunk
numbers each call with an `index`, and one naming index 1024 or more fails the
turn with `driver: tool call index <n> is past the limit of 1024 per turn`
before anything is stored for it or the call is reported. A chunk whose call has
no `index` is read as index 0.

The Responses surface keys its calls by `output_index` and has no such limit.
