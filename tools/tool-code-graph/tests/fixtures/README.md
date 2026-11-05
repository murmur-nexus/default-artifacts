# Fixtures

| File | What it is |
|---|---|
| `repos.json` | The four fixture repositories, as `{"<name>": {"<relative path>": "<content>", …}, …}`: `rust` (package `fixture_crate`), `impact` (package `impact_fixture`, every confidence case and classification bucket), `python` (project `pyfixture`, with `pyproject.toml`) and `mixed` (a Rust package beside Python files under no project, whose package falls back to the folder name). Kept as JSON so no `Cargo.toml` lands under `tests/`. `tests/common` writes them (`write_repo`) |
| `native-0.6.0-cases.json` | Named cases: `{"cases": [{"name", "fixture", "steps": [...]}]}`. A step is `{"call": {<arguments>}}`, `{"write": {"path", "content"}}` (a file under the repository, written on the host) or `{"remove": "<path>"}`. `@REPO@` in an argument stands for the repository (below) |
| `native-0.6.0-results.json` | What the native 0.6.0 tool returned for every call of every case, recorded with the recipe below: `{"cases": [{"name", "calls": [...], "snapshots": [...]}]}`. A call holds its `args`, the `status`, `summary` and `data` the model saw, `payload` (the parsed `data_path` file, or null) and `db` (an index into the case's `snapshots`, or null when there was no database). A snapshot is the database's rows: `files` by path, `symbols` (every column but `id`/`file_id`, plus the file's path) by `symbol_id`, `edges` (every column but `id`) by `src_symbol_id \| dst_name \| edge_kind`, and `symbols_fts` by `symbol_id`. The repository's absolute path is written `@REPO@` |

`tests/wasm_component.rs` (`native_0_6_0_results`) replays every case through the component twice,
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

## Recording `native-0.6.0-results.json`

Never edit the results by hand; re-record them after changing the cases. The recipe builds the last
native release of the tool (commit `72c8f32`, `tool-code-graph` 0.6.0) in a scratch tree. For each
case it lays the fixture out at `<scratch>/<case>/fixture_repo`, runs the steps with the working
directory at `<scratch>/<case>`, calls with the repository's canonical absolute path, sending
`{"data": json.dumps(args), "log_path": null}` on stdin as murmur did, and maps each output the way
murmur's native dispatch (`dispatch_native_tool` in `crates/capsule-runtime/src/runtime.rs`) maps it
for the model:

- `status` is `"status"` when it is `passed` or `failed`, and `error` otherwise;
- `summary` is `"summary"` when it is a string;
- `data` is `"data"` when it is a string, and its compact JSON when it is any other non-null value.

Nothing else of the native output reached the model. After every call it stores the `data_path`
file and the database snapshot, and finally replaces the repository's path with `@REPO@`.

Run it from the repository root, with the scratch paths anywhere you like (`record.py` is the
source below, saved outside the repository):

```bash
SCRATCH=/space/scratch/a99f-rec              # any empty folder outside the repo
mkdir -p "$SCRATCH/src"
git archive 72c8f32 | tar -x -C "$SCRATCH/src"
( cd "$SCRATCH/src" && CARGO_TARGET_DIR="$SCRATCH/target" cargo build --release -p tool-code-graph )
CASES=$(mktemp -d "$SCRATCH/cases.XXXXXX")
python3 -I record.py "$SCRATCH/target/release/tool-code-graph" tools/tool-code-graph/tests/fixtures "$CASES" \
  > tools/tool-code-graph/tests/fixtures/native-0.6.0-results.json
```

`record.py`:

```python
# Record native tool-code-graph 0.6.0 results for tests/fixtures/native-0.6.0-cases.json.
# Usage: python3 -I record.py <native binary> <fixtures dir> <scratch dir>
import json, os, sqlite3, subprocess, sys
binary, fixtures, scratch = sys.argv[1:4]
repos = json.load(open(os.path.join(fixtures, "repos.json")))
cases = json.load(open(os.path.join(fixtures, "native-0.6.0-cases.json")))["cases"]

def snapshot(db):
    if not os.path.exists(db):
        return None
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    def rows(sql, keys):
        cur = conn.execute(sql)
        names = [d[0] for d in cur.description]
        out = {}
        for r in cur:
            key = " | ".join(str(v) for v in r[:keys])
            assert key not in out, key
            out[key] = dict(zip(names, r))
        return out
    snap = {
        "files": rows("SELECT path, content_hash, language FROM files", 1),
        "symbols": rows("""SELECT s.symbol_id, s.language, s.package, s.module, s.qualified_name,
            s.simple_name, s.signature, s.kind, f.path AS file, s.start_line, s.end_line,
            s.doc_comment, s.visibility, s.attributes
            FROM symbols s JOIN files f ON f.id = s.file_id""", 1),
        "edges": rows("""SELECT src_symbol_id, dst_name, edge_kind, dst_symbol_id, call_style,
            confidence FROM edges""", 3),
        "symbols_fts": rows("SELECT symbol_id, qualified_name, doc_comment FROM symbols_fts", 1),
    }
    conn.close()
    return snap

results = []
for case in cases:
    cwd = os.path.join(scratch, case["name"])
    repo = os.path.join(cwd, "fixture_repo")
    os.makedirs(repo)
    for rel, content in repos[case["fixture"]].items():
        os.makedirs(os.path.dirname(os.path.join(repo, rel)), exist_ok=True)
        open(os.path.join(repo, rel), "w").write(content)
    repo = os.path.realpath(repo)
    calls, snapshots = [], []
    for step in case["steps"]:
        if "write" in step:
            open(os.path.join(repo, step["write"]["path"]), "w").write(step["write"]["content"])
            continue
        if "remove" in step:
            os.remove(os.path.join(repo, step["remove"]))
            continue
        args = json.loads(json.dumps(step["call"]).replace("@REPO@", repo))
        proc = subprocess.run([binary], cwd=cwd, capture_output=True, text=True,
                              input=json.dumps({"data": json.dumps(args), "log_path": None}))
        out = json.loads(proc.stdout)
        # murmur's dispatch_native_tool: status passed|failed|else error; summary when a
        # string; data passed through when a string, else compact JSON of non-null.
        status = out.get("status")
        status = status if status in ("passed", "failed") else "error"
        summary = out.get("summary") if isinstance(out.get("summary"), str) else None
        data = out.get("data")
        if data is not None and not isinstance(data, str):
            data = json.dumps(data, separators=(",", ":"), ensure_ascii=False)
        payload = None
        if data is not None:
            parsed = json.loads(data)
            dp = parsed.get("data_path") if isinstance(parsed, dict) else None
            if isinstance(dp, str):
                payload = json.load(open(dp))
        snap = snapshot(os.path.join(repo, ".murmur", "code-graph.db"))
        if snap is None:
            db = None
        else:
            if snap not in snapshots:
                snapshots.append(snap)
            db = snapshots.index(snap)
        sub = lambda s: None if s is None else s.replace(repo, "@REPO@")
        calls.append({
            "args": step["call"],
            "status": status,
            "summary": sub(summary),
            "data": None if data is None else json.loads(sub(data)),
            "payload": None if payload is None else json.loads(sub(json.dumps(payload))),
            "db": db,
        })
    results.append({"name": case["name"], "calls": calls, "snapshots": snapshots})

json.dump({"recorded_with": "tool-code-graph 0.6.0 (native)", "cases": results},
          sys.stdout, indent=1, ensure_ascii=False, sort_keys=True)
sys.stdout.write("\n")
```
