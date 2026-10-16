#!/usr/bin/env bash
# List every artifact this repository publishes, read from the artifacts themselves.
#
# This is the one enumeration of "what ships". An artifact is a directory
# `<category>/<name>/` holding a murmur.yaml, for category drivers, hooks, tools or
# skills; that manifest is the only place its name and version are written. Nothing
# else lists them: ci.yml runs this to hold every manifest to the rules below,
# package-release.sh packs one zip per row, and local-install.sh finds an
# artifact's directory here.
#
# Each row is refused, rather than skipped, when:
#
#   - the directory's basename differs from `name:`. `mur index` and the release
#     asset names take the name from the manifest, and every script here finds an
#     artifact by its directory, so the two must be one string;
#   - the name starts with `murmur-`. A default artifact installs as
#     `murmur/<name>`, so the prefix would read `murmur/murmur-...`;
#   - `name:`, `version:` or `runtime:` is missing;
#   - `implementation:` is anything other than `native` or `wasm`. Absent means
#     wasm, as it does for classify-crates.sh: drivers and hooks omit the key.
#
# Output: one tab-separated row per artifact, sorted by directory:
#
#   <dir>\t<name>\t<version>\t<runtime>\t<impl>
#
# where <dir> is repo-relative (`tools/tool-editor`) and <impl> is `native` for
# `implementation: native`, `skill` for `runtime: skill`, and `wasm` otherwise.
# On any refusal nothing is printed to stdout, so a caller never acts on a
# partial list.
#
# Usage:   scripts/list-artifacts.sh
# Exit:    0 — every manifest listed
#          1 — a manifest was refused (each message names the file on stderr)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# The value of a top-level `key:` in a murmur.yaml, unquoted, or nothing. Only
# scalar keys are read, and only from column 0, so a `name:` inside a block
# scalar or a nested mapping is never mistaken for the artifact's own.
yaml_scalar() {
    sed -n "s/^$2:[[:space:]]*//p" "$1" | head -1 \
        | sed 's/[[:space:]]#.*$//; s/[[:space:]]*$//; s/^"\(.*\)"$/\1/; s/^'"'"'\(.*\)'"'"'$/\1/'
}

cd "$REPO_ROOT"

rows=""
fail=0
shopt -s nullglob
manifests=(drivers/*/murmur.yaml hooks/*/murmur.yaml tools/*/murmur.yaml skills/*/murmur.yaml)
shopt -u nullglob

if [ "${#manifests[@]}" -eq 0 ]; then
    echo "error: no <category>/<name>/murmur.yaml found under $REPO_ROOT" >&2
    exit 1
fi

while IFS= read -r manifest; do
    dir="${manifest%/murmur.yaml}"
    base="${dir##*/}"
    name="$(yaml_scalar "$manifest" name)"
    version="$(yaml_scalar "$manifest" version)"
    runtime="$(yaml_scalar "$manifest" runtime)"
    implementation="$(yaml_scalar "$manifest" implementation)"

    bad=0
    for key in name version runtime; do
        if [ -z "${!key}" ]; then
            echo "error: $manifest declares no '$key:'" >&2
            bad=1
        fi
    done
    if [ -n "$name" ] && [ "$name" != "$base" ]; then
        echo "error: $manifest declares 'name: $name', but its directory is '$base'; the directory must be named after the artifact" >&2
        bad=1
    fi
    case "$name" in
        murmur-*)
            echo "error: $manifest declares 'name: $name'; drop the 'murmur-' prefix — it installs as 'murmur/${name#murmur-}', and as 'murmur/$name' otherwise" >&2
            bad=1
            ;;
    esac
    case "${implementation:-wasm}" in
        native) impl=native ;;
        wasm)   impl=wasm ;;
        *)
            echo "error: $manifest declares 'implementation: $implementation'; expected 'native' or 'wasm'" >&2
            bad=1
            ;;
    esac
    [ "$runtime" = "skill" ] && impl=skill

    if [ "$bad" -ne 0 ]; then
        fail=1
        continue
    fi
    rows+="$(printf '%s\t%s\t%s\t%s\t%s' "$dir" "$name" "$version" "$runtime" "$impl")"$'\n'
done < <(printf '%s\n' "${manifests[@]}" | LC_ALL=C sort)

if [ "$fail" -ne 0 ]; then
    exit 1
fi
printf '%s' "$rows"
