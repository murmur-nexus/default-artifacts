# murmur-tool-create

Scaffolds a new tool artifact directory with `murmur.yaml`, a stub
implementation, and a README.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). Input arrives on the stdin envelope; the scaffold
is written under `tools/<name>/` in the capsule workdir.

See [murmur.yaml](./murmur.yaml) for the full manifest.
