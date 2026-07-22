# murmur-hook-compact

Compacts conversation history when the session token threshold is reached, by asking
an LLM to summarise the transcript — there is no deterministic fallback.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-compaction` · mode: blocking · commit policy: `replace-context` —
the summarised history replaces the session context.

## Behaviour

1. One `murmur:runtime/inference` call using the model the host resolved for
   compaction (`event.model`, if the manifest configures one).
2. If that call fails **and** a distinct model was requested, exactly one retry
   with `model: none` (the capsule's primary model).
3. If both attempts fail, compaction fails hard — the same observable outcome as
   any other driver inference failure. No checkpoint file or deterministic
   summary is ever produced.

When the host supplies `event.system-prompt`, it fully replaces the hook's built-in
summarisation prompt for the compaction inference call (a replacement, not a
concatenation), and applies identically to both the primary attempt and the
`model: none` fallback. When it is absent, the hook uses its own built-in default.

See [murmur.yaml](./murmur.yaml) for the full manifest and configuration.
