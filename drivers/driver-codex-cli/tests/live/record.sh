#!/bin/bash
# Records codex against the mock provider and the mock bridge with exactly the plan `launch`
# builds, as printed by `cargo run --example print_plan`, plus only the mock provider overrides
# inserted right after `exec`.
#
# Usage: record.sh <codex-binary> <out-dir> <scratch-dir> [case ...]
#
# <scratch-dir> must be a fresh directory under /space/scratch/. It becomes codex's HOME (with no
# auth.json), the working directory and the mocks' logs. Nothing reads the operator's HOME.
# With no case named, every case is recorded. Case 15 talks to the real OpenAI endpoint with no
# credential: it is refused with 401 and spends nothing.
set -u
CODEX=$1; OUT=$2; SCRATCH=$3; shift 3
CASES=("$@")
[ ${#CASES[@]} -eq 0 ] && CASES=(01 02 03 04 05 06 07 08 09 10 11 12 13 14 15)
case "$SCRATCH" in /space/scratch/*) ;; *) echo "scratch must be under /space/scratch/" >&2; exit 1;; esac

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
MOCK_PORT=${MOCK_PORT:-18110}; BRIDGE_PORT=${BRIDGE_PORT:-18210}
TOKEN=tok-recording-bridge
SERVER=murmur
MISSING_ID=01a1cd99-0000-7000-8000-000000000000

mkdir -p "$OUT" "$SCRATCH/home/.codex" "$SCRATCH/work" "$SCRATCH/plans" "$SCRATCH/bodies" "$SCRATCH/requests"
[ -e "$SCRATCH/home/.codex/auth.json" ] && { echo "refusing: $SCRATCH/home/.codex/auth.json exists" >&2; exit 1; }

(cd "$ROOT" && cargo build -q -p driver-codex-cli --example print_plan) || exit 1
PLAN_BIN="$ROOT/target/debug/examples/print_plan"

python3 -I "$HERE/picture.py" --out "$SCRATCH/picture.png" --number 482 --background blue --expected "$SCRATCH/picture.expected" >/dev/null
python3 -I "$HERE/mock.py" "$MOCK_PORT" "$SCRATCH/mock.log" "$SCRATCH/bodies" & MOCK_PID=$!
python3 -I "$HERE/bridge.py" "$BRIDGE_PORT" "$TOKEN" "$SCRATCH/bridge.log" "$SCRATCH/picture.png" & BRIDGE_PID=$!
trap 'kill $MOCK_PID $BRIDGE_PID 2>/dev/null' EXIT
sleep 1

printf '%s' "- You are a Murmur capsule recording the codex driver.
- Answer in one short line." > "$SCRATCH/system-prompt.md"

MOCK_OVERRIDES=(-c model_provider=mock -c model_providers.mock.name=mock
  -c "model_providers.mock.base_url=http://127.0.0.1:$MOCK_PORT/v1"
  -c model_providers.mock.wire_api=responses -c model_providers.mock.env_key=MOCK_KEY -c model=gpt-5.4)

# plan <name> <task> [print_plan flags...]: writes the plan for <name> into $SCRATCH/plans/<name>.
plan() {
  local name=$1 task=$2; shift 2
  printf '%s' "$task" > "$SCRATCH/plans/$name.task"
  "$PLAN_BIN" --out "$SCRATCH/plans/$name" --system-prompt-file "$SCRATCH/system-prompt.md" \
    --task-file "$SCRATCH/plans/$name.task" --server "$SERVER" \
    --url "http://127.0.0.1:$BRIDGE_PORT/mcp" --token "$TOKEN" "$@"
}

# run <name> [mock|real] [sigint-after-seconds]
run() {
  local name=$1 provider=${2:-mock} sigint=${3:-}
  local dir="$SCRATCH/plans/$name" args=() envs=() a
  while IFS= read -r -d '' a; do args+=("${a//\{files_dir\}/$dir/files}"); done < "$dir/argv"
  while IFS= read -r -d '' a; do envs+=("$a"); done < "$dir/env"
  local full=("${args[0]}")
  [ "$provider" = mock ] && full+=("${MOCK_OVERRIDES[@]}")
  full+=("${args[@]:1}")
  printf '%s\n' "${full[@]}" > "$SCRATCH/plans/$name.full-argv"
  cd "$SCRATCH/work"
  env -i HOME="$SCRATCH/home" PATH=/usr/bin:/bin MOCK_KEY=sk-test "${envs[@]}" \
    "$CODEX" "${full[@]}" < "$dir/stdin" > "$OUT/$name.stdout.jsonl" 2> "$SCRATCH/$name.stderr.raw" &
  local pid=$!
  if [ -n "$sigint" ]; then sleep "$sigint"; kill -INT "$pid"; fi
  wait "$pid"; echo $? > "$OUT/$name.exit"
  sed "s|$SCRATCH|<scratch>|g" "$SCRATCH/$name.stderr.raw" > "$OUT/$name.stderr.txt"
  mkdir -p "$SCRATCH/requests/$name"
  find "$SCRATCH/bodies" -maxdepth 1 -name '*.json' -exec mv {} "$SCRATCH/requests/$name/" \;
  echo "$name: exit $(cat "$OUT/$name.exit"), $(wc -l < "$OUT/$name.stdout.jsonl") stdout lines"
}

thread_of() { python3 -I -c 'import json,sys
for l in open(sys.argv[1]):
    try: v=json.loads(l)
    except Exception: continue
    if v.get("type")=="thread.started": print(v["thread_id"]); break' "$OUT/$1.stdout.jsonl"; }

for c in "${CASES[@]}"; do
  case $c in
    01) plan 01-text-new-session "Remember SECRET-4242 for later. Reply briefly."; run 01-text-new-session ;;
    02) plan 02-resume-same-session "RECALL the secret." --mode resume --session "$(thread_of 01-text-new-session)"; run 02-resume-same-session ;;
    03) plan 03-resume-again "RECALL it once more." --mode resume --session "$(thread_of 01-text-new-session)"; run 03-resume-again ;;
    04) plan 04-resume-missing-session "RECALL the secret." --mode resume --session "$MISSING_ID"; run 04-resume-missing-session ;;
    05) plan 05-bridge-tool-call "CALL_TOOL=echo_tool"; run 05-bridge-tool-call ;;
    06) plan 06-bridge-tool-image "CALL_TOOL=picture_tool"; run 06-bridge-tool-image ;;
    07) plan 07-bridge-tool-error "CALL_TOOL=fail_tool"; run 07-bridge-tool-error ;;
    08) plan 08-thinking "THINK about it." --config '{"reasoning_summary":"auto"}'; run 08-thinking ;;
    09) plan 09-auth-401 "AUTH401"; run 09-auth-401 ;;
    10) plan 10-quota-429 "QUOTA429"; run 10-quota-429 ;;
    11) plan 11-interrupt-sigint "SLOW"; run 11-interrupt-sigint mock 4 ;;
    12) plan 12-apply-patch-refused "CALL_PATCH"; run 12-apply-patch-refused ;;
    13) plan 13-ask-user-refused "CALL_ASK"; run 13-ask-user-refused ;;
    14) plan 14-code-mode-model "CALL_TOOL=echo_tool" --model gpt-5.6-terra; run 14-code-mode-model ;;
    15) plan 15-auth-401-unauthenticated-real-endpoint "Reply briefly."; run 15-auth-401-unauthenticated-real-endpoint real ;;
    *) echo "unknown case $c" >&2 ;;
  esac
done
