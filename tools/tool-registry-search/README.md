# tool-registry-search

Searches a Murmur artifact index for artifacts matching a keyword, and lists every published
version of each match under its full `<scope>/<name>`.

It reads schema 2 indexes, the kind `mur index` writes. A schema 1 index is refused, with the
`mur index` command its publisher regenerates it with.

## Requires

| Needs | From |
|---|---|
| `mur index`, `murmur/<name>` installs, and fit chosen at install | a `mur` released after v0.6.0; until then, `mur` built from murmur's `main` |
| The default index | the first default-artifacts release after v0.24.0; until then the default answers `fetch_error` `HTTP 404` |

The tool's description tells the agent to install `<scope>/<name>` with no version and let `mur`
pick the newest fitting one. A `mur` at v0.6.0 or earlier does not do that.

## Input

| Field | Required | Meaning |
|---|---|---|
| `query` | yes | Matched case-insensitively against the full `<scope>/<name>`, the bare name, the description and the tags |
| `index` | no | A URL (`http://`/`https://`), or a path to an `artifacts-index.json` file, relative to the working directory or absolute |
| `limit` | no | Maximum number of results. Default: 10 |

With no `index`, the tool reads the index on the latest default-artifacts release:
`https://github.com/murmur-nexus/default-artifacts/releases/latest/download/artifacts-index.json`.
That URL redirects to `release-assets.githubusercontent.com`, so the capsule's own
`capabilities.network.allow` must list both `https://github.com` and
`https://release-assets.githubusercontent.com`; an index on another host needs that host there too
(see [What the capsule must grant](#what-the-capsule-must-grant)).

The `registry` input and `index: "local"` are refused. The global artifact store is on the host,
and a capsule cannot see it; `mur search <query> --index local` searches it there.

## Output

Results are ranked: an exact name first, then a name prefix, then a substring of the name, the
description or a tag. Within a rank they keep the index's name order.

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

| `error_kind` | When |
|---|---|
| `fetch_error` | The URL answered an HTTP error status, could not be reached, or sent more than 16 MiB |
| `io_error` | The file could not be read |
| `parse_error` | The index is not JSON, not a JSON object, or a field is missing or of the wrong type |
| `schema_error` | The index is schema 1, has another `schema_version`, or has none |

## What the capsule must grant

On Linux, from the first `mur` released after v0.6.0, the tool runs in the capsule's own sandbox, with the capsule-wide grants ([Native tools](https://docs.murmur.nexus/reference/containment/#native-tools)). A `capabilities:` block on the tool's entry in the capsule's `artifacts:` does not narrow them and prints `W-SEC-008`.

| Need | Capsule grant |
|---|---|
| The default index | `https://github.com` and `https://release-assets.githubusercontent.com` in `capabilities.network.allow` |
| An `index` URL | Its host, and any host it redirects to, in `capabilities.network.allow` |
| An `index` file | The file inside the session workdir |

## Build

Native binary tool (`runtime: tool`, `implementation: native`), built with
`cargo build --release -p tool-registry-search`. HTTPS goes through rustls with its bundled
webpki roots and the *ring* crypto provider, so it needs no system certificate store and works
under `scoped` containment, where `/etc/ssl` is not readable.

`tests/fixtures/README.md` says how the test indexes were made.

See [murmur.yaml](./murmur.yaml) for the full manifest.
