# tool-code-graph

A Murmur tool, packaged as a `wasm32-wasip2` component exporting `murmur:tool/run@0.2.0`, that
indexes a Rust and/or Python repository into a SQLite-backed symbol/edge graph and answers
structured queries over it. Symbols are addressed by a stable identity that survives unrelated
edits elsewhere in the repository, not by `file:line`:

| Language | Identity |
|---|---|
| Rust | `rust://<package>/<module>#<qualified-name>(<signature>)` |
| Python | `python://<package>/<module>#<qualified-name>(<params>)` |

SQLite and tree-sitter, with the Rust and Python grammars, are compiled into the component. One
`.mur.zip` serves every platform. See [murmur.yaml](./murmur.yaml) for the full manifest.

## Contract

| Operation | Inputs (required in bold) | `data` |
|---|---|---|
| `index_repository` | `repo_path` | `repo_path`, `changed_files`, `unchanged_files`, `removed_files`, `total_symbols`, `total_edges` |
| `find_symbol` | `repo_path`, **`query`**, `limit` (20, clamped to 1–500) | `query`, `count`, `matches` |
| `get_symbol` | `repo_path`, **`symbol_id`** | the symbol's columns, `callees`, `callers` |
| `slice_symbol` | `repo_path`, **`symbol_id`**, `max_depth` (3, 0–64), `max_nodes` (50, 1–10,000), `direction` (`callees` or `callers`) | `root`, `direction`, `node_count`, `edge_count`, `truncated`, `nodes`, `data_path` |
| `explain_path` | `repo_path`, **`from`**, **`to`**, `max_depth` (6, 1–64) | `from`, `to`, `found`, `depth`, `path` |
| `impact_analysis` | `repo_path`, **`file`**, **`start_line`**, **`end_line`**, `max_depth` (3, 0–64), `max_nodes` (50, 1–10,000) | `roots`, `callers`, `tests`, `routes`, `persistence_operations`, `public_apis`, `truncated`, `data_path` |

`operation` is always required. The model receives three things from every call:

| Field | Value |
|---|---|
| `status` | `passed`; `failed` for a call that could not be carried out (a missing input, an unknown symbol, a `repo_path` that does not open); `error` when the database fails |
| `summary` | one line, such as `2 changed, 0 unchanged, 0 removed (9 symbols, 7 edges)`, or the reason |
| `data` | the operation's JSON (above); nothing for a call that could not be carried out |

`find_symbol` with no match, `explain_path` with no path within `max_depth`, and an
`impact_analysis` range that overlaps no symbol are `passed`. An `impact_analysis` `file` that was
never indexed is `failed`.

`slice_symbol` and `impact_analysis` also write their full payload to
`<repo_path>/.murmur/slices/slice-<hash>.json` or `<repo_path>/.murmur/impacts/impact-<hash>.json`,
and name it in `data.data_path`. When the write fails, as in a host folder opened read-only,
`data_path` is `null`.

### Where `repo_path` opens

| Value | Opens |
|---|---|
| omitted, or `.` | the work folder itself |
| relative, such as `repo` | in the work folder |
| absolute, inside a host folder the session opened (`mur run --unconfined`, or a `host_paths` entry of the operator's config) | that host folder |
| absolute, with no host folder opened, or outside every one | nothing: the call fails, naming the value and listing the host folders it may open |
| relative, starting with `murmur-host` while a host folder is open | nothing: the call fails, suggesting the absolute path, because host folders are opened under that name |

`index_repository` writes `<repo_path>/.murmur/code-graph.db`, so it needs the folder read-write.
In a host folder opened read-only it fails before touching the database. The other five
operations read an existing database there without a lock or journal (SQLite's `immutable=1`);
a missing database, or one that needs a migration, is an `error`.

The tool names the repository the way the call wrote it: `data.repo_path` (a trailing `/`
trimmed, `.` when omitted), `data_path` (`.murmur/…` for `.`, an absolute host path for an
absolute value) and messages. Symlinks that lead out of the opened folder are not followed.

### The package of a file

A Rust file's package is the `name` of the nearest `Cargo.toml` with a `[package]` above it,
inside the repository. A Python file's is the `name` of the nearest `pyproject.toml` (`[project]`
or `[tool.poetry]`), `setup.py` or `setup.cfg`. With none, it is the last segment of `repo_path`
as written (`repo` for `repo/`, `y` for `/x/y`). When the value has none (omitted, `.`, `./`,
`..`, `/`), it is `crate` for Rust and `package` for Python.

### `impact_analysis`'s `file`

| `file` | Is matched as |
|---|---|
| relative, such as `src/lib.rs`, `./src/lib.rs` or `.\src\lib.rs` | `src/lib.rs`: `\` becomes `/` and a leading `./` is dropped |
| absolute, with an absolute `repo_path` | the path below `repo_path`, compared whole segment by whole segment |
| absolute, with a relative `repo_path` | the longest indexed path that `file` ends with, whole segment by whole segment: `/w/crates/a/src/lib.rs` matches `crates/a/src/lib.rs` before `src/lib.rs`, and `/w/xsrc/lib.rs` matches neither |

A path that matches no indexed file fails as `file not indexed`.

## What the capsule must grant

| Need | Grant |
|---|---|
| Index and query a `repo_path` given relative | the work folder, which every session has |
| Query a `repo_path` given absolute | a `host_paths` entry covering it (`access: read` is enough to query a database indexed beforehand), or `mur run --unconfined` |
| Index a `repo_path` given absolute | a `host_paths` entry covering it with `access: read-write`, or `mur run --unconfined` |

A `capabilities:` block on the tool's entry in the capsule's `artifacts:` narrows the tool. The
artifact's own `murmur.yaml` carries one you can copy there, which narrows it to the work folder:

```yaml
capabilities:
  filesystem:
    allow:
      - "."
```

## Locking

The database keeps SQLite's rollback journal (`code-graph.db-journal`, deleted when a write
commits). SQLite's WASI build locks a database by creating the directory `code-graph.db.lock`
beside it, for reads as well as writes, and removes it when the call is done.

- A call cut off mid-write can leave `code-graph.db.lock` behind. Every later call then fails
  with `database is locked`, naming the directory. Remove it once no other call is using the
  graph.
- The lock directory does not exclude a host process using the same database at the same
  time, such as `sqlite3`. `tool-git` and
  `tool-test-report` read the database with `immutable=1`, taking no lock at all, so a read
  while `index_repository` writes can see a half-written graph.
- A database switched to WAL mode cannot be opened: the WASI build has no WAL support.

## Breaking in 0.7.0

0.6.0 was a native binary; 0.7.0 is a WASM component. What changed for a capsule:

- One zip serves every platform; there are no per-platform zips.
- It needs murmur v0.9.0 or later: it exports `murmur:tool/run@0.2.0` and finds host folders
  through the `murmur-host` preopens, which both arrived in v0.9.0. On an older `mur`, keep 0.6.0.
- `repo_path` is relative to the work folder and defaults to `.`, the work folder itself. An
  absolute value opens only inside a host folder the session opened, and `index_repository`
  needs that folder read-write.
- A `capabilities:` block on the tool's entry now narrows it.
- `data.repo_path`, `data_path` and messages name the repository as the call named it, not by
  its canonical absolute path.
- For a repository indexed as `.` (or with `repo_path` omitted), a file under no Rust package or
  Python project name now gets the package `crate` (Rust) or `package` (Python), where 0.6.0 used
  the folder's name. Those symbol ids change: delete `.murmur/code-graph.db` and re-index.
- An absolute `impact_analysis` `file` under a relative `repo_path` is matched by its longest
  indexed suffix.
- Symlinks that lead out of the opened folder are not followed.
- A database in WAL mode cannot be opened.
- Locking uses a `code-graph.db.lock` directory: a native process writing at the same time is not
  excluded, and a call cut off mid-write can leave the directory behind.

## Build & test

SQLite and tree-sitter are compiled into the component by the pinned wasi-sdk (`wasi-sdk.toml`).
Install it once, then build:

```bash
./scripts/wasi-sdk.sh                                              # from the repo root, once
cargo build -p tool-code-graph --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/tool_code_graph.wasm
cargo test -p tool-code-graph                                      # behaviour, 0.6.0 parity, SQLite and host paths, against the component
MUR_BIN=$(command -v mur) cargo test -p tool-code-graph --test mur_run_code_graph -- --ignored
./scripts/package-artifact.sh tools/tool-code-graph release/       # → release/tool-code-graph-0.7.0.mur.zip
```

`tests/fixtures/README.md` names the tool-code-graph 0.6.0 build that recorded the frozen results the
component is held to.
