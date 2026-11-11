#!/usr/bin/env bash
# Check that every skill folder follows the Agent Skills standard (agentskills.io), so
# `npx skills add murmur-nexus/default-artifacts` installs it into any agent and
# `mur build <folder>` packs it.
#
# A skill is a folder `skills/<name>/` whose root holds a file named exactly
# SKILL.md, opening with a front block:
#
#   ---
#   name: <the folder name>
#   description: "<what the skill does and when to use it, at most 1024 characters>"
#   metadata:
#     version: "<semver>"
#   ---
#
# The front block is the only place a skill's name and version are written. Each
# folder is refused, one stderr line per broken rule, when:
#
#   1. its root holds no regular file named exactly SKILL.md, or holds another
#      casing of skill.md (compared by exact directory entry, as a case-insensitive
#      filesystem would otherwise answer for both);
#   2. its root holds a murmur.yaml;
#   3. SKILL.md's first line is not exactly `---`, or no later line closes the block;
#   4. `name` is missing, is not 1-64 of a-z0-9-, starts or ends with `-`, holds
#      `--`, differs from the folder name, or starts with `murmur-`;
#   5. `description` is missing, empty, or over 1024 characters (code points,
#      after unquoting);
#   6. `metadata` is not a mapping holding `version`, or `version` is not semver;
#   7. `compatibility`, when present, is over 500 characters;
#   8. a symlink is anywhere in the folder;
#   9. any other regular file in it, at any depth, dot-names included, is not named
#      by its folder-relative path (`references/x.md`) in the SKILL.md body.
#
# The front block is read by scripts/lib/skill_folder.py, the repository's one
# reader of it, which also refuses YAML forms it does not read (a block scalar, a
# multi-line value, a flow collection, a duplicate key, a plain value holding `: `
# or ` #`). list-artifacts.sh and package-artifact.sh run the same checks.
#
# Usage:   scripts/check-skill-folders.sh                every skills/*/ in this repository
#          scripts/check-skill-folders.sh <folder>...    exactly those folders, from any path
# Output:  `OK: <n> skill folder(s) follow the Agent Skills standard` on stdout, or one
#          `error: <folder>: <what and the fix>` line per broken rule on stderr, with
#          every folder still checked.
# Exit:    0 — every folder passes
#          1 — a folder breaks a rule
#          2 — usage error, or a named path that is not a directory
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

folders=()
for arg in "$@"; do
    case "$arg" in
        -h | --help)
            awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
            exit 0
            ;;
        -*)
            echo "error: unknown option '$arg'" >&2
            echo "usage: $0 [<folder>...]" >&2
            exit 2
            ;;
        *)
            if [ ! -d "$arg" ]; then
                echo "error: $arg: not a directory" >&2
                exit 2
            fi
            folders+=("$arg")
            ;;
    esac
done

if [ "${#folders[@]}" -eq 0 ]; then
    cd "$REPO_ROOT"
    shopt -s nullglob
    for d in skills/*/; do
        folders+=("${d%/}")
    done
    shopt -u nullglob
    if [ "${#folders[@]}" -eq 0 ]; then
        echo "OK: 0 skill folder(s) follow the Agent Skills standard"
        exit 0
    fi
fi

exec python3 "$SCRIPT_DIR/lib/skill_folder.py" check "${folders[@]}"
