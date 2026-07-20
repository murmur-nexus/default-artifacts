#!/usr/bin/env bash
# Package murmur-tool-registry-search into a platform-tagged .mur.zip artifact.
#
# Canonical .mur.zip layout for native tools:
#   murmur.yaml       — artifact manifest at zip root
#   bin/<tool-name>     — compiled binary with executable permissions
#
# Usage:
#   ./package.sh                     # auto-detect platform, build and package
#   ./package.sh darwin-aarch64      # explicit platform override
#   ./package.sh darwin-aarch64 --publish  # package then publish to local Nexus
set -euo pipefail

ARTIFACT_NAME="murmur-tool-registry-search"
VERSION="0.1.0"

# Platform detection from the current host OS and architecture.
detect_platform() {
    local os arch
    os=$(uname -s)
    arch=$(uname -m)
    case "$os-$arch" in
        Darwin-arm64)  echo "darwin-aarch64" ;;
        Darwin-x86_64) echo "darwin-x86_64" ;;
        Linux-aarch64) echo "linux-aarch64" ;;
        Linux-x86_64)  echo "linux-x86_64" ;;
        *)             echo "unknown" ;;
    esac
}

PLATFORM="${1:-$(detect_platform)}"
PUBLISH="${2:-}"

if [ "$PLATFORM" = "unknown" ]; then
    echo "error: cannot detect platform on this host; pass platform as first argument" >&2
    echo "  usage: ./package.sh darwin-aarch64" >&2
    exit 1
fi

# Resolve paths relative to this script's location.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The default-artifacts workspace root is two levels up from tools/murmur-tool-registry-search/.
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BINARY_PATH="$WORKSPACE_ROOT/target/release/$ARTIFACT_NAME"
ZIP_NAME="${ARTIFACT_NAME}-${VERSION}-${PLATFORM}.mur.zip"
ZIP_PATH="$SCRIPT_DIR/$ZIP_NAME"

echo "Building $ARTIFACT_NAME..."
cargo build -p "$ARTIFACT_NAME" --release --manifest-path "$WORKSPACE_ROOT/Cargo.toml"

echo "Binary: $BINARY_PATH"

# Stage the canonical layout.
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/bin"
cp "$SCRIPT_DIR/murmur.yaml" "$STAGE/"
cp "$BINARY_PATH" "$STAGE/bin/$ARTIFACT_NAME"
chmod 755 "$STAGE/bin/$ARTIFACT_NAME"

# Create the zip with entries relative to the staging root.
rm -f "$ZIP_PATH"
(cd "$STAGE" && zip -r "$ZIP_PATH" murmur.yaml "bin/$ARTIFACT_NAME")

echo ""
echo "Packaged: $ZIP_PATH"
echo ""
unzip -l "$ZIP_PATH"

if [ "${PUBLISH:-}" = "--publish" ]; then
    echo ""
    echo "Publishing to local registry..."
    mur publish "$ZIP_PATH" --platform "$PLATFORM"
fi
