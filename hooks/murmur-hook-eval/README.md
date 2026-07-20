# murmur-hook-eval

Structured evaluation hook for Murmur capsule sessions. Scores each session against configured scorers and writes results to `workdir/eval.jsonl`.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-eval
    version: 0.1.0
    runtime: hook

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
