# murmur-hook-diff-summary

Snapshots files before each editor tool call and emits a structured
unified-diff summary as an artifact event at end of turn.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: blocking · commit policy: none.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-diff-summary
    version: 0.2.0
    runtime: hook
    capabilities:
      filesystem:
        scope: "."
```

`capabilities.filesystem.scope: "."` is required — read access over the whole
workdir. A narrower scope silently breaks diffing for every file outside it:
the hook reads the path reported by each `murmur-tool-editor` call to take the
"before" snapshot, and reads it again at end of turn for the "after" snapshot.
That path comes out of the tool-call payload and changes turn to turn, so the
hook cannot know in advance which subtree the editor will touch — anything it
cannot read is simply omitted from the summary rather than reported as an
error.

No write access is needed beyond what the scope grant implies: the hook returns
its summary as a `HookOutput::Artifact` event and never writes a file itself.
No network capability is required either — the hook makes no outbound calls.

The `capabilities:` block belongs on **your** capsule manifest's `artifacts:`
entry, not in this hook's bundled `murmur.yaml`. The runtime only reads the
operator-side grant; a `capabilities:` key inside a hook artifact is never
consulted for enforcement, which is what stops a hook from widening its own
access.

See [murmur.yaml](./murmur.yaml) for the full manifest.
