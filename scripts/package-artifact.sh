#!/usr/bin/env bash
# Pack one artifact into the .mur.zip a release publishes, with `mur build`.
#
# The zip's name, `<name>-<version>[-<platform>].mur.zip`, is the one `mur index`
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
#   native  the host binary, by default target/release/<name>. murmur.yaml and
#           bin/<name> (mode 0755) are staged; the runtime reads a native payload
#           from bin/<artifact name>. Always platform-tagged.
#
# This script compiles nothing: it refuses a missing payload with the cargo
# command that builds it.
#
# Usage:   scripts/package-artifact.sh <artifact-dir> <out-dir> [--platform <p>] [--payload <file>]
#            --platform  native only: darwin-aarch64, darwin-x86_64, linux-aarch64 or
#                        linux-x86_64. Default: the host's.
#            --payload   the .wasm or binary to pack instead of the default path.
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
    echo "usage: $0 <artifact-dir> <out-dir> [--platform <p>] [--payload <file>]" >&2
    exit 2
}

ARTIFACT_DIR="" OUT_DIR="" PLATFORM="" PAYLOAD=""
while [ $# -gt 0 ]; do
    case "$1" in
        --platform) [ $# -ge 2 ] || usage; PLATFORM="$2"; shift 2 ;;
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
        native) KIND=native ;;
        wasm)   KIND=wasm ;;
        *) echo "error: $MANIFEST declares 'implementation: $IMPLEMENTATION'; expected 'native' or 'wasm'" >&2; exit 1 ;;
    esac
fi

# ---- the payload and the zip name, all checked before anything is written ----
case "$KIND" in
    skill|wasm)
        if [ -n "$PLATFORM" ]; then
            echo "error: $NAME is a $KIND artifact and ships one untagged payload; --platform applies to native tools only" >&2
            exit 1
        fi
        ZIP_NAME="$NAME-$VERSION.mur.zip"
        ;;
    native)
        if [ -z "$PLATFORM" ]; then
            case "$(uname -s)-$(uname -m)" in
                Darwin-arm64)  PLATFORM=darwin-aarch64 ;;
                Darwin-x86_64) PLATFORM=darwin-x86_64 ;;
                Linux-aarch64) PLATFORM=linux-aarch64 ;;
                Linux-x86_64)  PLATFORM=linux-x86_64 ;;
                *)
                    echo "error: cannot map host '$(uname -s)-$(uname -m)' to a platform; pass --platform with one of: $PLATFORMS" >&2
                    exit 1 ;;
            esac
        fi
        case " $PLATFORMS " in
            *" $PLATFORM "*) ;;
            *) echo "error: unknown platform '$PLATFORM'; expected one of: $PLATFORMS" >&2; exit 1 ;;
        esac
        ZIP_NAME="$NAME-$VERSION-$PLATFORM.mur.zip"
        ;;
esac

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

if [ "$KIND" = "native" ]; then
    PAYLOAD="${PAYLOAD:-$REPO_ROOT/target/release/$NAME}"
    if [ ! -f "$PAYLOAD" ]; then
        echo "error: no binary at $PAYLOAD; build it with:" >&2
        echo "  cargo build --release -p $NAME" >&2
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
    native)
        SRC="$STAGE/$NAME"
        mkdir -p "$SRC/bin"
        cp "$MANIFEST" "$SRC/murmur.yaml"
        cp "$PAYLOAD" "$SRC/bin/$NAME"
        chmod 0755 "$SRC/bin/$NAME"
        ;;
esac

if ! "$MUR" build "$SRC" -o "$OUT" >&2; then
    rm -f "$OUT"
    echo "error: mur build failed for $NAME (see above)" >&2
    exit 1
fi
echo "$OUT"
