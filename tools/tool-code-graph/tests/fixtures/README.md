# Fixtures

| File | What it is |
|---|---|
| `repos.json` | The four fixture repositories, as `{"<name>": {"<relative path>": "<content>", …}, …}`: `rust` (package `fixture_crate`), `impact` (package `impact_fixture`, every confidence case and classification bucket), `python` (project `pyfixture`, with `pyproject.toml`) and `mixed` (a Rust package beside Python files under no project, whose package falls back to the folder name). Kept as JSON so no `Cargo.toml` lands under `tests/`. `tests/common` writes them (`write_repo`) |
| `native-0.6.0-cases.json` | Named cases: `{"cases": [{"name", "fixture", "steps": [...]}]}`. A step is `{"call": {<arguments>}}`, `{"write": {"path", "content"}}` (a file under the repository, written on the host) or `{"remove": "<path>"}`. `@REPO@` in an argument stands for the repository (below) |
| `native-0.6.0-results.json` | What tool-code-graph 0.6.0 returned for every call of every case: `{"cases": [{"name", "calls": [...], "snapshots": [...]}]}`. A call holds its `args`, the `status`, `summary` and `data` the model saw, `payload` (the parsed `data_path` file, or null) and `db` (an index into the case's `snapshots`, or null when there was no database). A snapshot is the database's rows: `files` by path, `symbols` (every column but `id`/`file_id`, plus the file's path) by `symbol_id`, `edges` (every column but `id`) by `src_symbol_id \| dst_name \| edge_kind`, and `symbols_fts` by `symbol_id`. The repository's absolute path is written `@REPO@` |

`tests/wasm_component.rs` replays every case through the component twice,
each in a fresh scratch folder holding `<case>/fixture_repo`:

| Mode | Work folder | `repo_path` | `@REPO@` in results |
|---|---|---|---|
| relative | `<case>`, at `.` | `fixture_repo` | `fixture_repo` |
| host root | an empty folder (which must stay empty); `<case>` is a read-write `murmur-host<path>` folder | the repository's absolute path | that path |

`@REPO@` in any other argument (an absolute `impact_analysis` `file`) is the repository's absolute
path in both modes. It requires the same `status`, `summary` (with ` (os error N)` removed), `data`
as JSON, `data_path` file, and database rows after every call. `repo_path` is never omitted in a
case: 0.7.0 defaults it to `.` where 0.6.0 used its working directory, and `tests/wasm_component.rs`
(`s8_…`) covers the default.

The cases and results are frozen: never edit them by hand. The results were recorded from
tool-code-graph 0.6.0, built from commit `72c8f32`.
