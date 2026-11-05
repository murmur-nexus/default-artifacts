# Fixtures

| File | What it is |
|---|---|
| `cargo_test.txt`, `go_test.txt`, `jest.txt`, `pytest.txt` | Raw console output of each runner, with passing and failing tests |
| `not-a-test-run.txt` | Plain prose that no format detects |
| `native-0.5.0-results.json` | What the model saw from the native 0.5.0 tool for a set of calls, recorded with the recipe below. `tests/wasm_component.rs` (`native_0_5_0_results`) runs every case through the component and requires the same `status`, `summary` and `data` |

## Recording `native-0.5.0-results.json`

Never edit the JSON by hand; re-record it. The recipe builds the last native release of the tool
(commit `95e17e1`, `tool-test-report` 0.5.0) in a scratch tree, runs each case with the working
directory set to a scratch copy of this folder, so every path is relative, and maps each output the
way murmur's native dispatch (`dispatch_native_tool` in `crates/capsule-runtime/src/runtime.rs`)
maps it for the model:

- `status` is `"status"` when it is `passed` or `failed`, and `error` otherwise;
- `summary` is `"summary"`;
- `data` is `"data"` when it is not null, and null otherwise.

Nothing else of the native output reached the model.

Run it from the repository root, with the scratch paths anywhere you like:

```bash
SCRATCH=/space/scratch/2448-native            # any empty folder outside the repo
mkdir -p "$SCRATCH/src" "$SCRATCH/cwd"
git archive 95e17e1 | tar -x -C "$SCRATCH/src"
( cd "$SCRATCH/src" && CARGO_TARGET_DIR="$SCRATCH/target" cargo build --release -p tool-test-report )
cp tools/tool-test-report/tests/fixtures/*.txt "$SCRATCH/cwd/"

python3 -I - "$SCRATCH/target/release/tool-test-report" "$SCRATCH/cwd" \
    tools/tool-test-report/tests/fixtures/native-0.5.0-results.json <<'PY'
import json, subprocess, sys

binary, cwd, out = sys.argv[1:4]
cases = []
for fmt, fixture in [("cargo_test", "cargo_test.txt"), ("pytest", "pytest.txt"),
                     ("go_test", "go_test.txt"), ("jest", "jest.txt")]:
    cases.append((f"{fmt}-explicit", {"operation": "parse", "input_path": fixture, "format": fmt}))
    cases.append((f"{fmt}-auto", {"operation": "parse", "input_path": fixture, "format": "auto"}))
cases += [
    ("prose-auto", {"operation": "parse", "input_path": "not-a-test-run.txt", "format": "auto"}),
    ("format-junit", {"operation": "parse", "input_path": "cargo_test.txt", "format": "junit"}),
    ("no-input-path", {"operation": "parse"}),
    ("operation-explode", {"operation": "explode", "input_path": "cargo_test.txt"}),
    ("missing-file", {"operation": "parse", "input_path": "missing.txt"}),
]

results = []
for name, args in cases:
    # murmur hands a native tool the call arguments as a JSON string in `data`.
    envelope = json.dumps({"data": json.dumps(args), "log_path": None})
    proc = subprocess.run([binary], input=envelope, capture_output=True, text=True, cwd=cwd, check=True)
    output = json.loads(proc.stdout)
    status = output.get("status")
    results.append({
        "name": name,
        "args": args,
        "status": status if status in ("passed", "failed") else "error",
        "summary": output.get("summary"),
        "data": output.get("data"),
    })

with open(out, "w") as f:
    json.dump(results, f, indent=2, ensure_ascii=False)
    f.write("\n")
print(f"recorded {len(results)} cases into {out}")
PY
```
