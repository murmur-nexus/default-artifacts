#!/usr/bin/env bash
# Check that every artifact version a doc pins is that artifact's current version.
#
# An artifact's version is written once, in the `version:` of its own murmur.yaml.
# READMEs, skills and BUILD.md copy it into install lines and example manifests, and a
# copy left behind by a bump tells the reader to install a version that is not the
# one documented beside it. This holds every copy to `./scripts/list-artifacts.sh`.
#
# Usage:
#   ./scripts/check-readme-pins.sh                 every *.md outside .nexus/, tracked or
#                                                  untracked but not ignored
#   ./scripts/check-readme-pins.sh <file.md>...    exactly those files
#   ./scripts/check-readme-pins.sh --help          print this text
#
# A pin is one of:
#   inline    `murmur/<name>@<x.y.z>`, as in `mur install -g murmur/<name>@<x.y.z>`
#   mapping   a YAML mapping, usually a `- ` list item, holding `name: murmur/<name>` and
#             `version: <x.y.z>` (bare or quoted) at the same key column, in either order
#             with any keys between. Deeper keys are nested values and never count. The
#             mapping ends at a line indented less than its key column, at the next list
#             item, at a code fence, or at a non-key line at or left of the key column.
#             Reported on the `version:` line.
#   zip       `<name>-<x.y.z>[-<suffix>].mur.zip` where <name> is an artifact listed here;
#             other names are examples of someone else's artifact and are ignored
# `<x.y.z>` is three dot-separated integers. An inline or mapping pin naming no artifact
# of this repository is an error.
#
# Output, one line per offending pin:
#   <file>:<line>: murmur/<name> pinned at <v>, its murmur.yaml says <v'>
#   <file>:<line>: murmur/<name> names no artifact in this repository
#
# Exit codes:
#   0 — every pin matches (prints `OK: <n> pins in <m> files match their murmur.yaml version`)
#   1 — a stale or unknown pin
#   2 — usage error, a missing file, or list-artifacts.sh failed
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

files=()
for arg in "$@"; do
    case "$arg" in
        -h | --help)
            awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
            exit 0
            ;;
        -*)
            echo "error: unknown flag '$arg'" >&2
            exit 2
            ;;
        *)
            if [ ! -f "$arg" ]; then
                echo "error: no such file: $arg" >&2
                exit 2
            fi
            files+=("$arg")
            ;;
    esac
done

artifacts="$("$SCRIPT_DIR/list-artifacts.sh")" || {
    echo "error: list-artifacts.sh failed; no expected versions to check against" >&2
    exit 2
}

if [ "${#files[@]}" -eq 0 ]; then
    cd "$REPO_ROOT"
    while IFS= read -r f; do
        case "$f" in
            .nexus/*) continue ;;
        esac
        # A tracked file deleted from the working tree is still listed.
        [ -f "$f" ] && files+=("$f")
    done < <(git ls-files -co --exclude-standard -- '*.md' | sort -u)
fi

python3 - "$artifacts" "${files[@]}" <<'PY'
import re
import sys

expected = {}
for row in sys.argv[1].splitlines():
    cols = row.split("\t")
    expected[cols[1]] = cols[2]
files = sys.argv[2:]

VERSION = r"\d+\.\d+\.\d+"
NAME = r"[a-z0-9][a-z0-9-]*"
INLINE = re.compile(rf"(?<![\w./-])murmur/({NAME})@({VERSION})(?![\w-]|\.\d)")
ZIP = re.compile(rf"(?<![\w-])({NAME})-({VERSION})(?:-[A-Za-z0-9_-]+)?\.mur\.zip")
# A YAML key line: indent, an optional `- ` list marker, then `key: value`.
KEY = re.compile(r"^( *)(- +)?([A-Za-z_][\w-]*):(?:\s+(.*?))?\s*$")
NAME_VALUE = re.compile(rf"""^(["']?)murmur/({NAME})\1(?:\s+#.*)?$""")
VERSION_VALUE = re.compile(rf"""^(["']?)({VERSION})\1(?:\s+#.*)?$""")
FENCE = re.compile(r"^\s*(```|~~~)")

pins = 0
pinned_files = set()
findings = []


def check(path, lineno, name, version, unknown_is_error=True):
    global pins
    want = expected.get(name)
    if want is None:
        if unknown_is_error:
            pins += 1
            pinned_files.add(path)
            findings.append((path, lineno, f"murmur/{name} names no artifact in this repository"))
        return
    pins += 1
    pinned_files.add(path)
    if version != want:
        findings.append((path, lineno, f"murmur/{name} pinned at {version}, its murmur.yaml says {want}"))


def key_line(line):
    """(key column, has list marker, key, value) for a YAML key line, else None."""
    m = KEY.match(line)
    if not m:
        return None
    indent, dash, key, value = m.groups()
    return len(indent) + len(dash or ""), dash is not None, key, value or ""


def mapping_lines(lines, i, col, has_dash):
    """Indices of the key lines at column `col` in the mapping holding line i."""
    def ends(j):
        line = lines[j]
        if not line.strip():
            return False
        if FENCE.match(line):
            return True
        indent = len(line) - len(line.lstrip(" "))
        k = key_line(line)
        if indent < col:
            return True
        # At the key column but not a key (prose, a closing fence handled above).
        return indent == col and k is None and not line.lstrip().startswith("- ")

    members = [i]
    # Backwards to the mapping's first line, unless line i opens it.
    if not has_dash:
        j = i - 1
        while j >= 0:
            line = lines[j]
            k = key_line(line)
            if k and k[0] == col and k[1]:
                members.append(j)  # the `- ` line that opens this item
                break
            if ends(j):
                break
            if k and k[0] == col:
                members.append(j)
            j -= 1
    j = i + 1
    while j < len(lines):
        if ends(j):
            break
        k = key_line(lines[j])
        if k and k[0] == col:
            if k[1]:
                break  # the next item of the same list
            members.append(j)
        j += 1
    return members


for path in files:
    with open(path, encoding="utf-8") as fh:
        lines = fh.read().splitlines()

    for n, line in enumerate(lines, 1):
        for m in INLINE.finditer(line):
            check(path, n, m.group(1), m.group(2))
        for m in ZIP.finditer(line):
            check(path, n, m.group(1), m.group(2), unknown_is_error=False)

    for i, line in enumerate(lines):
        k = key_line(line)
        if not k or k[2] != "name":
            continue
        col, has_dash, _, value = k
        nm = NAME_VALUE.match(value)
        if not nm:
            continue
        for j in mapping_lines(lines, i, col, has_dash):
            kj = key_line(lines[j])
            if kj[2] != "version":
                continue
            vm = VERSION_VALUE.match(kj[3])
            if vm:
                check(path, j + 1, nm.group(2), vm.group(2))
            break

order = {path: k for k, path in enumerate(files)}
for path, lineno, text in sorted(findings, key=lambda f: (order[f[0]], f[1])):
    print(f"{path}:{lineno}: {text}")
if findings:
    sys.stdout.flush()
    print(f"error: {len(findings)} stale or unknown pin(s)", file=sys.stderr)
    sys.exit(1)
print(f"OK: {pins} pins in {len(pinned_files)} files match their murmur.yaml version")
PY
