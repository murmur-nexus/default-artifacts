#!/usr/bin/env bash
# Print, or move, the one murmur commit this repository is pinned to.
#
# The pin is the `  MURMUR_REV: <commit>` line under the top-level `env:` of
# .github/workflows/build.yml. The release builds `mur` from it, ci.yml's
# build-isolation job builds the same `mur`, and the WIT mirror (wit/guest,
# wit/hook, wit/process-driver) is a byte-identical copy of murmur's
# crates/capsule-runtime/wit at it. This script is the only reader of that line.
#
# Usage:
#   ./scripts/murmur-pin.sh                 print the pinned commit
#   ./scripts/murmur-pin.sh --set <commit>  move the pin and the mirror to <commit>
#   ./scripts/murmur-pin.sh --help          print this text
#
# With no arguments, prints the 40-hex value of the one line in build.yml that
# matches `^  MURMUR_REV: *[0-9a-f]{40}[[:space:]]*$`, and nothing else.
#
# --set <commit> takes a full 40-hex object name. Get one for a release with
#   git ls-remote https://github.com/murmur-nexus/murmur.git refs/tags/v<x.y.z>
# which prints the tag object of an annotated tag; --set peels it to the commit
# it points at and writes the commit. In order, --set:
#   1. refuses a <commit> that is not 40 lowercase hex, and a working tree with
#      uncommitted changes under wit/ or to build.yml;
#   2. finds the murmur checkout (MURMUR_DIR, else `murmur` beside the main
#      checkout — the rule check-wit-sync.sh uses), which must be a git checkout;
#   3. fetches <commit> from murmur's origin once if the checkout lacks it;
#   4. exports crates/capsule-runtime/wit at the commit with `git archive`, and
#      requires each mirrored subtree to be in it;
#   5. replaces wit/guest, wit/hook and wit/process-driver wholesale with the
#      export, rewrites only the MURMUR_REV line of build.yml, and stages the
#      mirror (`git add -A`) so check-version-drift.sh sees added files;
#   6. runs check-wit-sync.sh against the new pin;
#   7. runs check-version-drift.sh and says which artifacts need a minor bump;
#   8. prints the old and new pin and the wit/ files that changed.
# Commit the MURMUR_REV line, the mirror and the version bumps together.
#
# Exit codes:
#   0 — printed the pin; or --set moved (or re-mirrored) it and wit-sync passed
#   1 — build.yml has no MURMUR_REV line or more than one (no-argument form); or
#       check-wit-sync.sh failed after --set wrote, leaving the changes in place
#   2 — usage error, or --set refused before writing: bad <commit>, dirty tree,
#       no murmur checkout, commit not found, subtree missing, unreadable pin;
#       or check-version-drift.sh exited 2
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKFLOW_REL=".github/workflows/build.yml"
WORKFLOW="$REPO_ROOT/$WORKFLOW_REL"
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
}

# Echo the pinned commit, or print why there is not exactly one and return 1.
read_pin() {
    local revs count
    revs="$(sed -n 's/^  MURMUR_REV: *\([0-9a-f]\{40\}\)[[:space:]]*$/\1/p' "$WORKFLOW" 2>/dev/null)" || true
    count="$(printf '%s' "$revs" | grep -c . || true)"
    if [ "$count" -eq 0 ]; then
        echo "error: $WORKFLOW has no '  MURMUR_REV: <40-hex SHA>' line; expected exactly one" >&2
        return 1
    fi
    if [ "$count" -ne 1 ]; then
        echo "error: $WORKFLOW has $count '  MURMUR_REV: <40-hex SHA>' lines ($(printf '%s' "$revs" | paste -sd ' ' -)); expected exactly one" >&2
        return 1
    fi
    echo "$revs"
}

set_pin() {
    local want="$1" old dirty new tmp export_wit subtree drift_out drift_rc changed

    if ! [[ "$want" =~ ^[0-9a-f]{40}$ ]]; then
        echo "error: --set needs a full 40-character lowercase hex murmur commit, got '$want'" >&2
        echo "  Get one with: git ls-remote https://github.com/murmur-nexus/murmur.git refs/tags/v<x.y.z>" >&2
        exit 2
    fi
    old="$(read_pin)" || exit 2

    dirty="$(git -C "$REPO_ROOT" status --porcelain -- wit "$WORKFLOW_REL")"
    if [ -n "$dirty" ]; then
        echo "error: uncommitted changes under wit/ or to $WORKFLOW_REL; commit or discard them first:" >&2
        echo "$dirty" | sed 's/^/  /' >&2
        exit 2
    fi

    locate_murmur_root "$SCRIPT_DIR"
    require_murmur_checkout

    if ! git -C "$MURMUR_ROOT" cat-file -e "$want" 2>/dev/null; then
        echo "Fetching $want into $MURMUR_ROOT"
        git -C "$MURMUR_ROOT" fetch --quiet origin "$want" 2>/dev/null || true
        if ! git -C "$MURMUR_ROOT" cat-file -e "$want" 2>/dev/null; then
            echo "error: murmur commit $want is not in $MURMUR_ROOT, and \`git fetch origin $want\` did not bring it" >&2
            exit 2
        fi
    fi
    if ! new="$(git -C "$MURMUR_ROOT" rev-parse --verify --quiet "$want^{commit}")"; then
        echo "error: $want in $MURMUR_ROOT is not a commit, nor a tag pointing at one" >&2
        exit 2
    fi
    [ "$new" = "$want" ] || echo "Peeled tag $want to commit $new"

    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064 # expand now: tmp is local
    trap "rm -rf '$tmp'" EXIT
    git -C "$MURMUR_ROOT" archive "$new" "$MURMUR_WIT_PATH" | tar -x -C "$tmp"
    export_wit="$tmp/$MURMUR_WIT_PATH"
    for subtree in $WIT_SUBTREES; do
        if [ ! -d "$export_wit/$subtree" ]; then
            echo "error: murmur $new has no $MURMUR_WIT_PATH/$subtree; nothing was written" >&2
            exit 2
        fi
    done

    for subtree in $WIT_SUBTREES; do
        rm -rf "${REPO_ROOT:?}/wit/$subtree"
        cp -R "$export_wit/$subtree" "$REPO_ROOT/wit/$subtree"
    done
    # Only the matched line changes; sed passes every other byte through.
    sed "s/^  MURMUR_REV: *[0-9a-f]\{40\}[[:space:]]*$/  MURMUR_REV: $new/" "$WORKFLOW" > "$tmp/build.yml"
    cat "$tmp/build.yml" > "$WORKFLOW"
    # shellcheck disable=SC2086 # WIT_SUBTREES is a word list
    (cd "$REPO_ROOT" && git add -A -- $(printf 'wit/%s ' $WIT_SUBTREES))
    echo "Mirrored wit/{${WIT_SUBTREES// /,}} from murmur $new and staged them (git add -A), so check-version-drift.sh sees added files."
    echo "Set MURMUR_REV in $WORKFLOW_REL; it is not staged."

    echo ""
    if ! "$SCRIPT_DIR/check-wit-sync.sh"; then
        echo "error: check-wit-sync.sh failed against murmur $new; the changes are left in place for inspection" >&2
        exit 1
    fi

    echo ""
    echo "check-version-drift.sh:"
    drift_rc=0
    drift_out="$(cd "$REPO_ROOT" && "$SCRIPT_DIR/check-version-drift.sh" 2>&1)" || drift_rc=$?
    echo "$drift_out"
    case "$drift_rc" in
        0) echo "No artifact needs a version bump." ;;
        1) echo "The artifacts listed above need a minor \`version:\` bump in their murmur.yaml before tagging." ;;
        *)
            echo "error: check-version-drift.sh exited $drift_rc" >&2
            exit 2
            ;;
    esac

    echo ""
    if [ "$old" = "$new" ]; then
        echo "MURMUR_REV unchanged ($new)"
    else
        echo "MURMUR_REV $old → $new"
    fi
    changed="$(git -C "$REPO_ROOT" status --short -- wit)"
    if [ -n "$changed" ]; then
        echo "wit/ files changed:"
        echo "$changed" | sed 's/^/  /'
    else
        echo "wit/ files changed: none"
    fi
}

case "${1:-}" in
    "")
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        read_pin || exit 1
        ;;
    -h | --help)
        usage
        ;;
    --set)
        if [ "$#" -ne 2 ]; then
            echo "error: --set takes exactly one <commit>" >&2
            exit 2
        fi
        set_pin "$2"
        ;;
    *)
        echo "error: unknown argument '$1'" >&2
        echo "  Usage: ./scripts/murmur-pin.sh [--set <commit> | --help]" >&2
        exit 2
        ;;
esac
