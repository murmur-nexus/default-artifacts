# tool-code-graph

Indexes a Rust repository into a SQLite-backed symbol/edge graph and answers
structured queries over it. Symbols are addressed by a stable identity —
`rust://<package>/<module>#<qualified-name>(<signature>)` — that survives
unrelated edits elsewhere in the repo, not by `file:line`.

Native binary tool (`runtime: tool`, `implementation: native`) — it links
SQLite and tree-sitter C sources that do not cross-compile to `wasm32-wasip2`.
Operations: `index_repository`, `find_symbol`, `get_symbol`, `slice_symbol`,
`explain_path`, `impact_analysis`.

See [murmur.yaml](./murmur.yaml) for the full manifest.

## What the capsule must grant

On Linux, with `mur` 0.7.0 or later, the tool runs in the capsule's own sandbox, with the capsule-wide grants ([Native tools](https://docs.murmur.nexus/reference/containment/#native-tools)). A `capabilities:` block on the tool's entry in the capsule's `artifacts:` does not narrow them and prints `W-SEC-008`.

| Need | Capsule grant |
|---|---|
| Read the sources and write `<repo_path>/.murmur/code-graph.db` | `repo_path` inside the session workdir; under `scoped` and `sealed` nothing outside it is reachable |

`index_repository` and `find_symbol` were run confined under both `scoped` and `sealed` with no grant beyond the workdir. See [What each native tool needs](https://docs.murmur.nexus/reference/default-artifacts/#native-tool-needs).
