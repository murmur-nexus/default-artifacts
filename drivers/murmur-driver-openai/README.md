# murmur-driver-openai

OpenAI inference driver for Murmur agent capsules — Chat Completions, with the
Responses API for `gpt-5` and later models.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the OpenAI API,
including SSE streaming. Optional stateful continuation via
`previous_response_id` is gated behind an explicit `inference.driver.config`
store grant.

The driver reads no provider key and sends no authentication header. Murmur's
runtime authenticates each call with the header declared under
`upstream_auth:` in [murmur.yaml](./murmur.yaml), the full manifest; the
capsule author still supplies the key as `inference.api_key`.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable,
resolved from the capsule manifest's `inference.endpoint`. That field is
required, and the driver carries no provider URL of its own to fall back on: a
value that never arrives fails the call with `driver: missing
MURMUR_INFERENCE_ENDPOINT`, and one that arrives empty or whitespace-only with
`driver: MURMUR_INFERENCE_ENDPOINT is set but empty`. Neither reaches the
network. A usable value is trimmed and used exactly as given.

## Inference dials

`inference.driver.config` is this driver's operator surface. Four keys are read; any other key
fails the first inference call, before any request is dispatched.

```yaml
inference:
  driver:
    artifact: murmur-driver-openai
    config: { thinking: enabled, reasoning_effort: high }
```

| Key | Type | Accepted values | Default | Effect |
|---|---|---|---|---|
| `store` | boolean | `true` / `false` | `false` | Server-side response retention, and with it the `previous_response_id` continuation feature. |
| `thinking` | string | `enabled` / `disabled`, trimmed and case-insensitive | `disabled` | `enabled` asks the provider for a reasoning summary. |
| `reasoning_effort` | string | forwarded verbatim — the provider decides which tiers exist | absent | The quality-against-cost dial on a reasoning model. |
| `verbosity` | string | forwarded verbatim | absent | Output verbosity. |

`thinking` and `reasoning_effort` are spelled as `murmur-driver-anthropic` and
`murmur-driver-deepseek` spell them, so one concept keeps one name across the fleet.

### Where each dial lands

The API surface a model routes to decides which dials it can carry. A dial the surface has no
field for is dropped, by the same rule that strips the sampling parameters an o-series model
rejects.

| Key | Responses (`gpt-<N≥5>`) | Chat Completions, o-series | Chat Completions, gpt-classic |
|---|---|---|---|
| `thinking: enabled` | `reasoning.summary = "auto"` | dropped | dropped |
| `reasoning_effort` | `reasoning.effort` | top-level `reasoning_effort` | dropped |
| `verbosity` | `text.verbosity` | dropped | dropped |
| `store` | `store` | not applicable | not applicable |

A dial set here overwrites a same-named key supplied under `params`, and a `reasoning` object
supplied under `params` is replaced wholesale rather than merged.

### Reasoning is unavailable on the o-series

o-series models route to Chat Completions, which returns no reasoning content — only an effort
parameter. A capsule on `o3` therefore cannot display reasoning, whatever `thinking` is set to.
The key is still accepted there rather than rejected, so one manifest survives a model switch;
it simply asks for nothing. Reasoning summaries arrive only on the Responses surface, which is
`gpt-5` and later.

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

The driver imports `murmur:stream/events@0.1.0`, which murmur serves from the first release after
v0.5.0; an older murmur cannot load it. A newer murmur refuses a release of this driver built
before that interface (0.8.0 and earlier) at `mur run` with `E-RUN-029`.

Pinned at 0.8.0, the driver is refused like this:

```text
error[E-RUN-029]: artifact 'murmur-driver-openai@0.8.0' imports murmur:text/chunks@0.1.0, which this host does not serve; a driver must import murmur:stream/events@0.1.0 in its place
  hint: pin a release of murmur-driver-openai built against murmur:stream/events@0.1.0 in murmur.yaml and run `mur install`, or rebuild it against that interface
```

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

A failed task status and a `task_failed` trace line with cause `driver_error`
need a murmur runtime released after v0.4.0. On v0.4.0 and earlier the runtime
records the error but still reports the task as `ok` and exits 0.
