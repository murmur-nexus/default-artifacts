# murmur-hook-memory-jsonl

Durable per-Turn Memory Log — appends each Turn to a JSONL file in the capsule
workdir, reloads prior Turns at task start, and writes a close-out marker at
task end.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: blocking · commit policy: `replace-context`.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-memory-jsonl
    version: 0.2.0
    runtime: hook
    capabilities:
      filesystem:
        scope: "."
```

`capabilities.filesystem.scope: "."` is required — read **and** write. The hook
appends each Turn to `memory-log.jsonl` at the workdir root and reads the whole
file back at task start to reload prior Turns. Without a filesystem grant the
hook has no preopened directory, the append fails, and no memory survives
across tasks.

The scope must stay `"."` even if you override the log location. The path is
swappable via the `MURMUR_MEMORY_LOG_PATH` environment variable and is
interpreted relative to the workdir root (e.g. `custom/mem.jsonl`); the hook
creates any missing parent directory itself, so the grant has to cover the
workdir root rather than one fixed subdirectory.

No network capability is required — the hook makes no outbound calls.

The `capabilities:` block belongs on **your** capsule manifest's `artifacts:`
entry, not in this hook's bundled `murmur.yaml`. The runtime only reads the
operator-side grant; a `capabilities:` key inside a hook artifact is never
consulted for enforcement, which is what stops a hook from widening its own
access.

See [murmur.yaml](./murmur.yaml) for the full manifest.
