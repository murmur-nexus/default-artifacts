# murmur-hook-compact

Compacts conversation history when the session token threshold is reached.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-compaction` · mode: blocking · commit policy: `replace-context` —
the summarised history replaces the session context.

See [murmur.yaml](./murmur.yaml) for the full manifest and configuration.
