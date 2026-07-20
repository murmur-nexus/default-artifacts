# murmur-hook-memory-jsonl

Durable per-Turn Memory Log — appends each Turn to a JSONL file in the capsule
workdir, reloads prior Turns at task start, and writes a close-out marker at
task end.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: blocking · commit policy: `replace-context`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
