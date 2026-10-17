# hook-shell-desc

Returns enriched tool manifests for common shell binaries at staging time, so
the agent sees accurate descriptions for the shell commands it is allowed to
run.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-stage` · mode: blocking · commit policy: `write-manifests`.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur/hook-shell-desc
    version: 0.6.0
    runtime: hook
```

No `capabilities:` block is required — this hook touches neither the network
nor the filesystem, and runs fully confined under the runtime's default-deny
capability model.

The enriched manifests are returned as its `HookOutput`; the host — not the
hook — writes them to `tools/<binary>/murmur.yaml` in the session's runtime
directory (`$MURMUR_RUNTIME_DIR`) under the `write-manifests` commit policy.
`mur` v0.6.0 and earlier write them to `tools/<binary>/murmur.yaml` in the
capsule workdir instead.

See [murmur.yaml](./murmur.yaml) for the full manifest.
