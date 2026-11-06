# Fixtures

| File | What it is |
|---|---|
| `cargo_test.txt`, `go_test.txt`, `jest.txt`, `pytest.txt` | Raw console output of each runner, with passing and failing tests |
| `not-a-test-run.txt` | Plain prose that no format detects |
| `native-0.5.0-results.json` | What the model saw from tool-test-report 0.5.0 for a set of calls: `status`, `summary`, and `data` when it was not null. `tests/wasm_component.rs` runs every case through the component and requires the same `status`, `summary` and `data` |

The results file is frozen: never edit it by hand. Recorded from tool-test-report 0.5.0, built from
commit `95e17e1`.
