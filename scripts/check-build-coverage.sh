#!/usr/bin/env bash
# Check that every publishable artifact in artifacts.toml is built by exactly one
# build.yml job matrix, and by the matrix its own manifest calls for.
#
# artifacts.toml is the source of truth for what ships. The build.yml matrices are
# hand-maintained YAML, so a newly-added artifact silently misses every release
# until someone notices its .mur.zip is absent — it still appears in
# artifacts-index.json with a version, so `mur install` fails on a phantom entry.
# This check closes that gap.
#
# The matrices stay hand-listed rather than generated: each entry carries a
# `path:` alongside its `name:`, GitHub needs a whole extra job and runner to feed
# `fromJSON` a computed matrix, and this script reads those literal entries to do
# its own job. So the second check below holds them to the derivation instead —
# scripts/classify-crates.sh decides from each crate's manifest whether it targets
# wasm32-wasip2, and an artifact listed under the wrong matrix (or moved between
# wasm and native without its matrix following) fails CI here rather than failing
# a release.
#
# Run from anywhere inside the default-artifacts repo.
#
# Exit codes:
#   0 — every artifact is covered exactly once, by the right matrix
#   1 — an artifact is uncovered, covered by more than one matrix, or covered by
#       a matrix that disagrees with its manifest
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

# Every artifact named in a build.yml matrix entry: `- { name: <x>, path: <y> }`,
# tagged with the job it sits under (`build-wasm`, `build-native`, `build-skills`)
# as `<job><TAB><name>`. Job names are the two-space-indented keys under `jobs:`.
# The `path:` field is required in the match so this does not also pick up the
# build-native `platform:` matrix rows, which are `- { name: <x>, runner: <y> }`.
# Kept with duplicates so a double-listed artifact is caught too.
built_by_job=$(awk '
    /^  [a-z][a-z0-9-]*:[[:space:]]*$/ { job = $1; sub(/:$/, "", job); next }
    /^[[:space:]]*-[[:space:]]*\{[[:space:]]*name:[[:space:]]*[a-z0-9-]+[[:space:]]*,[[:space:]]*path:/ {
        entry = $0
        sub(/.*name:[[:space:]]*/, "", entry)
        sub(/[[:space:]]*,[[:space:]]*path:.*/, "", entry)
        print job "\t" entry
    }
' "$BUILD_YML" | sort)

built=$(printf '%s\n' "$built_by_job" | cut -f2 | sort)

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

# Second check: each Rust artifact sits under the matrix its manifest calls for.
# classify-crates.sh derives that from `implementation:` in the artifact's own
# murmur.yaml — the same derivation scripts/build-wasm.sh excludes natives with,
# so the wasm build and the release matrices cannot drift apart. Skill artifacts
# are not workspace members, so they are absent from the classification and left
# to the coverage check above.
classification=$(bash "$SCRIPT_DIR/classify-crates.sh")

matrix_for_class() {
    case "$1" in
        wasm-artifact)   echo "build-wasm"   ;;
        native-artifact) echo "build-native" ;;
        *)               echo ""             ;;
    esac
}

checked=0

while IFS=$'\t' read -r class crate _path; do
    [ -n "$class" ] || continue
    expected=$(matrix_for_class "$class")
    # internal-bin / internal-lib crates are unpublishable; the UNDECLARED check
    # above already fails if one reaches a matrix.
    [ -n "$expected" ] || continue
    checked=$((checked + 1))
    actual=$(printf '%s\n' "$built_by_job" | awk -F'\t' -v c="$crate" '$2 == c { print $1 }' | sort -u)
    if [ -z "$actual" ]; then
        # Already reported as UNCOVERED above; nothing to add.
        continue
    fi
    if [ "$actual" != "$expected" ]; then
        echo "WRONG MATRIX $crate  (classified $class, expected the $expected matrix, found in: ${actual//$'\n'/, })"
        fail=1
    fi
done <<< "$classification"

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "error: a build.yml matrix disagrees with the artifact's own murmur.yaml." >&2
    echo "Either move the entry to the matrix its 'implementation:' calls for, or" >&2
    echo "change 'implementation:' if the artifact genuinely changed target." >&2
    echo "Run ./scripts/classify-crates.sh to see how each crate is classified." >&2
    exit 1
fi

echo "OK: all $declared_count artifacts in artifacts.toml are built exactly once."
echo "OK: all $checked Rust artifacts sit under the matrix their murmur.yaml calls for."
