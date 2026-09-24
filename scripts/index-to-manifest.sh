#!/usr/bin/env bash
# Turn artifacts-index.json into a minimal murmur.yaml whose `artifacts:` list
# names every published artifact at its indexed version, so a plain
# `mur install` in the output directory pulls all of them into its project store.
#
# Usage:
#   scripts/index-to-manifest.sh [out-dir] [index.json]
#
# Example:
#   scripts/index-to-manifest.sh /tmp/all-artifacts && (cd /tmp/all-artifacts && mur install)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-.}"
INDEX="${2:-$REPO_ROOT/artifacts-index.json}"

command -v jq >/dev/null || { echo "error: jq is required" >&2; exit 1; }
mkdir -p "$OUT_DIR"

{
  echo "name: all-default-artifacts"
  echo 'version: "0.0.0"'
  echo
  echo "artifacts:"
  jq -r '.artifacts[] | "  - name: \(.name)\n    runtime: \(.runtime)\n    version: \"\(.version)\""' "$INDEX"
} > "$OUT_DIR/murmur.yaml"

echo "wrote $OUT_DIR/murmur.yaml ($(jq '.artifacts | length' "$INDEX") artifacts)"
