# murmur-hook-diff-summary

Snapshots files before each editor tool call and emits a structured
unified-diff summary as an artifact event at end of turn.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: blocking · commit policy: none.

See [murmur.yaml](./murmur.yaml) for the full manifest.
