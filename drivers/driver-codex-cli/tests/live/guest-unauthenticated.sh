#!/bin/bash
# A second guest run: the capsule an operator would write (no wrapper, no capabilities.shell.allow),
# against OpenAI's real endpoint with no credential at all. Nothing is spent: the turn is refused
# with 401, which also shows the harness reached the endpoint over TLS from inside the class.
#
# Usage: [SHELL_ALLOW=<binary>] guest-unauthenticated.sh <scratch-dir> [codex-binary]
#
# With SHELL_ALLOW set, the capsule also declares `capabilities.shell.allow: [<binary>]`. Under
# mur 0.11.0 a process harness is handed the host's TLS trust store only when that list is
# non-empty, so without it codex cannot verify any HTTPS certificate.
#
# <scratch-dir> must be a fresh directory under /space/scratch/; it is HOME for `mur` and `codex`
# (a scratch ~/.codex with no auth.json) and holds a copy of the codex binary, named directly as
# `inference.command`.
set -euo pipefail
SCRATCH=$1
CODEX_SRC=${2:-$(readlink -f "$(command -v codex)")}
case "$SCRATCH" in /space/scratch/*) ;; *) echo "scratch must be under /space/scratch/" >&2; exit 1;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
MUR=${MUR_BIN:-$(command -v mur)}
CAPSULE="$SCRATCH/capsule"
mkdir -p "$SCRATCH/home/.codex" "$SCRATCH/bin" "$CAPSULE"
[ -e "$SCRATCH/home/.codex/auth.json" ] && { echo "refusing: auth.json exists" >&2; exit 1; }
cp "$CODEX_SRC" "$SCRATCH/bin/codex"
DRIVER_VERSION=$(sed -n 's/^version:[[:space:]]*//p' "$ROOT/drivers/driver-codex-cli/murmur.yaml" | head -1)
cat > "$CAPSULE/murmur.yaml" <<YAML
name: codex-guest-unauthenticated
version: 0.1.0
artifacts:
  - name: local/driver-codex-cli
    version: "$DRIVER_VERSION"
    runtime: driver
capabilities:
  containment: scoped
  env:
    allow: [HOME, PATH]
$( [ -n "${SHELL_ALLOW:-}" ] && printf '  shell:\n    allow: [%s]\n' "$SHELL_ALLOW" )
  harness:
    paths:
      - { path: ~/.codex, access: read-write }
  network:
    allow: [chatgpt.com, auth.openai.com, api.openai.com]
trace:
  capture: content
inference:
  transport: process
  driver:
    artifact: local/driver-codex-cli
  command: $SCRATCH/bin/codex
YAML
printf '%s' 'Reply briefly.' > "$CAPSULE/task.md"
(cd "$ROOT" && ./scripts/local-install.sh driver-codex-cli "$CAPSULE") > "$SCRATCH/install.log" 2>&1
export HOME="$SCRATCH/home"
status=0
(cd "$CAPSULE" && "$MUR" run --task task.md --workdir "$CAPSULE") > "$SCRATCH/run.log" 2>&1 || status=$?
echo "mur run exit $status"
grep -v '^\[capsule-runtime\] warning\[W-SEC-01[13]\]\|W-SEC-005' "$SCRATCH/run.log" | cut -c1-400
