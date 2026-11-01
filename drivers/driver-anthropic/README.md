# driver-anthropic

Anthropic Messages API inference driver for Murmur agent capsules.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the Anthropic
Messages API, including SSE streaming, extended-thinking blocks, prompt-cache
breakpoints, and model-family handling (Claude 3.x vs Claude 4+ naming and
parameter rules).

The driver reads no provider key and sends no authentication header. Murmur's
runtime authenticates each call with the header declared under
`upstream_auth:` in [murmur.yaml](./murmur.yaml), the full manifest; the
capsule author supplies the key as `gateway.api_key` on the driver's
`artifacts:` entry, as a reference such as `${ANTHROPIC_API_KEY}`. An entry that
sets neither `api_key` nor `keyless: true` fails the launch with
`E-CAP-018`.

The endpoint arrives in the `MURMUR_INFERENCE_ENDPOINT` environment variable.
The runtime sets it to its own loopback gateway plus the path of the entry's
`gateway.endpoint`, and readdresses each request to the real upstream. The
driver carries no provider URL of its own to fall back on: a value that never
arrives fails the call with `driver: missing MURMUR_INFERENCE_ENDPOINT`, and one
that arrives empty or whitespace-only with `driver: MURMUR_INFERENCE_ENDPOINT is
set but empty`. Neither reaches the network. Requests go to
`<endpoint>/v1/messages`.

## Model limits

The driver accepts any model string; the model only picks the Claude 3 or
Claude 4+ request rules. `murmur.yaml` declares limits under `models:` for every
current and legacy model on Anthropic's
[models overview](https://platform.claude.com/docs/en/about-claude/models/overview),
under each "Claude API ID" it lists and under the "Claude API alias" where that
differs. Models before the 4.6 generation have both a dated ID and a dateless
alias, and both are declared; from 4.6 on, the dateless ID is the snapshot and
there is no dated one. Each id is matched exactly.

The numbers are the synchronous Messages API limits, which need no beta header.

| Model | max_output_tokens | context_window | max_input_tokens | Source |
|---|---|---|---|---|
| `claude-fable-5-1` | 128000 | 1000000 | none | <https://platform.claude.com/docs/en/about-claude/models/overview>, read 2026-10-23: "Claude API ID claude-fable-5-1", "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-opus-5-5` | 128000 | 1000000 | none | <https://platform.claude.com/docs/en/about-claude/models/overview>, read 2026-10-23: "Claude API ID claude-opus-5-5", "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-sonnet-5-5` | 128000 | 1000000 | none | <https://platform.claude.com/docs/en/about-claude/models/overview>, read 2026-10-23: "Claude API ID claude-sonnet-5-5", "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-haiku-5-5` | 128000 | 1000000 | none | <https://platform.claude.com/docs/en/about-claude/models/overview>, read 2026-10-23: "Claude API ID claude-haiku-5-5", "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-fable-5` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/fable-5/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-opus-5` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/opus-5/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-sonnet-5` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/sonnet-5/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-opus-4-8` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/opus-4-8/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-opus-4-7` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/opus-4-7/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-opus-4-6` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/opus-4-6/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-sonnet-4-6` | 128000 | 1000000 | none | Legacy model. <https://platform.claude.com/docs/en/models/sonnet-4-6/overview>, read 2026-10-23: "Context window 1M tokens", "Max output 128K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 1000000`, `max_tokens: 128000` |
| `claude-opus-4-5-20251101` | 64000 | 200000 | none | Legacy model. <https://platform.claude.com/docs/en/models/opus-4-5/overview>, read 2026-10-23: "Context window 200K tokens", "Max output 64K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 200000`, `max_tokens: 64000` |
| `claude-opus-4-5` | 64000 | 200000 | none | The "Claude API alias" for `claude-opus-4-5-20251101` on <https://platform.claude.com/docs/en/models/opus-4-5/overview>, read 2026-10-23, which resolves to that snapshot: "Context window 200K tokens", "Max output 64K tokens"; the Models API (`GET /v1/models`, 2026-10-23) entry for `claude-opus-4-5-20251101` gives `max_input_tokens: 200000`, `max_tokens: 64000` |
| `claude-sonnet-4-5-20250929` | 64000 | 200000 | none | Legacy model. Deprecated; retires 2026-11-30 (<https://platform.claude.com/docs/en/about-claude/model-deprecations>). <https://platform.claude.com/docs/en/models/sonnet-4-5/overview>, read 2026-10-23: "Context window 200K tokens", "Max output 64K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 200000`, `max_tokens: 64000` |
| `claude-sonnet-4-5` | 64000 | 200000 | none | The "Claude API alias" for `claude-sonnet-4-5-20250929` on <https://platform.claude.com/docs/en/models/sonnet-4-5/overview>, read 2026-10-23, which resolves to that snapshot: "Context window 200K tokens", "Max output 64K tokens"; the Models API (`GET /v1/models`, 2026-10-23) entry for `claude-sonnet-4-5-20250929` gives `max_input_tokens: 200000`, `max_tokens: 64000` |
| `claude-haiku-4-5-20251001` | 64000 | 200000 | none | Legacy model. <https://platform.claude.com/docs/en/models/haiku-4-5/overview>, read 2026-10-23: "Context window 200K tokens", "Max output 64K tokens". Exact integers: Models API (`GET /v1/models`, 2026-10-23): `max_input_tokens: 200000`, `max_tokens: 64000` |
| `claude-haiku-4-5` | 64000 | 200000 | none | The "Claude API alias" for `claude-haiku-4-5-20251001` on <https://platform.claude.com/docs/en/models/haiku-4-5/overview>, read 2026-10-23, which resolves to that snapshot: "Context window 200K tokens", "Max output 64K tokens"; the Models API (`GET /v1/models`, 2026-10-23) entry for `claude-haiku-4-5-20251001` gives `max_input_tokens: 200000`, `max_tokens: 64000` |

`murmur.yaml` declares the same ids and numbers under `models:`, and a test
fails if the two disagree. The Models API's `max_input_tokens` equals the
context window for every declared model, so every `max_input_tokens` cell is
`none` and no entry declares the key.

Several of these models' pages also give "Max output (Batch API, beta) 300K
tokens", with the `output-300k-2026-03-24` beta header. That limit applies to the
Message Batches API only. This driver calls the Messages API, so it is not
reachable here at any `inference.max_tokens`.

Anthropic counts output against the window: "The output Claude generates for the
turn, including its extended thinking, counts too." murmur compacts against one
input budget per call: `min(max_input_tokens, context_window − output cap)`,
which here is `context_window − output cap`. `inference.compaction.threshold`
and `context.seed_budget` are fractions of that budget. With the default output
cap:

| Models | Input budget | Basis |
|---|---|---|
| the `1000000`-window models | `872000` | `1000000 − 128000` |
| the 4.5 models and their aliases | `136000` | `200000 − 64000` |

An `inference.max_tokens` the capsule sets changes the `window − output cap`
term. On Claude 4.5 models and newer, a request whose input plus `max_tokens`
exceeds the window is accepted, and a reply that reaches the window stops with
`model_context_window_exceeded`. This driver refuses that stop reason (see
[Stop reasons](#stop-reasons)).

Whether the runtime reads `models:` depends on the murmur release:

| murmur | What it does with `models:` | What the capsule sets |
|---|---|---|
| v0.7.0 and earlier | ignores it; an unknown manifest key is a `W-SEC-019` warning, never a refusal | `inference.max_tokens` and `context.max_tokens`; left unset, the runtime's own defaults apply, not the model's limits |
| the first release after v0.7.0 | defaults the output cap and context window from it when the capsule leaves them unset, reads `max_input_tokens` as the input ceiling, measures compaction and the seed budget against the input budget, and caps a hook's `run-inference` (e.g. `hook-compact`) at its `max_output_tokens` | nothing, unless it wants tighter limits than the table |
| murmur `a9ea4726` and later; no release has it yet (v0.8.0, the newest tag, predates WIT level 3) | also reads `vision`, and serves WIT level 3, which driver-anthropic 0.18.0 and later need: an older `mur` refuses the `vision` key (`E-BLD-005` when packing, `E-RUN-052` at launch) and does not load the driver's `murmur:tool/run@0.2.0` export. `vision: false` makes `mur run` refuse (`E-RUN-057`) a `transport: http` capsule that declares a computer tool and runs that model | nothing |

A capsule value always wins: the runtime never clamps or raises
`inference.max_tokens` or `context.max_tokens` to the table.

## Models without declared limits

These ids are accepted by the driver but have no entry under `models:`:

| Model | Why it has no entry |
|---|---|
| `claude-mythos-5-1`, `claude-mythos-5` | listed under "Specialized models", not on the overview's current or legacy list; available only to organizations verified through Anthropic's verification programs |
| `claude-mythos-preview` | deprecated, and not on the overview |
| retired models, such as `claude-opus-4-1-20250805` or `claude-3-7-sonnet-20250219` | retired: Anthropic fails every request to them |
| dated snapshot ids for the 4.6 generation and later | none exist: from 4.6 on, the dateless ID is the snapshot |
| any other id | not on the overview |

A capsule on one of these sets `inference.max_tokens` and `context.max_tokens`
itself:

| Capsule sets | Result |
|---|---|
| neither | the launch is refused with `E-RUN-052`, naming the driver and the model, before any HTTP call |
| only `inference.max_tokens` | it launches with `W-RUN-009`, and compaction does not run |
| both | it launches |

## Images

murmur hands the driver each image a tool attaches, or an A2A message carries, as a base64 `image`
block. Before it translates the request, the driver sizes every image in a `user` or `tool`
message to the model's limits, so Anthropic never downscales it again and the driver knows the
exact size the model sees. A request that holds no image is sent byte for byte as before, and
nothing is decoded.

### Placement

| murmur message | Where the image goes |
|---|---|
| `tool` | Inside the one `tool_result` that answers the call. It holds the message's text and image blocks in the order murmur sent them, including any `[image not shown: …]` or `[image unavailable: …]` line. Anthropic refuses a second `tool_result` for the same `tool_use_id` with HTTP 400, so the driver never sends one |
| `user` | An `image` block in the user message, where murmur put it |

The prompt-cache marker stays on the `tool_result`, never on a block inside it.

### Sizing

Each tier is a long-edge cap and a visual-token cap. A `width`×`height` image fits when each side,
padded up to a multiple of 28 px, is within the long edge, and `⌈w/28⌉ × ⌈h/28⌉` visual tokens
are within the token cap. An image that does not fit is resized to the largest aspect-preserving
size that does, found with the binary search of Anthropic's reference implementation
(<https://platform.claude.com/docs/en/build-with-claude/vision-coordinates>, "How Claude resizes and pads images"). A 3000×2000 screenshot is sent at 2352×1568 on the
high-resolution tier and at 1344×896 on the standard tier.

| Model | Reads images | Max long edge (px) | Max visual tokens | Source |
|---|---|---|---|---|
| `claude-fable-5-1` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-fable-5-1` |
| `claude-opus-5-5` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-opus-5-5` |
| `claude-sonnet-5-5` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-sonnet-5-5` |
| `claude-haiku-5-5` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-haiku-5-5` |
| `claude-fable-5` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-fable-5` |
| `claude-opus-5` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-opus-5` |
| `claude-sonnet-5` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-sonnet-5` |
| `claude-opus-4-8` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-opus-4-8` |
| `claude-opus-4-7` | yes | 2576 | 4784 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "High-resolution · Claude 4.7 and later models · 2,576 px · 4,784"; `supportedModels` lists `claude-opus-4-7` |
| `claude-opus-4-6` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; `supportedModels` lists `claude-opus-4-6` |
| `claude-sonnet-4-6` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; `supportedModels` lists `claude-sonnet-4-6` |
| `claude-opus-4-5-20251101` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; `supportedModels` lists `claude-opus-4-5-20251101` |
| `claude-opus-4-5` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; the alias of `claude-opus-4-5-20251101`, which `supportedModels` lists |
| `claude-sonnet-4-5-20250929` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; `supportedModels` lists `claude-sonnet-4-5-20250929` |
| `claude-sonnet-4-5` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; the alias of `claude-sonnet-4-5-20250929`, which `supportedModels` lists |
| `claude-haiku-4-5-20251001` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; `supportedModels` lists `claude-haiku-4-5-20251001` |
| `claude-haiku-4-5` | yes | 1568 | 1568 | <https://platform.claude.com/docs/en/build-with-claude/vision>, read 2026-11-01: "Standard · All other models · 1,568 px · 1,568"; the alias of `claude-haiku-4-5-20251001`, which `supportedModels` lists |

An id outside the table, such as `claude-mythos-5-1`, is sized at the standard tier: every current
Claude model reads images, and no model rescales an image that fits the smaller tier.

The vision page's cost table gives 1269×952 for a 2000×1500 image on the standard tier. Its
reference implementation, which the driver follows, gives 1270×952.

| Rule | Value |
|---|---|
| Decode ceiling | 8000 px a side and 40000000 pixels, checked on the header before any pixel is decoded. The decoder's own allocation is capped at 192 MiB |
| Per-image limit | 10000000 bytes of base64: the vision page's "10 MB (base64-encoded) on the Claude API", read as decimal megabytes, the smaller reading |
| Resampling filter | Catmull-Rom |
| PNG | resized as a PNG, deflate level 1 with adaptive filtering |
| JPEG | resized as a JPEG at quality 90 |
| GIF, WebP | sent as they are when they fit, since Anthropic reads an animated GIF's first frame; otherwise resized as a PNG of the first frame |
| An image that fits | its original bytes, untouched |
| A source that is not base64 | sent as it is, unmeasured |

### The report

When the request held an image, the response the driver returns to murmur carries a top-level
`images` list: one entry per image block, in request order. murmur reads only `stop_reason`,
`content`, `usage` and `error` from that response. With `trace.capture: content` the whole response,
report included, is stored as the `inference` event's response blob. An error response carries no
report.

| Field | Type | Present | Meaning |
|---|---|---|---|
| `message` | integer | always | index of the message in the request's `messages` |
| `block` | integer | always | index of the block in that message's `content` |
| `media_type` | string | always | the media type murmur sent |
| `bytes` | integer | always | length of the decoded image; of the source data as given when it is not valid base64 or not a base64 source |
| `sha256` | string | always | hex sha256 of the bytes `bytes` counts |
| `outcome` | string | always | `sent` (original bytes), `resized` (resized or re-encoded) or `replaced` (a text line instead) |
| `width`, `height` | integer | when the header was read | the original size |
| `sent_media_type` | string | `sent`, `resized` | the media type sent |
| `sent_width`, `sent_height` | integer | `sent`, `resized`, when the original size is known | the size sent, which is the size the model sees |
| `scale` | number | `sent`, `resized`, when the original size is known | `sent_width / width` |
| `placement` | string | `sent`, `resized` | `tool_result` or `user` |
| `reason` | string | `replaced` | why the image was not sent |

### Images the driver does not send

An image the driver cannot send becomes a text block in its place, and the request is still sent:

```
[image not shown: <media_type>, <n> bytes, sha256 <hex>; <reason>]
```

| Reason | When |
|---|---|
| `model '<model>' does not take image input in driver-anthropic's model table` | the model's tier reads no image; no model in the table is one today |
| `the image data is not valid base64: <detail>` | the block's data does not decode |
| `the image's dimensions could not be read: <detail>` | the header does not parse as the media type murmur named |
| `the image is <w>x<h> px, over the driver's decode ceiling of 8000 px a side and 40000000 pixels` | the header names a size over the decode ceiling |
| `the image could not be decoded: <detail>` | the pixels do not decode |
| `the image could not be re-encoded after resizing: <detail>` | the encoder refuses the resized image |
| `the image is <n> bytes as base64, over Anthropic's per-image limit of 10000000 bytes` | the image to send is over the per-image limit |

murmur's own `[image not shown: …; the <keep> newer images in this conversation are shown instead]`
and `[image unavailable: …]` lines arrive as text and are sent as they are.

### Cost

The pre-pass runs inside the driver component on every request that holds an image, and on no
other. murmur re-sends the newest `context.images.keep` images on every turn, so an image that is
resized is resized again on each turn, to the same bytes: sizing is deterministic, so the prompt
prefix that holds it stays cacheable. An image that already fits costs only a base64 decode, a
sha256 and a header read.

Measured on 2026-11-01 by
`cargo test -p murmur-driver-image --test wasm_timing -- --ignored --nocapture`: the whole
pre-pass for one 3840×2160 screenshot-like PNG (8742731 bytes), 7 runs per case, as a
wasm32-wasip2 command under wasmtime 46 and natively, on an Intel Core i7-8565U (8 threads) at a
load average of 7 to 8.

| Case | Sent at | Sent bytes | wasm32-wasip2, median (range) | native, median (range) |
|---|---|---|---|---|
| Anthropic high-resolution tier | 2576×1449 | 4813605 | 1557.5 ms (1535.0–1609.4) | 724.8 ms (702.0–865.9) |
| Anthropic standard tier | 1456×819 | 1579629 | 936.7 ms (874.8–964.9) | 440.8 ms (421.7–456.7) |
| OpenAI 2048 px / 2,500 patches | 2048×1152 | 1558098 | 1340.0 ms (1139.7–1446.9) | 539.7 ms (506.2–579.9) |
| Fits as it is: header read only | 3840×2160 | 8742731 | 108.8 ms (94.5–122.9) | 88.9 ms (83.4–97.1) |

### Checked against the live API

On 2026-11-01 a `claude-haiku-4-5` Messages request whose `tool_result` held a text block and a PNG
of the word LANTERN, shaped as this driver sends it with the cache marker on the `tool_result`,
returned HTTP 200, and the model answered with the word.

Also on 2026-11-01, a request answering one `tool_use` with two `tool_result` blocks, one holding
the tool's text and one an `[image not shown: …]` line, returned HTTP 400: "each tool_use must
have a single result. Found multiple `tool_result` blocks with id". A `claude-sonnet-5-5` run with
`context.images.keep: 1` and two tool images, where every later request carries an aged image line
beside the tool's text in one `tool_result`, ran seven turns without an error.

## Prompt caching

Anthropic caches a prompt prefix only where the request carries a
`cache_control` marker. The driver places three, in the order Anthropic renders
a prompt:

| # | Marker position | Emitted when |
|---|---|---|
| 1 | the last tool definition | `tools` is non-empty |
| 2 | the system prompt block | the system prompt is non-blank |
| 3 | the last cacheable content block of the conversation | at least one cacheable block exists |

Marker 2 covers the tool inventory and the system prompt together, because tools
render ahead of the system prompt. Marker 3 sits at the end of the settled
conversation, so the next turn reads everything before it and writes only what
the last turn appended.

Marker 3 searches backwards and crosses message boundaries: a `thinking` block
cannot carry a marker, and a message can translate to an empty content array.
When no cacheable block exists anywhere, marker 3 is left out and the other two
still stand.

Caching is on by default and needs no capsule change. Configure it under
`config:` on this driver's entry in the capsule's `artifacts:` list, which the
runtime lowers to compact JSON and delivers to this artifact alone as
`MURMUR_ARTIFACT_CONFIG`:

| Key | Accepted | Default | Effect |
|---|---|---|---|
| `prompt_cache` | `enabled` / `disabled` (case-insensitive) or `true` / `false` | `enabled` | `disabled` or `false` emits no marker anywhere |
| `prompt_cache_ttl` | `5m` / `1h` (case-insensitive) | `5m` | `1h` puts the 1-hour TTL on every marker |

```yaml
artifacts:
  - name: murmur/driver-anthropic
    runtime: driver
    config:
      prompt_cache: enabled
      prompt_cache_ttl: 1h
```

Neither key errors or warns. Any other value — an unrecognised string, a number,
a missing key, an entry with no `config:` block at all, or config that is not
valid JSON — falls back to the default. Only the exact value `disabled` or
`false` turns caching off, so a typo cannot silently disable it.

These two keys are the only ones read from `MURMUR_ARTIFACT_CONFIG`; everything
else this driver takes — extended thinking, `anthropic-beta` features, `params`
— is read from `inference.driver.config`. Caching belongs on the artifact
because a `cache_control` breakpoint is an Anthropic construct: the other
drivers have nowhere to put one, since OpenAI and DeepSeek cache a prefix
automatically with no marker in the request.

**The `system` field changes shape with caching on.** It is sent as a
one-element array of text blocks so a marker has a block to sit on:

```json
"system": [
  {"type": "text", "text": "You are helpful", "cache_control": {"type": "ephemeral"}}
]
```

With `prompt_cache: disabled` it is a bare JSON string, which is the escape
hatch for an Anthropic-compatible gateway that rejects `cache_control`, and for
a capsule that supplies its own marker through `inference.driver.config.params`
— the driver never inspects or reconciles `params`.

Behaviour worth knowing:

| Situation | What happens |
|---|---|
| Prefix shorter than the model's minimum cacheable length | The marker is silently a no-op; `cache_write_tokens` stays `0`. The driver applies no length heuristic of its own. |
| Every marker in one request | Carries the same TTL, so a longer-TTL entry can never follow a shorter-TTL one. |
| A turn that appends more than 20 content blocks | Anthropic looks back at most 20 blocks for a prior entry, so the next request rewrites the conversation instead of reading it. |
| The first turn after a compaction | Marker 3 misses and writes a fresh entry; markers 1 and 2 still hit, because compaction changes neither the tools nor the system prompt. |

A cache read costs about 0.1x the base input price; a cache write costs 1.25x at
the 5-minute TTL and 2x at the 1-hour TTL. Two requests over the same prefix
break even at `5m`, three at `1h` — which is why `5m` is the default. Reading an
entry refreshes its timer at no cost.

Prompt caching is generally available, so the driver adds no `anthropic-beta`
header for it.

## Tool-call progress

While the model is still writing a tool call, the driver reports it to the host through
`murmur:stream/events@0.1.0`:

| Call | Sent on | Carries |
|---|---|---|
| `tool-call-started` | `content_block_start` opening a `tool_use` block | the block's `id` and `name`, which the returned `tool_call` carries too |
| `tool-call-input-bytes` | each non-empty `input_json_delta` for that block | the call's argument bytes so far |

The size is a running UTF-8 byte total of the call's argument text. It only rises, and its last
value is the byte length of the text parsed into the returned `input`. No argument text is ever
forwarded: the host learns a call's id, name and size, and the arguments reach it only in the
returned tool call. On `transport: http`, the host writes these as `tool-call-started` and
`tool-call-progress` frames ahead of the call's `artifact`.

The Messages API sends a call's id in the event that opens its block, so a call is reported from
its first byte. A `tool_use` block with an empty `id` or `name` is not reported, and is returned
as before. Blocks the driver drops, such as `server_tool_use`, are not reported.

The driver imports `murmur:stream/events@0.1.0`, which murmur serves from v0.6.0; an older murmur
cannot load it. `mur` 0.7.0 or later refuses a release of this driver built before that
interface (0.9.0 and earlier) at launch with `E-WIT-LEVEL-001`: it imports
`murmur:text/chunks@0.1.0`, and WIT level 1 has no version of `murmur:text`.

## Token usage

Every translated response carries an optional top-level `usage` object, on both
the SSE streaming path and the non-streaming JSON fallback. Murmur records the
members on the `inference` trace event as `input_tokens_actual`,
`output_tokens_actual`, `cached_tokens` and `cache_write_tokens`.

| `usage` member | Anthropic Messages field |
|---|---|
| `input_tokens` | `usage.input_tokens` |
| `output_tokens` | `usage.output_tokens` |
| `cached_tokens` | `usage.cache_read_input_tokens` |
| `cache_write_tokens` | `usage.cache_creation_input_tokens` |

`input_tokens` is the request tokens neither read from nor written to the cache;
`cached_tokens` and `cache_write_tokens` sit beside it, so
`input_tokens + cached_tokens + cache_write_tokens` is Anthropic's total request
count. Anthropic's own `input_tokens` already excludes both cache counts, so each
member is passed through unchanged.

While streaming, the counts are seeded from `message_start` and each member a
later `message_delta` reports replaces the seeded value — so the cumulative
`output_tokens` wins over the placeholder `message_start` carries.

Each member is independently optional. A count the provider did not report is
omitted rather than sent as `0`; a reported `0` is kept as `0`. When no member
survives, the response carries no `usage` key at all.

## Stop reasons

| Anthropic `stop_reason` | murmur `stop_reason` |
|---|---|
| `end_turn` | `end_turn` |
| `stop_sequence` | `end_turn` |
| `tool_use` | `tool_call` |
| `max_tokens` | `max_tokens` |
| `refusal` | `error`, with `Anthropic response refused` |
| `model_context_window_exceeded` | refused: `driver: unsupported Anthropic stop_reason 'model_context_window_exceeded'` |

murmur has no stop reason for a reply cut at the context window. Its
`max_tokens` means the turn stopped at the `inference.max_tokens` output cap: it
warns `W-RUN-001` naming that cap and records the turn as truncated at it. So
`model_context_window_exceeded` is not mapped to `max_tokens`, and the turn
fails like any other unsupported stop reason.

Any other value, `pause_turn` included, is refused with
`driver: unsupported Anthropic stop_reason '<value>'`. So is `""` in a JSON
body; in a stream an empty `stop_reason` is not recorded, so the stream ends
with no stop reason (see below).

### Failed turns

A turn the driver cannot report as finished is returned as
`{"stop_reason":"error","error":"<message>"}`, so the task ends failed with
cause `driver_error`. The message says which of three kinds of failure it was:

| Kind | `error` message | Example |
|---|---|---|
| Translation failure: the response cannot be read as a finished Messages API turn | starts with `driver: ` | `driver: malformed Anthropic stream: tool_use block 1 has no id` |
| Provider error | starts with `HTTP <status>: `, or with `Anthropic error: <type>: <message>` for an error object in a 2xx JSON body or an SSE `event: error` | `Anthropic error: overloaded_error: Overloaded` |
| Refusal | exactly `Anthropic response refused` | `stop_reason: "refusal"` |

In `Anthropic error: <type>: <message>`, a part the error object does not carry
as a string reads `unknown`. Reading a stream stops at its `event: error`, so no
later text is shown.

A failed task status and a `task_failed` trace line with cause `driver_error`
need a murmur runtime released after v0.4.0. On v0.4.0 and earlier the runtime
records the error but still reports the task as `ok` and exits 0.

The translation failures:

| Response | Refused with |
|---|---|
| JSON body whose `stop_reason` is absent, `null` or not a string, such as a chat-completions body | `driver: Anthropic response has no stop_reason` |
| Body that holds lines but no SSE `event:` line, such as a chat-completions stream | `driver: response is not an Anthropic event stream` |
| SSE stream that ends before any `message_delta` carries a non-empty `stop_reason`: a dropped connection, or an empty body. murmur retries it (see [Transient failures](#transient-failures)) | `driver: Anthropic stream ended with no stop_reason` |
| JSON body missing a field that changes what the turn means | `driver: malformed Anthropic response: <what is missing>` |
| SSE event missing a field that changes what the turn means | `driver: malformed Anthropic stream: <what is missing>` |
| SSE `content_block_start` at an `index` of 1024 or more | `driver: malformed Anthropic stream: content block index <n> is past the limit of 1024 per turn` |
| Streamed tool input that does not parse, on a turn stopped by `max_tokens` | `driver: Anthropic turn stopped at the inference.max_tokens output cap; tool call '<name>' was cut off mid-input and cannot be run — raise the cap and re-run` |

A JSON body must carry a `content` array. Each block must carry a string `type`;
a `text` block its `text`; a `tool_use` block its `id`, `name` and an object
`input`; a `thinking` block its `thinking`.

Every line of a stream must be UTF-8, as a JSON body must be. In a stream, the
driver reads the data of `message_start`,
`content_block_start`, `content_block_delta` and `message_delta`, and that data
must be JSON. A block event must carry an integer `index`, and a block start
one from 0 through 1023: a turn holds at most 1024 blocks, and a start past
that is refused before anything is stored for it or a tool call is reported.
A block start must carry its block `type`, and a `tool_use` start its `id` and
`name`; a delta its `type`, for a block that started, of the matching kind,
with a string payload. Streamed tool input must parse to a JSON object.

No message includes any part of the response body or of a tool call's input.
Text already streamed through `murmur:stream/events` before a failure has been
shown and is not withdrawn; it is just not recorded as a finished reply.

### Not refused

- `content: []` with `stop_reason: "end_turn"`: a successful empty turn.
- A tool call with no arguments: `input: {}`, or a streamed `tool_use` block
  with no `input_json_delta` or only empty ones.
- A `thinking` block with no `signature`. It is kept with `signature: ""`, and
  never replayed on the next request. `thinking` blocks are kept on the JSON
  fallback as well as on the SSE path.
- A `redacted_thinking` block, and any block type the driver does not know:
  dropped.
- An SSE event name or delta type the driver does not know, such as
  `citations_delta`: ignored.
- `ping`, `content_block_stop` and `message_stop` data that is not JSON: never
  read.
- Absent or malformed `usage`: the turn reports no usage.
- A stream whose `message_delta` carried a stop reason but that ended before
  `message_stop`: the reply is already complete at that point.

## Transient failures

murmur makes an inference call again when it fails transiently. Its credential
gateway sees the status, and a connection that breaks before the response head.
What goes wrong after a `2xx` head only the driver sees, and it says so with the
`transient` key of its `tool-result.metadata`. The `error` message is the same
with or without the key.

The driver marks these failures `"true"`, so murmur retries them:

| Failure after a `2xx` head | `error` message |
|---|---|
| SSE stream that ends before any `message_delta` carries a non-empty `stop_reason`: a dropped connection, or an empty body | `driver: Anthropic stream ended with no stop_reason` |
| A read of the body that fails, on the SSE path or the JSON fallback | `driver: failed to read response stream: <error>` |
| SSE `event: error`, or a JSON body with `"type": "error"`, whose `error.type` is `overloaded_error`, `api_error`, `rate_limit_error` or `timeout_error` | `Anthropic error: <type>: <message>` |

A `rate_limit_error` whose `error.details.error_code` is
`enforced_spend_limit_reached` reports the tier spend cap, which does not clear
by waiting, so it is not marked `"true"`.

The driver marks one failure `"false"`, so murmur does not spend its retries on
it:

| Status | Body | `error` message |
|---|---|---|
| `429` | `error.details.error_code` is `enforced_spend_limit_reached`: the tier spend cap, reached until the next month or a tier increase | `HTTP 429: <body>` |

Anthropic reports every other billing problem with a status murmur does not
retry: a spend limit you set is a `400` `invalid_request_error`, and a payment
problem a `402` `billing_error`.

Every other failure carries no `transient` key:

| Failure | Who decides |
|---|---|
| Any other status, `408`, a rate-limit `429`, `500`, `529` and the rest of `5xx` included, and a connection that breaks before the response head | murmur, from what its gateway saw |
| A read that fails while reading a body of status `400` or more | murmur, from the status |
| `event: error` or a JSON error body of any other `error.type`, such as `invalid_request_error`, or with no readable type, also when the stream then ends | not retried |
| `Anthropic response refused`, an unsupported `stop_reason` such as `model_context_window_exceeded`, a malformed stream or body, and `driver: response is not an Anthropic event stream` | not retried |
| A request the driver refuses before sending it | not retried |

An `event: error` takes precedence over the stream ending: a plain error event
followed by the end of the stream reports the provider's message and stays
plain.

murmur discards everything a failed attempt streamed before it retries. Anthropic
sends its delay hint as an HTTP `retry-after` header, which murmur reads itself;
the driver sends no delay of its own and never repeats a request. murmur 0.8.0
and later read the `transient` key. An earlier murmur ignores it and fails the
call as before.

## Prompt cache key

Murmur puts a `prompt_cache_key` on every driver request. This driver drops it:
the Messages API rejects a body carrying any field it does not define, and it
defines no cache-key field. The value reaches the provider body at no nesting
level. It is unrelated to the `cache_control` markers above, which are how this
driver caches a prefix.
