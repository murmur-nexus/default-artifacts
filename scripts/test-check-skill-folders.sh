#!/usr/bin/env bash
# Exercise scripts/check-skill-folders.sh against one fixture folder per rule.
#
# Every fixture is built under one `mktemp -d` and removed on exit; none is committed.
# For each case the check runs on the fixture, and the case passes when the exit code
# and a distinctive substring of the output (stdout and stderr together) are the
# expected ones.
#
# Usage:   scripts/test-check-skill-folders.sh
# Output:  `ok   <case>` or `FAIL <case>` per case, then `<passed>/<total> cases passed`.
# Exit:    0 — every case behaved
#          1 — a case did not
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$SCRIPT_DIR/check-skill-folders.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

total=0
passed=0

# The SKILL.md of a valid folder named $1, with an optional extra body line $2.
valid_skill_md() {
    printf -- '---\nname: %s\ndescription: "Do one thing: a fixture. Use when testing."\nmetadata:\n  version: "1.0.0"\n---\n# Fixture\n\n%s\n' "$1" "${2:-}"
}

# A fixture folder $1 holding SKILL.md with the content on stdin.
fixture() {
    mkdir -p "$WORK/$1"
    cat > "$WORK/$1/${2:-SKILL.md}"
}

# expect <case> <exit> <substring> <folder>...
expect() {
    local name="$1" want_exit="$2" want_text="$3"
    shift 3
    local args=() f out got
    for f in "$@"; do args+=("$WORK/$f"); done
    total=$((total + 1))
    set +e
    out="$("$CHECK" "${args[@]}" 2>&1)"
    got=$?
    set -e
    if [ "$got" -eq "$want_exit" ] && [[ "$out" == *"$want_text"* ]]; then
        passed=$((passed + 1))
        echo "ok   $name (exit $got)"
    else
        echo "FAIL $name: want exit $want_exit with '$want_text', got exit $got:"
        printf '%s\n' "$out" | sed 's/^/       /'
    fi
}

# ---- the file name ------------------------------------------------------------------
valid_skill_md lower-skill-md | fixture lower-skill-md skill.md
expect "lowercase skill.md" 1 "rename it: git mv skill.md SKILL.md" lower-skill-md

valid_skill_md title-skill-md | fixture title-skill-md Skill.md
expect "Skill.md" 1 "rename it: git mv Skill.md SKILL.md" title-skill-md

# ---- the front block ----------------------------------------------------------------
printf '# No front block\n' | fixture no-front-block
expect "no front block" 1 "first line must be exactly '---'" no-front-block

printf -- '---\nname: unclosed\ndescription: "x"\n# body\n' | fixture unclosed
expect "unclosed block" 1 "front block is not closed" unclosed

# ---- name ---------------------------------------------------------------------------
printf -- '---\ndescription: "x"\nmetadata:\n  version: "1.0.0"\n---\nbody\n' | fixture no-name
expect "no name" 1 "front block has no name; add 'name: no-name'" no-name

valid_skill_md other-name | fixture name-differs
expect "name differs from folder" 1 "differs from the folder name 'name-differs'" name-differs

valid_skill_md Bad_Name | fixture Bad_Name
expect "Bad_Name" 1 "holds characters other than lowercase letters, digits and '-'" Bad_Name

valid_skill_md -x | fixture -x
expect "-x" 1 "starts or ends with '-'" -x

valid_skill_md a--b | fixture a--b
expect "a--b" 1 "holds '--'" a--b

long_name="$(printf 'a%.0s' $(seq 1 65))"
valid_skill_md "$long_name" | fixture "$long_name"
expect "65-character name" 1 "is 65 characters; a skill name is at most 64" "$long_name"

# ---- description --------------------------------------------------------------------
printf -- '---\nname: no-description\nmetadata:\n  version: "1.0.0"\n---\nbody\n' | fixture no-description
expect "no description" 1 "front block has no description" no-description

printf -- '---\nname: empty-description\ndescription: ""\nmetadata:\n  version: "1.0.0"\n---\nbody\n' | fixture empty-description
expect "empty description" 1 "front block has no description" empty-description

d1025="$(printf 'd%.0s' $(seq 1 1025))"
printf -- '---\nname: long-description\ndescription: "%s"\nmetadata:\n  version: "1.0.0"\n---\nbody\n' "$d1025" | fixture long-description
expect "1025-character description" 1 "description is 1025 characters; a description is at most 1024" long-description

# ---- metadata.version ---------------------------------------------------------------
printf -- '---\nname: no-metadata\ndescription: "x"\n---\nbody\n' | fixture no-metadata
expect "no metadata" 1 "front block has no metadata" no-metadata

printf -- '---\nname: no-version\ndescription: "x"\nmetadata:\n  author: someone\n---\nbody\n' | fixture no-version
expect "metadata without version" 1 "metadata holds no version" no-version

printf -- '---\nname: short-version\ndescription: "x"\nmetadata:\n  version: "1.0"\n---\nbody\n' | fixture short-version
expect 'version "1.0"' 1 "metadata.version '1.0' is not a semver version" short-version

# ---- the rest of the folder ---------------------------------------------------------
valid_skill_md manifest-beside | fixture manifest-beside
printf 'name: manifest-beside\nversion: 1.0.0\nruntime: skill\n' > "$WORK/manifest-beside/murmur.yaml"
expect "murmur.yaml beside SKILL.md" 1 "a skill folder holds no murmur.yaml" manifest-beside

valid_skill_md manifest-casing "Murmur.yaml is named here, so only the casing rule refuses it." | fixture manifest-casing
printf 'name: manifest-casing\n' > "$WORK/manifest-casing/Murmur.yaml"
expect "Murmur.yaml beside SKILL.md" 1 "Murmur.yaml: a skill folder holds no murmur.yaml" manifest-casing

valid_skill_md build-info "murmur-build.json is named here, so only the reserved-name rule refuses it." | fixture build-info
echo '{}' > "$WORK/build-info/murmur-build.json"
expect "murmur-build.json in the folder" 1 "murmur-build.json: mur build writes this file itself" build-info

valid_skill_md unreferenced | fixture unreferenced
echo notes > "$WORK/unreferenced/notes.md"
expect "unreferenced notes.md" 1 "notes.md is not named in the SKILL.md body" unreferenced

valid_skill_md with-symlink "See target.md and link.md." | fixture with-symlink
echo target > "$WORK/with-symlink/target.md"
ln -s target.md "$WORK/with-symlink/link.md"
expect "symlink" 1 "link.md is a symlink" with-symlink

printf -- '---\nname: block-scalar\ndescription: >\n  Folded\n  text.\nmetadata:\n  version: "1.0.0"\n---\nbody\n' | fixture block-scalar
expect "block-scalar description" 1 "description is a block scalar ('>')" block-scalar

# ---- passing folders ----------------------------------------------------------------
valid_skill_md valid | fixture valid
expect "valid folder" 0 "OK: 1 skill folder(s) follow the Agent Skills standard" valid

valid_skill_md with-reference "Read references/x.md for the details." | fixture with-reference
mkdir -p "$WORK/with-reference/references"
echo detail > "$WORK/with-reference/references/x.md"
expect "valid folder with references/x.md" 0 "OK: 1 skill folder(s)" with-reference

# 1023 ASCII characters and one 'é': 1024 code points, 1025 bytes.
d1024="é$(printf 'd%.0s' $(seq 1 1023))"
printf -- '---\nname: description-1024\ndescription: "%s"\nmetadata:\n  version: "1.0.0"\n---\nbody\n' "$d1024" | fixture description-1024
expect "1024-character non-ASCII description" 0 "OK: 1 skill folder(s)" description-1024

# ---- two folders at once ------------------------------------------------------------
valid_skill_md pair-good | fixture pair-good
valid_skill_md pair-bad | fixture pair-bad
echo stray > "$WORK/pair-bad/stray.txt"
expect "two folders, one broken: exit 1" 1 "pair-bad: stray.txt is not named" pair-good pair-bad
total=$((total + 1))
set +e
pair_out="$("$CHECK" "$WORK/pair-good" "$WORK/pair-bad" 2>&1)"
set -e
if [[ "$pair_out" != *"pair-good"* ]] && [ "$(printf '%s\n' "$pair_out" | grep -c '^error: ')" -eq 1 ]; then
    passed=$((passed + 1))
    echo "ok   two folders, one broken: only the broken one reported"
else
    echo "FAIL two folders, one broken: only the broken one reported, got:"
    printf '%s\n' "$pair_out" | sed 's/^/       /'
fi

echo "$passed/$total cases passed"
[ "$passed" -eq "$total" ]
