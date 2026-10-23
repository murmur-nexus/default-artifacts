# shellcheck shell=bash
# Helpers shared by the release and WIT scripts. Sourced, never run:
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

# The subtrees of murmur's crates/capsule-runtime/wit that wit/ mirrors, byte for
# byte. murmur's other top-level WIT files are not mirrored.
# shellcheck disable=SC2034 # read by the scripts that source this
WIT_SUBTREES="guest hook process-driver"

# Where murmur's WIT lives under a murmur checkout or tree.
# shellcheck disable=SC2034 # read by the scripts that source this
MURMUR_WIT_PATH="crates/capsule-runtime/wit"

# Exit 2 with what was tried and why.
#
#   murmur_not_found <what failed> <why that place was tried>
murmur_not_found() {
    {
        echo "error: $1"
        echo "  $2"
        echo "  To use a murmur checkout elsewhere, set MURMUR_DIR to its root."
    } >&2
    exit 2
}

# Set MURMUR_ROOT to the murmur checkout, found by one rule, and MURMUR_ROOT_WHY
# to the reason it was looked for there:
#   MURMUR_DIR set and non-empty — that directory (relative to $PWD), and the only
#                                  place looked.
#   otherwise                    — the directory `murmur` beside the main checkout,
#                                  where the main checkout is the parent of
#                                  `git rev-parse --git-common-dir`. A plain checkout
#                                  and each of its worktrees share that answer, and so
#                                  does CI's $GITHUB_WORKSPACE/{default-artifacts,murmur}.
# Exits 2 when there is no path to try. Does not check that the directory exists.
#
#   locate_murmur_root <a directory inside this repository's checkout>
locate_murmur_root() {
    local common main_checkout
    if [ -n "${MURMUR_DIR:-}" ]; then
        case "$MURMUR_DIR" in
            /*) MURMUR_ROOT="$MURMUR_DIR" ;;
            *) MURMUR_ROOT="$PWD/$MURMUR_DIR" ;;
        esac
        MURMUR_ROOT_WHY="Looked there because MURMUR_DIR=$MURMUR_DIR."
    else
        common="$(git -C "$1" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" ||
            murmur_not_found "murmur not found: no path to try, because MURMUR_DIR is unset and \`git rev-parse --git-common-dir\` failed in $1" \
                "Without MURMUR_DIR, murmur is looked for in the directory beside the main checkout, which git names."
        main_checkout="$(dirname "$common")"
        MURMUR_ROOT="$(dirname "$main_checkout")/murmur"
        MURMUR_ROOT_WHY="Looked there because it is the directory beside the main checkout $main_checkout, found with \`git rev-parse --git-common-dir\`."
    fi
}

# Exit 2 unless MURMUR_ROOT exists and is the toplevel of a git checkout. A
# `git archive` export, or a directory merely inside some other repository, has no
# commits to read the pinned WIT from.
require_murmur_checkout() {
    local top
    [ -d "$MURMUR_ROOT" ] || murmur_not_found "murmur not found at $MURMUR_ROOT" "$MURMUR_ROOT_WHY"
    if ! top="$(git -C "$MURMUR_ROOT" rev-parse --show-toplevel 2>/dev/null)" ||
        [ "$(cd "$top" && pwd -P)" != "$(cd "$MURMUR_ROOT" && pwd -P)" ]; then
        murmur_not_found "$MURMUR_ROOT is not a git checkout of murmur; the pinned WIT is read from its commits" \
            "$MURMUR_ROOT_WHY"
    fi
}
