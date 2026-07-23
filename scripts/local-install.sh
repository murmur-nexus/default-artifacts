#!/usr/bin/env bash
# Build a default-artifacts WASM artifact and install it into a capsule.
#
# Usage:
#   scripts/local-install.sh <artifact-name> <target-capsule-dir> [artifact-path]
#
# Example:
#   scripts/local-install.sh murmur-hook-compact /path/to/capsule-folder
set -euo pipefail

NAME="${1:?artifact name, e.g. murmur-hook-compact}"
CAPSULE_DIR="${2:?target capsule dir (the folder whose .murmur/ receives the install)}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ART_PATH="${3:-$(awk -v n="$NAME" '
  $0 ~ "name = \"" n "\"$" {found=1; next}
  found && /^path *=/ { gsub(/[" ]/,"",$3); print $3; exit }
  /^\[\[artifact\]\]/ {found=0}
' "$REPO_ROOT/artifacts.toml")}"
[ -n "$ART_PATH" ] || { echo "error: could not resolve path for '$NAME' in artifacts.toml (pass it as arg 3)" >&2; exit 1; }

SRC_DIR="$REPO_ROOT/$ART_PATH"
WASM_CRATE="${NAME//-/_}"
WASM="$REPO_ROOT/target/wasm32-wasip2/release/${WASM_CRATE}.wasm"
VERSION="$(grep '^version:' "$SRC_DIR/murmur.yaml" | awk '{print $2}' | tr -d '"')"
ZIP="/tmp/${NAME}-${VERSION}.mur.zip"
LOCK="$CAPSULE_DIR/murmur.lock"

echo "→ [1/5] cargo build $NAME ($VERSION)"
( cd "$REPO_ROOT" && cargo build -p "$NAME" --target wasm32-wasip2 --release )

echo "→ [2/5] validate component"
"$REPO_ROOT/scripts/validate-component.sh" "$WASM"

echo "→ [3/5] package via mur build"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$SRC_DIR/." "$STAGE/"
cp "$WASM" "$STAGE/${WASM_CRATE}.wasm"
rm -f "$ZIP"
mur build "$STAGE" -o "$ZIP"

echo "→ [4/5] install into $CAPSULE_DIR"
( cd "$CAPSULE_DIR" && mur install "$ZIP" )

# A stale murmur.lock is what breaks `mur run` after a local rebuild: it pins the
# old version + integrity hash and mur trusts it over the store. `mur install`
# (no args) can't refresh it for an unpublished local build — it re-resolves from
# the registry, where this version doesn't exist. So update the lock entry in
# place. Critically, do NOT recompute the hash: mur's lock records the sha of the
# .mur.zip, and `mur install` already wrote that exact value to a .sha256 sidecar
# next to the artifact — read it back so the lock always matches mur's own
# integrity check. If no lock exists, leave it: `mur run` generates a correct one.
echo "→ [5/5] update lock $NAME@$VERSION"
SIDECAR=$(ls "$CAPSULE_DIR/.murmur/artifacts/$NAME/$VERSION/"*.sha256 2>/dev/null | head -1 || true)
if [ ! -f "$LOCK" ]; then
  echo "    no murmur.lock present — next 'mur run' will generate one"
elif [ -z "$SIDECAR" ]; then
  echo "    warning: no .sha256 sidecar under .murmur/artifacts/$NAME/$VERSION/ — left lock unchanged" >&2
else
  SHA=$(tr -d '[:space:]' < "$SIDECAR")
  python3 - "$LOCK" "$NAME" "$VERSION" "$SHA" <<'PY'
import sys, yaml
lock_path, name, version, sha = sys.argv[1:5]
doc = yaml.safe_load(open(lock_path)) or {}
arts = doc.setdefault("artifacts", [])
entry = next((a for a in arts if a.get("name") == name), None)
if entry is None:
    entry = {"name": name}
    arts.append(entry)
entry["resolved_version"] = version
entry.setdefault("sha256", {})["wasm"] = sha
yaml.safe_dump(doc, open(lock_path, "w"), sort_keys=False)
print(f"    lock updated: {name}@{version} sha256.wasm={sha[:12]}…")
PY
fi

# Regeneration is driven by the manifest pin, so it must name this version, or the
# next run re-locks the wrong one. Check read-only; never edit the manifest here
# (it may carry comments / block scalars a YAML round-trip would mangle).
PINNED="$(python3 - "$CAPSULE_DIR/murmur.yaml" "$NAME" <<'PY'
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
  echo "⚠  $CAPSULE_DIR/murmur.yaml pins '$NAME' at \"${PINNED:-<unset>}\", not \"$VERSION\"."
  echo "   Update that pin to \"$VERSION\" or the next run will re-lock the wrong version."
fi

echo
echo "✓ installed $NAME@$VERSION → $CAPSULE_DIR (lock refreshes on next 'mur run')"
echo "  Optional: remove stale older version dirs under .murmur/artifacts/$NAME/"
