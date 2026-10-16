# shellcheck shell=bash
# Helpers shared by the release scripts. Sourced, never run:
#
#   . "$SCRIPT_DIR/lib/common.sh"

# The platforms a native payload can be tagged with: the set `mur index` accepts.
# shellcheck disable=SC2034 # read by the scripts that source this
PLATFORMS="darwin-aarch64 darwin-x86_64 linux-aarch64 linux-x86_64"

# The value of a top-level `key:` in a murmur.yaml, unquoted, or nothing. Only
# scalar keys are read, and only from column 0, so a `name:` inside a block
# scalar or a nested mapping is never mistaken for the artifact's own.
#
#   yaml_scalar <murmur.yaml> <key>
yaml_scalar() {
    sed -n "s/^$2:[[:space:]]*//p" "$1" | head -1 \
        | sed 's/[[:space:]]#.*$//; s/[[:space:]]*$//; s/^"\(.*\)"$/\1/; s/^'"'"'\(.*\)'"'"'$/\1/'
}

# Set MUR to $MUR_BIN, else `mur` on PATH, and exit 1 unless it has the `index`
# subcommand. Packing and indexing must use the `mur` the release pins, and one
# without `index` predates scoped names. An explicit MUR_BIN that fails the check
# is refused rather than replaced by PATH's `mur`.
require_mur() {
    MUR="${MUR_BIN:-$(command -v mur || true)}"
    if [ -z "$MUR" ] || ! "$MUR" index --help >/dev/null 2>&1; then
        echo "error: need a 'mur' with the 'index' subcommand (tried: ${MUR:-<none on PATH>})." >&2
        echo "       Build one from murmur at the MURMUR_REV in .github/workflows/build.yml and set MUR_BIN to it." >&2
        exit 1
    fi
}

# The sha256 of a file, as bare hex. GNU coreutils has sha256sum and macOS has
# shasum; fails when neither can read the file.
#
#   sha256_of <file>
sha256_of() {
    local sum
    sum="$(sha256sum "$1" 2>/dev/null || shasum -a 256 "$1")" || return 1
    echo "${sum%% *}"
}
