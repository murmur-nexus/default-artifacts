# murmur-hook-debug

Writes hook lifecycle events to `hook-debug.jsonl` in the capsule workdir.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: async · commit policy: none. Useful for inspecting
exactly what the runtime dispatches to hooks.

See [murmur.yaml](./murmur.yaml) for the full manifest.
