#!/usr/bin/env bash
# Pack every artifact list-artifacts.sh reports into one release directory.
#
# One package-artifact.sh call per row: one `<name>-<version>.mur.zip` for each wasm
# artifact and skill. The result is exactly the set of .mur.zip assets a release
# uploads, ready for index-release.sh.
#
# With --previous, a row whose <name>@<version> that index already lists is skipped:
# the release packs only versions it adds. A published version's zip stays on the
# release that published it, and index-release.sh carries its index entry forward
# unchanged. Repacking it would not reproduce it: `mur build` writes the packing
# mur's WIT level into every zip (murmur-build.json → built_at_level), and an
# artifact that names no changed murmur:* package fits every level, so its label
# — and its bytes — move each time murmur adds a level.
# check-version-drift.sh is what catches a payload change at an unchanged version.
#
# This compiles nothing. Build first:
#
#   ./scripts/build-wasm.sh
#
# Usage:   scripts/package-release.sh <out-dir> [--previous <index.json>]
#          <out-dir> must be absent or empty.
#          --previous  the latest published release's artifacts-index.json, the same
#                      file index-release.sh is given. Omitted, every row is packed.
# Output:  one line per zip written, one per version skipped, then the counts.
#          Requires `jq` with --previous.
# Exit:    0 — every artifact not already published packed
#          1 — refused, or an artifact failed to pack; <out-dir> is emptied again
#          2 — usage error
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    echo "usage: $0 <out-dir> [--previous <index.json>]" >&2
    exit 2
}

OUT_DIR="" PREVIOUS=""
while [ $# -gt 0 ]; do
    case "$1" in
        --previous) [ $# -ge 2 ] || usage; PREVIOUS="$2"; shift 2 ;;
        -*) echo "error: unknown option '$1'" >&2; usage ;;
        *) [ -z "$OUT_DIR" ] || usage; OUT_DIR="$1"; shift ;;
    esac
done
[ -n "$OUT_DIR" ] || usage

if [ -e "$OUT_DIR" ] && { [ ! -d "$OUT_DIR" ] || [ -n "$(ls -A "$OUT_DIR")" ]; }; then
    echo "error: $OUT_DIR is not an empty directory; a release directory must hold only what this run packs" >&2
    exit 1
fi

require_mur
export MUR_BIN="$MUR"

rows="$(bash "$SCRIPT_DIR/list-artifacts.sh")"

# <name>\t<version>\t<release> for every version the previous index lists.
published=""
if [ -n "$PREVIOUS" ]; then
    if [ ! -f "$PREVIOUS" ]; then
        echo "error: --previous $PREVIOUS is not a file" >&2
        exit 1
    fi
    command -v jq > /dev/null || { echo "error: jq is required with --previous" >&2; exit 1; }
    published="$(jq -r '.artifacts[] | .name as $n | .versions[] | [$n, .version, .release] | @tsv' "$PREVIOUS")" || {
        echo "error: --previous $PREVIOUS is not an artifact index" >&2
        exit 1
    }
fi

# The release that published <name>@<version>, or nothing when no earlier one did.
published_in() {
    [ -n "$published" ] || return 0
    awk -F'\t' -v n="$1" -v v="$2" '$1 == n && $2 == v { print $3; exit }' <<< "$published"
}

# Split the rows into the ones to pack and the ones an earlier release published.
to_pack="" skipped=""
while IFS=$'\t' read -r dir name version runtime impl; do
    [ -n "$name" ] || continue
    release="$(published_in "$name" "$version")"
    if [ -n "$release" ]; then
        skipped+="$name@$version"$'\t'"$release"$'\n'
    else
        to_pack+="$dir"$'\t'"$name"$'\t'"$version"$'\t'"$runtime"$'\t'"$impl"$'\n'
    fi
done <<< "$rows"
rows="${to_pack%$'\n'}"

mkdir -p "$OUT_DIR"
# The directory was empty, so anything in it on failure is this run's partial output.
empty_on_failure() {
    local status=$?
    if [ "$status" -ne 0 ]; then
        find "$OUT_DIR" -mindepth 1 -delete
        echo "error: packing failed; $OUT_DIR emptied" >&2
    fi
}
trap empty_on_failure EXIT

while IFS=$'\t' read -r version release; do
    [ -n "$version" ] || continue
    echo "Skipped $version: published in $release"
done <<< "$skipped"

count=0
while IFS=$'\t' read -r dir name _version _runtime _impl; do
    [ -n "$name" ] || continue
    bash "$SCRIPT_DIR/package-artifact.sh" "$REPO_ROOT/$dir" "$OUT_DIR"
    count=$((count + 1))
done <<< "$rows"

echo "Packed $count .mur.zip file(s) into $OUT_DIR; skipped $(grep -c . <<< "$skipped" || true) version(s) an earlier release published."
