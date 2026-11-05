#!/usr/bin/env bash
# Install the pinned WASI C toolchain (wasi-sdk) and link it at <repo>/.wasi-sdk.
#
# wasm32-wasip2 artifacts that link C sources — today rusqlite's bundled SQLite, in
# tool-test-report — compile them with wasi-sdk's clang against its wasi-libc sysroot.
# .cargo/config.toml points CC_wasm32_wasip2 / AR_wasm32_wasip2 at .wasi-sdk/bin, so once
# this script has run, a plain `cargo build -p <name> --target wasm32-wasip2 --release`
# finds them.
#
# The release is pinned in wasi-sdk.toml at the repo root: its version, its GitHub release
# tag, and the sha256 of its tarball per platform. That file is the only place they are
# written; this script reads it with plain shell tools.
#
# The install lives in a cache shared by every checkout and worktree:
#
#   ${WASI_SDK_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/murmur-default-artifacts}/wasi-sdk-<version>-<arch>-<os>
#
# It counts as installed when bin/clang and bin/llvm-ar exist and the first line of its
# VERSION file starts with the pinned version, so a second run downloads nothing. A fresh
# install is downloaded and verified into a temporary file, extracted into a temporary
# directory, and only then moved into place, so an interrupted run never leaves a
# half-installed directory that a later run would take for a complete one.
#
# scripts/build-wasm.sh and scripts/local-install.sh run this before compiling wasm, and
# ci.yml runs it (with the cache directory cached) in each job that compiles wasm.
#
# Usage:   scripts/wasi-sdk.sh
#          Prints the install directory on stdout; progress goes to stderr.
# Exit:    0 — installed (or already installed) and linked
#          1 — no sha256 for this platform in wasi-sdk.toml, a malformed wasi-sdk.toml,
#              a failed download or extraction, or a sha256 mismatch
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PIN="$REPO_ROOT/wasi-sdk.toml"
. "$SCRIPT_DIR/lib/common.sh"

# A top-level `key = "value"` of wasi-sdk.toml (before any [table]).
toml_top() {
    awk -v key="$1" '
        /^\[/ { exit }
        $0 ~ "^" key "[[:space:]]*=" {
            sub("^[^=]*=[[:space:]]*\"", ""); sub("\".*$", ""); print; exit
        }' "$PIN"
}

# A `key = "value"` inside the [sha256] table of wasi-sdk.toml.
toml_sha256() {
    awk -v key="$1" '
        /^\[/ { in_table = ($0 ~ /^\[sha256\][[:space:]]*$/); next }
        in_table && $0 ~ "^" key "[[:space:]]*=" {
            sub("^[^=]*=[[:space:]]*\"", ""); sub("\".*$", ""); print; exit
        }' "$PIN"
}

VERSION="$(toml_top version)"
TAG="$(toml_top tag)"
if [ -z "$VERSION" ] || [ -z "$TAG" ]; then
    echo "error: $PIN has no version or tag" >&2
    exit 1
fi

case "$(uname -s)" in
    Linux) OS=linux ;;
    Darwin) OS=macos ;;
    *) OS="" ;;
esac
case "$(uname -m)" in
    x86_64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=arm64 ;;
    *) ARCH="" ;;
esac
EXPECTED=""
if [ -n "$OS" ] && [ -n "$ARCH" ]; then
    EXPECTED="$(toml_sha256 "$ARCH-$OS")"
fi
if [ -z "$EXPECTED" ]; then
    echo "error: wasi-sdk.toml pins no wasi-sdk build for this platform (uname -s: $(uname -s), uname -m: $(uname -m))" >&2
    exit 1
fi

CACHE="${WASI_SDK_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/murmur-default-artifacts}"
BASENAME="wasi-sdk-$VERSION-$ARCH-$OS"
INSTALL="$CACHE/$BASENAME"

installed() {
    [ -x "$1/bin/clang" ] && [ -x "$1/bin/llvm-ar" ] && [ -f "$1/VERSION" ] || return 1
    local first
    first="$(head -n 1 "$1/VERSION")"
    case "$first" in
        "$VERSION"*) return 0 ;;
        *) return 1 ;;
    esac
}

TMP_FILE=""
TMP_DIR=""
cleanup() {
    [ -z "$TMP_FILE" ] || rm -f "$TMP_FILE"
    [ -z "$TMP_DIR" ] || rm -rf "$TMP_DIR"
}
trap cleanup EXIT

if installed "$INSTALL"; then
    echo "wasi-sdk $VERSION already installed at $INSTALL" >&2
else
    mkdir -p "$CACHE"
    URL="https://github.com/WebAssembly/wasi-sdk/releases/download/$TAG/$BASENAME.tar.gz"
    TMP_FILE="$(mktemp "$CACHE/.$BASENAME.tar.gz.XXXXXX")"
    echo "downloading $URL" >&2
    if ! curl -fsSL -o "$TMP_FILE" "$URL"; then
        echo "error: could not download $URL" >&2
        exit 1
    fi
    ACTUAL="$(sha256_of "$TMP_FILE")"
    if [ "$ACTUAL" != "$EXPECTED" ]; then
        echo "error: $BASENAME.tar.gz ($URL) does not match wasi-sdk.toml: expected sha256 $EXPECTED, got $ACTUAL; download deleted" >&2
        exit 1
    fi
    echo "sha256 verified: $ACTUAL" >&2

    TMP_DIR="$(mktemp -d "$CACHE/.$BASENAME.extract.XXXXXX")"
    echo "extracting into $CACHE" >&2
    if ! tar -xzf "$TMP_FILE" -C "$TMP_DIR"; then
        echo "error: could not extract $BASENAME.tar.gz" >&2
        exit 1
    fi
    rm -f "$TMP_FILE"; TMP_FILE=""
    if ! installed "$TMP_DIR/$BASENAME"; then
        echo "error: $BASENAME.tar.gz does not unpack to $BASENAME/ holding bin/clang, bin/llvm-ar and a VERSION starting with $VERSION" >&2
        exit 1
    fi
    # Another run may have finished first; keep its install. Otherwise replace whatever
    # incomplete directory sits at the install path.
    if ! installed "$INSTALL"; then
        rm -rf "$INSTALL"
        mv "$TMP_DIR/$BASENAME" "$INSTALL"
    fi
    echo "installed wasi-sdk $VERSION at $INSTALL" >&2
fi

LINK="$REPO_ROOT/.wasi-sdk"
if [ -e "$LINK" ] && [ ! -L "$LINK" ]; then
    echo "error: $LINK exists and is not a symlink; move it aside so it can link to $INSTALL" >&2
    exit 1
fi
ln -sfn "$INSTALL" "$LINK"
echo "linked $LINK -> $INSTALL" >&2
echo "$INSTALL"
