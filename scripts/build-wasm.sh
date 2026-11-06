#!/usr/bin/env bash
# Build every workspace crate that targets wasm32-wasip2, each artifact in its own
# cargo invocation.
#
# This is the single definition of "build everything that targets wasm" — ci.yml
# and build.yml's `release` job call it rather than inlining cargo invocations, so
# a developer reproducing CI's build runs the same command with nothing to retype.
#
# One invocation per artifact, never `--workspace`: cargo unifies dependency
# features across every crate a single invocation builds, so a component built
# beside the others links differently compiled dependencies than one built alone.
# Its bytes would then depend on which other artifacts exist, and the release's
# bytes gate (index-release.sh) would fail on an artifact nobody touched. Built
# alone, a component's bytes are the ones `cargo build -p <name>`,
# local-install.sh and the `wasm_component` tests produce.
# scripts/check-build-isolation.sh proves it. The `internal-lib` crates ship
# nothing, so they share one invocation, which still proves they cross-compile.
#
# The crate lists are derived by scripts/classify-crates.sh from each crate's own
# manifest, never hand-listed here. One rule excludes a crate: an internal
# `[[bin]]` crate with no murmur.yaml. That script's header documents which crates
# it catches and fails on a member it cannot classify.
#
# C sources that are compiled for wasm32-wasip2 (rusqlite's bundled SQLite, in
# tool-test-report, tool-code-coverage and tool-code-graph, and tree-sitter with
# its grammars, in tool-code-graph) need the pinned wasi-sdk (wasi-sdk.toml), and this script
# installs it itself through scripts/wasi-sdk.sh before its first cargo
# invocation. The release workflow (build.yml) calls this script and is not
# edited for a toolchain, so the script provisions its own; with the toolchain
# already in the cache, that costs nothing.
#
# Usage:   scripts/build-wasm.sh [<extra cargo args>...]
#          Extra arguments are appended to every cargo invocation (e.g. --locked).
# Exit:    0 — build succeeded
#          1 — a workspace member could not be classified, wasi-sdk could not be
#              installed, or cargo failed
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

classification="$(bash "$SCRIPT_DIR/classify-crates.sh")"

excluded_bins="$(printf '%s\n' "$classification"   | awk -F'\t' '$1 == "internal-bin"    { print $2 }')"
libs="$(printf '%s\n' "$classification"            | awk -F'\t' '$1 == "internal-lib"    { print $2 }')"
artifacts="$(printf '%s\n' "$classification"       | awk -F'\t' '$1 == "wasm-artifact"   { print $2 }')"

if [ -z "$libs$artifacts" ]; then
    echo "error: no workspace member targets wasm32-wasip2 — refusing to run a build that would produce nothing." >&2
    exit 1
fi

echo "Excluded — internal [[bin]] crate (no murmur.yaml, host-only):"
for crate in $excluded_bins; do echo "  $crate"; done

n_libs="$(printf '%s' "$libs" | grep -c . || true)"
n_artifacts="$(printf '%s' "$artifacts" | grep -c . || true)"
echo "Building $((n_libs + n_artifacts)) crates for wasm32-wasip2: $n_libs internal libs in one cargo invocation, then $n_artifacts artifacts one invocation each."

cd "$REPO_ROOT"

bash "$SCRIPT_DIR/wasi-sdk.sh" >/dev/null

if [ -n "$libs" ]; then
    lib_args=""
    for crate in $libs; do lib_args="$lib_args -p $crate"; done
    # shellcheck disable=SC2086 # one word per -p flag
    if ! (set -x; cargo build --target wasm32-wasip2 --release $lib_args "$@"); then
        echo "error: cargo build failed for the internal libs" >&2
        exit 1
    fi
fi

for crate in $artifacts; do
    if ! (set -x; cargo build -p "$crate" --target wasm32-wasip2 --release "$@"); then
        echo "error: cargo build failed for $crate" >&2
        exit 1
    fi
done
