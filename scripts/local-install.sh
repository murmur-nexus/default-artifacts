#!/usr/bin/env bash
# Build one default-artifacts artifact and install it into a capsule, packed exactly as
# a release packs it.
#
# The artifact's directory and kind come from scripts/list-artifacts.sh. A wasm
# artifact is built for wasm32-wasip2 and validated, a native tool is built for the
# host with `cargo build --release -p <name>`, and a skill compiles nothing; then
# scripts/package-artifact.sh packs it with `mur build`, and `mur install ./<zip>`
# installs it into the capsule.
#
# An artifact installed from a file belongs to scope `local`, not `murmur`: the store
# holds it at .murmur/artifacts/local/<name>/<version>/, `mur install` pins it in
# murmur.lock as `local/<name>`, and the capsule names it `local/<name>` in its
# `artifacts:`. `mur install` replaces that pin on every install, so a rebuild at an
# unchanged version re-pins the new bytes; this script checks the pin came out as the
# zip's sha256 rather than writing it.
#
# Usage:
#   scripts/local-install.sh <artifact-name> <target-capsule-dir>
#
# <target-capsule-dir> must be a capsule folder holding murmur.yaml; the install lands
# in its .murmur/ and its murmur.lock. Any other folder is refused before anything is built.
#
# Example:
#   scripts/local-install.sh hook-compact /path/to/capsule-folder
#
# `mur` is $MUR_BIN, else `mur` on PATH, and must have the `index` subcommand.
set -euo pipefail

NAME="${1:?artifact name, e.g. hook-compact}"
# Unquoted so the escaped apostrophe reads the same in every bash: inside "${2:?…}" a
# bare ' opens a quote, and \' keeps its backslash in some shells.
CAPSULE_DIR=${2:?target capsule folder (the folder holding the capsule\'s murmur.yaml)}
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

# `mur install` walks up to the nearest murmur.yaml, so from a folder without one it
# either fails with E-IO-001 or installs into a parent capsule.
if [ ! -f "$CAPSULE_DIR/murmur.yaml" ]; then
  echo "error: $CAPSULE_DIR holds no murmur.yaml; the target must be a capsule folder holding murmur.yaml" >&2
  exit 1
fi

require_mur
export MUR_BIN="$MUR"

ROW="$(bash "$SCRIPT_DIR/list-artifacts.sh" | awk -F'\t' -v n="$NAME" '$2 == n')"
if [ -z "$ROW" ]; then
  echo "error: no artifact named '$NAME'; ./scripts/list-artifacts.sh lists them" >&2
  exit 1
fi
IFS=$'\t' read -r ART_PATH _name VERSION _runtime KIND <<< "$ROW"
CAPSULE_DIR="$(cd "$CAPSULE_DIR" && pwd)"
LOCK="$CAPSULE_DIR/murmur.lock"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

case "$KIND" in
  wasm)
    echo "→ [1/4] cargo build $NAME ($VERSION) — wasm32-wasip2"
    ( cd "$REPO_ROOT" && cargo build -p "$NAME" --target wasm32-wasip2 --release )
    "$REPO_ROOT/scripts/validate-component.sh" "$REPO_ROOT/target/wasm32-wasip2/release/${NAME//-/_}.wasm"
    ;;
  native)
    # A native tool compiled with --target wasm32-wasip2 fails in its C dependencies
    # (libsqlite3-sys can't find stdio.h — there is no wasi libc sysroot here), so it
    # builds for the host, as build.yml's native job does.
    echo "→ [1/4] cargo build $NAME ($VERSION) — native, host"
    ( cd "$REPO_ROOT" && cargo build --release -p "$NAME" )
    ;;
  skill)
    echo "→ [1/4] build $NAME ($VERSION) — skill, nothing to compile"
    ;;
esac

echo "→ [2/4] package via mur build"
ZIP="$(bash "$SCRIPT_DIR/package-artifact.sh" "$REPO_ROOT/$ART_PATH" "$OUT")"
echo "    $(basename "$ZIP")"

echo "→ [3/4] install into $CAPSULE_DIR"
( cd "$CAPSULE_DIR" && "$MUR" install "$ZIP" )

# The pin `mur install` wrote must be this zip's sha256, under the key its payload
# shape takes: sha256.any for a wasm component or skill, sha256.platforms.<tag> for a
# native tool (lock_version 3 refuses an entry carrying both).
echo "→ [4/4] check lock local/$NAME@$VERSION"
SHA="$(sha256_of "$ZIP")"
PLATFORM=""
if [ "$KIND" = "native" ]; then
  PLATFORM="$(basename "$ZIP" .mur.zip)"; PLATFORM="${PLATFORM#"$NAME-$VERSION-"}"
fi
if [ ! -f "$LOCK" ]; then
  echo "error: mur install wrote no murmur.lock in $CAPSULE_DIR" >&2
  exit 1
fi
python3 - "$LOCK" "local/$NAME" "$VERSION" "$SHA" "$PLATFORM" <<'PY'
import sys, yaml
lock_path, name, version, sha, platform = sys.argv[1:6]
doc = yaml.safe_load(open(lock_path)) or {}
entry = next((a for a in doc.get("artifacts", []) or [] if a.get("name") == name), None)
pinned = None
if entry is not None and str(entry.get("resolved_version")) == version:
    hashes = entry.get("sha256") or {}
    pinned = (hashes.get("platforms") or {}).get(platform) if platform else hashes.get("any")
key = f"sha256.platforms.{platform}" if platform else "sha256.any"
if pinned != sha:
    print(f"error: murmur.lock does not pin {name}@{version} {key} at {sha} (found {pinned!r})", file=sys.stderr)
    sys.exit(1)
print(f"    lock pins {name}@{version} {key}={sha[:12]}…")
PY

# Regeneration is driven by the manifest pin, so it must name this version, or the
# next run re-locks the wrong one. Check read-only; never edit the manifest here
# (it may carry comments / block scalars a YAML round-trip would mangle).
PINNED="$(python3 - "$CAPSULE_DIR/murmur.yaml" "local/$NAME" <<'PY'
import sys, yaml
try:
    doc = yaml.safe_load(open(sys.argv[1])) or {}
except Exception:
    sys.exit(0)
for a in doc.get("artifacts", []) or []:
    if a.get("name") == sys.argv[2]:
        print(a.get("version", "")); break
PY
)"
if [ "$PINNED" != "$VERSION" ]; then
  echo
  echo "⚠  $CAPSULE_DIR/murmur.yaml pins 'local/$NAME' at \"${PINNED:-<unset>}\", not \"$VERSION\"."
  echo "   Reference this build in the capsule's artifacts: as"
  echo "     - name: local/$NAME"
  echo "       version: \"$VERSION\""
  echo "   — 'murmur/$NAME' names the published release, not this build."
fi

echo
echo "✓ installed local/$NAME@$VERSION → $CAPSULE_DIR"
echo "  Optional: remove stale older version dirs under .murmur/artifacts/local/$NAME/"
