# murmur-hook-shell-desc

Returns enriched tool manifests for common shell binaries at staging time, so
the agent sees accurate descriptions for the shell commands it is allowed to
run.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-stage` · mode: blocking · commit policy: `write-manifests`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
