# murmur-tool-code-graph

Indexes a Rust repository into a SQLite-backed symbol/edge graph and answers
structured queries over it. Symbols are addressed by a stable identity —
`rust://<package>/<module>#<qualified-name>(<signature>)` — that survives
unrelated edits elsewhere in the repo, not by `file:line`.

Native binary tool (`runtime: tool`, `implementation: native`) — it links
SQLite and tree-sitter C sources that do not cross-compile to `wasm32-wasip2`.
Operations: `index_repository`, `find_symbol`, `get_symbol`, `slice_symbol`,
`explain_path`, `impact_analysis`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
