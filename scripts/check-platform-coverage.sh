#!/usr/bin/env bash
# Check that every artifact's declared platform set matches the payloads the
# release actually builds for it.
#
# An artifact's platforms are a derived fact with one home: artifacts.toml's
# top-level `native_platforms` list, which names the platforms
# .github/workflows/build.yml's build-native job has a runner for. Each artifact
# picks its own set from that list and its own `implementation:`, the same
# absent-means-wasm rule scripts/classify-crates.sh uses:
#
#   implementation: native   one platform-tagged .mur.zip per native_platforms
#                            entry → declares exactly native_platforms
#   implementation: wasm,     a single untagged .mur.zip → declares no platform,
#   or the key absent         which is what the local store records for an
#   (drivers, hooks, skills)  untagged payload
#
# A declared platform with no payload behind it does not degrade quietly: it puts
# an entry in artifacts-index.json that no release asset answers, so resolution
# fails on a promise instead of falling back visibly. A platform that is built
# but undeclared is the same drift running the other way, and is what happens the
# next time a runner is added to the matrix. Both directions fail here.
#
# The enumeration this prints is the list itself — nothing hand-maintains a
# paragraph naming which artifacts are native.
#
# Usage:
#   scripts/check-platform-coverage.sh                # offline; what CI runs
#   scripts/check-platform-coverage.sh --release v1.2.3
#   scripts/check-platform-coverage.sh --help
#
# --release <tag> adds one check that needs network and a published release: that
# every asset belonging to a native tool carries a platform tag, and that each
# declared platform has a payload. It is opt-in precisely so the default run —
# and therefore CI — depends on no release and no network.
#
# Exit codes:
#   0 — every declaration matches; enumeration printed
#   1 — artifacts.toml, build.yml and artifacts-index.json disagree
#   2 — bad usage, or a required input could not be read (missing file,
#       unparseable native_platforms, `gh` unavailable under --release)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_TOML="$REPO_ROOT/artifacts.toml"
BUILD_YML="$REPO_ROOT/.github/workflows/build.yml"
INDEX_JSON="$REPO_ROOT/artifacts-index.json"

usage() {
    cat <<'EOF'
usage: scripts/check-platform-coverage.sh [--release <tag>]

  (no flags)        Offline check: artifacts.toml's native_platforms against
                    build.yml's build-native platform: matrix (both directions),
                    and every artifacts-index.json entry against the derivation.
                    Prints the artifact/implementation/platforms enumeration.
  --release <tag>   Also check the published release's assets for that tag
                    (needs network and the gh CLI).
  -h, --help        Print this message.
EOF
}

release_tag=""

while [ $# -gt 0 ]; do
    case "$1" in
        --release)
            if [ $# -lt 2 ] || [ -z "$2" ]; then
                echo "error: --release needs a release tag, e.g. --release v0.11.0" >&2
                exit 2
            fi
            release_tag="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument '$1'" >&2
            usage >&2
            exit 2
            ;;
    esac
done

for f in "$ARTIFACTS_TOML" "$BUILD_YML" "$INDEX_JSON"; do
    if [ ! -f "$f" ]; then
        echo "error: not found: $f" >&2
        exit 2
    fi
done

# ---------------------------------------------------------------------------
# The declaration: artifacts.toml's native_platforms, one platform per line.
# ---------------------------------------------------------------------------
declared_platforms=$(sed -n 's/^native_platforms[[:space:]]*=[[:space:]]*\[\(.*\)\].*/\1/p' "$ARTIFACTS_TOML" \
    | tr ',' '\n' | tr -d '"' | tr -d '[:blank:]' | grep . || true)

if [ -z "$declared_platforms" ]; then
    echo "error: could not parse a non-empty native_platforms list from artifacts.toml" >&2
    echo "       expected a top-level line such as: native_platforms = [\"darwin-aarch64\", \"linux-x86_64\"]" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# The ground truth: the `platform:` matrix rows of build.yml's build-native job.
# Those rows are `- { name: <x>, runner: <y> }`; requiring `runner:` is what
# keeps this from also reading the `artifact:` rows, which carry `path:`.
# (scripts/check-build-coverage.sh reads those, requiring `path:` for the same
# reason in reverse.)
# ---------------------------------------------------------------------------
matrix_platforms=$(awk '
    /^  [a-z][a-z0-9-]*:[[:space:]]*$/ { job = $1; sub(/:$/, "", job); next }
    job == "build-native" && /^[[:space:]]*-[[:space:]]*\{[[:space:]]*name:[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*,[[:space:]]*runner:/ {
        row = $0
        sub(/.*name:[[:space:]]*/, "", row)
        sub(/[[:space:]]*,[[:space:]]*runner:.*/, "", row)
        print row
    }
' "$BUILD_YML" | sort -u)

if [ -z "$matrix_platforms" ]; then
    echo "error: found no 'platform:' matrix rows in the build-native job of $BUILD_YML" >&2
    echo "       expected rows shaped: - { name: linux-x86_64, runner: ubuntu-latest }" >&2
    exit 2
fi

fail=0

while IFS= read -r platform; do
    [ -z "$platform" ] && continue
    if ! printf '%s\n' "$matrix_platforms" | grep -qx "$platform"; then
        echo "UNBUILT PLATFORM   $platform is declared in artifacts.toml's native_platforms but absent from the 'platform:' matrix in build.yml's build-native job, so no runner builds a payload for it."
        fail=1
    fi
done <<< "$declared_platforms"

while IFS= read -r platform; do
    [ -z "$platform" ] && continue
    if ! printf '%s\n' "$declared_platforms" | grep -qx "$platform"; then
        echo "UNDECLARED PLATFORM  $platform is built by the 'platform:' matrix in build.yml's build-native job but absent from artifacts.toml's native_platforms, so no artifact declares the payload it publishes."
        fail=1
    fi
done <<< "$matrix_platforms"

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "error: artifacts.toml's native_platforms and build.yml's build-native 'platform:' matrix disagree." >&2
    echo "A platform belongs in both or in neither: the matrix row is what builds the payload," >&2
    echo "and native_platforms is what every native artifact's index entry declares." >&2
    echo "Add or remove the platform in both, then run ./scripts/apply-versions.sh." >&2
    exit 1
fi

expected_native=$(printf '%s\n' "$declared_platforms" | sort | paste -sd, -)

# ---------------------------------------------------------------------------
# Every artifact, its implementation, and the platform set it declares.
# artifacts.toml gives the name and path; the artifact's own murmur.yaml gives
# `runtime:` and `implementation:`; artifacts-index.json gives what is published.
# ---------------------------------------------------------------------------
artifact_rows=$(awk '
    /^\[\[artifact\]\]/                        { name = ""; path = ""; next }
    /^name[[:space:]]*=/                       { name = $0; sub(/^name[[:space:]]*=[[:space:]]*/, "", name); gsub(/"/, "", name) }
    /^path[[:space:]]*=/                       { path = $0; sub(/^path[[:space:]]*=[[:space:]]*/, "", path); gsub(/"/, "", path)
                                                 if (name != "") print name "\t" path }
' "$ARTIFACTS_TOML")

if [ -z "$artifact_rows" ]; then
    echo "error: no [[artifact]] entries found in $ARTIFACTS_TOML" >&2
    exit 2
fi

# Each index entry's `platforms`, as `<name><TAB><comma-joined, sorted>`.
index_rows=$(python3 - "$INDEX_JSON" <<'PYEOF'
import json, sys

with open(sys.argv[1]) as f:
    index = json.load(f)

for entry in index.get("artifacts", []):
    print("{}\t{}".format(entry.get("name", ""), ",".join(sorted(entry.get("platforms", []) or []))))
PYEOF
)

index_platforms_for() {
    printf '%s\n' "$index_rows" | awk -F'\t' -v n="$1" '$1 == n { print $2; found = 1 } END { if (!found) print "<absent>" }'
}

printf '%-8s  %-40s  %s\n' "IMPL" "ARTIFACT" "PLATFORMS"

native_names=""
native_count=0

while IFS=$'\t' read -r name path; do
    [ -n "$name" ] || continue
    manifest="$REPO_ROOT/$path/murmur.yaml"
    if [ ! -f "$manifest" ]; then
        echo "error: murmur.yaml not found for '$name' at $manifest" >&2
        exit 2
    fi

    runtime=$(sed -n 's/^runtime:[[:space:]]*\([A-Za-z0-9_-][A-Za-z0-9_-]*\).*/\1/p' "$manifest" | head -1)
    # Absent `implementation:` means wasm — drivers, hooks and skills omit it.
    implementation=$(sed -n 's/^implementation:[[:space:]]*\([A-Za-z0-9_-][A-Za-z0-9_-]*\).*/\1/p' "$manifest" | head -1)
    implementation="${implementation:-wasm}"

    case "$implementation" in
        native|wasm) ;;
        *)
            echo "error: $path/murmur.yaml declares 'implementation: $implementation'; expected 'native' or 'wasm'" >&2
            exit 2
            ;;
    esac

    # The platform decision reads `implementation:` alone. `runtime: skill` only
    # changes how the row prints: a skill omits `implementation:`, so the rule
    # above already gives it the empty set.
    if [ "$implementation" = native ]; then
        expected="$expected_native"
        class=native
        native_names="$native_names$name"$'\n'
        native_count=$((native_count + 1))
    else
        expected=""
        if [ "$runtime" = skill ]; then class=skill; else class=wasm; fi
    fi

    actual=$(index_platforms_for "$name")

    if [ -n "$actual" ]; then display="${actual//,/, }"; else display="(none)"; fi
    printf '%-8s  %-40s  %s\n' "$class" "$name" "$display"

    if [ "$actual" = "<absent>" ]; then
        echo "MISSING ENTRY      $name is declared in artifacts.toml but has no artifacts-index.json entry, so nothing declares its platforms. Run ./scripts/apply-versions.sh."
        fail=1
    elif [ "$actual" != "$expected" ]; then
        if [ "$implementation" != native ]; then
            echo "PLATFORM ON $class    $name declares platforms [${actual}] in artifacts-index.json; a $class artifact publishes one untagged payload and must declare no platform."
        else
            missing=$(comm -23 <(printf '%s\n' "$expected" | tr ',' '\n' | sort) <(printf '%s\n' "$actual" | tr ',' '\n' | sort) | grep . | paste -sd, - || true)
            extra=$(comm -13 <(printf '%s\n' "$expected" | tr ',' '\n' | sort) <(printf '%s\n' "$actual" | tr ',' '\n' | sort) | grep . | paste -sd, - || true)
            echo "WRONG PLATFORMS    $name declares platforms [${actual}] in artifacts-index.json; expected [${expected}] (missing: ${missing:-none}, unexpected: ${extra:-none})."
        fi
        fail=1
    fi
done <<< "$artifact_rows"

# An index entry with no artifacts.toml declaration behind it is a platform set
# nobody derived, and a version nobody applies.
while IFS=$'\t' read -r name _platforms; do
    [ -n "$name" ] || continue
    if ! printf '%s\n' "$artifact_rows" | cut -f1 | grep -qx "$name"; then
        echo "UNDECLARED ENTRY   $name has an artifacts-index.json entry but no [[artifact]] block in artifacts.toml, so its platforms are derived from nothing. Run ./scripts/apply-versions.sh."
        fail=1
    fi
done <<< "$index_rows"

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "error: artifacts-index.json does not match the platform derivation." >&2
    echo "Every entry's 'platforms' is derived from artifacts.toml's native_platforms and the" >&2
    echo "artifact's own 'implementation:' — native tools get the full list, everything else" >&2
    echo "gets none. Never hand-edit artifacts-index.json: run ./scripts/apply-versions.sh." >&2
    exit 1
fi

declared_count=$(printf '%s\n' "$artifact_rows" | grep -c . || true)
platform_count=$(printf '%s\n' "$declared_platforms" | grep -c . || true)

echo ""
echo "OK: native_platforms ($expected_native) matches build.yml's build-native 'platform:' matrix."
echo "OK: all $declared_count artifacts in artifacts.toml declare the platform set their implementation calls for ($native_count native × $platform_count platforms, the rest none)."

# ---------------------------------------------------------------------------
# --release <tag>: the published assets. Needs network and the gh CLI, so it is
# never part of the default run.
#
# A native tool's payloads are always platform-tagged
# (<name>-<version>-<platform>.mur.zip, produced by its package.sh). An untagged
# native asset is the only thing a generic-path resolve could ever come from, so
# its absence is the check that matters here.
# ---------------------------------------------------------------------------
if [ -n "$release_tag" ]; then
    if ! command -v gh >/dev/null 2>&1; then
        echo "error: --release needs the gh CLI on PATH to read the release's assets" >&2
        exit 2
    fi

    echo ""
    echo "Release $release_tag assets:"

    if ! assets=$(gh release view "$release_tag" --json assets -q '.assets[].name' 2>&1); then
        echo "error: could not read assets of release '$release_tag':" >&2
        printf '%s\n' "$assets" >&2
        exit 2
    fi

    while IFS= read -r name; do
        [ -n "$name" ] || continue
        version=$(grep -A2 "^name = \"$name\"\$" "$ARTIFACTS_TOML" | sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' | head -1)
        # Match on `<name>-<digit>` so one tool's name is not a prefix of another
        # artifact's asset.
        own_assets=$(printf '%s\n' "$assets" | grep -E "^${name}-[0-9]" || true)

        if [ -z "$own_assets" ]; then
            echo "NOTE               $name has no asset in $release_tag; the release predates this artifact."
            continue
        fi

        # Every asset of a native tool must carry a declared platform tag.
        while IFS= read -r asset; do
            [ -n "$asset" ] || continue
            tagged=0
            while IFS= read -r platform; do
                case "$asset" in *"-$platform.mur.zip") tagged=1 ;; esac
            done <<< "$declared_platforms"
            if [ "$tagged" -eq 0 ]; then
                echo "UNTAGGED ASSET     $asset in release $release_tag belongs to native tool $name but carries no declared platform tag; a generic-path resolve can only come from an untagged native payload."
                fail=1
            fi
        done <<< "$own_assets"

        # And each declared platform must have a payload at this version.
        if printf '%s\n' "$own_assets" | grep -q "^${name}-${version}-"; then
            while IFS= read -r platform; do
                [ -n "$platform" ] || continue
                if printf '%s\n' "$own_assets" | grep -qx "${name}-${version}-${platform}.mur.zip"; then
                    echo "published          ${name}-${version}-${platform}.mur.zip"
                else
                    echo "MISSING PAYLOAD    $name@$version declares platform $platform but release $release_tag publishes no ${name}-${version}-${platform}.mur.zip."
                    fail=1
                fi
            done <<< "$declared_platforms"
        else
            echo "NOTE               $name@$version is not in release $release_tag; it publishes $(printf '%s\n' "$own_assets" | paste -sd' ' -)."
        fi
    done <<< "$(printf '%s\n' "$native_names" | grep . || true)"

    if [ "$fail" -ne 0 ]; then
        echo ""
        echo "error: release $release_tag does not publish what artifacts.toml declares." >&2
        echo "Every native payload is platform-tagged by its package.sh; an untagged one means" >&2
        echo "something packaged a native tool outside build-native's matrix." >&2
        exit 1
    fi

    echo ""
    echo "OK: every native asset in $release_tag carries a declared platform tag, and no untagged native asset exists."
fi
