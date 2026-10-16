#!/usr/bin/env bash
# Pack every artifact list-artifacts.sh reports into one release directory.
#
# One package-artifact.sh call per row: an untagged zip for each wasm artifact and
# skill, and one platform-tagged zip per native tool per platform. The result is
# exactly the set of .mur.zip assets a release uploads, ready for index-release.sh.
#
# This compiles nothing. Build first:
#
#   ./scripts/build-wasm.sh
#   cargo build --release -p <crate> ...   one -p per classify-crates.sh
#                                          `native-artifact` row
#
# Native payloads come from one of two places:
#
#   --native-dir <dir>   <dir>/<platform>/<name> for every platform subdirectory of
#                        <dir>, which is the layout build.yml downloads each
#                        platform's binaries into. Every native tool must be present
#                        in every platform subdirectory; a missing one is refused
#                        before anything is packed, so a release never ships a
#                        platform for some tools and not others.
#   (absent)             target/release/<name>, packed for the host platform.
#
# Usage:   scripts/package-release.sh <out-dir> [--native-dir <dir>]
#          <out-dir> must be absent or empty.
# Output:  one line per zip written, then a count.
# Exit:    0 — every artifact packed
#          1 — refused, or an artifact failed to pack; <out-dir> is emptied again
#          2 — usage error
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    echo "usage: $0 <out-dir> [--native-dir <dir>]" >&2
    exit 2
}

OUT_DIR="" NATIVE_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --native-dir) [ $# -ge 2 ] || usage; NATIVE_DIR="$2"; shift 2 ;;
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
natives="$(printf '%s\n' "$rows" | awk -F'\t' '$5 == "native" { print $2 }')"

# The platforms to pack native tools for, each checked before anything is written.
platforms=()
if [ -n "$NATIVE_DIR" ]; then
    if [ ! -d "$NATIVE_DIR" ]; then
        echo "error: --native-dir $NATIVE_DIR is not a directory" >&2
        exit 1
    fi
    while IFS= read -r platform; do
        [ -n "$platform" ] || continue
        case " $PLATFORMS " in
            *" $platform "*) ;;
            *) echo "error: $NATIVE_DIR/$platform is not a platform; expected subdirectories named from: $PLATFORMS" >&2; exit 1 ;;
        esac
        platforms+=("$platform")
    done < <(find "$NATIVE_DIR" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; | LC_ALL=C sort)
    if [ "${#platforms[@]}" -eq 0 ]; then
        echo "error: --native-dir $NATIVE_DIR holds no platform subdirectory" >&2
        exit 1
    fi
    missing=0
    for platform in "${platforms[@]}"; do
        while IFS= read -r name; do
            [ -n "$name" ] || continue
            if [ ! -f "$NATIVE_DIR/$platform/$name" ]; then
                echo "error: native tool '$name' has no $platform binary at $NATIVE_DIR/$platform/$name" >&2
                missing=1
            fi
        done <<< "$natives"
    done
    [ "$missing" -eq 0 ] || exit 1
fi

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

count=0
while IFS=$'\t' read -r dir name _version _runtime impl; do
    if [ "$impl" = "native" ] && [ "${#platforms[@]}" -gt 0 ]; then
        for platform in "${platforms[@]}"; do
            bash "$SCRIPT_DIR/package-artifact.sh" "$REPO_ROOT/$dir" "$OUT_DIR" \
                --platform "$platform" --payload "$NATIVE_DIR/$platform/$name"
            count=$((count + 1))
        done
    else
        bash "$SCRIPT_DIR/package-artifact.sh" "$REPO_ROOT/$dir" "$OUT_DIR"
        count=$((count + 1))
    fi
done <<< "$rows"

echo "Packed $count .mur.zip file(s) into $OUT_DIR."
