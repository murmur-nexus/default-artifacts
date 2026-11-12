#!/bin/bash
# The probes behind tests/live/README.md, run against one codex binary with the plan `launch`
# builds (via `cargo run --example print_plan`) plus the mock provider overrides.
#
# Usage: probes.sh <codex-binary> <scratch-dir> <probe> ...
# Probes: features, context, ratelimit, slash, codemode, env, strict, root
#
# <scratch-dir> must be a fresh directory under /space/scratch/. It is codex's HOME (no
# auth.json) and holds every log and captured request. Nothing reads the operator's HOME.
set -u
CODEX=$1; SCRATCH=$2; shift 2
case "$SCRATCH" in /space/scratch/*) ;; *) echo "scratch must be under /space/scratch/" >&2; exit 1;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
MOCK_PORT=${MOCK_PORT:-18120}; BRIDGE_PORT=${BRIDGE_PORT:-18220}
TOKEN=tok-probe-bridge
mkdir -p "$SCRATCH/home/.codex" "$SCRATCH/work" "$SCRATCH/bodies" "$SCRATCH/runs"
[ -e "$SCRATCH/home/.codex/auth.json" ] && { echo "refusing: auth.json exists" >&2; exit 1; }
(cd "$ROOT" && cargo build -q -p driver-codex-cli --example print_plan) || exit 1
PLAN_BIN="$ROOT/target/debug/examples/print_plan"
python3 -I "$HERE/picture.py" --out "$SCRATCH/picture.png" --number 482 --background blue >/dev/null
python3 -I "$HERE/mock.py" "$MOCK_PORT" "$SCRATCH/mock.log" "$SCRATCH/bodies" & MOCK_PID=$!
python3 -I "$HERE/bridge.py" "$BRIDGE_PORT" "$TOKEN" "$SCRATCH/bridge.log" "$SCRATCH/picture.png" & BRIDGE_PID=$!
trap 'kill $MOCK_PID $BRIDGE_PID 2>/dev/null' EXIT
sleep 1
printf '%s' "- You are a Murmur capsule probing the codex driver." > "$SCRATCH/system-prompt.md"
MOCK_OVERRIDES=(-c model_provider=mock -c model_providers.mock.name=mock
  -c "model_providers.mock.base_url=http://127.0.0.1:$MOCK_PORT/v1"
  -c model_providers.mock.wire_api=responses -c model_providers.mock.env_key=MOCK_KEY -c model=gpt-5.4)

# probe <name> <task> <workdir> <env-mode> [extra args after the plan's own, before `-`] -- [print_plan flags]
# env-mode: full (HOME, PATH, MOCK_KEY), nohome, nopath, root (full, as uid 0 in a user
# namespace through `unshare -r`).
# nohome drops HOME but sets CODEX_HOME to the scratch store: without HOME, codex finds its home
# directory from the passwd entry, and would otherwise read and write the operator's ~/.codex.
probe() {
  local name=$1 task=$2 work=$3 envmode=$4; shift 4
  local extra=() pflags=() a
  while [ $# -gt 0 ] && [ "$1" != "--" ]; do extra+=("$1"); shift; done
  [ "${1:-}" = "--" ] && shift; pflags=("$@")
  local dir="$SCRATCH/runs/$name"; mkdir -p "$dir"
  printf '%s' "$task" > "$dir/task"
  "$PLAN_BIN" --out "$dir/plan" --system-prompt-file "$SCRATCH/system-prompt.md" --task-file "$dir/task" \
    --server murmur --url "http://127.0.0.1:$BRIDGE_PORT/mcp" --token "$TOKEN" "${pflags[@]}" || return
  local args=() envs=()
  while IFS= read -r -d '' a; do args+=("${a//\{files_dir\}/$dir/plan/files}"); done < "$dir/plan/argv"
  while IFS= read -r -d '' a; do envs+=("$a"); done < "$dir/plan/env"
  local last=$(( ${#args[@]} - 1 ))
  local full=("${args[0]}" "${MOCK_OVERRIDES[@]}" "${args[@]:1:$((last - 1))}" "${extra[@]}" "${args[$last]}")
  local base=(HOME="$SCRATCH/home" PATH=/usr/bin:/bin)
  [ "$envmode" = nohome ] && base=(CODEX_HOME="$SCRATCH/home/.codex" PATH=/usr/bin:/bin)
  [ "$envmode" = nopath ] && base=(HOME="$SCRATCH/home")
  local asroot=()
  [ "$envmode" = root ] && asroot=(unshare -r)
  (cd "$work" && "${asroot[@]}" env -i "${base[@]}" MOCK_KEY=sk-test "${envs[@]}" timeout 120 "$CODEX" "${full[@]}" \
    < "$dir/plan/stdin" > "$dir/stdout.jsonl" 2> "$dir/stderr.txt"); echo $? > "$dir/exit"
  mkdir -p "$dir/requests"; find "$SCRATCH/bodies" -maxdepth 1 -name '*.json' -exec mv {} "$dir/requests/" \;
  echo "== $name: exit $(cat "$dir/exit"), first stderr line: $(grep -v 'fail to delete session' "$dir/stderr.txt" | head -1)"
}
first_request() { ls "$SCRATCH/runs/$1/requests/"*.json 2>/dev/null | head -1; }
tools_of() { local r; r=$(first_request "$1"); [ -n "$r" ] && python3 -I "$HERE/request_summary.py" "$r" | sed -n 's/^TOOLS //p'; }

for p in "$@"; do case $p in
strict)
  probe strict-unknown-key "hi" "$SCRATCH/work" full -c no_such_key_for_probe=1
  probe strict-unknown-feature "hi" "$SCRATCH/work" full --disable no_such_feature_for_probe
  ;;
features)
  probe features-base "hi" "$SCRATCH/work" full
  echo "base tools: $(tools_of features-base)"
  for f in $(env -i HOME="$SCRATCH/home" PATH=/usr/bin:/bin "$CODEX" features list 2>/dev/null | awk '$2=="stable" && $3=="true" {print $1}'); do
    case " shell_tool unified_exec view_image multi_agent goals browser_use computer_use image_generation apps plugins in_app_browser tool_suggest skill_search sleep_tool code_mode_host " in *" $f "*) continue;; esac
    probe "features-$f" "hi" "$SCRATCH/work" full --disable "$f" >/dev/null
    t=$(tools_of "features-$f"); b=$(tools_of features-base)
    [ "$t" = "$b" ] && echo "  --disable $f: tools unchanged (exit $(cat "$SCRATCH/runs/features-$f/exit"))" \
                     || echo "  --disable $f: TOOLS CHANGED: $t (exit $(cat "$SCRATCH/runs/features-$f/exit"))"
  done
  ;;
context)
  printf '%s\n' "GLOBAL-AGENTS-MD-MARKER: the operator's global instructions." > "$SCRATCH/home/.codex/AGENTS.md"
  mkdir -p "$SCRATCH/cwdwork/.codex"
  printf '%s\n' "PROJECT-AGENTS-MD-MARKER: project instructions." > "$SCRATCH/cwdwork/AGENTS.md"
  cat > "$SCRATCH/cwdwork/.codex/config.toml" <<TOML
developer_instructions = "PROJECT-CONFIG-MARKER developer instructions from cwd .codex/config.toml"
[mcp_servers.cwd_srv]
url = "http://127.0.0.1:$BRIDGE_PORT/mcp"
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "touch $SCRATCH/cwd-hook-ran"
TOML
  probe context-global "hi" "$SCRATCH/work" full
  python3 -I "$HERE/request_summary.py" "$(first_request context-global)" --full > "$SCRATCH/runs/context-global/request.txt"
  probe context-cwd "hi" "$SCRATCH/cwdwork" full
  python3 -I "$HERE/request_summary.py" "$(first_request context-cwd)" --full > "$SCRATCH/runs/context-cwd/request.txt"
  echo "global AGENTS.md marker in request: $(grep -c GLOBAL-AGENTS-MD-MARKER "$SCRATCH/runs/context-global/request.txt")"
  echo "cwd AGENTS.md marker in request: $(grep -c PROJECT-AGENTS-MD-MARKER "$SCRATCH/runs/context-cwd/request.txt")"
  echo "cwd config developer marker in request: $(grep -c PROJECT-CONFIG-MARKER "$SCRATCH/runs/context-cwd/request.txt")"
  echo "cwd config MCP server tools: $(tools_of context-cwd)"
  echo "cwd_srv in bridge log: $(grep -c cwd_srv "$SCRATCH/bridge.log" 2>/dev/null)"
  echo "cwd hook ran: $([ -e "$SCRATCH/cwd-hook-ran" ] && echo yes || echo no)"
  echo "message roles and openings (global run):"
  grep '^INPUT' "$SCRATCH/runs/context-global/request.txt" | cut -c1-200
  rm -f "$SCRATCH/home/.codex/AGENTS.md"
  ;;
ratelimit)
  probe ratelimit-200 "RATELIMIT then reply" "$SCRATCH/work" full
  echo "stdout:"; cat "$SCRATCH/runs/ratelimit-200/stdout.jsonl"
  probe ratelimit-usage-limit "USAGELIMIT" "$SCRATCH/work" full
  echo "stdout:"; cat "$SCRATCH/runs/ratelimit-usage-limit/stdout.jsonl"
  ;;
slash)
  for t in /status /review; do
    probe "slash-${t#/}" "$t" "$SCRATCH/work" full
    echo "last user message: $(python3 -I "$HERE/request_summary.py" "$(first_request "slash-${t#/}")" | grep '^INPUT message user' | tail -1)"
    echo "stdout: $(tr '\n' ' ' < "$SCRATCH/runs/slash-${t#/}/stdout.jsonl" | cut -c1-600)"
  done
  ;;
codemode)
  probe codemode-host-on "CALL_TOOL=echo_tool" "$SCRATCH/work" full -- --model gpt-5.6-terra
  probe codemode-host-off "CALL_TOOL=echo_tool" "$SCRATCH/work" full --disable code_mode_host -- --model gpt-5.6-terra
  for r in codemode-host-on codemode-host-off; do
    python3 -I "$HERE/request_summary.py" "$(first_request $r)" --full > "$SCRATCH/runs/$r/request.txt"
    echo "-- $r: stdout:"; cat "$SCRATCH/runs/$r/stdout.jsonl"
    echo "-- $r: tools: $(sed -n 's/^TOOLS //p' "$SCRATCH/runs/$r/request.txt")"
    grep -o 'node_repl_disabled[^,]*' "$SCRATCH/runs/$r/request.txt" | sort | uniq -c
    grep -o 'Deferred tool namespaces[^\\]*' "$SCRATCH/runs/$r/request.txt" | sort | uniq -c
  done
  diff <(grep -v '^FIELD client_metadata\|^FIELD prompt_cache_key' "$SCRATCH/runs/codemode-host-on/request.txt") \
       <(grep -v '^FIELD client_metadata\|^FIELD prompt_cache_key' "$SCRATCH/runs/codemode-host-off/request.txt") | cut -c1-400
  ;;
env)
  echo "env -i codex --version: $(env -i "$CODEX" --version 2>&1 | head -1) (exit $?)"
  env -i "$CODEX" --version >/dev/null 2>&1; echo "  exit $?"
  echo "scratch HOME + PATH: $(env -i HOME="$SCRATCH/home" PATH=/usr/bin:/bin "$CODEX" --version 2>&1 | head -1)"
  env -i HOME="$SCRATCH/home" PATH=/usr/bin:/bin "$CODEX" --version >/dev/null 2>&1; echo "  exit $?"
  probe env-nohome "hi" "$SCRATCH/work" nohome
  head -3 "$SCRATCH/runs/env-nohome/stdout.jsonl"
  probe env-nopath "hi" "$SCRATCH/work" nopath
  head -3 "$SCRATCH/runs/env-nopath/stdout.jsonl"
  ;;
root)
  probe root-text "hi" "$SCRATCH/work" root
  cat "$SCRATCH/runs/root-text/stdout.jsonl"
  probe root-tool "CALL_TOOL=echo_tool" "$SCRATCH/work" root
  cat "$SCRATCH/runs/root-tool/stdout.jsonl"
  ;;
*) echo "unknown probe $p" >&2 ;;
esac; done
