# murmur-tool-report

A capsule states its own conclusion, in its own words, where a consumer can find it without
coordination.

`report` is terminal: it carries `{outcome, summary, deliverables, notes_for}` and records
the capsule's verdict. `progress` is not: it carries a `note` and records that work is
advancing without concluding anything. There is no third operation and no read-back — the
file is the interface.

Both write one fixed file, `state/report.json`, inside the durable state store the capsule
opens with `capabilities.state`. Nothing else is written, no other file is read, and the
trace is untouched.

## The three states

| File | `concluded` | Means |
|---|---|---|
| absent | — | the capsule never reported; a consumer must handle this and must never read it as success |
| present | `false` | the capsule filed progress notes and never concluded; also never success |
| present | `true` | the capsule's verdict is in `outcome` |

A second `report` wins and bumps `revision`, and the verdict it replaced is pushed onto
`superseded` in the same file — a silently overwritten first verdict is how a consumer ends
up trusting the wrong one.

## The file

```json
{
  "report_version": 1,
  "capsule": "porter",
  "session_id": "01JD…",
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
  - name: murmur-tool-report
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
| `config_invalid` | `error` | the `config:` block is present but not usable |
| `io_error` | `error` | the filesystem refused a read, a write or the rename |

## Tests

```
cargo test -p murmur-tool-report
```

`tests/report_ops.rs` runs the operations against a real directory on the host.
`tests/wasm_component.rs` builds the `wasm32-wasip2` component and instantiates it under
Wasmtime with two preopens, which is the only place the state grant, the workdir isolation
and the guest environment can be proved.
