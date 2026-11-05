# tool-registry-search

Searches a Murmur artifact index for artifacts matching a keyword, and lists every published
version of each match under its full `<scope>/<name>`.

It reads schema 2 indexes, the kind `mur index` writes. A schema 1 index is refused, with the
`mur index` command its publisher regenerates it with.

## Requires

| Needs | From |
|---|---|
| `mur index`, `murmur/<name>` installs, and fit chosen at install | `mur` 0.7.0 or later |
| A WASM tool exporting `murmur:tool/run@0.2.0` | `mur` 0.9.0 or later |
| The default index | a default-artifacts release that publishes `artifacts-index.json`; with none, the default answers `HTTP 404 fetching …` |

The tool's description tells the agent to install `<scope>/<name>` with no version and let `mur`
pick the newest fitting one. A `mur` at v0.6.0 or earlier does not do that.

## Input

| Field | Required | Meaning |
|---|---|---|
| `query` | yes | Matched case-insensitively against the full `<scope>/<name>`, the bare name, the description and the tags |
| `index` | no | A URL (`http://`/`https://`), or a path to an `artifacts-index.json` file: relative to the work folder (or the entry's `capabilities.filesystem.scope`), or absolute inside a host folder the session opened |
| `limit` | no | Maximum number of results. Default: 10 |

With no `index`, the tool reads the index on the latest default-artifacts release:
`https://github.com/murmur-nexus/default-artifacts/releases/latest/download/artifacts-index.json`.
That URL redirects to `release-assets.githubusercontent.com`, so the network allow-list the tool
runs under must list both `https://github.com` and `https://release-assets.githubusercontent.com`
(see [What the capsule must grant](#what-the-capsule-must-grant)).

The `registry` input and `index: "local"` are refused. The global artifact store is on the host,
and a capsule cannot see it; `mur search <query> --index local` searches it there.

## Output

Results are ranked: an exact name first, then a name prefix, then a substring of the name, the
description or a tag. Within a rank they keep the index's name order.

The result's `summary` is `N result(s)`, and its `data` is:

```json
{"index": "<url or path read>", "scope": "murmur", "count": 1,
 "results": [{"name": "murmur/tool-editor", "runtime": "tool",
              "description": "Structured file editing interface for Murmur capsules.",
              "tags": ["tool", "editor"],
              "versions": [{"version": "0.6.0", "release": "v0.25.0", "built_at_level": 1,
                            "platforms": ["any"]}]}]}
```

`versions` is newest first. `platforms` lists the platforms a version's payloads cover, with `any`
for a platform-independent payload. `built_at_level` is `null` when the index does not record it.

The tool lists what is published, not what will load. `mur` decides fit at install time:
`mur install <scope>/<name>`, or the `pull-artifact` tool, installs the newest version that fits
the running `mur`, and refuses an explicit version that does not fit. Pass `<scope>/<name>`
without a version.

## Errors

A failure has status `error`, no `data`, and one of these summaries. `<index>` is the `index`
value as the caller wrote it, or the default URL.

| When | Summary |
|---|---|
| The URL answered 400 or more, after any redirects | `HTTP <code> fetching <index>` |
| The URL, or a host it redirects to, is outside the network allow-list | `failed to fetch <index>: <host> is not in the network allow-list this tool runs under; list it in the capsule's capabilities.network.allow, and in capabilities.network.allow on this tool's entry if the entry narrows it` |
| More than 5 redirects | `failed to fetch <index>: more than 5 redirects` |
| A redirect with no `Location` | `failed to fetch <index>: HTTP <code> from <url> redirects with no Location header` |
| A redirect to a scheme other than `http`/`https` | `failed to fetch <index>: HTTP <code> from <url> redirects to <location>, which is not an http or https URL` |
| The host could not be reached, the TLS session failed, or a timeout passed | `failed to fetch <index>: <wasi:http error code>` |
| The body ran past 16 MiB | `<index> answered with more than 16 MiB; an artifact index is never that large` |
| An absolute path, and the session opens no host folder to the tool | `failed to read <index>: this session opens no host folder to this tool, so an absolute path cannot be read; a relative path opens in the work folder` |
| An absolute path outside every host folder the tool holds | `failed to read <index>: it is outside every host folder this session opens to this tool (<folders>); a relative path opens in the work folder` |
| The file could not be read | `failed to read <index>: <OS error>` |
| The index is not JSON, not a JSON object, or a field is missing or of the wrong type | `failed to parse the artifact index from <index>: …` |
| The index is schema 1, has another `schema_version`, or has none | `<index> is a schema 1 artifact index, …`, `<index> has schema_version <v>; …`, `<index> has no schema_version; …` |

The tool also tags each failure with an internal kind (`fetch_error`, `io_error`, `parse_error`,
`schema_error`); it is not part of what reaches the model.

## What the capsule must grant

The tool fetches over `wasi:http`. The runtime checks every request, and every redirect hop, as
a separate request against the capsule's `capabilities.network.allow`, narrowed by any
`capabilities.network` on the tool's entry. The tool follows at most 5 redirects itself.

| Need | Grant |
|---|---|
| The default index | `https://github.com` and `https://release-assets.githubusercontent.com` in the capsule's `capabilities.network.allow` |
| An `index` URL | Its host, and every host it redirects to, in the capsule's `capabilities.network.allow` |
| Only those hosts for this tool | The same list as `capabilities.network.allow` on the tool's entry; `allow: []` there takes the network away |
| An `index` file | The file inside the work folder, or the entry's `capabilities.filesystem.scope` |
| An absolute `index` path | A host folder the session opened that covers it: the whole host under `mur run --unconfined`, or a `host_paths` entry in `~/.murmur/config.yaml` |

```yaml
capabilities:
  network:
    allow:
      - "https://github.com"
      - "https://release-assets.githubusercontent.com"
artifacts:
  - name: murmur/tool-registry-search
    version: "0.6.0"
    runtime: tool
    capabilities:            # optional: this tool reaches only these two hosts
      network:
        allow:
          - "https://github.com"
          - "https://release-assets.githubusercontent.com"
```

The `capabilities:` block in the tool's own bundled `murmur.yaml` grants nothing; the runtime
reads grants only from the capsule's manifest.

murmur hands each host folder to the tool as a WASI preopen named `murmur-host` followed by the
folder's absolute path, `murmur-host` alone for `/`. The tool resolves the value's `.` and `..`
segments lexically and opens it only when a folder covers it whole-component: `/home/op/idx`
covers `/home/op/idx/artifacts-index.json` but not `/home/op/idx2/…`.

## Breaking in 0.6.0

- The tool is a `wasm32-wasip2` component: one `.mur.zip` for every platform, and no
  per-platform zips.
- An absolute `index` path opens only inside a host folder the session opened. Under
  `--unconfined` that is the whole host; in a contained session with no `host_paths`, every
  absolute path is refused.
- A `capabilities:` block on the tool's entry now narrows the tool: its `network.allow` is the
  list the tool's requests are checked against, within the capsule's. Before 0.6.0 it narrowed
  nothing and printed `W-SEC-008`.

## Build

WASM tool (`runtime: tool`, `implementation: wasm`), built with
`cargo build -p tool-registry-search --target wasm32-wasip2 --release`. The tool opens no socket
itself: the runtime's `wasi:http` makes each connection and its TLS session.

`tests/fixtures/README.md` says how the test indexes and the recorded 0.5.0 answers were made.

See [murmur.yaml](./murmur.yaml) for the full manifest.
