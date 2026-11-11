#!/usr/bin/env bash
# Pack one artifact into the .mur.zip a release publishes, with `mur build`.
#
# The zip's name, `<name>-<version>.mur.zip`, is the one `mur index`
# requires, and both values come from the one place they are written: a skill's
# SKILL.md front block (`name`, `metadata.version`), every other artifact's own
# murmur.yaml. `mur build` dates every entry 1980-01-01 and writes murmur.yaml,
# then the payload files, then murmur-build.json, so identical inputs give a
# byte-identical zip. That is what
# lets the release's bytes gate (index-release.sh) tell a real payload change
# from a repack.
#
# How the payload gets into the zip depends on the artifact's kind, as
# list-artifacts.sh reports it:
#
#   skill   a directory holding SKILL.md. It is held to check-skill-folders.sh's
#           rules first, then packed with `mur build <dir>` directly; `mur`
#           generates the zip's murmur.yaml from the front block. The written zip
#           is then checked: its entries are exactly the folder's packed files
#           (every regular file but dot-names and *.mur.zip) plus murmur.yaml and
#           murmur-build.json, its SKILL.md is the folder's byte for byte, and its
#           murmur.yaml says the front block's name and version and `runtime:
#           skill`. A `mur` that predates murmur 35653488 fails here, or in `mur
#           build`, rather than shipping a mislabelled zip.
#   wasm    the component built for wasm32-wasip2, by default
#           target/wasm32-wasip2/release/<name with - as _>.wasm. The artifact
#           directory is copied to a scratch dir and the component is added under
#           the `*.wasm` name its `requires_files:` declares. A murmur.yaml
#           declaring `runtime: skill` is refused: a skill is a SKILL.md folder.
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
# the same `mur` the release indexes with. A skill needs a `mur` containing murmur
# 35653488 (v0.11.0 or later); the release builds `mur` at MURMUR_REV in
# .github/workflows/build.yml.
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

if [ ! -d "$ARTIFACT_DIR" ]; then
    echo "error: $ARTIFACT_DIR is not a directory" >&2
    exit 1
fi
BASE="$(basename "$(cd "$ARTIFACT_DIR" && pwd)")"
MANIFEST="$ARTIFACT_DIR/murmur.yaml"

# A skill is a directory whose root holds SKILL.md. Any casing counts here, so that
# a lowercase skill.md reaches the folder check and is refused with the rename,
# rather than as a missing murmur.yaml.
if [ -n "$(find "$ARTIFACT_DIR/" -maxdepth 1 -iname skill.md -print -quit)" ]; then
    KIND=skill
    if ! SKILL_ROW="$(python3 "$SCRIPT_DIR/lib/skill_folder.py" rows "$ARTIFACT_DIR")"; then
        echo "error: $ARTIFACT_DIR is not a standard skill folder (see above); nothing was packed" >&2
        exit 1
    fi
    IFS=$'\t' read -r _dir NAME VERSION <<< "$SKILL_ROW"
else
    if [ ! -f "$MANIFEST" ]; then
        echo "error: $MANIFEST not found" >&2
        exit 1
    fi

    NAME="$(yaml_scalar "$MANIFEST" name)"
    VERSION="$(yaml_scalar "$MANIFEST" version)"
    RUNTIME="$(yaml_scalar "$MANIFEST" runtime)"
    IMPLEMENTATION="$(yaml_scalar "$MANIFEST" implementation)"

    if [ -z "$NAME" ] || [ -z "$VERSION" ]; then
        echo "error: $MANIFEST must declare both 'name:' and 'version:'" >&2
        exit 1
    fi
    if [ "$NAME" != "$BASE" ]; then
        echo "error: $MANIFEST declares 'name: $NAME', but its directory is '$BASE'; the directory must be named after the artifact" >&2
        exit 1
    fi
    if [ "$RUNTIME" = "skill" ]; then
        echo "error: $MANIFEST declares 'runtime: skill'; a skill is a skills/<name>/SKILL.md folder, not a murmur.yaml" >&2
        exit 1
    fi
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
    [ "$KIND" != "skill" ] || echo "       A skill needs a mur containing murmur 35653488 (v0.11.0 or later; the release builds mur at MURMUR_REV in .github/workflows/build.yml); this one is $MUR." >&2
    exit 1
fi

# ---- a skill's zip holds what the folder and its front block say ----------------
if [ "$KIND" = "skill" ]; then
    if ! python3 - "$ARTIFACT_DIR" "$OUT" "$NAME" "$VERSION" <<'PY'
import os
import re
import sys
import zipfile

folder, out, name, version = sys.argv[1:5]

# What `mur build` packs from a skill folder: every regular file at any depth,
# skipping any path with a component starting with `.` and any *.mur.zip.
packed = {}
for dirpath, dirnames, filenames in os.walk(folder):
    dirnames[:] = [d for d in dirnames if not d.startswith(".")]
    for f in filenames:
        if f.startswith(".") or f.endswith(".mur.zip"):
            continue
        full = os.path.join(dirpath, f)
        if os.path.isfile(full) and not os.path.islink(full):
            packed[os.path.relpath(full, folder).replace(os.sep, "/")] = full

problems = []
try:
    with zipfile.ZipFile(out) as z:
        names = z.namelist()
        want = set(packed) | {"murmur.yaml", "murmur-build.json"}
        got = set(names)
        if len(names) != len(got):
            problems.append("the zip holds a duplicate entry")
        for n in sorted(want - got):
            problems.append(f"the zip lacks {n}")
        for n in sorted(got - want):
            problems.append(f"the zip holds {n}, which the folder does not pack")
        if "SKILL.md" in got:
            with open(packed["SKILL.md"], "rb") as fh:
                if z.read("SKILL.md") != fh.read():
                    problems.append("the zip's SKILL.md differs from the folder's")
        if "murmur.yaml" in got:
            manifest = {}
            for line in z.read("murmur.yaml").decode("utf-8", "replace").splitlines():
                m = re.match(r"^(name|version|runtime):\s*(.*?)\s*$", line)
                if m and m.group(1) not in manifest:
                    value = m.group(2)
                    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
                        value = value[1:-1]
                    manifest[m.group(1)] = value
            for key, expected in (("name", name), ("version", version), ("runtime", "skill")):
                if manifest.get(key) != expected:
                    problems.append(
                        f"the zip's murmur.yaml says {key}: {manifest.get(key, '<absent>')}, "
                        f"but the SKILL.md front block says {expected}"
                        if key != "runtime" else
                        f"the zip's murmur.yaml says runtime: {manifest.get(key, '<absent>')}, not skill"
                    )
except zipfile.BadZipFile as error:
    problems.append(f"the written file is not a zip: {error}")

for p in problems:
    print(f"error: {out}: {p}", file=sys.stderr)
sys.exit(1 if problems else 0)
PY
    then
        rm -f "$OUT"
        echo "error: the zip mur build wrote for $NAME does not match $ARTIFACT_DIR; it was deleted." >&2
        echo "       A skill needs a mur containing murmur 35653488 (v0.11.0 or later; the release builds mur at MURMUR_REV in .github/workflows/build.yml); this one is $MUR." >&2
        exit 1
    fi
fi
echo "$OUT"
