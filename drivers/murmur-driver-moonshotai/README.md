# murmur-driver-moonshotai

Moonshot AI inference driver for Murmur agent capsules — `kimi-k3` on the Chat
Completions surface.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and Moonshot's
OpenAI-shaped Chat Completions API, including SSE streaming, a full tool-calling
round trip and preserved reasoning across turns.

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. There is no `MOONSHOT_API_KEY` variable in the driver environment: a
capsule author writes `api_key: ${MOONSHOT_API_KEY}` under `inference:` and the
manifest parser resolves it, after which the runtime delivers the value under
the name above. See [murmur.yaml](./murmur.yaml) for the full manifest.

## Using it from a capsule manifest

```yaml
inference:
  driver:
    artifact: murmur-driver-moonshotai
  model: kimi-k3
  endpoint: https://api.moonshot.ai/v1
  api_key: ${MOONSHOT_API_KEY}
```

| Field | Required | Default |
|---|---|---|
| `inference.driver.artifact` | yes | — |
| `inference.model` | yes | — (must be `kimi-k3`) |
| `inference.endpoint` | no | `https://api.moonshot.ai/v1` |
| `inference.api_key` | no in form, yes in practice | unset — requests go out unauthenticated and Moonshot rejects them |
| `inference.max_tokens` | no | `8192`, from the host; ceiling `1048576` |
| `inference.driver.config` | no | `{}` — see below |

## Model scope

`kimi-k3` is the only model this driver accepts, and the check runs before a
request body is built, so an unsupported model costs no HTTP call. Moonshot also
ships `kimi-k2.7-code`, `kimi-k2.7-code-highspeed` and `kimi-k2.6`; none of them
are implemented here. Adding one means extending `SUPPORTED_MODELS` — nothing
else in the crate branches on the model name.

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
    artifact: murmur-driver-moonshotai
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

This is a deliberate divergence from `murmur-driver-anthropic` and
`murmur-driver-deepseek`, both of which read a real `thinking` toggle. If
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
spells it; no `max_tokens` member is emitted. Moonshot's ceiling is `1048576`,
and a larger `inference.max_tokens` is refused pre-flight, naming the field and
the ceiling, rather than clamped silently or spent on a provider 400. Moonshot's
own default is `131072`, but the host always supplies a value (default `8192`),
so the driver never substitutes one.

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
argument fragments. `data: [DONE]` ends the stream.

A non-streaming JSON fallback is selected by sniffing the first non-whitespace
byte of the response body for `{` or `[`, which is where scripted test servers
and any provider that ignores `stream: true` land.

The assembled response's `content` puts the `thinking` block first, then either
the `tool_call` blocks or the `text` block. Thinking-first is what makes the
tool-call round trip work and what the UI renders separately.

Moonshot delivers reasoning on its own delta field, so there is no inline
reasoning-tag scanner anywhere in this crate.

## Stop reasons

| Moonshot `finish_reason` | Murmur `stop_reason` |
|---|---|
| `stop`, or no reason at all | `end_turn` |
| `tool_calls` | `tool_call` |
| `length` | `max_tokens` |
| anything else | an error naming the unmapped reason |

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | Moonshot field |
|---|---|
| `input_tokens` | `usage.prompt_tokens` |
| `output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `cache_write_tokens` | not reported by Moonshot — always absent |

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
