#!/usr/bin/env bash
# Does Claude Code show its model an image that comes back from a murmur tool?
#
# Runs one real capsule on `transport: process` with driver-claude-code and the workspace's
# `tool-read`, which attaches `picture.png` from the work folder: a PNG showing a random
# three-digit number on a coloured background. The task asks the model for the number and the
# colour, and to answer NO-PICTURE rather than guess. The model can answer only by seeing the
# image: the file name, the tool's text (type, size and dimensions), the task and the work folder
# give nothing away, and the expected values are written only outside the capsule.
#
# The turn spends the Claude subscription `claude` is logged in to. Prerequisites:
#   - a `mur` built from murmur 4f65bb98 or later, whose tool bridge sends MCP image items;
#   - `claude` on PATH, logged in to a subscription;
#   - python3, and a murmur git checkout to check the `mur` commit against.
#
# Usage (from the default-artifacts repo root):
#   MUR_BIN=/path/to/mur drivers/driver-claude-code/tests/live/image-check.sh
#
# Env:
#   MUR_BIN     the `mur` to run (default: `mur` on PATH)
#   MURMUR_DIR  a murmur git checkout (default: /space/_murmur/murmur)
#   MODEL       the model `claude` is launched with (default: haiku)
#
# Prints the expected values, the model's answer, the session's auth, the trace's `tool_call`
# record, then one verdict: PASS, NOT SHOWN or WRONG. Exits 0 on PASS and 1 otherwise.
set -euo pipefail

NEEDED=4f65bb98b32a20e6af3764df03afc259b072fd7d
REPO_ROOT="$(pwd)"
LIVE_DIR="$REPO_ROOT/drivers/driver-claude-code/tests/live"
MURMUR_DIR="${MURMUR_DIR:-/space/_murmur/murmur}"
MODEL="${MODEL:-haiku}"

if [ ! -f "$LIVE_DIR/picture.py" ] || [ ! -x "$REPO_ROOT/scripts/local-install.sh" ]; then
  echo "image-check: run this from the default-artifacts repo root" >&2
  exit 1
fi

# ── 1. a mur whose bridge sends image items ─────────────────────────────────────
MUR_BIN="$(command -v "${MUR_BIN:-mur}" || true)"
if [ -z "$MUR_BIN" ]; then
  echo "image-check: no mur found (set MUR_BIN); need a mur containing murmur $NEEDED" >&2
  exit 1
fi
FOUND="$("$MUR_BIN" version 2>/dev/null | awk '$1 == "commit" { print $2; exit }')"
git -C "$MURMUR_DIR" fetch -q 2>/dev/null || true
if [ -z "$FOUND" ] || ! git -C "$MURMUR_DIR" merge-base --is-ancestor "$NEEDED" "$FOUND" 2>/dev/null; then
  echo "image-check: $MUR_BIN is murmur commit ${FOUND:-<unknown>}, which does not contain $NEEDED (the bridge's image items); build a newer mur with: mkdir /tmp/murmur-src && git -C $MURMUR_DIR archive origin/main | tar -x -C /tmp/murmur-src && (cd /tmp/murmur-src && CARGO_PROFILE_DEV_OPT_LEVEL=1 CARGO_PROFILE_DEV_DEBUG=0 cargo build --locked -p murmur-cli), then set MUR_BIN=/tmp/murmur-src/target/debug/mur" >&2
  exit 1
fi
command -v python3 >/dev/null || { echo "image-check: python3 is required" >&2; exit 1; }
command -v claude >/dev/null || { echo "image-check: claude is not on PATH" >&2; exit 1; }
# package-artifact.sh, run by local-install.sh, runs `mur build` from PATH.
export MUR_BIN
PATH="$(dirname "$MUR_BIN"):$PATH"
export PATH

# ── 2. scratch: the capsule, and the expected values outside it ────────────────
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/cc-image-check.XXXXXX")"
CAPSULE="$SCRATCH/capsule"
EXPECTED="$SCRATCH/expected.txt"
mkdir -p "$CAPSULE"
echo "→ scratch: $SCRATCH"

# ── 3. the picture ─────────────────────────────────────────────────────────────
python3 -I "$LIVE_DIR/picture.py" --out "$CAPSULE/picture.png" --expected "$EXPECTED"

# ── 5. the capsule (written before step 4: `mur install` needs its murmur.yaml) ─
DRIVER_VERSION="$(sed -n 's/^version:[[:space:]]*//p' "$REPO_ROOT/drivers/driver-claude-code/murmur.yaml" | head -1)"
TOOL_VERSION="$(sed -n 's/^version:[[:space:]]*//p' "$REPO_ROOT/tools/tool-read/murmur.yaml" | head -1)"
cat > "$CAPSULE/murmur.yaml" <<YAML
name: cc-image-check
version: 0.1.0
artifacts:
  - name: local/driver-claude-code
    version: "$DRIVER_VERSION"
    runtime: driver
  - name: local/tool-read
    version: "$TOOL_VERSION"
    runtime: tool
capabilities:
  env:
    allow: [HOME, PATH]
trace:
  capture: content
inference:
  transport: process
  driver:
    artifact: local/driver-claude-code
  model: $MODEL
YAML

# ── 4. the tool ────────────────────────────────────────────────────────────────
"$REPO_ROOT/scripts/local-install.sh" tool-read "$CAPSULE"

"$REPO_ROOT/scripts/local-install.sh" driver-claude-code "$CAPSULE"

# ── 6. the task ────────────────────────────────────────────────────────────────
cat > "$CAPSULE/task.md" <<'TASK'
Call the tool tool-read exactly once, with path picture.png. It returns a picture. Then answer with one line in exactly this form: NUMBER=<the number drawn in the picture> BACKGROUND=<the background colour, one word>. If the tool gave you no picture you can see, answer exactly NO-PICTURE and do not guess.
TASK

# ── 7. run ─────────────────────────────────────────────────────────────────────
echo "→ $MUR_BIN run --task task.md --workdir $CAPSULE   (model: $MODEL)"
RUN_STATUS=0
( cd "$CAPSULE" && "$MUR_BIN" run --task task.md --workdir "$CAPSULE" ) \
  > "$SCRATCH/run.log" 2>&1 || RUN_STATUS=$?
echo "→ mur run exited $RUN_STATUS; its output is in $SCRATCH/run.log"

# ── 8. report ──────────────────────────────────────────────────────────────────
"$MUR_BIN" trace list --workdir "$CAPSULE" --json > "$SCRATCH/sessions.json"
python3 -I - "$SCRATCH/sessions.json" "$EXPECTED" "$SCRATCH/run.log" <<'PY'
import json, os, re, sys

sessions = json.load(open(sys.argv[1])).get("sessions") or []
expected = open(sys.argv[2]).read().strip()
want = dict(field.split("=", 1) for field in expected.split())
print(f"expected:   {expected}")
if not sessions:
    print("answer:     <no session recorded>; the end of mur run's output:")
    with open(sys.argv[3], errors="replace") as log:
        for line in log.read().splitlines()[-10:]:
            print(f"  {line}")
    print("VERDICT: WRONG")
    sys.exit(1)

records = sessions[0]["records_dir"]  # newest first
print(f"records:    {records}")
trace = []
with open(os.path.join(records, "trace.jsonl")) as f:
    for line in f:
        if line.strip():
            trace.append(json.loads(line))
try:
    answer = open(os.path.join(records, "out", "result.txt")).read().strip()
except FileNotFoundError:
    answer = ""
print(f"answer:     {answer or '<none>'}")
auths = [e.get("auth") for e in trace if e.get("event_type") == "harness_session"]
print(f"auth:       {', '.join(map(str, auths)) or '<no harness_session record>'}")
calls = [e for e in trace if e.get("event_type") == "tool_call"]
if not calls:
    print("tool_call:  <none>")
for call in calls:
    print(f"tool_call:  {call.get('tool_name')} status={call.get('status')}")
    print(f"  output:      {json.dumps(call.get('output'))}")
    print(f"  attachments: {json.dumps(call.get('attachments'))}")
failures = [e for e in trace if e.get("event_type") in ("harness_failed", "harness_warning")]
for failure in failures:
    print(f"{failure['event_type']}: {json.dumps({k: v for k, v in failure.items() if k in ('kind', 'code', 'message')})}")

number = re.search(r"NUMBER\s*=\s*(\d+)", answer, re.I)
colour = re.search(r"BACKGROUND\s*=\s*([A-Za-z]+)", answer, re.I)
if number and colour and number.group(1) == want["NUMBER"] \
        and colour.group(1).lower() == want["BACKGROUND"].lower():
    verdict = "PASS"
elif "NO-PICTURE" in answer.upper():
    verdict = "NOT SHOWN"
else:
    verdict = "WRONG"
print(f"VERDICT: {verdict}")
sys.exit(0 if verdict == "PASS" else 1)
PY
