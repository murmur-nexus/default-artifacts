# murmur-hook-debug

Writes hook lifecycle events to `hook-debug.jsonl` in the capsule workdir.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: async · commit policy: none. Useful for inspecting
exactly what the runtime dispatches to hooks.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-debug
    version: 0.5.0
    runtime: hook
    capabilities:
      filesystem:
        scope: "."
```

`capabilities.filesystem.scope: "."` is required. The hook opens
`hook-debug.jsonl` with a relative path, i.e. at the root of whatever directory
the runtime preopens for it, and appends one JSON line per lifecycle event.
Without a filesystem grant the hook has no preopened directory at all, the open
fails, and the hook logs nothing.

## Emitted lines

One line per event, with these keys:

| Event | Keys |
| --- | --- |
| `tool-call` | `event`, `turn`, `tool_name`, `input_bytes`, `output_bytes`, `duration_ms`, `status` |
| `shell` | `event`, `turn`, `command`, `exit_code`, `stdout_bytes`, `stderr_bytes`, `duration_ms` |

`on-tool-call` and `on-shell` are each dispatched twice for a call that runs:
once *before* it runs, so a policy hook can deny it, and once *after* with the
result. This hook only writes the second one, so one call is always one line —
never two, and never a line for a call that was denied or never made. The
before-the-call dispatch is the only one that can reach a hook declaring
`commit_policy: deny`; this hook declares `commit_policy: none` and so does not
see it today.

No network capability is required — the hook makes no outbound calls.

The `capabilities:` block belongs on **your** capsule manifest's `artifacts:`
entry, not in this hook's bundled `murmur.yaml`. The runtime only reads the
operator-side grant; a `capabilities:` key inside a hook artifact is never
consulted for enforcement, which is what stops a hook from widening its own
access.

See [murmur.yaml](./murmur.yaml) for the full manifest.
