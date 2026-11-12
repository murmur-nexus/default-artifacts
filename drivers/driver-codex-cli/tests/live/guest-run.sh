#!/bin/bash
# The guest run: the compiled driver component under a real `mur run`, against the mock provider,
# with murmur's own tool bridge and one real tool artifact (tool-read).
#
# Usage: guest-run.sh <scratch-dir> [codex-binary]
#
# <scratch-dir> must be a fresh directory under /space/scratch/. It becomes HOME for `mur run`,
# `mur doctor` and `codex` (a scratch ~/.codex with no auth.json), and holds the capsule, the
# wrapper and a copy of the codex binary, so no run reaches the operator's ~/.codex or ~/.murmur.
# The two artifacts are built and installed with the caller's own HOME, so cargo, rustup and
# wasi-sdk are the usual ones; the install lands in the capsule's project store.
#
# The capsule names a wrapper as `inference.command`. The wrapper puts the mock provider's
# overrides right after the subcommand and execs the copied binary. A wrapper may exec only a
# binary the class allows, and `capabilities.shell.allow` takes bare names, so the capsule allows
# `codex` and `mur` runs with the scratch bin directory first on PATH.
#
# The wrapper also appends the argv it was given (one entry per line, a blank line after each
# run) to harness-argv.log in the work folder, which the trace does not record.
#
# Two tasks run in one context: the first makes the mock call tool-read, the second is plain
# text and resumes the thread the first started.
set -euo pipefail
SCRATCH=$1
CODEX_SRC=${2:-$(readlink -f "$(command -v codex)")}
case "$SCRATCH" in /space/scratch/*) ;; *) echo "scratch must be under /space/scratch/" >&2; exit 1;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
MUR=${MUR_BIN:-$(command -v mur)}
MOCK_PORT=${MOCK_PORT:-18150}
CONTEXT=ctx-codex-guest
CAPSULE="$SCRATCH/capsule"
mkdir -p "$SCRATCH/home/.codex" "$SCRATCH/bin" "$CAPSULE" "$SCRATCH/bodies"
[ -e "$SCRATCH/home/.codex/auth.json" ] && { echo "refusing: auth.json exists" >&2; exit 1; }
cp "$CODEX_SRC" "$SCRATCH/bin/codex"

cat > "$SCRATCH/bin/codex-mock" <<SH
#!/bin/sh
export MOCK_KEY=sk-test
printf '%s\n' "\$@" '' >> ./harness-argv.log
sub=\$1
shift
exec $SCRATCH/bin/codex "\$sub" -c model_provider=mock -c model_providers.mock.name=mock -c model_providers.mock.base_url=http://127.0.0.1:$MOCK_PORT/v1 -c model_providers.mock.wire_api=responses -c model_providers.mock.env_key=MOCK_KEY -c model=gpt-5.4 "\$@"
SH
chmod +x "$SCRATCH/bin/codex-mock"

DRIVER_VERSION=$(sed -n 's/^version:[[:space:]]*//p' "$ROOT/drivers/driver-codex-cli/murmur.yaml" | head -1)
TOOL_VERSION=$(sed -n 's/^version:[[:space:]]*//p' "$ROOT/tools/tool-read/murmur.yaml" | head -1)
cat > "$CAPSULE/murmur.yaml" <<YAML
name: codex-guest-run
version: 0.1.0
artifacts:
  - name: local/driver-codex-cli
    version: "$DRIVER_VERSION"
    runtime: driver
  - name: local/tool-read
    version: "$TOOL_VERSION"
    runtime: tool
capabilities:
  containment: scoped
  env:
    allow: [HOME, PATH]
  shell:
    allow: [codex]
  harness:
    paths:
      - { path: ~/.codex, access: read-write }
  network:
    allow: [chatgpt.com, auth.openai.com, "127.0.0.1:$MOCK_PORT"]
trace:
  capture: content
lifecycle:
  conversation: threaded
inference:
  transport: process
  driver:
    artifact: local/driver-codex-cli
  command: $SCRATCH/bin/codex-mock
YAML
printf 'GUEST-RUN-NOTES: the capsule read this file through tool-read.\n' > "$CAPSULE/notes.txt"
printf '%s' 'CALL_TOOL=tool_read ARGS={"path":"notes.txt"} Read the notes.' > "$CAPSULE/task1.md"
printf '%s' 'Now answer in one line.' > "$CAPSULE/task2.md"

(cd "$ROOT" && ./scripts/local-install.sh tool-read "$CAPSULE" && ./scripts/local-install.sh driver-codex-cli "$CAPSULE") > "$SCRATCH/install.log" 2>&1
tail -n 3 "$SCRATCH/install.log"

export HOME="$SCRATCH/home"
export PATH="$SCRATCH/bin:$PATH"
python3 -I "$HERE/mock.py" "$MOCK_PORT" "$SCRATCH/mock.log" "$SCRATCH/bodies" & MOCK_PID=$!
trap 'kill $MOCK_PID 2>/dev/null' EXIT
sleep 1

for n in 1 2; do
  echo "→ $MUR run --task task$n.md --context $CONTEXT --workdir $CAPSULE"
  status=0
  (cd "$CAPSULE" && "$MUR" run --task "task$n.md" --context "$CONTEXT" --workdir "$CAPSULE") > "$SCRATCH/run$n.log" 2>&1 || status=$?
  echo "  exit $status; output in $SCRATCH/run$n.log"
done
echo "→ $MUR doctor (scratch HOME)"
(cd "$CAPSULE" && "$MUR" doctor) > "$SCRATCH/doctor-scratch.log" 2>&1 || true
grep -i -n "login\|process harness\|harness" "$SCRATCH/doctor-scratch.log" | head -20
