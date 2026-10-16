#!/usr/bin/env bash
# Write a release's artifacts-index.json with `mur index`, then hold it to the bytes gate.
#
# The index is schema 2, scope `murmur` (the scope `mur install murmur/<name>@<version>`
# resolves to this repository), and lists every version of every artifact: the ones
# packed into <release-dir> by this release, merged with every version the previous
# release's index already carried. `mur install` reads the index of the latest
# release only, so a version not carried forward here stops being installable.
#
# A version is a promise about bytes. `mur install` writes a payload's sha256 into
# the consumer's murmur.lock and a store that holds <name>/<version> never fetches
# it again, so a version republished with different bytes does not upgrade anyone:
# it fails their next resolve on the pin, with no version to move to. `mur index
# --merge` replaces such a version silently, so the gate is here:
#
#   bytes gate      every name@version in both the previous index and the new one
#                   must carry the identical asset set — each file and its sha256.
#                   A mismatch is the artifact's payload changing at an unchanged
#                   version; the fix is to bump `version:` in its murmur.yaml.
#   level gate      every version must carry `built_at_level`. A version without
#                   one fits no WIT level of the `mur` that indexed it, and an
#                   explicit `mur install` of it is refused.
#
# On either failure the new artifacts-index.json is deleted, so a release that runs
# this before uploading uploads no index that breaks a promise.
#
# Running it again over the same assets, with --previous set to its own output,
# reproduces that output byte for byte: `mur index` sorts everything and writes no
# timestamp.
#
# Usage:   scripts/index-release.sh <release-dir> --release <tag> [--previous <index.json>]
#            --release   the tag the assets in <release-dir> are uploaded to.
#            --previous  the latest published release's artifacts-index.json.
#                        Omitted only for the first release that has none.
# Output:  mur index's summary line, then the gate's verdict.
#          Writes <release-dir>/artifacts-index.json.
# Exit:    0 — index written and both gates passed
#          1 — a gate failed or mur index refused; no index is left behind
#          2 — usage error
#
# `mur` is $MUR_BIN, else `mur` on PATH, and must have the `index` subcommand.
# Requires `jq`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="murmur-nexus/default-artifacts"

usage() {
    echo "usage: $0 <release-dir> --release <tag> [--previous <index.json>]" >&2
    exit 2
}

RELEASE_DIR="" TAG="" PREVIOUS=""
while [ $# -gt 0 ]; do
    case "$1" in
        --release)  [ $# -ge 2 ] || usage; TAG="$2"; shift 2 ;;
        --previous) [ $# -ge 2 ] || usage; PREVIOUS="$2"; shift 2 ;;
        -*) echo "error: unknown option '$1'" >&2; usage ;;
        *) [ -z "$RELEASE_DIR" ] || usage; RELEASE_DIR="$1"; shift ;;
    esac
done
[ -n "$RELEASE_DIR" ] && [ -n "$TAG" ] || usage

MUR="${MUR_BIN:-$(command -v mur || true)}"
if [ -z "$MUR" ] || ! "$MUR" index --help >/dev/null 2>&1; then
    echo "error: need a 'mur' with the 'index' subcommand (tried: ${MUR:-<none on PATH>})." >&2
    echo "       Build one from murmur at the MURMUR_REV in .github/workflows/build.yml and set MUR_BIN to it." >&2
    exit 1
fi
command -v jq >/dev/null || { echo "error: jq is required" >&2; exit 1; }

if [ ! -d "$RELEASE_DIR" ]; then
    echo "error: $RELEASE_DIR is not a directory" >&2
    exit 1
fi
INDEX="$RELEASE_DIR/artifacts-index.json"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The previous index is copied aside first: --previous may name the very file this
# run overwrites, and the gate needs the published one after the merge.
merge=()
if [ -n "$PREVIOUS" ]; then
    if ! jq -e . "$PREVIOUS" >/dev/null 2>&1; then
        echo "error: --previous $PREVIOUS is not a readable JSON file" >&2
        exit 1
    fi
    cp "$PREVIOUS" "$WORK/previous.json"
    merge=(--merge "$WORK/previous.json")
    echo "Merging the previous index $PREVIOUS (copied to $WORK/previous.json)."
fi

if ! "$MUR" index "$RELEASE_DIR" --repo "$REPO" --release "$TAG" ${merge[@]+"${merge[@]}"} -o "$INDEX"; then
    rm -f "$INDEX"
    echo "error: mur index refused the assets in $RELEASE_DIR (see above)" >&2
    exit 1
fi

# The directory an artifact lives in, for the fix the gate names. Falls back to a
# bare name when the artifact is not in this checkout (one only an older release
# carried, say).
artifact_dir() {
    bash "$SCRIPT_DIR/list-artifacts.sh" 2>/dev/null | awk -F'\t' -v n="$1" '$2 == n { print $1; found = 1 } END { if (!found) print "<category>/" n }'
}

fail=0

# ---- bytes gate -------------------------------------------------------------------
# One line per file of a version present on both sides whose sha256 differs or that
# only one side carries: name, version, file, published sha256, new sha256.
if [ -n "$PREVIOUS" ]; then
    mismatches="$(jq -r -n --slurpfile old "$WORK/previous.json" --slurpfile new "$INDEX" '
        def files($idx):
            [ $idx.artifacts[] as $a | $a.versions[] as $v
              | { key: "\($a.name)@\($v.version)",
                  value: ($v.assets | map({ (.file): .sha256 }) | add // {}) } ]
            | from_entries;
        files($old[0]) as $o | files($new[0]) as $n
        | ($o | keys[]) as $k
        | select($n | has($k))
        | select($o[$k] != $n[$k])
        | (($o[$k] | keys) + ($n[$k] | keys) | unique[]) as $f
        | select($o[$k][$f] != $n[$k][$f])
        | ($k | split("@")) as [$name, $version]
        | [$name, $version, $f, ($o[$k][$f] // "(absent)"), ($n[$k][$f] // "(absent)")]
        | @tsv
    ')"
    if [ -n "$mismatches" ]; then
        fail=1
        echo "error: a published version would be republished with different bytes:" >&2
        last=""
        while IFS=$'\t' read -r name version file old new; do
            if [ "$name@$version" != "$last" ]; then
                [ -z "$last" ] || echo "    bump its version in $(artifact_dir "${last%@*}")/murmur.yaml" >&2
                echo "  $name@$version" >&2
                last="$name@$version"
            fi
            echo "    $file: published sha256 $old, new sha256 $new" >&2
        done <<< "$mismatches"
        echo "    bump its version in $(artifact_dir "${last%@*}")/murmur.yaml" >&2
    fi
fi

# ---- level gate -------------------------------------------------------------------
unlevelled="$(jq -r '.artifacts[] as $a | $a.versions[] | select(has("built_at_level") | not) | "\($a.name)@\(.version)"' "$INDEX")"
if [ -n "$unlevelled" ]; then
    fail=1
    echo "error: these versions carry no built_at_level, so an explicit 'mur install' of them is refused:" >&2
    while IFS= read -r v; do echo "  $v" >&2; done <<< "$unlevelled"
    echo "  Their interfaces fit no WIT level of $("$MUR" version | head -1); rebuild them against this repo's wit/ mirror." >&2
fi

if [ "$fail" -ne 0 ]; then
    rm -f "$INDEX"
    echo "error: $INDEX deleted; nothing from this run may be published" >&2
    exit 1
fi

echo "Bytes gate passed: no published version changed its assets. Every version carries built_at_level."
echo "Wrote $INDEX"
