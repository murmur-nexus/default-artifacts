# driver-openai-compatible

Inference driver for Murmur agent capsules that run on a self-hosted OpenAI
compatible server — vLLM, or anything else that speaks `/v1/chat/completions`
and `/v1/completions` — with whatever model that server serves.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the OpenAI-shaped
Chat Completions and Completions APIs, including SSE streaming, a full
tool-calling round trip on the chat surface, constrained output through
`structured_outputs`, and one plain prompt string on the completions surface.

The driver reads no key and sends no authentication header. Murmur's runtime
authenticates each call with the header declared under `upstream_auth:` in
[murmur.yaml](./murmur.yaml), the full manifest, when the capsule supplies a key
as `gateway.api_key` on the driver's `artifacts:` entry; with
`gateway.keyless: true` no credential header is sent at all.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable.
The runtime sets it to its own loopback gateway plus the path of the entry's
`gateway.endpoint`, and readdresses each request to the real upstream. The
driver carries no server URL of its own to fall back on: a value that never
arrives fails the call with `driver: missing MURMUR_INFERENCE_ENDPOINT`, and one
that arrives empty or whitespace-only with `driver: MURMUR_INFERENCE_ENDPOINT is
set but empty`. Neither reaches the network. Requests go to
`<endpoint>/chat/completions` or `<endpoint>/completions`, by
[surface](#surfaces), with any trailing `/` on the endpoint dropped first.

## Using it from a capsule manifest

```yaml
artifacts:
  - name: murmur/driver-openai-compatible
    version: "0.1.0"
    runtime: driver
    gateway:
      endpoint: http://127.0.0.1:8000/v1   # loopback http, or any https URL
      keyless: true                        # or api_key: ${VLLM_API_KEY} for `vllm serve --api-key`

inference:
  transport: http
  model: llama31-8b-instruct               # whatever `GET /v1/models` lists; passed through unchanged
  max_tokens: 1024                         # required: the driver declares no model limits
  driver:
    artifact: murmur/driver-openai-compatible
    config:
      surface: chat                        # or completions
      temperature: 1
      structured_outputs: { choice: ["accept", "reject"] }

context:
  max_tokens: 8192                         # the server's --max-model-len; without it compaction does not run
```

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — any id the server serves, see [Model scope](#model-scope) |
| `inference.max_tokens` | yes | — without it murmur refuses the launch with `E-RUN-052`: the driver declares no `models:`, so there is no output cap to fall back on |
| `context.max_tokens` | strongly advised | — without it murmur warns `W-RUN-009` at launch, knows no context window, does not compact, and refuses an `on-task-start` seed. Set it to the server's context length (vLLM: `--max-model-len`) |
| `gateway.endpoint` | yes | — the server's OpenAI base URL, usually ending in `/v1` |
| `gateway.keyless: true` or `gateway.api_key` | one of them | — `keyless: true` for a server started without a key; `api_key` for one started with one (vLLM: `--api-key`). Neither is `E-CAP-018`; both is `E-MAN-003` |
| `inference.driver.config` | no | `{}` — see [Config keys](#config-keys) |

`gateway.endpoint` takes `https://` to any host and `http://` only to
`localhost` or a loopback literal such as `127.0.0.1`; any other `http://` host
is refused at load with `E-MAN-003`. A server on another machine needs TLS in
front of it (a reverse proxy with a certificate) and an `https://` endpoint, or
an SSH tunnel that brings its port to loopback:

```sh
ssh -N -L 8000:127.0.0.1:8000 gpu-host     # then endpoint: http://127.0.0.1:8000/v1
```

## Model scope

The driver serves any model id the server serves. `inference.model` goes to the
server as the body's `model` byte for byte — never trimmed, case-folded or
matched against a list — so it must be one of the ids the server's
`GET <endpoint>/models` lists (vLLM: the `--served-model-name`, or the model
path when that flag is not given).

There is no model table in the source and no `models:` block in `murmur.yaml`.
The hosted drivers in this repository (driver-openai, driver-huggingface,
driver-baseten and others) accept an exact list of ids because each id there is
a fixed product whose output cap and context window the provider publishes, and
the driver declares those numbers to murmur. A self-hosted server serves
whatever its operator chose to load, under whatever name the operator gave it,
with whatever context length the operator set: the driver cannot know any of
it, so the capsule states the limits itself with `inference.max_tokens` and
`context.max_tokens`. `max_tokens` is sent exactly as the envelope gives it,
with no ceiling of the driver's own; the server refuses one larger than the
model allows.

An empty or whitespace-only id is refused before any request is built:

```text
driver: inference.model is empty: driver-openai-compatible sends the model id to the server unchanged, so it must be an id the server serves (its GET /models lists them)
```

A current `mur` refuses a blank `inference.model` itself when it loads the
manifest (`E-MAN-003 … 'inference.model': missing required field`), so this
message appears only when a runtime hands the driver an empty id anyway.

A server that refuses the model — its error's `param` is `"model"`, or it
answers `404` with a message naming the id — fails the call with:

```text
driver: the server refused model '<model>': HTTP <status>: <body> — driver-openai-compatible sends inference.model unchanged; use an id the server's GET /models lists (vLLM: --served-model-name)
```

Any other `404` means the request reached no route at all, which is almost
always a `gateway.endpoint` missing its `/v1`:

```text
HTTP 404: <body> — driver: no route at <path>; gateway.endpoint must be the server's OpenAI base URL, usually ending in /v1
```

`<path>` is the path the request was sent to, such as `/v1/chat/completions`.

## Surfaces

`surface` in `inference.driver.config` chooses the endpoint.

| Surface | Request | Carries |
|---|---|---|
| `chat` (default) | `POST <endpoint>/chat/completions` | the system prompt and the messages, tool definitions and a full tool-call round trip, reasoning shown as thinking |
| `completions` | `POST <endpoint>/completions` | one `prompt` string; text only |

**Chat** sends `model`, `max_tokens`, `messages`, `tools` when the runtime
offers any, the configured `temperature` and `structured_outputs`, then
`stream: true` and `stream_options: {"include_usage": true}`. The system prompt,
trimmed, is the first message. An assistant turn goes back with its text and its
tool calls (`content: null` when it carried no text); a `thinking` block is
never sent back. A tool result goes back as a `tool` message with its
`tool_call_id`. The driver sends no `tool_choice`, `parallel_tool_calls`,
`max_completion_tokens` or `reasoning_effort`, so the server's defaults apply.

**Completions** sends `model`, `max_tokens`, `prompt`, the configured
`temperature` and `structured_outputs`, then the same two streaming flags. The
`prompt` is the system text, trimmed, when it is non-empty, then the text of each
message that has any, in order, joined by a blank line (`"\n\n"`). Message text
is not trimmed, so the prompt ends with the task's last bytes. With the system
prompt `You are player two.` and the task `Offer: 3 of 10. You choose option <<`,
the wire carries:

```json
{"prompt": "You are player two.\n\nOffer: 3 of 10. You choose option <<"}
```

and a model trained to continue after `<<` (Minitaur) sees exactly that.
Thinking blocks are left out. Tool definitions are not sent: the runtime offers
every capsule at least the `session-status` tool, and on this surface the model
cannot see or call it. A history that holds a tool call or a tool result is
refused before any request, `<index>` counting the request's messages from 0:

```text
driver: the completions surface carries no tool calls, but message <index> holds a <tool call|tool result>; use surface: chat for a model that calls tools
```

Pick `chat` for an instruction-tuned model with a chat template, and whenever the
model should call tools. Pick `completions` for a base model, or a model trained
on a raw prompt format of its own, where a chat template would get in the way.

## Config keys

`inference.driver.config` accepts three keys, all optional. Every other key —
`extra_body`, `guided_choice` and `reasoning_effort` included — is refused by
name on the first inference call, before any request:

```text
driver: unrecognised inference.driver.config key(s): <keys, sorted>. Accepted keys: structured_outputs, surface, temperature
```

| Key | Accepted values | Default | Body field |
|---|---|---|---|
| `surface` | `"chat"` or `"completions"`, trimmed, any case | `chat` | none; it picks the endpoint |
| `temperature` | a number at least `0`, no upper bound | not sent: the server's default | `temperature`, exactly as written (`1` stays `1`) |
| `structured_outputs` | a non-empty object; its `choice`, when present, a non-empty array of strings | not sent | `structured_outputs`, verbatim, at the top level, on either surface |

An absent config, `{}` and a whitespace-only one take the defaults. The
refusals:

| Config | Refused with |
|---|---|
| not JSON | `driver: inference.driver.config is not valid JSON: <error>` |
| JSON but not an object | `driver: inference.driver.config must be a JSON object` |
| `surface` anything else | `driver: inference.driver.config 'surface' must be "chat" or "completions", got <value>` |
| `temperature` negative or not a number | `driver: inference.driver.config 'temperature' must be a number at least 0, got <value>` |
| `structured_outputs` empty or not an object | `driver: inference.driver.config 'structured_outputs' must be a non-empty object, got <value>` |
| `structured_outputs.choice` empty, or not all strings | `driver: inference.driver.config 'structured_outputs.choice' must be a non-empty array of strings, got <value>` |

`<value>` is the value as the capsule wrote it, in JSON.

### `temperature`

Left unset, no `temperature` is sent and the server's default applies. On vLLM
that default comes from the model's `generation_config.json` when the model
ships one, not from the OpenAI default of `1`: `--generation-config` defaults to
`auto`, which loads it from the model path (`vllm/config/model.py`, `main`,
read 2026-11-26). A capsule that wants sampling at `1` — the ultimatum swarm's
twins did — sets `temperature: 1` explicitly.

### `structured_outputs`

Forwarded verbatim as the body's top-level `structured_outputs`, on both
surfaces. vLLM reads it as its structured-outputs request field and constrains
generation with it (<https://docs.vllm.ai/en/latest/features/structured_outputs/>,
read 2026-11-26):

| Member | Constrains the answer to |
|---|---|
| `choice` | exactly one of the listed strings: `{choice: ["A", "B"]}` answers `A` or `B` |
| `regex` | text matching the regular expression |
| `json` | JSON matching the schema |
| `grammar` | text in the context-free grammar |

The driver checks only `choice`'s shape; every other member is the server's to
read, and vLLM refuses a request that sets more than one of `json`, `regex` and
`choice`.

vLLM ignores a body field it does not know without an error: the request
answers `200` with unconstrained output. Its older `guided_choice`,
`guided_json`, `guided_regex` and `guided_grammar` fields were removed in
v0.12.0 and are among those ignored (the same page; vLLM logs a warning for
them, and nothing at all for other unknown fields). That is why this driver has
no free-form body pass-through and refuses `guided_choice` by name. Another
OpenAI compatible server may ignore `structured_outputs` itself, in which case
the answer is not constrained; check one response by hand.

## Streaming

Requests are always sent with `stream: true` and
`stream_options: {"include_usage": true}`, stamped together after everything
else and overriding anything from `params`. SSE lines are parsed incrementally:

| Surface | `emit_chunk` | `emit_thinking_chunk` | Tool calls |
|---|---|---|---|
| `chat` | `delta.content` | `delta.reasoning`, else `delta.reasoning_content` | `delta.tool_calls`, accumulated by `index` |
| `completions` | `choices[0].text` | — | — |

Servers differ in which reasoning field they use; a chunk carrying both is read
from `reasoning` alone. `data: [DONE]` ends the stream. Lines that are not
`data: ` lines, and `data:` payloads that are not JSON, are skipped.

A non-streaming JSON fallback is selected by sniffing the first non-whitespace
byte of the response body for `{` or `[`, which is where scripted test servers
and any server that ignores `stream: true` land. It reads
`choices[0].message.{content, reasoning | reasoning_content, tool_calls}` on chat
and `choices[0].text` on completions.

The assembled response's `content` puts the `thinking` block first, then the
`text` block, then the `tool_call` blocks, each only when non-empty. Reasoning
arrives on its own field, so there is no inline reasoning-tag scanner anywhere in
this crate.

## Tool-call progress

While the model is still writing a tool call on the chat surface, the driver
reports it to the host through `murmur:stream/events@0.1.0`:

| Call | Sent on | Carries |
|---|---|---|
| `tool-call-started` | the first `delta.tool_calls[]` fragment by which the call's `id` and `function.name` have both arrived | `tool_calls[].id` and the function name, which the returned `tool_call` carries too |
| `tool-call-input-bytes` | each fragment that grows the call's `function.arguments` | the call's argument bytes so far |

The size is a running UTF-8 byte total of the call's argument text. It only rises, and its last
value is the byte length of the text parsed into the returned `input`. No argument text is ever
forwarded: the host learns a call's id, name and size, and the arguments reach it only in the
returned tool call. On `transport: http`, the host writes these as `tool-call-started` and
`tool-call-progress` frames ahead of the call's `artifact`.

A call whose `id` the server has not sent yet is not reported. Once it arrives, the start and the
size so far go out together. A call that never gets an id is not reported at all, and is returned
as before.

## Stop reasons

| `finish_reason` | Murmur `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `stop`, on chat, with one or more tool calls returned | `tool_call` |
| `length` | `max_tokens` |
| `tool_calls` (chat only) | `tool_call` |
| `abort` | a transient failure, see [Transient failures](#transient-failures) |
| anything else, `tool_calls` on completions and vLLM's `repetition` included | `driver: unsupported finish_reason '<reason>'` |

Some OpenAI compatible servers end a tool-call turn with `stop` rather than
`tool_calls`; reading that as `end_turn` would drop the calls, so on chat a
`stop` beside returned calls is a `tool_call`.

vLLM reports a request its engine aborted with `finish_reason: "abort"`: its
`FinishReason` enum is `STOP`, `LENGTH`, `ABORT`, `ERROR`, `REPETITION`, with
`ABORT` documented as "aborted by client", rendered as `"abort"`, and passed
through unchanged into both surfaces' final chunk
(`vllm/v1/engine/__init__.py`, `vllm/v1/engine/output_processor.py`,
`vllm/entrypoints/serve/utils/tool_calls_utils.py`, vllm-project/vllm `main` at
`4af09371`, read 2026-11-26). The turn did not finish, so the driver asks murmur
to retry it.

A turn that never says why it stopped is refused rather than read as
`end_turn`. A `finish_reason` that is absent, `null`, not a string or empty
counts as none: the JSON fallback refuses it with
`driver: response has no finish_reason`, and a stream with
`driver: stream ended with no finish_reason`.

A `max_tokens` stop does not end the task: murmur starts another turn to let the
model continue. A capsule that wants exactly one short answer — a vote, a
choice — either gives `inference.max_tokens` room for the whole answer, or
constrains the answer with `structured_outputs` so that the model stops with
`stop` inside the cap.

## Transient failures

murmur makes an inference call again when it fails transiently. It sees the
status and a connection that broke before the response head itself. What goes
wrong after a `2xx` head only the driver sees, so on those failures the driver
adds one reserved tuple to the result's `metadata`: `("transient", "true")` asks
murmur to retry. murmur 0.8.0 and later read the key; an earlier murmur ignores
it and fails the call as before.

The driver marks these `"true"`, each only after the server answered `2xx`:

| What happened | Error message |
|---|---|
| The stream ended, or the body was empty, before `data: [DONE]`, even after a `finish_reason` | `driver: stream ended before data: [DONE]` |
| The stream sent `data: [DONE]` with no `finish_reason` before it | `driver: stream ended with no finish_reason` |
| A read of the response body failed | `driver: failed to read response stream: <error>` |
| A stream chunk was an error naming a status murmur retries — `408`, `429`, or a `5xx` other than `501` and `505` — in its `code`, `status` or `status_code` | `driver: server stream error: <error>` |
| A `2xx` JSON body was such an error | `driver: server error: <error>` |
| The turn ended with `finish_reason: "abort"` | `driver: the server aborted the turn (finish_reason 'abort')` |

An error chunk is one with a non-null top-level `error` member (vLLM's
`{"error": {"message", "type", "param", "code"}}`, which it streams as a
`data:` line before `[DONE]`), or one whose top-level `object` is `"error"`
(vLLM's older flat error); `<error>` is that member, or the whole flat chunk,
serialised compactly. An error chunk ends the turn when it arrives, before any
`choices` in it are read.

Nothing is ever marked `"false"`: a self-hosted server has no quota whose
exhaustion would make a retry pointless. Everything else carries no `transient`
tuple, and murmur alone decides from the status or the connection failure:

| Failure | Error message |
|---|---|
| Any status `>= 400` | the model refusal, the route-404 hint, or `HTTP <status>: <body>`, see [Model scope](#model-scope) |
| A read of a `>= 400` body failed | `driver: failed to read response stream: <error>` |
| A connection failure before the response head | murmur's own |
| An error chunk or `2xx` JSON error naming any other status, or none | `driver: server stream error: <error>`, `driver: server error: <error>` |
| An unsupported `finish_reason` | `driver: unsupported finish_reason '<reason>'` |
| A JSON body with no `choices[0]` or no `finish_reason` | `driver: response missing choices[0]`, `driver: response has no finish_reason` |
| A tool call index past the limit, or tool-call arguments that are not JSON | the `driver: ` message under [Errors](#errors) |
| Any refusal before the request is sent | the refusal's own message |

On a retry murmur discards everything the failed attempt streamed. The driver
passes no delay of its own and never repeats a request: one call is one HTTP
request.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
surfaces and both the SSE streaming path and the non-streaming JSON fallback.
Murmur records the members on the `inference` trace event.

| `usage` member | Server field |
|---|---|
| `input_tokens` | `usage.prompt_tokens` − `cached_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `thinking_tokens` | `usage.completion_tokens_details.reasoning_tokens` |

`input_tokens` is the request tokens not read from the cache; `cached_tokens`
sits beside it, so `input_tokens + cached_tokens` is the server's total. An
unreported cache count is subtracted as `0`; a cache count above the total is
inconsistent, and `input_tokens` is then omitted rather than invented. Each
member is independently optional: a count the server did not report is omitted
rather than sent as `0`, and when no member survives the response carries no
`usage` key at all. Streaming usage arrives on a final chunk whose `choices` is
empty, so it is read from any chunk before the `choices` are; the last report of
each member wins.

vLLM reports `prompt_tokens_details.cached_tokens` only when started with
`--enable-prompt-tokens-details`; without it no `cached_tokens` is reported and
`input_tokens` equals the total.

## Prompt cache key

Murmur puts a reserved top-level `prompt_cache_key` on every driver request.
This driver drops it: the servers it reaches define no cache-key field, and vLLM
caches prefixes by content on its own. The field is not declared on the request
struct at all, so serde discards it and it reaches the body at no nesting level.

## Errors

Every fallible function returns a message opening `driver: `, or the
`HTTP <status>: <body>` form for a server status `>= 400`, and nothing panics.
The `run` wrapper turns an error into a `ToolResult` with `status: error`, the
message as its summary, and `{"stop_reason":"error","error":…}` as its data,
plus the `transient` tuple described under
[Transient failures](#transient-failures) when the failure carries one. A
completed turn carries no metadata.

Other refusals before any request:

| Request | Refused with |
|---|---|
| an `image` block in any message | `driver: driver-openai-compatible sends text only and refuses an image block` |
| a `tool` message with no `tool_call_id` (chat) | `driver: tool message is missing required field 'tool_call_id'` |
| a message role other than `user`, `assistant` or `tool` | `driver: unsupported message role '<role>'` |

A turn holds at most 1024 tool calls: a streamed chunk numbers each call with an
`index`, and one naming index 1024 or more fails the turn with
`driver: tool call index <n> is past the limit of 1024 per turn` before anything
is stored for it or the call is reported. A chunk whose call has no `index` is
read as index 0. Tool-call arguments that are not JSON fail the turn with
`driver: failed to parse tool call arguments JSON: <error>`; empty arguments are
read as `{}`.

## Running against vLLM

Start the server on loopback with the name the capsule will use
(vLLM v0.34.0 was the current release on 2026-11-26):

```sh
vllm serve meta-llama/Llama-3.1-8B-Instruct \
  --served-model-name llama31-8b-instruct \
  --host 127.0.0.1 --port 8000 \
  --max-model-len 8192
```

| vLLM flag | Capsule field |
|---|---|
| `--served-model-name <id>` | `inference.model: <id>` |
| `--api-key <key>` | `gateway.api_key: ${VLLM_API_KEY}` in place of `keyless: true`; vLLM then requires `Authorization: Bearer <key>`, which murmur attaches from `upstream_auth:` |
| `--max-model-len <n>` | `context.max_tokens: <n>` |
| `--enable-auto-tool-choice --tool-call-parser <parser>` | the chat surface on a recent vLLM, because the runtime always sends tools (`session-status` at least); see below |
| `--enable-prompt-tokens-details` | `usage.cached_tokens` is reported |

The runtime's request carries `tools` and no `tool_choice`, which vLLM reads as
`tool_choice: "auto"`. vLLM's tool-calling docs
(<https://docs.vllm.ai/en/latest/features/tool_calling/>, read 2026-11-26) call
`--enable-auto-tool-choice` mandatory for auto tool choice, and say vLLM loads
the parser named by `--tool-call-parser` only when that flag is set; the server
refuses to start with one flag and not the other. A chat request with tools to
a server started without them is answered `400` with
`"auto" tool choice requires --enable-auto-tool-choice and --tool-call-parser to be set`
(`vllm/renderers/online_renderer.py`, `main` at `4af09371`). The ultimatum swarm
ran Llama 3.1 on the chat surface of vLLM 0.31 without either flag, with a
driver that sent the same `session-status` tool, and got answers, so older
releases differ here: start the chat server with the flags, or check one chat
request with tools by hand on the version you pin. For Llama 3.1 the docs give:

```sh
  --enable-auto-tool-choice --tool-call-parser llama3_json \
  --chat-template examples/tool_chat_template_llama3.1_json.jinja
```

and note that Llama 3 makes no parallel tool calls. The completions surface sends
no tools, so a completions server needs none of these flags.

Pin the vLLM version a capsule runs against, and after an upgrade check one
response by hand: that the served model name is listed, that
`structured_outputs` still constrains the answer, and that the stream still ends
with a `finish_reason` and `data: [DONE]`:

```sh
curl -s http://127.0.0.1:8000/v1/models
curl -sN http://127.0.0.1:8000/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"llama31-8b-instruct","max_tokens":4,"temperature":1,"stream":true,
       "stream_options":{"include_usage":true},
       "structured_outputs":{"choice":["accept","reject"]},
       "messages":[{"role":"user","content":"Accept or reject an offer of 3 of 10?"}]}'
```
