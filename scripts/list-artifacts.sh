#!/usr/bin/env bash
# List every artifact this repository publishes, read from the artifacts themselves.
#
# This is the one enumeration of "what ships". An artifact is a directory
# `<category>/<name>/`:
#
#   drivers, hooks, tools   a directory holding a murmur.yaml; that manifest is the
#                           only place its name and version are written.
#   skills                  every directory `skills/<name>/`, a standard Agent Skills
#                           folder holding SKILL.md; its front block's `name` and
#                           `metadata.version` are the only place its name and
#                           version are written. No skill has a murmur.yaml.
#
# Nothing else lists them: ci.yml runs this to hold every artifact to the rules
# below, package-release.sh packs one zip per row, and local-install.sh finds an
# artifact's directory here.
#
# Each skill folder is held to scripts/check-skill-folders.sh's rules, read by
# scripts/lib/skill_folder.py; a folder that breaks one refuses the whole run with
# that check's messages. Each murmur.yaml row is refused, rather than skipped, when:
#
#   - the directory's basename differs from `name:`. `mur index` and the release
#     asset names take the name from the manifest, and every script here finds an
#     artifact by its directory, so the two must be one string;
#   - the name starts with `murmur-`. A default artifact installs as
#     `murmur/<name>`, so the prefix would read `murmur/murmur-...`;
#   - `name:`, `version:` or `runtime:` is missing;
#   - `implementation:` is anything other than `wasm`. Absent means wasm, as it
#     does for classify-crates.sh: drivers and hooks omit the key;
#   - `runtime:` is `skill`. A skill is a skills/<name>/SKILL.md folder.
#
# Output: one tab-separated row per artifact, sorted by directory:
#
#   <dir>\t<name>\t<version>\t<runtime>\t<impl>
#
# where <dir> is repo-relative (`tools/tool-read`). A skill's row is
# `skills/<name>\t<name>\t<metadata.version>\tskill\tskill`; every other row has
# <impl> `wasm`.
# On any refusal nothing is printed to stdout, so a caller never acts on a
# partial list.
#
# Usage:   scripts/list-artifacts.sh
# Exit:    0 — every artifact listed
#          1 — an artifact was refused (each message names the file or folder on
#              stderr), or there is no artifact at all
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

cd "$REPO_ROOT"

rows=""
fail=0
shopt -s nullglob
manifests=(drivers/*/murmur.yaml hooks/*/murmur.yaml tools/*/murmur.yaml)
skill_dirs=(skills/*/)
shopt -u nullglob
skill_dirs=("${skill_dirs[@]%/}")

if [ "${#manifests[@]}" -eq 0 ] && [ "${#skill_dirs[@]}" -eq 0 ]; then
    echo "error: no <category>/<name>/murmur.yaml and no skills/<name>/ found under $REPO_ROOT" >&2
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
        wasm)   impl=wasm ;;
        *)
            echo "error: $manifest declares 'implementation: $implementation'; expected 'wasm'" >&2
            bad=1
            ;;
    esac
    if [ "$runtime" = "skill" ]; then
        echo "error: $manifest declares 'runtime: skill'; a skill is a skills/<name>/SKILL.md folder, not a murmur.yaml" >&2
        bad=1
    fi

    if [ "$bad" -ne 0 ]; then
        fail=1
        continue
    fi
    rows+="$(printf '%s\t%s\t%s\t%s\t%s' "$dir" "$name" "$version" "$runtime" "$impl")"$'\n'
done < <([ "${#manifests[@]}" -eq 0 ] || printf '%s\n' "${manifests[@]}" | LC_ALL=C sort)

# Every skill folder in one run of the check, which prints `<dir>\t<name>\t<version>`
# per folder, or nothing on stdout and its refusals on stderr.
if [ "${#skill_dirs[@]}" -gt 0 ]; then
    if skill_rows="$(python3 "$SCRIPT_DIR/lib/skill_folder.py" rows "${skill_dirs[@]}")"; then
        while IFS=$'\t' read -r dir name version; do
            rows+="$(printf '%s\t%s\t%s\t%s\t%s' "$dir" "$name" "$version" skill skill)"$'\n'
        done <<< "$skill_rows"
    else
        fail=1
    fi
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
printf '%s' "$rows" | LC_ALL=C sort
