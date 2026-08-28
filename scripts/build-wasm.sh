#!/usr/bin/env bash
# Build every workspace crate that targets wasm32-wasip2.
#
# This is the single definition of "build everything that targets wasm" — ci.yml
# calls it rather than inlining the cargo invocation, so a developer reproducing
# CI's build runs the same command with nothing to retype.
#
# The `--exclude` flags are derived by scripts/classify-crates.sh from each
# crate's own manifest, never hand-listed here. Two rules produce them:
# `implementation: native` in an artifact's murmur.yaml, and an internal
# `[[bin]]` crate with no murmur.yaml. That script's header documents which
# crates each rule catches and fails on a member it cannot classify.
#
# Native tools are excluded because they are host binaries, not components:
# they link C sources (bundled SQLite, tree-sitter) or need host capabilities
# (spawning `git`, raw sockets and a native TLS stack) that a wasm32-wasip2
# guest does not have. Attempting one anyway fails deep inside a C build with an
# error naming the C compiler rather than the real mistake. build.yml's
# `build-native` matrix builds them instead.
#
# Usage:   scripts/build-wasm.sh [<extra cargo args>...]
#          Extra arguments are appended to the cargo invocation (e.g. --locked).
# Exit:    0 — build succeeded
#          1 — a workspace member could not be classified, or cargo failed
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

classification="$(bash "$SCRIPT_DIR/classify-crates.sh")"

excluded_native="$(printf '%s\n' "$classification" | awk -F'\t' '$1 == "native-artifact" { print $2 }')"
excluded_bins="$(printf '%s\n' "$classification"   | awk -F'\t' '$1 == "internal-bin"    { print $2 }')"
included="$(printf '%s\n' "$classification"        | awk -F'\t' '$1 ~ /^(wasm-artifact|internal-lib)$/ { print $2 }')"

if [ -z "$included" ]; then
    echo "error: no workspace member targets wasm32-wasip2 — refusing to run a build that would produce nothing." >&2
    exit 1
fi

args=()
echo "Excluded — native artifact (implementation: native in murmur.yaml):"
while IFS= read -r crate; do
    [ -n "$crate" ] || continue
    echo "  $crate"
    args+=(--exclude "$crate")
done <<< "$excluded_native"

echo "Excluded — internal [[bin]] crate (no murmur.yaml, host-only):"
while IFS= read -r crate; do
    [ -n "$crate" ] || continue
    echo "  $crate"
    args+=(--exclude "$crate")
done <<< "$excluded_bins"

echo "Building $(printf '%s\n' "$included" | grep -c .) crates for wasm32-wasip2."

cd "$REPO_ROOT"
set -x
exec cargo build --workspace --target wasm32-wasip2 --release "${args[@]}" "$@"
