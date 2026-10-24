# tool-report

A capsule session states its own conclusion, in its own words, where a consumer can find it
without coordination.

`report` is terminal: it carries `{outcome, summary, deliverables, notes_for}` and records
the capsule's verdict. `progress` is not: it carries a `note` and records that work is
advancing without concluding anything. There is no third operation and no read-back — the
file is the interface.

Both write the calling session's own file inside the durable state store the capsule opens
with `capabilities.state`. Nothing else is written, no other file is read, and the trace is
untouched.

## Where a session's report is

| Seen from | Path |
|---|---|
| Inside the component | `state/reports/<session_id>.json` |
| On the host | `~/.murmur/state/<store>/reports/<session_id>.json` |

| Placeholder | Value |
|---|---|
| `<store>` | `capabilities.state.store` on the tool's entry; defaults to the capsule name |
| `<session_id>` | the session id exactly as the runtime gives it: `MURMUR_SESSION_ID` in the guest, the same `ses_<32 hex>` id `mur run` prints and the session's trace carries. Used unchanged: no case folding, no escaping, no prefix |

Every launch mints a fresh session id — a plain `mur run`, a formation member, a delegated
child and a `--resume` alike — so a resumed conversation gets a new file, and a reader uses
the id of the launch it is reading. Two sessions of one capsule running at once never share a
file, and a new session never starts on top of the last one's. One session that serves
several tasks keeps one file, inside which the `revision` / `superseded` rule below applies.

The `session_id` field inside the file always equals the file's stem. The session id is
required: one that is absent, empty, longer than 128 characters or holds any character
outside `[A-Za-z0-9_-]` fails the call with `session_unavailable` and nothing is written. The
capsule name is best-effort and recorded as `""` when absent.

`state/report.json`, the single shared file earlier versions wrote, is no longer written,
read, migrated or removed; a store that still holds one keeps it untouched.

## The three states

| File | `concluded` | Means |
|---|---|---|
| absent | — | the session never reported; a consumer must handle this and must never read it as success |
| present | `false` | the session filed progress notes and never concluded; also never success |
| present | `true` | the session's verdict is in `outcome` |

A second `report` wins and bumps `revision`, and the verdict it replaced is pushed onto
`superseded` in the same file — a silently overwritten first verdict is how a consumer ends
up trusting the wrong one.

## The file

```json
{
  "report_version": 1,
  "capsule": "porter",
  "session_id": "ses_0192f3a4b5c67d8e9f0a1b2c3d4e5f60",
  "concluded": true,
  "outcome": "success",
  "summary": "Ported the parser and all tests pass.",
  "deliverables": [{ "name": "patch", "kind": "diff", "uri": "out/parser.patch" }],
  "notes_for": [{ "stage": "review", "body": "The lexer change is the risky part." }],
  "reported_at": "2026-09-07T11:04:12.881Z",
  "revision": 1,
  "superseded": [],
  "superseded_count": 0,
  "progress": [],
  "progress_count": 0
}
```

`outcome`, `summary` and `reported_at` are `null` together until a `report` call sets them
together. `superseded` and `progress` are capped (`MAX_SUPERSEDED`, `MAX_PROGRESS_NOTES` in
`src/report.rs`), oldest dropped first, while `superseded_count` and `progress_count` stay
the true totals.

## Retention

After every successful write the tool trims `state/reports/`:

| Constant | Value |
|---|---|
| `MAX_REPORT_FILES` | 64 |
| `REPORT_RETENTION_GRACE` | 7 days |

1. Only files named `<valid session id>.json` count, and the calling session's own file is
   never a candidate. Nothing else is touched: not `state/report.json`, not a `.tmp` file, not a
   foreign file.
2. While more than `MAX_REPORT_FILES` report files exist, candidates are removed oldest last
   write (file mtime) first, ties broken by file name.
3. A file last written less than `REPORT_RETENTION_GRACE` ago is never removed, even over the
   bound.
4. Retention is best-effort and never changes a call's status or envelope. A file another
   session already removed is fine, and a candidate's mtime is re-read just before removal so
   a file written inside the grace since is kept.

64 sits above the sessions of one capsule running at once on one machine plus the finished
sessions whose report has not been read yet; both cases that run sessions at once — two cards
in one Transit stage, two formation members on one capsule — are single-digit counts. Each
file is capped inside (`MAX_PROGRESS_NOTES`, `MAX_SUPERSEDED`), so a fixed count bounds the
store.

**The limit.** A component cannot see whether another session is still running: it holds
only its `state` and workdir preopens and its environment. So the guarantee is that a session
which called `report` or `progress` within the last 7 days keeps its file. A file can be lost
only when its session is still running, has made no call to this tool for more than 7 days,
and at least 64 other sessions of the same capsule have written since; that session's next
call recreates its file from empty. Closing the gap would need murmur to expose session
liveness to a component.

## Deliverables are references

Each is exactly `{name, kind, uri}` — no other key — where `uri` is a workspace path or a
URI. An extra key, a `data:` URI, a newline or a value over `MAX_REFERENCE_CHARS` is refused
with `inline_payload_refused`, and a refused report writes nothing at all.

## Configuration

The `config:` block on this tool's entry in the *capsule's* `murmur.yaml`, delivered by the
runtime and out of the agent's reach. It is optional: with no block, any non-empty `kind`
and any non-empty `stage` is accepted.

```yaml
artifacts:
  - name: murmur/tool-report
    runtime: tool
    capabilities:
      state: {}
    config:
      config_version: 1
      require_report: true
      deliverables:
        kinds: [diff, dataset, report]
      notes_for:
        stages: [review, deploy]
```

`require_report` is the operator's declaration that a report is expected, readable by a
consumer holding this manifest. The tool type-checks it and branches on it nowhere:
enforcing it — making a capsule that exits without a report failed whatever its exit code —
is the consumer's job.

## Errors

| `error_kind` | Status | Raised when |
|---|---|---|
| `invalid_input` | `failed` | unparseable input, not an object, or a required field missing or empty |
| `unknown_operation` | `failed` | `operation` is neither `report` nor `progress` |
| `unknown_outcome` | `failed` | `outcome` is outside `success \| rejected \| blocked \| failed` |
| `inline_payload_refused` | `failed` | a deliverable carries a payload rather than a reference |
| `undeclared_kind` | `failed` | a deliverable `kind` is outside the operator's declared set |
| `undeclared_stage` | `failed` | a note `stage` is outside the operator's declared set |
| `state_unavailable` | `error` | the durable-state grant is missing |
| `session_unavailable` | `error` | `MURMUR_SESSION_ID` is absent, empty, over 128 characters or holds a character outside `[A-Za-z0-9_-]` |
| `config_invalid` | `error` | the `config:` block is present but not usable |
| `io_error` | `error` | the filesystem refused a read, a write or the rename |

## Tests

```
cargo test -p tool-report
```

`tests/report_ops.rs` runs the operations against a real directory on the host.
`tests/wasm_component.rs` builds the `wasm32-wasip2` component and instantiates it under
Wasmtime with two preopens, which is the only place the state grant, the workdir isolation
the guest environment and retention through WASI can be proved.
