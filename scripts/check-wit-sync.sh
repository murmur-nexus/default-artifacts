#!/usr/bin/env bash
# Check that artifact-facing WIT files (guest/ and hook/) are in sync with murmur/capsule-runtime.
#
# Only guest/ and hook/ are compared — those are the subdirectories used by wit_bindgen::generate!
# in every WASM artifact. Top-level files (host/, runtime/, worlds.wit) are trimmed reference
# copies that intentionally omit runtime-internal interfaces and are not compared here.
#
# The vendored subtrees are not a freely-trimmable subset: each one must be closed under the
# imports and exports its own world declarations name. A world that references an interface no
# vendored file defines does not resolve, and the failure surfaces later and less clearly as a
# wit_bindgen::generate! error during cargo build. So every interface reference is resolved to
# the vendored file expected to define it, and an unresolved one is a hard failure (UNRESOLVED).
# A file murmur has that no vendored world references is still informational (ONLY_MURMUR), as
# guest/deps/murmur-shell/shell-execute.wit legitimately is.
#
# Run from anywhere inside the default-artifacts repo.
#
# Exit codes:
#   0 — all shared files match and every vendored world resolves
#   1 — a shared file has drifted, default-artifacts has a file murmur does not, or a vendored
#       world references an interface no vendored file defines
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
unresolved=0

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

# Locate the file under a WIT subtree that defines an interface reference, and echo its path
# relative to that subtree. Returns 1 when no file defines it.
#
# A reference is either a bare interface name — resolved against the referencing file's own
# package — or a fully-qualified `ns:pkg/iface@version`. Files are matched on their declared
# package and interface names rather than on the deps/ directory layout, so a dependency copied
# under an unconventional directory name still resolves.
find_interface_file() {
    local root="$1" own_pkg="$2" ref="$3"
    local want_pkg want_iface pkg_re iface_re

    if [[ "$ref" == */* ]]; then
        want_pkg="${ref%%/*}"
        want_iface="${ref##*/}"
        if [[ "$want_iface" == *@* ]]; then
            want_pkg="$want_pkg@${want_iface##*@}"
            want_iface="${want_iface%%@*}"
        fi
    else
        want_pkg="$own_pkg"
        want_iface="$ref"
    fi

    pkg_re="^[[:space:]]*package[[:space:]]+${want_pkg//./\\.}"
    if [[ "$want_pkg" == *@* ]]; then
        pkg_re="$pkg_re[[:space:]]*;"
    else
        # Reference carries no version, so accept the package at any version.
        pkg_re="$pkg_re(@[^;[:space:]]*)?[[:space:]]*;"
    fi
    iface_re="^[[:space:]]*interface[[:space:]]+${want_iface}[[:space:]]*\{"

    [ -d "$root" ] || return 1
    while IFS= read -r f; do
        grep -qE "$pkg_re" "$f" || continue
        grep -qE "$iface_re" "$f" || continue
        echo "${f#"$root"/}"
        return 0
    done < <(find "$root" -type f -name "*.wit" | sort)
    return 1
}

# Check that every interface a vendored world imports or exports is defined by a vendored file.
check_subtree_closure() {
    local subtree="$1"
    local da_root="$DA_WIT/$subtree"
    [ -d "$da_root" ] || return 0

    while IFS= read -r wf; do
        local rel="${wf#"$da_root"/}"
        local pkg world="" depth=0 stripped line ref hint murmur_rel
        pkg="$(sed -nE 's/^[[:space:]]*package[[:space:]]+([^;]+);.*/\1/p' "$wf" | head -1)"

        while IFS= read -r line; do
            if [ -z "$world" ]; then
                if [[ "$line" =~ ^[[:space:]]*world[[:space:]]+([a-zA-Z0-9_-]+) ]]; then
                    world="${BASH_REMATCH[1]}"
                    depth=0
                    stripped="${line//\{/}"
                    depth=$((depth + ${#line} - ${#stripped}))
                    stripped="${line//\}/}"
                    depth=$((depth - ${#line} + ${#stripped}))
                    [ "$depth" -gt 0 ] || world=""
                fi
                continue
            fi

            # Only the world's own items sit at depth 1; anything deeper belongs to an inline
            # `interface { ... }` block, which defines its own items and needs no file.
            if [ "$depth" -eq 1 ] && [[ "$line" =~ ^[[:space:]]*(import|export)[[:space:]]+([^\;{]+)\; ]]; then
                ref="${BASH_REMATCH[2]}"
                ref="${ref%"${ref##*[![:space:]]}"}"
                check_interface_ref "$subtree" "$rel" "$world" "$pkg" "$ref"
            fi

            stripped="${line//\{/}"
            depth=$((depth + ${#line} - ${#stripped}))
            stripped="${line//\}/}"
            depth=$((depth - ${#line} + ${#stripped}))
            [ "$depth" -gt 0 ] || world=""
        done < "$wf"
    done < <(grep -rlE "^[[:space:]]*world[[:space:]]" --include="*.wit" "$da_root" | sort)
}

# Report a world's interface reference when no vendored file under the subtree defines it.
check_interface_ref() {
    local subtree="$1" rel="$2" world="$3" pkg="$4" ref="$5"
    local da_root="$DA_WIT/$subtree" hint murmur_rel

    # `import name: func(...)` and inline interface blocks are not interface references.
    [[ "$ref" =~ ^[a-zA-Z0-9_-]+(:[a-zA-Z0-9_-]+)*(/[a-zA-Z0-9_-]+)?(@[0-9][^[:space:]]*)?$ ]] || return 0

    find_interface_file "$da_root" "$pkg" "$ref" > /dev/null && return 0

    if murmur_rel="$(find_interface_file "$MURMUR_WIT/$subtree" "$pkg" "$ref")"; then
        hint="copy $MURMUR_WIT/$subtree/$murmur_rel -> wit/$subtree/$murmur_rel"
    else
        hint="no file under $MURMUR_WIT/$subtree/ defines it either — check the reference"
    fi
    echo "UNRESOLVED   $subtree/$rel: world '$world' references '$ref', which no vendored file defines"
    echo "             $hint"
    unresolved=$((unresolved + 1))
}

compare_subtree "guest"
compare_subtree "hook"

check_subtree_closure "guest"
check_subtree_closure "hook"

echo ""
echo "Results:"
printf "  %2d  matching\n"         "$matching"
printf "  %2d  drifted        (DRIFT — content differs; update whichever side is stale)\n" "$drift"
printf "  %2d  only-in-da     (ONLY_DA — default-artifacts has this, murmur does not)\n"  "$only_da"
printf "  %2d  only-in-murmur (ONLY_MURMUR — murmur has this; informational unless flagged UNRESOLVED above)\n" "$only_murmur"
printf "  %2d  unresolved     (UNRESOLVED — a vendored world references an interface no vendored file defines)\n" "$unresolved"

if [ "$drift" -gt 0 ] || [ "$only_da" -gt 0 ] || [ "$unresolved" -gt 0 ]; then
    echo ""
    echo "error: WIT files are out of sync. Resolve DRIFT, ONLY_DA and UNRESOLVED entries above." >&2
    exit 1
fi

echo ""
echo "OK: all artifact-facing WIT files are in sync and every vendored world resolves."
