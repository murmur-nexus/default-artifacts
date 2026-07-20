#!/usr/bin/env bash
# Check that artifact-facing WIT files (guest/ and hook/) are in sync with murmur/capsule-runtime.
#
# Only guest/ and hook/ are compared — those are the subdirectories used by wit_bindgen::generate!
# in every WASM artifact. Top-level files (host/, runtime/, worlds.wit) are trimmed reference
# copies that intentionally omit runtime-internal interfaces and are not compared here.
#
# Run from anywhere inside the default-artifacts repo.
#
# Exit codes:
#   0 — all shared files match; ONLY_MURMUR entries are informational (not failures)
#   1 — one or more shared files have drifted, or default-artifacts has a file murmur does not
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DA_WIT="$(cd "$SCRIPT_DIR/.." && pwd)/wit"
MURMUR_WIT="$(cd "$SCRIPT_DIR/../../murmur/crates/capsule-runtime/wit" 2>/dev/null && pwd)" || {
    echo "error: murmur repo not found at $SCRIPT_DIR/../../murmur" >&2
    echo "Expected layout: both repos checked out side-by-side under the same parent directory." >&2
    exit 1
}

drift=0
only_da=0
only_murmur=0
matching=0

compare_subtree() {
    local subtree="$1"
    local da_root="$DA_WIT/$subtree"
    local murmur_root="$MURMUR_WIT/$subtree"

    if [ ! -d "$da_root" ]; then
        echo "MISSING_DA   $subtree/  (directory not found in default-artifacts)"
        drift=$((drift + 1))
        return
    fi

    # Files in default-artifacts subtree
    while IFS= read -r rel; do
        local murmur_file="$murmur_root/$rel"
        if [ -f "$murmur_file" ]; then
            if diff -q "$da_root/$rel" "$murmur_file" > /dev/null 2>&1; then
                matching=$((matching + 1))
            else
                echo "DRIFT        $subtree/$rel"
                diff --unified=2 "$murmur_file" "$da_root/$rel" | tail -n +4 | head -24 || true
                echo ""
                drift=$((drift + 1))
            fi
        else
            echo "ONLY_DA      $subtree/$rel"
            only_da=$((only_da + 1))
        fi
    done < <(find "$da_root" -type f -name "*.wit" | sed "s|$da_root/||" | sort)

    # Files only in murmur subtree (informational)
    if [ -d "$murmur_root" ]; then
        while IFS= read -r rel; do
            if [ ! -f "$da_root/$rel" ]; then
                echo "ONLY_MURMUR  $subtree/$rel"
                only_murmur=$((only_murmur + 1))
            fi
        done < <(find "$murmur_root" -type f -name "*.wit" | sed "s|$murmur_root/||" | sort)
    fi
}

compare_subtree "guest"
compare_subtree "hook"

echo ""
echo "Results:"
printf "  %2d  matching\n"         "$matching"
printf "  %2d  drifted        (DRIFT — content differs; update whichever side is stale)\n" "$drift"
printf "  %2d  only-in-da     (ONLY_DA — default-artifacts has this, murmur does not)\n"  "$only_da"
printf "  %2d  only-in-murmur (ONLY_MURMUR — murmur has this, not needed by artifacts)\n" "$only_murmur"

if [ "$drift" -gt 0 ] || [ "$only_da" -gt 0 ]; then
    echo ""
    echo "error: WIT files are out of sync. Resolve DRIFT and ONLY_DA entries above." >&2
    exit 1
fi

echo ""
echo "OK: all artifact-facing WIT files are in sync."
