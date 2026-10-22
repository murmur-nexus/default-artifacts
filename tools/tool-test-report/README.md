# tool-test-report

A native Murmur tool that parses a raw test-runner output file — one the agent
has **already captured** via its own shell call — into a structured list of
failures. It never spawns a test process; it only reads a file on disk.

## Contract

stdin (runtime envelope): `{"data": "<json>", "log_path": "..."}` where `data`
carries the operation payload (a JSON object, or a JSON-encoded string).

```json
{ "operation": "parse", "input_path": "./out/tests.txt", "format": "auto", "repo_path": "/abs/rust/repo" }
```

- `operation` (required): `"parse"` (the only operation).
- `input_path` (required): path to the raw test-runner output file.
- `format` (optional, default `"auto"`): `auto` | `cargo_test` | `pytest` | `go_test` | `jest`.
  `auto` detects the runner from signature markers; a failed detection returns a
  `failed` result asking for an explicit format.
- `repo_path` (optional): a Rust repo containing `.murmur/code-graph.db`. When
  present, `cargo_test` failures get a best-effort `stable_id` (code-graph's
  symbol identity) resolved read-only from that db. Other formats always report
  `stable_id: null` (code-graph's MVP language scope is Rust only).

stdout: one JSON object. `data` carries:

```json
{
  "format_used": "cargo_test",
  "total": 5, "passed": 3, "failed": 2, "truncated": false,
  "failures": [
    { "test_name": "tests::foo", "file": "src/lib.rs", "line": 42,
      "exception": "panic", "message": "…", "stable_id": null }
  ],
  "data_path": null
}
```

Above 50 failures the inline `failures` array is capped, `truncated` is `true`,
and the full array is written to `<input-stem>.failures.json` next to the input,
referenced by `data_path`.

## What the capsule must grant

On Linux, with `mur` 0.7.0 or later, the tool runs in the capsule's own sandbox, with the capsule-wide grants ([Native tools](https://docs.murmur.nexus/reference/containment/#native-tools)). A `capabilities:` block on the tool's entry in the capsule's `artifacts:` does not narrow them and prints `W-SEC-008`.

| Need | Capsule grant |
|---|---|
| Read the captured output, and write `<input-stem>.failures.json` beside it | `input_path` (and `repo_path`, when given) inside the session workdir |

`parse` was run confined under both `scoped` and `sealed` with no grant beyond the workdir. See [What each native tool needs](https://docs.murmur.nexus/reference/default-artifacts/#native-tool-needs).

## Build & test

```bash
cargo build -p tool-test-report --release
cargo test -p tool-test-report
../../scripts/package-artifact.sh . ../../release   # pack a platform-tagged .mur.zip with mur build
```
