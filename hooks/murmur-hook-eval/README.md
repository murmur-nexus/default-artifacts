# murmur-hook-eval

Structured evaluation hook for Murmur capsule sessions. Scores each session against configured scorers and writes results to `workdir/eval.jsonl`.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-eval
    version: 0.6.0
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

Whenever `MURMUR_EVAL_CONFIG` is present and non-empty, the session ends with an
`eval.jsonl` whose **last line is a `dataset_run` record**. Only a capsule that
declares no `observability.eval` leaves no file behind. A broken config never
fails the session — it is reported on the `dataset_run` line.

Two record types are written per session:

**Per-event score** (one per scoring scorer, written at session_end):
```json
{"event_type":"session_end","reason":"turns=3 max=5","record_type":"event_score","result":"pass","score":1.0,"scorer":"turns","ts":1762000000000,"turn":3}
```

**Dataset run summary** (last record):
```json
{"case_id":"case_001","dataset_id":"regression-suite","overall":"pass","record_type":"dataset_run","scores":{"order":1.0,"success":1.0,"turns":1.0},"ts":1762000000000}
```

### `overall`

| Value | Meaning | `scores` | `event_score` lines | `config_error` key |
|---|---|---|---|---|
| `pass` | Every scorer that produced a score passed | one entry per scoring scorer | one per scoring scorer | absent |
| `fail` | At least one scorer failed | one entry per scoring scorer | one per scoring scorer | absent |
| `no_scores` | The config was valid and produced no score records — an empty `scorers` array, or one whose every entry is `llm_judge` | `{}` | none | absent |
| `config_error` | `MURMUR_EVAL_CONFIG` could not be read | `{}` | none | present |

`record_type`, `ts`, `dataset_id`, `case_id`, `overall` and `scores` are always
present on a `dataset_run` record; `dataset_id` and `case_id` are `null` when
`MURMUR_DATASET_ID` and `MURMUR_CASE_ID` are unset. `config_error` is the one
conditional key, present only when `overall` is `config_error`:

```json
{"case_id":"case_001","config_error":{"key":"scorers[1].type","message":"unknown scorer type 'tool_seqence'"},"dataset_id":"regression-suite","overall":"config_error","record_type":"dataset_run","scores":{},"ts":1762000000000}
```

`key` names the offending config path — the literal `MURMUR_EVAL_CONFIG`, the
literal `scorers`, or `scorers[<i>].<field>` with `i` the zero-based index in the
`scorers` array. The same key and message are appended to
`./logs/hook-murmur-hook-eval.log`, so an operator reading the log and one reading
`eval.jsonl` see the same offending key.

## Scorer types

| Type | Required keys | Description |
|---|---|---|
| `exit_ok` | — | Passes if exit_status == "ok" |
| `max_turns` | `max` (fits `u32`) | Passes if total_turns <= max |
| `max_tokens` | `max` | Passes if total tokens <= max |
| `tool_sequence` | `expected` (non-empty list of tool names) | Passes if expected tools appear as a subsequence of observed calls |
| `llm_judge` | — | **Deferred.** Recognized and accepted, scores nothing — no `event_score` record and no entry in `scores`. A config whose only scorers are `llm_judge` lands on `no_scores`. |

`name` is optional on every scorer type and defaults to the `type` string.

### Required keys are required

A scorer that omits a required key is a `config_error` naming that key; the hook
invents no threshold on the operator's behalf. `max_turns` with no `max` does not
mean 10, `max_tokens` with no `max` does not mean 100 000, and `tool_sequence`
with no `expected` does not vacuously score 1.0.

Murmur's own manifest parser fills those defaults **upstream of the hook**: a
capsule's `observability.eval` that omits `max` reaches the hook as `max: 10` for
`max_turns` and `max: 100000` for `max_tokens`, and an omitted `expected` reaches
it as `[]`. So requiring them here does not reject an ordinary capsule manifest —
it catches a hand-set `MURMUR_EVAL_CONFIG`, and it stops the hook from being a
second, silent source of the same defaults.

The first offending element wins: parsing stops there, and scorers that happened
to precede it are discarded rather than scored against a partially applied config.

## OTel integration

When `MURMUR_OTEL_ENDPOINT` is set, eval scores are exported as OTLP log records to `{endpoint}/v1/logs` with attributes `eval.scorer`, `eval.result`, `eval.score`, and `eval.case_id`. The same `trace_id` derived from `session_id` links eval logs to the corresponding Grafana Tempo trace.
