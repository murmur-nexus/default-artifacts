# tool-huggingface

Read-only Hugging Face access for Murmur capsules: search, repo inspection, dataset peek, file
download and paper lookup. The tool only reads. It sends `GET` and `HEAD`, never writes to the
Hub, and never holds, reads or builds the token: the operator binds the token on the tool's
entry, and the runtime attaches it.

The model gets one tool, `tool-huggingface`, with five operations selected by `op`:

| `op` | What it does | Where it goes |
|---|---|---|
| `search` | Keyword search over models, datasets, Spaces or papers (`kind`). Models and datasets also take `task`, `library`, `license`, `tag` and `sort` | Hub API, through the gateway |
| `repo` | One model, dataset or Space: license, tags, gated state, privacy, last update, commit, downloads, likes, the file list with sizes, and the card text | Hub API and the repo's `README.md`, through the gateway |
| `dataset` | Splits, schema and a page of rows of one dataset, a text search over rows (`query`) or a filter (`where`, `order_by`), without downloading it | Datasets server, direct and keyless |
| `download` | One file, or one byte range of it, into the work folder at `dest`, with its sha256 | `resolve/<revision>/<path>` through the gateway; a CDN redirect target direct and keyless |
| `paper` | One paper by arXiv id: title, authors, abstract, and the models and datasets that cite it | Hub API, through the gateway |

WASM component (`wasm32-wasip2`), exporting `murmur:tool/run@0.2.0` and importing no
`murmur:*` interface. It needs a murmur runtime with per-entry credential gateways and
`keyless:` gateways; it was built and proved against murmur v0.12.0 (WIT level 8).

## Input

The input is a JSON object, or the same object encoded twice (re-parsed once). A JSON `null`
counts as absent. A property the op does not use is refused with `invalid_input`, naming it and
the op. Every rule below, and the `dest` existence check, runs before any request, so a refused
call sends nothing.

| Property | Type | Used by | Rule |
|---|---|---|---|
| `op` | `search`, `repo`, `dataset`, `download`, `paper` | all | Required |
| `kind` | `model`, `dataset`, `space`, `paper` | `search` (required); `repo`, `download` (default `model`; `paper` refused) | |
| `query` | string | `search` (required); `dataset` (text search over rows) | Trimmed, not blank, at most 200 characters, no control characters |
| `task`, `library`, `license`, `tag` | string | `search` with `kind` `model` or `dataset` | Each at most 100 characters, not blank, no control characters |
| `sort` | `downloads`, `likes`, `updated`, `trending`, `created` | `search` with `kind` `model` or `dataset` | Always descending |
| `limit` | integer | `search` (1–50, default 10); `dataset` (1–100, default 10) | Above the maximum it is clamped, and the header says so; below 1 is refused |
| `offset` | integer ≥ 0 | `dataset` | Default 0 |
| `repo` | string | `repo`, `dataset`, `download` (required) | `owner/name` or a bare legacy `name`; each part `[A-Za-z0-9][A-Za-z0-9._-]*`, no `..` or `--`, at most 96 characters in all |
| `revision` | string | `repo`, `download` | Default `main`. Letters, digits, `.`, `_`, `/`, `-`; no `..`; at most 128 characters. Percent-encoded in the URL, so `refs/pr/1` works |
| `repo_file` | string | `download` (required) | Relative; no empty, `.` or `..` component; no leading `/`, no `\`, no control characters; at most 1024 bytes. Each segment is percent-encoded in the URL |
| `dest` | string | `download` (required) | Relative to the work folder. Refused: absolute (`/…`, `C:`), any `..`, `.` or empty component, a trailing `/`, `\`, NUL or control characters, a component over 255 bytes, over 1024 bytes in all, a path that already exists, a path whose `<dest>.partial` exists, or a parent that is a file. Parent folders are created |
| `byte_offset`, `byte_length` | integer | `download` | Both or neither. `byte_offset` ≥ 0; `byte_length` from 1 to `max_download_bytes` |
| `config`, `split` | string | `dataset` | Default: the first split `/splits` lists. One without the other picks the first split matching it |
| `where`, `order_by` | string | `dataset` | A filter. `where` cannot be combined with `query`; `order_by` needs `where`; each at most 1000 characters |
| `arxiv_id` | string | `paper` (required) | `^\d{4}\.\d{4,5}(v\d+)?$`; a version suffix is dropped |

`dest` carries `format: murmur-destination` in the schema, so murmur's `read_only` check judges
it as a write destination. The file inside the repository is named `repo_file` so murmur does not
judge it as a local path.

Examples:

```json
{"op": "search", "kind": "model", "query": "bert", "sort": "downloads", "limit": 5}
{"op": "repo", "repo": "openai-community/gpt2"}
{"op": "dataset", "repo": "cornell-movie-review-data/rotten_tomatoes", "where": "\"label\"=1", "order_by": "\"text\" DESC", "limit": 3}
{"op": "download", "repo": "openai-community/gpt2", "repo_file": "model.safetensors", "dest": "dl/range.bin", "byte_offset": 0, "byte_length": 1024}
{"op": "paper", "arxiv_id": "1706.03762"}
```

## Output

On success, `status` is `passed`:

| Field | Value |
|---|---|
| `summary` | One line: `5 models for "bert"`, `model openai-community/gpt2`, `3 rows of <repo> <config>/<split> (of N)`, `downloaded dl/config.json (665 bytes)`, `paper 1706.03762: <title>` |
| `data` | The plain text below, at most `max_output_bytes` |
| `data_path` | Set only when the text was truncated and the full upstream response was saved |
| `truncated` | Whether anything was omitted or cut |
| `metadata` | Reads: `state_effect: read`, `resource_id: hf:<op>:<subject>`. Download: `state_effect: mutate`, `resource_id: <dest>` |

The first line of `data` names the op, the kind and the subject. With no `config:` block, the
second line is `[defaults in use — no config: block on this entry]`.

```
Hugging Face search: models for "bert" — 5 shown (limit 5); sort downloads (descending)

[1] google-bert/bert-base-uncased
fill-mask · transformers
downloads 33456723 · likes 3084 · updated 2024-02-19T11:06:12.000Z · license apache-2.0
```

| Op | Body after the header |
|---|---|
| `search` | Numbered items: the id (and, for a paper, ` — <title>`); a description cut to 200 characters; then `downloads`, `likes`, `updated`, `license` (or, for a paper, `upvotes`, `published`). A field the endpoint does not report is omitted |
| `repo` | `license:`, `tags (N):` (at most 50, then `+N more`), `gated:`, `private:`, `last update:`, `commit:`, `downloads:`, `likes:`, `pipeline:`, `library:`, `sdk:`; then `files (N, total S bytes):` with `<size>  <path>` lines; then `card:` and the card text, or `card: not readable (<reason>)` |
| `dataset` | `splits (N):`, `schema:` (`name: dtype`, `name: ClassLabel [a, b]`), then `rows (N returned):` with `[row <row_idx>] {…}` lines. The header names the config/split, the search or filter, the row range and the total |
| `download` | `dest:`, `bytes: N (whole file)` or `bytes: N (range a–b of size)`, `sha256:` (marked when it matched the Hub's `x-linked-etag`), and `fetched:` listing each hop as `Hub via gateway (credential attached by the runtime when one is bound)` or `<host> direct (no credential)`. A signed URL's query never appears |
| `paper` | `title:`, `authors (N):` (at most 50, then `+N`), `published:`, `upvotes:`, `abstract:` and the text, then `models citing it (10 shown of N, by downloads):` and the same for datasets |

The search description comes from the best field each endpoint returns:

| `kind` | Description |
|---|---|
| `model` | `pipeline_tag · library_name` |
| `dataset` | `description` |
| `space` | `cardData.short_description`, else `cardData.title` |
| `paper` | `summary`, with `title` on the id line |

### Bounds and the spill file

The whole text, trailer included, is at most `max_output_bytes`:

| Op | How the budget is spent |
|---|---|
| `search` | The header, then whole items while they fit; at least one, cut if it must be |
| `repo` | The header and tags, then file lines within half of what is left, then the card cut into the rest |
| `dataset` | The header, the splits list (whole names, within a quarter of what is left), the schema, then whole rows while they fit, at least one. Every string inside a row is cut to 1000 bytes |
| `paper` | The header, title and authors, then the abstract (leaving room for the citing repos), then whole citing-repo lines |

When anything is cut, the text ends with a trailer naming what was cut:

```
[truncated: descriptions cut to 200 characters; 31 of 50 results shown; full response in hf-results/search-1.json]
```

The full upstream response is written to `hf-results/<op>-<n>.json` in the work folder, at the
first free `n`, and `data_path` names it. For `search` it is the Hub's JSON as received. For the
ops that send several requests it is an object keyed by request: `repo` →
`{"metadata", "readme"}`, `dataset` → `{"splits", "rows" | "search" | "filter"}`, `paper` →
`{"paper", "models", "datasets"}`. If the save fails, `data_path` is unset, `truncated` is still
set, and the trailer says the full response could not be saved.

## Errors

A failure returns `data` as compact JSON, `{"ok": false, "error_kind": …, "message": …}`, plus
the extra fields below. `summary` is the message and `metadata` is empty. Upstream text in a
message is the response's `x-error-message` header if present, else the body's `error`, cut to
300 characters.

| `error_kind` | `status` | When | Extra fields |
|---|---|---|---|
| `invalid_input` | `failed` | Any input rule above; an existing `dest`; `byte_offset` at or past the end of the file | — |
| `config_invalid` | `error` | The `config:` block breaks a rule below | — |
| `gateway_missing` | `error` | The entry has no `gateway:`, for every op. Nothing is sent. The message shows the keyed block, the `keyless: true` alternative and the `mur config set` command | — |
| `not_found` | `failed` | HTTP 404; `x-error-code` `RepoNotFound`, `EntryNotFound` or `RevisionNotFound`; a `401` with no error code on a repo, file or paper read (how the Hub says "does not exist, or is private"); a `config`/`split` the datasets server does not list | `http_status` |
| `gated` | `failed` | `x-error-code: GatedRepo` (401 or 403); a gated dataset on the dataset path | `http_status` |
| `upstream_unauthorized` | `error` | Any other 401 or 403; a private dataset on the dataset path. The message names `gateway.api_key` and `credentials.HUGGINGFACE_ACCESS_TOKEN` | `http_status` |
| `rate_limited` | `error` | HTTP 429. The tool never sleeps or retries | `http_status`, `retry_after` when sent |
| `too_large` | `failed` | The reported size, or the range, is over `max_download_bytes`. Only `HEAD` requests were sent | `size`, `cap` |
| `viewer_unavailable` | `failed` | The datasets server cannot serve this dataset, split, search or filter: a 5xx (501 included) with its `error` text, or `/splits` listing no splits with a `failed` or `pending` entry | `http_status` when any |
| `upstream_error` | `error` | Any other non-2xx, including a datasets-server 422 for a malformed `where` | `http_status` |
| `transport_error` | `error` | The request could not be sent: denied by the allow-list (the message names the host and the fix), DNS, TLS, timeout | `host` when denied |
| `response_invalid` | `error` | A 2xx body that is not the JSON expected; a redirect with no `Location` or to a refused target; more than 3 redirects; no reported size; a `200` to a ranged request; a byte-count or sha256 mismatch; an API body over 8 MiB | — |
| `write_failed` | `error` | The work folder could not be written: the destination folder or partial file could not be created, or the rename failed | — |

A download that fails after it started removes `<dest>.partial`; no partial or mismatched file is
ever left at `dest`.

## Configuration

The `config:` block on the entry is optional. With no block, the defaults apply and the output
says so. An empty block (`config: {}`) is refused, because `config_version` is required whenever
the block is present. Unknown keys are ignored, except credential-shaped ones (a name containing
`key`, `token`, `secret`, `password`, `authoriz`, `bearer` or `credential`), which are refused.

| Key | Allowed | Default | Effect |
|---|---|---|---|
| `config_version` | `1` | required when the block is present | — |
| `max_output_bytes` | integer 2048–65536 | 16384 | The budget for every op's text, trailer included |
| `max_download_bytes` | integer 1–2147483648 | 104857600 (100 MiB) | The download cap, checked against the size a `HEAD` reports before any byte is fetched; also the ceiling on `byte_length` |
| `datasets_server_endpoint` | `https://<host>[:port]`, or `http://` to `127.0.0.1`, `localhost` or `[::1]`; no path, query or userinfo | `https://datasets-server.huggingface.co` | Where `op: dataset` goes. Its host must be on `network.allow` |

## Binding the key

Bind the token on the tool's own entry as `gateway.api_key`, and list the direct hosts on the
entry and capsule-wide:

```yaml
capabilities:
  network:
    allow:
      - https://datasets-server.huggingface.co
      - https://us.aws.cdn.hf.co

artifacts:
  - name: murmur/tool-huggingface
    version: 0.1.0
    runtime: tool
    gateway:
      endpoint: https://huggingface.co
      api_key: ${HUGGINGFACE_ACCESS_TOKEN}
    capabilities:
      network:
        allow:
          - https://datasets-server.huggingface.co
          - https://us.aws.cdn.hf.co
    config:
      config_version: 1
      max_download_bytes: 104857600
```

Write `gateway.endpoint` with no path: the tool appends `/api/…` and `/<repo>/resolve/…` itself.
Store the token with:

```bash
mur config set -g credentials.HUGGINGFACE_ACCESS_TOKEN <token>
```

A read-only ("read") token is enough. For public data only, bind the gateway without a token:

```yaml
    gateway:
      endpoint: https://huggingface.co
      keyless: true
```

| Where the token comes from | When it is read |
|---|---|
| `credentials.HUGGINGFACE_ACCESS_TOKEN` in the global config | Again while the capsule runs, so a rotated token is used on the next call |
| The `HUGGINGFACE_ACCESS_TOKEN` process environment variable | Once at launch, with warning `W-SEC-027` |
| A literal value in `api_key:` | Once at launch, with warning `W-SEC-027` |

## Who holds the key

The tool never holds, reads or builds the token. Its bundled `murmur.yaml` declares only how the
Hub takes one:

```yaml
upstream_auth:
  header: Authorization
  value: "Bearer {key}"
```

The runtime gives the tool one address, `MURMUR_GATEWAY_ENDPOINT`, which carries no token. A
request the tool sends there is a *gateway request*: the runtime removes any `Authorization`
header, attaches exactly one rendered from the token (none with `keyless: true`), and forwards it
over TLS to `gateway.endpoint`. If the Hub answers `401`, the runtime re-reads the credential and
resends once when the value has changed. A `3xx` comes back to the tool unfollowed, and the token
is never sent to its `Location`.

Every other request is a *direct request* to an absolute URL, checked against `network.allow`,
and the runtime attaches nothing to it. The tool follows redirects itself:

| `Location` | Followed as |
|---|---|
| Relative (`/api/resolve-cache/…`), from a gateway request | A gateway request to the same Hub origin, query kept |
| Relative, from a direct request | A direct request to the same host |
| Absolute `https://` | A direct request, with no credential. An absolute `Location` naming the Hub host also goes direct, and is denied unless that host is on the allow-list |
| Absolute `http://` to `127.0.0.1`, `localhost` or `[::1]` | A direct request |
| Absolute, naming the gateway's own address (`127.0.0.1:9`) | Refused (`response_invalid`) |
| Absolute `http://` to any other host, any other scheme, or none | Refused (`response_invalid`) |

At most 3 redirects are followed per request. Every request has a 30-second connect, first-byte
and between-bytes timeout, and none is ever repeated.

The tool accepts no token any other way:

| Where a token might be put | Result |
|---|---|
| `config:` | Refused (`config_invalid`), as is any credential-shaped key |
| An environment variable the tool reads | Not supported: the tool reads only `MURMUR_ARTIFACT_CONFIG` and `MURMUR_GATEWAY_ENDPOINT` |
| No `gateway:` on the entry | Every call returns `gateway_missing` and nothing is sent |

### A gated or private dataset cannot be peeked

murmur gives an artifact one gateway, with one endpoint (murmur v0.12.0,
`crates/capsule-runtime/src/credential_gateway.rs`: `GatewayTable.by_artifact` holds one gateway
per artifact name). This tool's gateway is bound to the Hub, so the datasets server is reached
directly and without the token. It answers `401` for a gated, private or missing dataset alike;
the tool then asks the Hub (through the gateway) which it is, and returns `gated`,
`upstream_unauthorized` or `not_found`, saying the dataset cannot be peeked. `op: download` still
fetches that dataset's files through the gateway, when the token can read them.

## Network

| Choice | `network.allow` on the entry and capsule-wide | What works |
|---|---|---|
| Keyed gateway | `https://datasets-server.huggingface.co` and the CDN hosts. Never `huggingface.co` | Public and token-readable gated or private Hub reads and downloads; public dataset peek |
| Keyless gateway | The same list | Public Hub reads and downloads; public dataset peek. Gated reads return `gated` |
| `allow: []` | Nothing | `search`, `repo`, `paper`, and downloads of small (non-LFS) files, which the Hub serves itself through `/api/resolve-cache`. `dataset` and LFS/xet downloads return `transport_error` naming the denied host |

Do not list `huggingface.co` in any `network.allow`. Gateway requests are not checked against
the allow-list, so such an entry grants only direct, keyless reach to the Hub, and murmur warns
`W-SEC-025`.

CDN hosts observed from this build's live proofs on 2026-11-20: every LFS/xet file tried
(`openai-community/gpt2`, `google-bert/bert-base-uncased`, `sentence-transformers/all-MiniLM-L6-v2`,
`Qwen/Qwen2.5-0.5B` weights, and a `stanfordnlp/imdb` parquet file) answered `302` to
`https://us.aws.cdn.hf.co/xet-bridge-us/…`. Small files answered `307` to a relative
`/api/resolve-cache/…`, which stays on the Hub, or `200` directly. The CDN host can differ by
region and over time. A redirect to an unlisted host fails as `transport_error` and names the
host, so add the named host to both allow-lists.

## Cost

murmur brokers the Hub calls but **does not meter** them: every launch warns `W-SEC-030`. The
Hub's public API is free to read; heavy use is rate-limited (`rate_limited`, with `Retry-After`).
The lever that bounds what a call can pull is `max_download_bytes`: a download's size is read with
a `HEAD` and checked against it before any byte is fetched. `max_output_bytes` bounds what reaches
the model.

## Untrusted content

Search results, cards, dataset rows and paper abstracts are third-party text and may contain
instructions aimed at the model. The runtime fences every tool result as untrusted content, and
that fence is what protects the model. The tool passes upstream text through byte for byte,
cutting only to fit the budget. It does not sanitise, escape or fence anything itself, by design.

## Development

| Item | Location |
|---|---|
| Transport | `wasm_tool` in `src/lib.rs`: blocking `GET`/`HEAD` over `wasi:http`, and the crate's only two environment reads |
| Redirect rule, transport trait | `src/http.rs` (`Transport`, `Reply`, `BodySink`, `Client`) |
| Ops | `src/hub.rs` (`search`, `repo`, `paper`), `src/viewer.rs` (`dataset`), `src/download.rs` |
| Input and config rules | `src/input.rs`, `src/config.rs` |
| Error kinds, spill | `src/ops.rs` |

To add a search filter: map it in `hub::search_path`, accept it in `input::search` (and
`Op::properties`), add it to `input_schema` in `murmur.yaml`, to the URL assertion in
`tests/hf_ops.rs` (`search_models_maps_every_filter_and_renders_each_field`), and to the Input
table above. To add a config key: a const and a `Config` field in `src/config.rs`, parsed in
`parse_config` with a refusal naming the key, a case in its tests, and a row in the
Configuration table.

```bash
cargo test -p tool-huggingface
MUR_BIN=/path/to/mur cargo test -p tool-huggingface --test mur_run_gateway -- --ignored
```
