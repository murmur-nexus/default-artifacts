# murmur-hook-eval

Structured evaluation hook for Murmur capsule sessions. Scores each session against configured scorers and writes results to `workdir/eval.jsonl`.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-eval
    version: 0.2.0
    runtime: hook
    capabilities:
      filesystem:
        scope: "."                    # always required — eval.jsonl is written every session
      network:
        allow:
          - http://localhost:4318     # only for OTel export; match your observability.otel_endpoint host

observability:
  eval:
    dataset_id: my-dataset          # optional, labels dataset_run records
    scorers:
      - type: exit_ok
        name: success_check         # passes if exit_status == "ok"
      - type: max_turns
        name: turn_limit
        max: 5                      # passes if total_turns <= 5
      - type: max_tokens
        name: token_budget
        max: 50000                  # passes if total_input+output tokens <= 50000
      - type: tool_sequence
        name: tool_order
        expected: [bash, python]    # passes if observed calls contain this subsequence
```

### Capabilities

`capabilities.filesystem.scope: "."` is **always required**. The hook writes
`./eval.jsonl` at the workdir root on every `session_end`, and writes its own
warning log to `./logs/hook-murmur-hook-eval.log`, creating `./logs/` if
needed. Without a filesystem grant the hook has no preopened directory, the
write fails, and `on_session_end` returns an error.

`capabilities.network.allow` is **only required if you want OTel export**, and
must list the same host you set in `observability.otel_endpoint` — the example
above is a placeholder, not a default. The hook reads that endpoint from
`MURMUR_OTEL_ENDPOINT`, which the runtime injects from
`observability.otel_endpoint`.

The export is a hand-rolled HTTP/1.1 POST over a raw WASI socket
(`std::net::TcpStream`), not a `wasi-http` request. If your runtime enforces
network grants only on the `wasi-http` path, the grant above is necessary but
may not be sufficient — verify the log records actually reach your collector
after enabling it. `eval.jsonl` is unaffected either way.

Omitting the network grant while `otel_endpoint` is set does **not** fail the
session: the OTLP POST is denied, a warning goes to stderr, and `eval.jsonl`
has already been written by that point. Scores are still recorded on disk; only
the log-record export to your collector is lost.

The `capabilities:` block belongs on **your** capsule manifest's `artifacts:`
entry, not in this hook's bundled `murmur.yaml`. The runtime only reads the
operator-side grant; a `capabilities:` key inside a hook artifact is never
consulted for enforcement, which is what stops a hook from widening its own
access.

## eval.jsonl output

Two record types are written per session:

**Per-event score** (one per scorer, written at session_end):
```json
{"record_type":"event_score","ts":1234567890,"turn":3,"event_type":"session_end","scorer":"turn_limit","result":"pass","score":1.0,"reason":"turns=3 max=5"}
```

**Dataset run summary** (last record):
```json
{"record_type":"dataset_run","ts":1234567890,"dataset_id":"my-dataset","case_id":"case_001","overall":"pass","scores":{"success_check":1.0,"turn_limit":1.0}}
```

## Scorer types

| Type | Description | Deferred |
|---|---|---|
| `exit_ok` | Passes if exit_status == "ok" | — |
| `max_turns` | Passes if total_turns <= max | — |
| `max_tokens` | Passes if total tokens <= max | — |
| `tool_sequence` | Passes if expected tools appear as a subsequence of observed calls | — |
| `llm_judge` | LLM-as-judge scoring | — |

## OTel integration

When `MURMUR_OTEL_ENDPOINT` is set, eval scores are exported as OTLP log records to `{endpoint}/v1/logs` with attributes `eval.scorer`, `eval.result`, `eval.score`, and `eval.case_id`. The same `trace_id` derived from `session_id` links eval logs to the corresponding Grafana Tempo trace.
