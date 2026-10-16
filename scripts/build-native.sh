#!/usr/bin/env bash
# Build every native tool for the host, each in its own cargo invocation.
#
# This is the single definition of "build the native tools" — build.yml's `native`
# job calls it on every platform runner. The crates are the `native-artifact` rows
# of scripts/classify-crates.sh, which reads `implementation: native` from each
# murmur.yaml; nothing hand-lists them. Each binary lands at
# target/release/<name>, named after its crate, which is the artifact's name.
#
# One invocation per tool, never one over all of them: cargo unifies dependency
# features across every crate a single invocation builds, so a tool built beside
# the others links differently compiled dependencies than one built alone. Its
# bytes would then depend on which tools exist, and the release's bytes gate
# (index-release.sh) would fail on a tool nobody touched. Built alone, a tool's
# bytes are the ones `cargo build --release -p <name>` and local-install.sh
# produce. scripts/check-build-isolation.sh proves it.
#
# Runs under macOS's bash 3.2: no arrays, no associative arrays, no `mapfile`.
#
# Usage:   scripts/build-native.sh [<extra cargo args>...]
#          Extra arguments are appended to every cargo invocation (e.g. --locked).
# Exit:    0 — every tool built
#          1 — classification failed, no native tool exists, or cargo failed
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if ! classification="$(bash "$SCRIPT_DIR/classify-crates.sh")"; then
    echo "error: scripts/classify-crates.sh failed (see above); cannot tell which crates are native." >&2
    exit 1
fi

crates="$(printf '%s\n' "$classification" | awk -F'\t' '$1 == "native-artifact" { print $2 }')"

if [ -z "$crates" ]; then
    echo "error: no workspace member is a native artifact — refusing to run a build that would produce nothing." >&2
    exit 1
fi

echo "Building $(printf '%s\n' "$crates" | grep -c .) native tools, one cargo invocation each."

cd "$REPO_ROOT"
for crate in $crates; do
    if ! (set -x; cargo build --release -p "$crate" "$@"); then
        echo "error: cargo build failed for native tool $crate" >&2
        exit 1
    fi
done
