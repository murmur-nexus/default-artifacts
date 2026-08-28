#!/usr/bin/env bash
# Classify every `[workspace] members` crate as targeting wasm32-wasip2 or not.
#
# This is the single derivation behind "what does the workspace wasm build skip":
# scripts/build-wasm.sh turns it into `--exclude` flags, and
# scripts/check-build-coverage.sh uses it to hold build.yml's matrices to the
# same answer. Nothing hand-maintains a list of crate names.
#
# Two independent rules produce an exclusion, and each catches a different set:
#
#   native-artifact  A publishable artifact whose murmur.yaml says
#                    `implementation: native`. These ship as host binaries and
#                    link C sources (bundled SQLite, tree-sitter) or need host
#                    capabilities (subprocesses, raw sockets) that a
#                    wasm32-wasip2 guest does not have.
#                    Catches: murmur-tool-{git,registry-search,code-graph,
#                    code-coverage,test-report}.
#
#   internal-bin     A crate with no murmur.yaml (so: unpublishable, absent from
#                    artifacts.toml) whose Cargo.toml declares a `[[bin]]`. These
#                    are host-side helpers, built and run on the developer's
#                    machine, never shipped as a component.
#                    Catches: murmur-tool-git-validate.
#
# The two included classes:
#
#   wasm-artifact    A murmur.yaml with `implementation: wasm` or no
#                    `implementation:` key at all (drivers and hooks).
#   internal-lib     No murmur.yaml, no `[[bin]]`, but a library — it is linked
#                    into wasm artifacts, so it must cross-compile.
#                    Catches: murmur-test-parse.
#
# A member matching none of the four is a hard error rather than a silent guess
# in either direction: excluding it would drop an artifact out of the build, and
# including it would fail the build deep inside a C toolchain error that names
# the C compiler rather than the missing classification.
#
# Output: one tab-separated `<class>\t<crate-name>\t<path>` row per member, in
# `[workspace] members` order. The crate name is read from Cargo.toml, not
# inferred from the path — `tools/murmur-tool-git/validate` is the crate
# `murmur-tool-git-validate`.
#
# Usage:   scripts/classify-crates.sh
# Exit:    0 — every member classified
#          1 — a member could not be classified (message names it on stderr)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_TOML="$REPO_ROOT/Cargo.toml"

if [ ! -f "$WORKSPACE_TOML" ]; then
    echo "error: not found: $WORKSPACE_TOML" >&2
    exit 1
fi

# The `members = [ ... ]` array of the root [workspace] table, one path per line.
members="$(awk '
    /^members[[:space:]]*=[[:space:]]*\[/ { inside = 1; next }
    inside && /^[[:space:]]*\]/          { inside = 0 }
    inside                                { print }
' "$WORKSPACE_TOML" | sed 's/#.*//; s/[",]//g; s/[[:space:]]//g' | grep .)"

if [ -z "$members" ]; then
    echo "error: no [workspace] members found in $WORKSPACE_TOML" >&2
    exit 1
fi

fail=0

while IFS= read -r member; do
    dir="$REPO_ROOT/$member"
    cargo_toml="$dir/Cargo.toml"

    if [ ! -f "$cargo_toml" ]; then
        echo "error: workspace member '$member' has no Cargo.toml at $cargo_toml" >&2
        fail=1
        continue
    fi

    # `name = "..."` from the [package] table specifically — a [[bin]] table
    # carries its own `name` key.
    crate="$(awk '
        /^\[/                                        { table = $0 }
        table == "[package]" && /^name[[:space:]]*=/ {
            sub(/^name[[:space:]]*=[[:space:]]*/, ""); gsub(/"/, ""); print; exit
        }
    ' "$cargo_toml")"

    if [ -z "$crate" ]; then
        echo "error: workspace member '$member' declares no [package] name in Cargo.toml" >&2
        fail=1
        continue
    fi

    if [ -f "$dir/murmur.yaml" ]; then
        # Absent `implementation:` means wasm: drivers and hooks omit the key.
        implementation="$(sed -n 's/^implementation:[[:space:]]*\([A-Za-z0-9_-][A-Za-z0-9_-]*\).*/\1/p' "$dir/murmur.yaml" | head -1)"
        case "${implementation:-wasm}" in
            native) printf 'native-artifact\t%s\t%s\n' "$crate" "$member" ;;
            wasm)   printf 'wasm-artifact\t%s\t%s\n'   "$crate" "$member" ;;
            *)
                echo "error: $member/murmur.yaml declares 'implementation: $implementation'; expected 'native' or 'wasm'" >&2
                fail=1
                ;;
        esac
        continue
    fi

    # No murmur.yaml: an internal crate. A `[[bin]]` makes it a host-side helper;
    # a library is linked into wasm artifacts and must cross-compile. A crate
    # declaring both is treated as a binary, since that is what gets built.
    if grep -q '^\[\[bin\]\]' "$cargo_toml"; then
        printf 'internal-bin\t%s\t%s\n' "$crate" "$member"
    elif grep -q '^\[lib\]' "$cargo_toml" || [ -f "$dir/src/lib.rs" ]; then
        printf 'internal-lib\t%s\t%s\n' "$crate" "$member"
    else
        echo "error: cannot classify workspace member '$member' ($crate): it has no murmur.yaml, no [[bin]] and no library target." >&2
        echo "       Give it a murmur.yaml if it is a publishable artifact, or a [[bin]]/[lib] target so scripts/classify-crates.sh can tell whether it targets wasm32-wasip2." >&2
        fail=1
    fi
done <<< "$members"

exit "$fail"
