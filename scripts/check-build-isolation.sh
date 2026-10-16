#!/usr/bin/env bash
# Check that every compiled artifact packs the same bytes built alone as in the
# release build.
#
# The release's bytes gate (index-release.sh) refuses a version republished with
# different bytes. If an artifact's bytes depended on which other crates cargo
# built beside it — cargo unifies dependency features across one invocation — a
# release could fail that gate on an artifact nobody touched, and
# `cargo build -p <name>`, local-install.sh and the `wasm_component` tests would
# pack bytes the release does not. scripts/build-wasm.sh and
# scripts/build-native.sh therefore build each artifact in its own invocation, and
# this script is what proves it.
#
# Two sides, in one target dir:
#
#   release  scripts/build-wasm.sh --locked, then scripts/build-native.sh --locked,
#            exactly as build.yml runs them, always in full even for a subset;
#            then each selected artifact is packed with package-artifact.sh.
#   alone    per artifact, `cargo build -p <name> --target wasm32-wasip2 --release
#            --locked` (wasm) or `cargo build --release -p <name> --locked`
#            (native), packed straight after.
#
# The target dir is shared on purpose. Cargo keys every compiled unit by its
# resolved features, so a build that resolves the same graph reuses the release's
# units, and one that resolves a different graph rebuilds and overwrites the
# output. A second, fresh target dir would add path-embedding noise (OUT_DIR paths
# in panic locations) unrelated to feature unification. It is the one
# `cargo metadata` reports, so CARGO_TARGET_DIR is honoured, and each payload is
# passed to package-artifact.sh with --payload.
#
# Usage:   scripts/check-build-isolation.sh [<artifact-name>...]
#          No names: every wasm and native row of list-artifacts.sh.
# Output:  `ok    <name>  <zip file>  <sha256>` per artifact that matches, then a
#          count. Cargo's own output goes to stderr.
# Exit:    0 — every selected artifact packs identical bytes on both sides
#          1 — an artifact differs (each is named on stderr with both hashes), no
#              usable `mur`, or a build or pack failed
#          2 — usage error: an unknown option, a skill, or an unknown name
#
# `mur` is $MUR_BIN, else `mur` on PATH, and must have the `index` subcommand.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    echo "usage: $0 [<artifact-name>...]" >&2
    exit 2
}

names=""
for arg in "$@"; do
    case "$arg" in
        -*) echo "error: unknown option '$arg'" >&2; usage ;;
        *)  names="$names $arg" ;;
    esac
done

if ! artifacts="$(bash "$SCRIPT_DIR/list-artifacts.sh")"; then
    echo "error: scripts/list-artifacts.sh failed (see above)" >&2
    exit 1
fi

# The selected rows, as list-artifacts.sh prints them: path, name, version,
# runtime, kind.
if [ -z "$names" ]; then
    selected="$(printf '%s\n' "$artifacts" | awk -F'\t' '$5 == "wasm" || $5 == "native"')"
else
    selected=""
    for name in $names; do
        row="$(printf '%s\n' "$artifacts" | awk -F'\t' -v n="$name" '$2 == n')"
        if [ -z "$row" ]; then
            echo "error: no artifact named '$name'; ./scripts/list-artifacts.sh lists them" >&2
            exit 2
        fi
        case "$(printf '%s\n' "$row" | cut -f5)" in
            wasm|native) ;;
            skill) echo "error: $name is a skill; it compiles nothing" >&2; exit 2 ;;
            *)     echo "error: $name has an unknown kind in ./scripts/list-artifacts.sh" >&2; exit 2 ;;
        esac
        selected="$selected$row
"
    done
fi
count="$(printf '%s' "$selected" | grep -c . || true)"
if [ "$count" -eq 0 ]; then
    echo "error: ./scripts/list-artifacts.sh lists no wasm or native artifact" >&2
    exit 1
fi

require_mur
export MUR_BIN="$MUR"

TARGET_DIR="$(cd "$REPO_ROOT" && cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
if [ -z "$TARGET_DIR" ]; then
    echo "error: cargo metadata reported no target directory" >&2
    exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/release" "$TMP/alone"

sha256() {
    local sum
    sum="$(sha256sum "$1" 2>/dev/null || shasum -a 256 "$1")"
    echo "${sum%% *}"
}

payload() {
    case "$2" in
        wasm)   echo "$TARGET_DIR/wasm32-wasip2/release/$(printf '%s' "$1" | tr - _).wasm" ;;
        native) echo "$TARGET_DIR/release/$1" ;;
    esac
}

# pack <path> <name> <kind> <side>: the zip's path on stdout.
pack() {
    local zip
    if ! zip="$(bash "$SCRIPT_DIR/package-artifact.sh" "$REPO_ROOT/$1" "$TMP/$4" \
            --payload "$(payload "$2" "$3")" 2> "$TMP/pack.log")"; then
        cat "$TMP/pack.log" >&2
        echo "error: packing $2 from the $4 build failed: scripts/package-artifact.sh $1" >&2
        exit 1
    fi
    echo "$zip"
}

cd "$REPO_ROOT"

echo "Release build: scripts/build-wasm.sh --locked, then scripts/build-native.sh --locked" >&2
if ! bash "$SCRIPT_DIR/build-wasm.sh" --locked >&2; then
    echo "error: release build failed: scripts/build-wasm.sh --locked" >&2
    exit 1
fi
if ! bash "$SCRIPT_DIR/build-native.sh" --locked >&2; then
    echo "error: release build failed: scripts/build-native.sh --locked" >&2
    exit 1
fi

while IFS=$'\t' read -r path name _version _runtime kind; do
    [ -n "$path" ] || continue
    pack "$path" "$name" "$kind" release > /dev/null
done <<< "$selected"

mismatches=""
while IFS=$'\t' read -r path name _version _runtime kind; do
    [ -n "$path" ] || continue
    echo "Built alone: $name" >&2
    case "$kind" in
        wasm)   set -- cargo build -p "$name" --target wasm32-wasip2 --release --locked ;;
        native) set -- cargo build --release -p "$name" --locked ;;
    esac
    if ! "$@" >&2; then
        echo "error: building $name alone failed: $*" >&2
        exit 1
    fi
    alone_zip="$(pack "$path" "$name" "$kind" alone)"
    zip_file="$(basename "$alone_zip")"
    release_sha="$(sha256 "$TMP/release/$zip_file")"
    alone_sha="$(sha256 "$alone_zip")"
    if [ "$release_sha" = "$alone_sha" ]; then
        echo "ok    $name  $zip_file  $alone_sha"
    else
        mismatches="$mismatches  $name  $zip_file
    release build sha256 $release_sha
    built alone   sha256 $alone_sha
"
    fi
done <<< "$selected"

if [ -n "$mismatches" ]; then
    {
        echo "error: built alone and in the release build, these artifacts pack different bytes:"
        printf '%s' "$mismatches"
    } >&2
    exit 1
fi

echo "Build isolation holds: $count artifact(s) pack identical bytes alone and in the release build."
