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

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-compact
    version: 0.3.0
    runtime: hook
```

No `capabilities:` block is required — this hook touches neither the network
nor the filesystem, and runs fully confined under the runtime's default-deny
capability model.

It reaches the model through the `murmur:runtime/inference` host import, which
is not a capability the hook declares, and returns the summary as its
`HookOutput`; the runtime — not the hook — writes the result under
`workdir/checkpoints/` per the `replace-context` commit policy.

See [murmur.yaml](./murmur.yaml) for the full manifest and configuration.
