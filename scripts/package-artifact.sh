#!/usr/bin/env bash
# Pack one artifact into the .mur.zip a release publishes, with `mur build`.
#
# The zip's name, `<name>-<version>.mur.zip`, is the one `mur index`
# requires, and both values come from the artifact's own murmur.yaml — the only
# place they are written. `mur build` dates every entry 1980-01-01 and writes
# murmur.yaml, then each `requires_files:` entry in declared order, then
# murmur-build.json, so identical inputs give a byte-identical zip. That is what
# lets the release's bytes gate (index-release.sh) tell a real payload change
# from a repack.
#
# How the payload gets into the zip depends on the artifact's kind, as
# list-artifacts.sh reports it:
#
#   skill   `mur build <dir>` directly; murmur.yaml and skill.md are in the tree.
#   wasm    the component built for wasm32-wasip2, by default
#           target/wasm32-wasip2/release/<name with - as _>.wasm. The artifact
#           directory is copied to a scratch dir and the component is added under
#           the `*.wasm` name its `requires_files:` declares.
#
# This script compiles nothing: it refuses a missing payload with the cargo
# command that builds it.
#
# Usage:   scripts/package-artifact.sh <artifact-dir> <out-dir> [--payload <file>]
#            --payload   the .wasm to pack instead of the default path.
# Output:  the path of the written zip, on stdout. `mur build`'s own output goes
#          to stderr.
# Exit:    0 — packed
#          1 — refused or `mur build` failed; nothing is written either way
#          2 — usage error
#
# `mur` is $MUR_BIN, else `mur` on PATH, and must have the `index` subcommand —
# the same `mur` the release indexes with.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    echo "usage: $0 <artifact-dir> <out-dir> [--payload <file>]" >&2
    exit 2
}

ARTIFACT_DIR="" OUT_DIR="" PAYLOAD=""
while [ $# -gt 0 ]; do
    case "$1" in
        --payload)  [ $# -ge 2 ] || usage; PAYLOAD="$2"; shift 2 ;;
        -*) echo "error: unknown option '$1'" >&2; usage ;;
        *)
            if   [ -z "$ARTIFACT_DIR" ]; then ARTIFACT_DIR="$1"
            elif [ -z "$OUT_DIR" ];      then OUT_DIR="$1"
            else usage
            fi
            shift ;;
    esac
done
[ -n "$ARTIFACT_DIR" ] && [ -n "$OUT_DIR" ] || usage

require_mur

MANIFEST="$ARTIFACT_DIR/murmur.yaml"
if [ ! -f "$MANIFEST" ]; then
    echo "error: $MANIFEST not found" >&2
    exit 1
fi

NAME="$(yaml_scalar "$MANIFEST" name)"
VERSION="$(yaml_scalar "$MANIFEST" version)"
RUNTIME="$(yaml_scalar "$MANIFEST" runtime)"
IMPLEMENTATION="$(yaml_scalar "$MANIFEST" implementation)"
BASE="$(basename "$(cd "$ARTIFACT_DIR" && pwd)")"

if [ -z "$NAME" ] || [ -z "$VERSION" ]; then
    echo "error: $MANIFEST must declare both 'name:' and 'version:'" >&2
    exit 1
fi
if [ "$NAME" != "$BASE" ]; then
    echo "error: $MANIFEST declares 'name: $NAME', but its directory is '$BASE'; the directory must be named after the artifact" >&2
    exit 1
fi

if [ "$RUNTIME" = "skill" ]; then
    KIND=skill
else
    case "${IMPLEMENTATION:-wasm}" in
        wasm) KIND=wasm ;;
        *) echo "error: $MANIFEST declares 'implementation: $IMPLEMENTATION'; expected 'wasm'" >&2; exit 1 ;;
    esac
fi

# ---- the payload and the zip name, all checked before anything is written ----
ZIP_NAME="$NAME-$VERSION.mur.zip"

if [ "$KIND" = "skill" ] && [ -n "$PAYLOAD" ]; then
    echo "error: $NAME is a skill; it has no payload to pass with --payload" >&2
    exit 1
fi

if [ "$KIND" = "wasm" ]; then
    PAYLOAD="${PAYLOAD:-$REPO_ROOT/target/wasm32-wasip2/release/${NAME//-/_}.wasm}"
    if [ ! -f "$PAYLOAD" ]; then
        echo "error: no component at $PAYLOAD; build it with:" >&2
        echo "  cargo build -p $NAME --target wasm32-wasip2 --release" >&2
        exit 1
    fi
    # The `*.wasm` entry of the `requires_files:` block list; mur build packs exactly one.
    WASM_ENTRY="$(awk '
        /^requires_files:/              { inside = 1; next }
        inside && /^[^[:space:]-]/      { inside = 0 }
        inside && /^[[:space:]]*- /     { sub(/^[[:space:]]*- /, ""); gsub(/["'"'"']/, ""); print }
    ' "$MANIFEST" | grep '\.wasm$' | head -1 || true)"
    if [ -z "$WASM_ENTRY" ]; then
        echo "error: $MANIFEST lists no '*.wasm' entry under 'requires_files:'" >&2
        exit 1
    fi
fi

# ---- stage and pack -------------------------------------------------------------
mkdir -p "$OUT_DIR"
OUT="$(cd "$OUT_DIR" && pwd)/$ZIP_NAME"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

case "$KIND" in
    skill)
        SRC="$ARTIFACT_DIR"
        ;;
    wasm)
        SRC="$STAGE/$NAME"
        mkdir -p "$SRC"
        cp -R "$ARTIFACT_DIR/." "$SRC/"
        cp "$PAYLOAD" "$SRC/$WASM_ENTRY"
        ;;
esac

if ! "$MUR" build "$SRC" -o "$OUT" >&2; then
    rm -f "$OUT"
    echo "error: mur build failed for $NAME (see above)" >&2
    exit 1
fi
echo "$OUT"
