# tool-test-report

A Murmur tool, packaged as a `wasm32-wasip2` component exporting `murmur:tool/run@0.2.0`, that
parses a raw test-runner output file — one the agent has **already captured** via its own shell
call — into a structured list of failures. It never spawns a test process; it only reads a file.
One `.mur.zip` serves every platform.

## Contract

The call's arguments:

```json
{ "operation": "parse", "input_path": "out/tests.txt", "format": "auto", "repo_path": "." }
```

| Input | Required | What it is |
|---|---|---|
| `operation` | yes | `"parse"`, the only operation |
| `input_path` | yes | the raw test-runner output file |
| `format` | no, default `"auto"` | `auto` \| `cargo_test` \| `pytest` \| `go_test` \| `jest`. `auto` detects the runner from signature markers; a failed detection returns a `failed` result asking for an explicit format |
| `repo_path` | no | a Rust repository containing `.murmur/code-graph.db`. `cargo_test` failures then get a best-effort `stable_id` (code-graph's symbol identity), read from that database. Ignored for every other format, which always reports `stable_id: null` (code-graph's MVP language scope is Rust only) |

`input_path` and `repo_path` follow one path rule:

| Value | Opens |
|---|---|
| relative, such as `out/tests.txt` | in the work folder; `repo_path: "."` is the work folder itself |
| absolute, inside a host folder the session opened (`mur run --unconfined`, or a `host_paths` entry of the operator's config) | that host file or folder |
| absolute, with no host folder opened, or outside every one | nothing: the call fails, naming the value and listing the host folders it may open |
| relative, starting with `murmur-host` while a host folder is open | nothing: the call fails, suggesting the absolute path, because host folders are opened under that name |

The model receives three things from every call:

| Field | Value |
|---|---|
| `status` | `failed` when any test failed or the call could not be carried out, `passed` when every test passed |
| `summary` | `"<passed> passed, <failed> failed"`, or the reason the call could not be carried out |
| `data` | for a parse, the JSON below; nothing for a call that could not be carried out |

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

Above 50 failures the inline `failures` array is capped, `truncated` is `true`, and the full array
is written to `<input-stem>.failures.json` beside the input, named in `data_path` the way
`input_path` was written (an absolute input gives an absolute host path). When that write fails,
as it does in a `read` host folder, `data_path` is `null`.

`stable_id` is best-effort: a missing database, an unreadable one, a database in WAL mode, or a
test with no unique match leaves it `null`. The database is opened read-only with SQLite's
`immutable=1`, so the lookup works inside a `read` host folder and never creates a lock or journal
file in the project.

## What the capsule must grant

| Need | Grant |
|---|---|
| Read `input_path` and `repo_path` given relative, and write `<input-stem>.failures.json` | the work folder, which every session has |
| Read them given absolute | a `host_paths` entry covering them (`access: read` is enough to parse; `read-write` also lets the full list be written beside the input), or `mur run --unconfined` |

A `capabilities:` block on the tool's entry in the capsule's `artifacts:` narrows the tool. The
artifact's own `murmur.yaml` carries one you can copy there, which narrows it to the work folder:

```yaml
capabilities:
  filesystem:
    allow:
      - "."
```

## Breaking in 0.6.0

0.5.0 was a native binary; 0.6.0 is a WASM component. What changed for a capsule:

- It needs murmur v0.9.0 or later: it exports `murmur:tool/run@0.2.0` and finds host folders
  through the `murmur-host` preopens, which both arrived in v0.9.0. On an older `mur`, keep 0.5.0.
- One zip serves every platform; there are no per-platform zips.
- An absolute `input_path` or `repo_path` opens only inside a host folder the session opened.
  Relative paths open in the work folder, and `repo_path: "."` names the work folder.
- An absolute `repo_path` that no host folder covers fails a `cargo_test` call, where 0.5.0 left
  `stable_id` null.
- A `capabilities:` block on the tool's entry now narrows it.
- A code-graph database in WAL mode cannot be read, and `stable_id` stays `null`.

## Build & test

SQLite is compiled into the component from rusqlite's bundled C sources by the pinned wasi-sdk
(`wasi-sdk.toml`). Install it once, then build:

```bash
./scripts/wasi-sdk.sh                                              # from the repo root, once
cargo build -p tool-test-report --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/tool_test_report.wasm
cargo test -p tool-test-report                                     # behaviour, 0.5.0 parity, stable_id and host paths, against the component
MUR_BIN=$(command -v mur) cargo test -p tool-test-report --test mur_run_stable_id -- --ignored
./scripts/package-artifact.sh tools/tool-test-report release/      # → release/tool-test-report-0.7.0.mur.zip
```

`tests/fixtures/README.md` names the tool-test-report 0.5.0 build that recorded the frozen results the
component is held to.
