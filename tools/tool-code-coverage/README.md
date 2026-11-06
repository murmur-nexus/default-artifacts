# tool-code-coverage

A Murmur tool, packaged as a `wasm32-wasip2` component exporting `murmur:tool/run@0.2.0`, that
performs spectrum-based fault localization (Ochiai / Tarantula) over a Rust repository already
indexed by `murmur/tool-code-graph`. Input is a directory of per-test LCOV `.info` reports the
agent produced via its own `cargo llvm-cov --lcov` shell calls — this tool never runs tests or
coverage itself. Suspicion scores are written onto the code-graph database's `symbols` table. One
`.mur.zip` serves every platform.

See [murmur.yaml](./murmur.yaml) for the full manifest.

## Contract

The call's arguments:

```json
{ "operation": "localize", "repo_path": ".", "coverage_dir": "coverage",
  "failing_tests": ["tests::test_add"], "limit": 50 }
```

| Input | Required | What it is |
|---|---|---|
| `operation` | yes | `"localize"`, the only operation |
| `repo_path` | no, default `"."` | the repository root holding `.murmur/code-graph.db`, which `tool-code-graph index_repository` must already have built; this tool never creates it |
| `coverage_dir` | yes | a directory of per-test `<test name>.info` LCOV files; a file whose stem is not in `failing_tests` is a passing test |
| `failing_tests` | yes | the names of the failing tests, non-empty |
| `limit` | no, default 50 | how many suspects `top_suspects` holds inline, clamped to [1, 10000] |

`repo_path` and `coverage_dir` follow one path rule:

| Value | Opens |
|---|---|
| relative, such as `coverage` | in the work folder; `repo_path: "."` is the work folder itself |
| absolute, inside a host folder the session opened (`mur run --unconfined`, or a `host_paths` entry of the operator's config) | that host folder; for `repo_path` the host folder must be `read-write`, because `localize` writes its scores there |
| absolute, with no host folder opened, or outside every one | nothing: the call fails, naming the value and listing the host folders it may open |
| relative, starting with `murmur-host` while a host folder is open | nothing: the call fails, suggesting the absolute path, because host folders are opened under that name |

The model receives three things from every call:

| Field | Value |
|---|---|
| `status` | `passed` when the scores were written; `failed` for a call that could not be carried out (bad input, a missing database, no failing-test coverage); `error` when the database could not be opened, read or written |
| `summary` | `"localize: <n> symbol(s) scored over <F> failing / <P> passing test(s)"`, plus `", top <limit> shown (full list at data_path)"` when capped, or the reason the call could not be carried out |
| `data` | for a localize, the JSON below; nothing otherwise |

```json
{
  "repo_path": ".", "coverage_dir": "coverage",
  "failing_test_count": 1, "passing_test_count": 1, "scored_symbols": 4, "truncated": false,
  "top_suspects": [
    { "symbol_id": "rust://fixture/#add(->i64)", "qualified_name": "add", "kind": "function",
      "file": "src/lib.rs", "start_line": 5, "ef": 1, "ep": 0,
      "suspicion_ochiai": 1.0, "suspicion_tarantula": 1.0 }
  ],
  "failing_test_stable_ids": [{ "test_name": "tests::test_add", "stable_id": "rust://fixture/tests#test_add(->())" }],
  "failing_tests_without_coverage": [], "skipped_files": [], "data_path": null
}
```

`repo_path` and `data_path` name the repository as the call named it. When more symbols are scored
than `limit`, the full sorted list is written to `<repo_path>/.murmur/coverage/localize-<sha256 of
repo_path as named>.json`, and `data_path` names it under `repo_path` (an absolute `repo_path` gives
an absolute host path). A write failure leaves `data_path` `null`.

Every call first resets the four suspicion columns repo-wide, then writes the new scores. A symbol
no coverage touched stays `NULL` in all four, distinct from a computed `0`.

### How `SF:` paths are matched

`cargo llvm-cov` names each source file by its absolute host path (`SF:/home/me/repo/src/lib.rs`),
but a WASM tool never learns the host path of its work folder. The tool therefore never opens an
`SF:` path. It works out the repository's host prefix from the paths themselves:

1. Every absolute `SF:` path of the call (failing and passing reports alike) is normalised
   lexically: `\` becomes `/`, `.` segments are dropped and `..` pops.
2. Each path proposes every prefix whose removal leaves a path `tool-code-graph` indexed.
3. The prefix proposed by the most distinct paths wins; on a tie the one with the fewest segments,
   then the alphabetically first.
4. An absolute `SF:` path under the winning prefix maps to the rest of its path; one outside it
   matches nothing. A relative `SF:` path is relative to `repo_path` and is used as written.

The tie rule decides only when two prefixes explain equally many paths, such as coverage from two
checkouts of the same repository in one call. Give each localize the reports of one checkout.

## What the capsule must grant

| Need | Grant |
|---|---|
| Read `coverage_dir` and read and write `<repo_path>/.murmur/` given relative | the work folder, which every session has |
| Given absolute | a `host_paths` entry covering them, `access: read-write` for `repo_path` (`read` is enough for `coverage_dir`), or `mur run --unconfined` |

A `capabilities:` block on the tool's entry in the capsule's `artifacts:` narrows the tool. The
artifact's own `murmur.yaml` carries one you can copy there, which narrows it to the work folder:

```yaml
capabilities:
  filesystem:
    allow:
      - "."
```

The database must use SQLite's default rollback journal, which is `tool-code-graph`'s default. The
WASI build of SQLite has no WAL support, and locks the database by creating a
`code-graph.db.lock` directory beside it for the length of a write.

## Breaking in 0.6.0

0.5.0 was a native binary; 0.6.0 is a WASM component. What changed for a capsule:

- It needs murmur v0.9.0 or later: it exports `murmur:tool/run@0.2.0` and finds host folders
  through the `murmur-host` preopens, which both arrived in v0.9.0. On an older `mur`, keep 0.5.0.
- One zip serves every platform; there are no per-platform zips.
- `repo_path` and `coverage_dir` are relative to the work folder, and `repo_path` defaults to `.`.
  An absolute value opens only inside a host folder the session opened, and an absolute value no
  host folder covers fails the call.
- A `repo_path` inside a host folder opened read-only fails the call, because `localize` writes.
- A `capabilities:` block on the tool's entry now narrows it.
- `data.repo_path` and `data.data_path` name the repository as the call named it, and the capped
  list's file name changes: it is the hash of `repo_path` as named, not of its canonical host path.
- A code-graph database in WAL mode cannot be opened, and the call fails with `status: error`.
- The database is locked with a `code-graph.db.lock` directory. A native process writing it at
  the same time is not excluded, and a call cut off mid-write can leave the lock behind. The next
  call then fails with `status: error` naming the lock; delete it if no other call is using the
  database.

## Build & test

SQLite is compiled into the component from rusqlite's bundled C sources by the pinned wasi-sdk
(`wasi-sdk.toml`). Install it once, then build:

```bash
./scripts/wasi-sdk.sh                                                # from the repo root, once
cargo build -p tool-code-coverage --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/tool_code_coverage.wasm
cargo test -p tool-code-coverage                                     # behaviour, 0.5.0 parity, SQLite writes and host paths, against the component
MUR_BIN=$(command -v mur) cargo test -p tool-code-coverage --test mur_run_localize -- --ignored
./scripts/package-artifact.sh tools/tool-code-coverage release/      # → release/tool-code-coverage-0.6.0.mur.zip
```

`tests/fixtures/README.md` names the tool-code-coverage 0.5.0 build that recorded the frozen results the
component is held to.
