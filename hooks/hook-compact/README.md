# hook-compact

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

The built-in summarisation prompt asks for what has already been done, the
decisions taken and their reasons, the facts that were expensive to find, and
what is still outstanding.

When the host supplies `event.system-prompt`, it fully replaces the hook's built-in
summarisation prompt for the compaction inference call (a replacement, not a
concatenation), and applies identically to both the primary attempt and the
`model: none` fallback. When it is absent, the hook uses its own built-in default.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur/hook-compact
    version: 0.12.0
    runtime: hook
```

No `capabilities:` block is required — this hook touches neither the network
nor the filesystem, and runs fully confined under the runtime's default-deny
capability model.

It reaches the model through the `murmur:runtime/inference` host import, which
is not a capability the hook declares, and returns the summary as one `user`
message in its `HookOutput`. The runtime replaces the session context with that
message (`replace-context`). The runtime keeps the message that opened the
current task next to the summary, so the built-in prompt does not ask the
summary to restate the task. With `inference.compaction.dump_summaries: true`
the runtime also keeps a copy of each committed summary, as described in
murmur's context compaction guide.

| `mur` | Pin |
|---|---|
| The release after 0.7.0, or later | `0.10.0` |
| 0.7.0 or earlier | `0.9.0` |

`mur` 0.7.0 and earlier do not keep the task message through a compaction.
There, only the summary carries the task forward, and from 0.10.0 on the
summary leaves it out, so the agent continues without its task.

See [murmur.yaml](./murmur.yaml) for the full manifest and configuration.
