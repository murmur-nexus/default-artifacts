# murmur-hook-regression-verifier

In-the-loop regression enforcement. A single blocking hook, bound to all
lifecycle events, that both observes a task's test activity and gates at task
end.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: blocking · commit policy: reopen-task.

## What it does

- **`on-shell`** — recognizes a test-runner invocation from the command string
  (`pytest`, `cargo test`, `go test`, or a jest invocation — `npx jest` /
  `npm test` / `yarn test`), parses its combined stdout+stderr with the same
  parsers `murmur-tool-test-report` uses (shared via the `murmur-test-parse`
  crate — the hook cannot call that tool out-of-process), and records a
  per-command snapshot of `{passed count, failing test names}`. The **first**
  snapshot for a command seen **before the task's first source edit** is that
  command's *baseline*; every later snapshot for the same command is its
  *current* result.
- **`on-tool-call`** — a call to `murmur-tool-editor` or `murmur-tool-create`
  marks that the task's first source edit has occurred.
- **`on-task-end`** — diffs each command's latest current snapshot against its
  baseline. A previously-passing test now failing is a **regression**. When
  essentially the entire baseline-passing set stops passing at once (≥90% of it),
  the verdict is additionally flagged as a **collection failure** — the
  language-agnostic "0/644" module-breaker signature, derived purely from the
  passed/failed counts. Newly-passing tests are recorded for visibility but never
  gate. If any regression is found, the hook returns `reopen-task(reason)` with
  a message naming every regressed test; otherwise it returns `none`.

Every verdict — reopen or clean, on every `on-task-end` dispatch including those
after an earlier reopen — is appended as one JSON line to
`regression-verifier.jsonl` at the workdir root, so a run's regression state is
inspectable after the fact.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-regression-verifier
    version: 0.4.0
    runtime: hook
    capabilities:
      filesystem:
        scope: "."
```

`capabilities.filesystem.scope: "."` is required. The hook opens
`regression-verifier.jsonl` with a relative path, i.e. at the root of whatever
directory the runtime preopens for it, and appends one JSON line per
`on-task-end` verdict. Without a filesystem grant the hook has no preopened
directory at all, the open fails, and no verdict log is written (the reopen
decision itself still works).

No network capability is required — the hook makes no outbound calls.

The `capabilities:` block belongs on **your** capsule manifest's `artifacts:`
entry, not in this hook's bundled `murmur.yaml`. The runtime only reads the
operator-side grant; a `capabilities:` key inside a hook artifact is never
consulted for enforcement, which is what stops a hook from widening its own
access.

### Reopen budget

Because this hook returns `reopen-task`, the runtime consults two fields on
**your** capsule manifest (not on any hook's `murmur.yaml`):

```yaml
inference:
  max_task_reopens: 1   # default 1; 0 disables reopening; never exceeds max_turns
```

`max_task_reopens` bounds how many times a single task may be reopened before it
is finalized regardless; it is never allowed past the existing
`inference.max_turns` ceiling.

## Compatibility note

`commit_policy: reopen-task` and the `reopen-task(string)` case of
`murmur:hook/lifecycle` are the `@0.4.0` hook interface. Until the murmur
runtime that introduces them (branch `ac1e1848`) merges to `main`, a live
capsule cannot honor this hook's reopen decision — the runtime on `main` is
still at the `@0.3.0` lifecycle package and does not recognize
`commit_policy: reopen-task`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
