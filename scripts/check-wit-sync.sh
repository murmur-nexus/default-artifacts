#!/usr/bin/env bash
# Check that the artifact-facing WIT files (wit/guest, wit/hook and wit/process-driver) are a
# byte-identical copy of murmur's crates/capsule-runtime/wit at the pinned murmur commit.
#
# Only those subtrees (WIT_SUBTREES in lib/common.sh) are compared — they are the directories
# wit_bindgen::generate! reads in every WASM artifact. murmur's other top-level WIT files (host/,
# host.wit, worlds.wit, VERSIONING.md, ...) are not mirrored.
#
# The vendored subtrees are not a freely-trimmable subset: each one must be closed under the
# imports and exports its own world declarations name. A world that references an interface no
# vendored file defines does not resolve, and the failure surfaces later and less clearly as a
# wit_bindgen::generate! error during cargo build. So every interface reference is resolved to
# the vendored file expected to define it, and an unresolved one is a hard failure (UNRESOLVED).
# A file murmur has that no vendored world references is still informational (ONLY_MURMUR), as
# guest/deps/murmur-shell/shell-execute.wit legitimately is.
#
# Run from anywhere inside a default-artifacts checkout or one of its worktrees. The compared
# mirror is always the wit/ of the checkout this script belongs to.
#
# The pinned commit is MURMUR_REV in .github/workflows/build.yml, read with murmur-pin.sh. Its
# WIT is read with `git archive` from a murmur git checkout, whatever that checkout has checked
# out. The checkout is found by one rule:
#   MURMUR_DIR set and non-empty — that directory is the murmur root, and the only place looked.
#   otherwise                    — the directory `murmur` beside the main checkout, where the
#                                  main checkout is the parent of `git rev-parse --git-common-dir`.
#                                  A plain checkout and each of its worktrees share that answer,
#                                  and so does CI's $GITHUB_WORKSPACE/{default-artifacts,murmur}.
# The root must be the toplevel of a git checkout that holds the pinned commit; an exported tree
# is refused. This script never fetches: run `git -C <murmur root> fetch origin <commit>`.
#
# To move the pin and the mirror together, run ./scripts/murmur-pin.sh --set <commit>.
#
# Exit codes:
#   0 — all shared files match and every vendored world resolves
#   1 — a shared file has drifted, default-artifacts has a file murmur does not, or a vendored
#       world references an interface no vendored file defines
#   2 — the pin could not be read, murmur was not found or is not a git checkout, the pinned
#       commit is not in it, or it has no crates/capsule-runtime/wit; nothing was compared
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DA_WIT="$(cd "$SCRIPT_DIR/.." && pwd)/wit"
. "$SCRIPT_DIR/lib/common.sh"

case "${1:-}" in
    -h | --help)
        awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    "") ;;
    *)
        echo "error: unknown argument '$1'" >&2
        exit 2
        ;;
esac

MURMUR_REV="$("$SCRIPT_DIR/murmur-pin.sh")" || {
    echo "error: could not read the murmur pin; nothing was compared" >&2
    exit 2
}

locate_murmur_root "$SCRIPT_DIR"
require_murmur_checkout
if ! git -C "$MURMUR_ROOT" cat-file -e "$MURMUR_REV^{commit}" 2>/dev/null; then
    {
        echo "error: murmur commit $MURMUR_REV (MURMUR_REV in .github/workflows/build.yml) is not in $MURMUR_ROOT"
        echo "  $MURMUR_ROOT_WHY"
        echo "  Fetch it with: git -C $MURMUR_ROOT fetch origin $MURMUR_REV"
    } >&2
    exit 2
fi
MURMUR_SHORT="$(git -C "$MURMUR_ROOT" rev-parse --short "$MURMUR_REV")"

EXPORT_DIR="$(mktemp -d)"
trap 'rm -rf "$EXPORT_DIR"' EXIT
if [ -z "$(git -C "$MURMUR_ROOT" ls-tree -d --name-only "$MURMUR_REV" "$MURMUR_WIT_PATH")" ]; then
    echo "error: murmur $MURMUR_SHORT has no $MURMUR_WIT_PATH; nothing was compared" >&2
    exit 2
fi
git -C "$MURMUR_ROOT" archive "$MURMUR_REV" "$MURMUR_WIT_PATH" | tar -x -C "$EXPORT_DIR"
MURMUR_WIT="$EXPORT_DIR/$MURMUR_WIT_PATH"

echo "Comparing against murmur $MURMUR_SHORT (MURMUR_REV in .github/workflows/build.yml) from $MURMUR_ROOT"

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
#
# A `%` before a name is WIT keyword escaping: `package murmur:%stream@0.1.0;` declares the
# package `murmur:stream`. The reference is expected unescaped (check_interface_ref strips it),
# and a declaration matches with or without the `%`.
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

    pkg_re="${want_pkg//./\\.}"
    pkg_re="^[[:space:]]*package[[:space:]]+%?${pkg_re//:/:%?}"
    if [[ "$want_pkg" == *@* ]]; then
        pkg_re="$pkg_re[[:space:]]*;"
    else
        # Reference carries no version, so accept the package at any version.
        pkg_re="$pkg_re(@[^;[:space:]]*)?[[:space:]]*;"
    fi
    iface_re="^[[:space:]]*interface[[:space:]]+%?${want_iface}[[:space:]]*\{"

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
        pkg="${pkg//%/}"

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
    local da_root="$DA_WIT/$subtree" hint murmur_rel name

    # Resolve on the unescaped name (`murmur:%stream/events` is `murmur:stream/events`); report
    # the reference as the world writes it.
    name="${ref//:%/:}"
    name="${name//\/%//}"
    [[ "$name" == %* ]] && name="${name#%}"

    # `import name: func(...)` and inline interface blocks are not interface references.
    [[ "$name" =~ ^[a-zA-Z0-9_-]+(:[a-zA-Z0-9_-]+)*(/[a-zA-Z0-9_-]+)?(@[0-9][^[:space:]]*)?$ ]] || return 0

    find_interface_file "$da_root" "$pkg" "$name" > /dev/null && return 0

    if murmur_rel="$(find_interface_file "$MURMUR_WIT/$subtree" "$pkg" "$name")"; then
        hint="copy murmur $MURMUR_SHORT:$MURMUR_WIT_PATH/$subtree/$murmur_rel -> wit/$subtree/$murmur_rel"
        hint="$hint (git -C $MURMUR_ROOT show $MURMUR_SHORT:$MURMUR_WIT_PATH/$subtree/$murmur_rel > wit/$subtree/$murmur_rel),"
        hint="$hint or re-mirror every subtree with ./scripts/murmur-pin.sh --set $MURMUR_REV"
    else
        hint="no file under $MURMUR_WIT_PATH/$subtree/ at murmur $MURMUR_SHORT defines it either — check the reference"
    fi
    echo "UNRESOLVED   $subtree/$rel: world '$world' references '$ref', which no vendored file defines"
    echo "             $hint"
    unresolved=$((unresolved + 1))
}

for subtree in $WIT_SUBTREES; do
    compare_subtree "$subtree"
done
for subtree in $WIT_SUBTREES; do
    check_subtree_closure "$subtree"
done

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
