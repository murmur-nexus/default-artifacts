# murmur-tool-tavily

Web search for Murmur capsules through Tavily's `POST /search`. The agent sends a query and
gets back a bounded plain-text result: an optional answer, then numbered results with title,
URL and snippet. The operator controls every cost-bearing and size-bearing lever. The Tavily
key is bound by the operator and attached by the runtime, so the tool never holds, reads or
builds it.

WASM component (`wasm32-wasip2`), exporting `murmur:tool/run`. It needs a murmur runtime that
supports per-entry credential gateways: the first murmur release after v0.3.0 (murmur #191).
An older runtime has no `gateway:` on artifact entries and cannot run this tool.

## Binding the key

Bind the key on the tool's own entry in the capsule manifest, as `gateway.api_key`:

```yaml
artifacts:
  - name: murmur-tool-tavily
    version: 0.1.0
    runtime: tool
    gateway:
      endpoint: https://api.tavily.com
      api_key: ${TAVILY_API_KEY}
    capabilities:
      network:
        allow: []
    config:
      config_version: 1
      search_depth: basic
      max_results: { default: 5, max: 10 }
```

Store the key with:

```bash
mur config set -g credentials.TAVILY_API_KEY <key>
```

| Where the key comes from | When it is read |
|---|---|
| `credentials.TAVILY_API_KEY` in the global config | Again while the capsule runs, so a rotated key is used on the next search |
| The `TAVILY_API_KEY` process environment variable | Once at launch, with warning `W-SEC-027` |
| A literal value in `api_key:` | Once at launch, with warning `W-SEC-027` |

## Who holds the key

The tool never holds, reads or builds the key. Its bundled `murmur.yaml` declares only how
Tavily takes a key:

```yaml
inference_auth:
  header: Authorization
  value: "Bearer {key}"
```

The runtime gives the tool one address, `MURMUR_GATEWAY_ENDPOINT`, and that variable carries
no key. The tool sends plain HTTP to that address. The runtime removes any `Authorization`
header, attaches exactly one rendered from the key, and forwards the request over TLS to
`gateway.endpoint`. If Tavily answers `401`, the runtime re-reads the credential and resends
once when the value has changed.

The tool accepts no key any other way:

| Where a key might be put | Result |
|---|---|
| A literal key in `config:` | Refused, as is any `config:` key whose name contains `key`, `token`, `secret`, `password`, `authoriz`, `bearer` or `credential` (`config_invalid`) |
| A key in an environment variable the tool reads | Not supported: the tool reads only `MURMUR_ARTIFACT_CONFIG` and `MURMUR_GATEWAY_ENDPOINT` |
| No `gateway:` on the entry | Every call returns `gateway_missing` and nothing is sent |

## Network

Do not list `api.tavily.com` in any `network.allow`. Gateway requests are not checked against
the allow-list, so such an entry grants only direct, keyless reach to Tavily, and murmur warns
`W-SEC-025`. `allow: []` on the entry removes all direct egress, leaving the gateway as the
tool's only way out.

## Cost

murmur brokers the call but **does not meter** it. Every launch warns `W-SEC-030`: the call
counts toward neither `inference.max_session_tokens` nor `spend.machine_tokens_per_day`. Two
operator levers bound the cost:

| Lever | Effect on cost |
|---|---|
| `search_depth` | `basic`, `fast` and `ultra-fast` cost 1 credit per search; `advanced` costs 2 |
| `max_results` | The operator's `max` is a ceiling the agent cannot exceed |

Each result reports the credits Tavily charged, in its header line and summary.

## Untrusted content

Search results are third-party text and may contain instructions aimed at the model. The
runtime fences every tool result as untrusted content, and that fence is what protects the
model. The tool passes Tavily's text through byte for byte, cutting only to fit the budget. It
does not sanitise, escape or strip anything, by design.

## Configuration

The `config:` block on the entry is optional. With no block at all, the defaults below apply
and the output says so. An empty block (`config: {}`) is refused, because `config_version`
is required whenever the block is present. Unknown keys are ignored, so the block can carry
annotations, except credential-shaped keys, which are refused.

| Key | Allowed | Default | Sent to Tavily as |
|---|---|---|---|
| `config_version` | `1` | required when the block is present | — |
| `search_depth` | `basic`, `advanced`, `fast`, `ultra-fast` | `basic` | `search_depth` |
| `max_results.default` | integer 1–20 | 5 | `max_results` when the agent omits it |
| `max_results.max` | integer 1–20, at least `default` | 10 | the ceiling the agent's value is clamped to |
| `include_answer` | `false`, `basic`, `advanced` | `false` | `include_answer` |
| `topic` | `general`, `news`, `finance` | `general` | `topic` |
| `time_range` | `day`, `week`, `month`, `year` | absent | `time_range`, only when set |
| `include_domains` | non-empty strings, at most 300 | `[]` | `include_domains`, only when non-empty |
| `exclude_domains` | non-empty strings, at most 150 | `[]` | `exclude_domains`, only when non-empty |
| `chunks_per_source` | integer 1–3 | 3 | `chunks_per_source`, only when `search_depth: advanced` |
| `max_output_bytes` | integer 2048–65536 | 16384 | the budget for the rendered text |

Every refusal names its key, and an unknown value lists the allowed ones.
`include_answer: true` is refused: write `basic` or `advanced`. Under `max_results`, only
`default` and `max` are accepted.

Every request also carries these fixed values:

| Field | Value |
|---|---|
| `include_raw_content` | `false` |
| `include_images` | `false` |
| `include_image_descriptions` | `false` |
| `include_favicon` | `false` |
| `auto_parameters` | `false` |
| `include_usage` | `true` |

### Declined levers

| Lever | Why it is not configurable |
|---|---|
| `include_raw_content` | Returns an unbounded blob, and the runtime puts no cap on a tool result |
| `auto_parameters` | Silently costs 2 credits and overrides the operator's `search_depth` |
| `include_images`, `include_image_descriptions`, `include_favicon` | The model consumes text |
| `country`, `language`, `start_date`/`end_date`, `exact_match`, `safe_search`, `include_domains_mode`, `include_published_date` | Beyond the usual levers; each can be added later without breaking a manifest |

## Input

| Shape | Example |
|---|---|
| Object | `{"query": "rust wasi http", "max_results": 3}` |
| Bare JSON string, taken as the query | `"rust wasi http"` |
| Object encoded twice, re-parsed once | `"{\"query\": \"rust wasi http\"}"` |

| Field | Rule |
|---|---|
| `query` | Required. A string of at most 400 characters, trimmed, not blank |
| `max_results` | Optional. An integer of at least 1, clamped to the operator's `max_results.max` |

Any other key is refused with `invalid_input`, naming the key as the operator's to set in
`config:`.

## Output

On success, `status` is `passed`:

| Field | Value |
|---|---|
| `summary` | `N results for "<query>"`, followed by `; credits: C` when Tavily reports credits |
| `data` | The rendered text below |
| `data_path` | Set only when the text was truncated and the full response was saved |
| `truncated` | Whether anything was omitted or cut |
| `metadata` | `state_effect: read`, `resource_id: tavily:<query>` |

```
Tavily search: "<query>" — <returned> results (requested <r>, capped at <max>); depth <d>; credits <c>
[defaults in use — no config: block on this entry]

Answer: <answer>

[1] <title>
<url>
<content>

[2] …
[truncated: <shown> of <total> results shown within <max_output_bytes> bytes; full results in tavily-results/<file>]
```

Some lines appear only in some cases:

| Line or part | Appears when |
|---|---|
| `; credits <c>` | Tavily reports credits |
| `[defaults in use …]` | The entry has no `config:` block |
| `Answer:` | `include_answer` is on and Tavily returned an answer |
| `[truncated: …]` | Something was cut |

The whole text, trailer included, is at most `max_output_bytes` bytes:

1. The answer is capped at half the budget, cut at a character boundary and ended with `…`.
2. Whole results are appended in order while they fit.
3. If not even the first result fits, it is cut, so at least one result appears.
4. If anything was omitted or cut, the full Tavily response is saved in the workdir as
   `tavily-results/<request_id>.json`. When Tavily's `request_id` is missing or not a plain
   identifier, the file is `search-<n>.json`, with the first free `n`.
5. If that save fails, `data_path` is unset, `truncated` is still set, and the trailer says
   the full results could not be saved.

## Errors

A failure returns `data` as compact JSON, `{"ok": false, "error_kind": …, "message": …}`, plus
the extra fields below. `summary` is the message and `metadata` is empty.

| `error_kind` | `status` | When | Extra fields |
|---|---|---|---|
| `invalid_input` | `failed` | The input breaks a rule above | — |
| `config_invalid` | `error` | The `config:` block breaks a rule above | — |
| `gateway_missing` | `error` | The entry has no `gateway:`. The message shows the block to add and the `mur config set` command | — |
| `upstream_unauthorized` | `error` | HTTP 401 or 403, after the runtime's single re-read. The message names `gateway.api_key` and `credentials.<NAME>` | `http_status` |
| `rate_limited` | `error` | HTTP 429. The tool never sleeps or retries | `http_status`, `retry_after` when Tavily sent one |
| `quota_exceeded` | `error` | HTTP 432 (plan limit) or 433 (pay-as-you-go limit) | `http_status` |
| `upstream_rejected` | `error` | HTTP 400 or 422 | `http_status` |
| `upstream_error` | `error` | Any other non-2xx | `http_status` |
| `transport_error` | `error` | The request could not be sent, including when the runtime denies it | — |
| `response_invalid` | `error` | A 2xx body that is not JSON or has no `results` array | — |

For the `upstream_*`, `rate_limited` and `quota_exceeded` kinds, the message includes Tavily's
own error text, cut to 300 characters.

Every check runs before the request, so a refused call sends nothing. Each call sends at most
one request and never retries.

## Development

| Item | Location |
|---|---|
| Transport | `wasm_tool` in `src/lib.rs`: one blocking POST over `wasi:http`, and the crate's only two environment reads |
| Everything else | Host-testable modules that take their inputs as parameters |

To add a config lever:

1. Add a `DEFAULT_*` const and a `Config` field in `src/config.rs`, and parse it in
   `parse_config`, with a refusal that names the key.
2. Send it in `request::body`.
3. Add it to the body key-set test in `tests/tavily_ops.rs`, the configuration table above,
   and the `murmur.yaml` description.

```bash
cargo test -p murmur-tool-tavily
MUR_BIN=/path/to/mur cargo test -p murmur-tool-tavily --test mur_run_gateway -- --ignored
```
