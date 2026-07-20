#!/usr/bin/env bash
# Check that every publishable artifact in artifacts.toml is built by exactly one
# build.yml job matrix.
#
# artifacts.toml is the source of truth for what ships. The build.yml matrices are
# hand-maintained YAML, so a newly-added artifact silently misses every release
# until someone notices its .mur.zip is absent — it still appears in
# artifacts-index.json with a version, so `mur install` fails on a phantom entry.
# This check closes that gap.
#
# Run from anywhere inside the default-artifacts repo.
#
# Exit codes:
#   0 — every artifact is covered exactly once
#   1 — an artifact is uncovered, or covered by more than one matrix
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_TOML="$REPO_ROOT/artifacts.toml"
BUILD_YML="$REPO_ROOT/.github/workflows/build.yml"

for f in "$ARTIFACTS_TOML" "$BUILD_YML"; do
    if [ ! -f "$f" ]; then
        echo "error: not found: $f" >&2
        exit 1
    fi
done

# Every artifact declared in artifacts.toml.
declared=$(grep -E '^name[[:space:]]*=' "$ARTIFACTS_TOML" \
    | sed 's/name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/' | sort)

# Every artifact named in a build.yml matrix entry: `- { name: <x>, path: <y> }`.
# The `path:` field is required in the match so this does not also pick up the
# build-native `platform:` matrix rows, which are `- { name: <x>, runner: <y> }`.
# Counted with duplicates so a double-listed artifact is caught too.
built=$(grep -oE '^[[:space:]]*-[[:space:]]*\{[[:space:]]*name:[[:space:]]*[a-z0-9-]+[[:space:]]*,[[:space:]]*path:' "$BUILD_YML" \
    | sed 's/.*name:[[:space:]]*//; s/[[:space:]]*,[[:space:]]*path:.*//' | sort)

fail=0

while IFS= read -r name; do
    [ -z "$name" ] && continue
    count=$(printf '%s\n' "$built" | grep -cx "$name" || true)
    if [ "$count" -eq 0 ]; then
        echo "UNCOVERED    $name  (in artifacts.toml, built by no build.yml matrix)"
        fail=1
    elif [ "$count" -gt 1 ]; then
        echo "DUPLICATE    $name  (appears in $count build.yml matrix entries)"
        fail=1
    fi
done <<< "$declared"

# The reverse direction: a matrix entry with no artifacts.toml declaration would
# build something that has no version surface and no index entry.
while IFS= read -r name; do
    [ -z "$name" ] && continue
    if ! printf '%s\n' "$declared" | grep -qx "$name"; then
        echo "UNDECLARED   $name  (built by build.yml, absent from artifacts.toml)"
        fail=1
    fi
done <<< "$(printf '%s\n' "$built" | sort -u)"

declared_count=$(printf '%s\n' "$declared" | grep -c . || true)

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "error: artifacts.toml and build.yml disagree on what ships." >&2
    echo "Add the missing artifact to the matching build.yml matrix (build-wasm," >&2
    echo "build-native, or build-skills), or remove it from artifacts.toml." >&2
    exit 1
fi

echo "OK: all $declared_count artifacts in artifacts.toml are built exactly once."
