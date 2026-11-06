# Fixtures

| File | What it is |
|---|---|
| `native-0.5.0-cases.json` | Hand-written inputs: per case, the code-graph symbols (or `null` for no database), the folder whose `.murmur/code-graph.db` is built (`db_at`), extra files, the LCOV files (`@SCRATCH@` stands for the case's scratch folder) and the calls, in order |
| `native-0.5.0-results.json` | What tool-code-coverage 0.5.0 returned for every call of every case: what the model saw, the capped-list file when one was written, and the database's suspicion columns after the call. `tests/wasm_component.rs` replays every case through the component and requires the same |

## The cases

They mirror the 0.5.0 suite's scenarios 1, 3, 5 (two calls, `cov1` then `cov2`), 6, 7, 8, 9, 10
(a missing relative `missing-cov`, then a file), 11 (each malformed input as its own call), 12, 13,
14 (`limit: 2`) and 15, then two more:

- `repo_omitted_absolute_sf`: no `repo_path`, the database in the work folder itself, absolute
  `SF:` paths; a second call with `limit: 1` writes the capped list;
- `relative_sf`: no `repo_path`, the database in the work folder itself, `SF:src/lib.rs` and
  `SF:./src/lib.rs`.

Scenario 16 (a repository named through a symlink) is not recorded, because a symlink spelling
cannot be recorded portably; `tests/code_coverage_integration.rs` covers it.

Both files are frozen: never edit them by hand. The results were recorded from tool-code-coverage
0.5.0, built from commit `95e17e1`.
