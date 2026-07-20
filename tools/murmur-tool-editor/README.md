# murmur-tool-editor

Structured file editing for Murmur capsules — read, write, surgical patch, and
search operations (`read_file`, `write_file`, `replace_in_file`,
`find_in_files`) without requiring shell capability grants.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`).

See [murmur.yaml](./murmur.yaml) for the full manifest and per-operation
input/output schemas.
