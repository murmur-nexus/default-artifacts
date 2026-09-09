#!/usr/bin/env bash
# Reads artifacts.toml and applies all version strings to every version surface:
#   - [workspace.package] version in root Cargo.toml
#   - version: field in each artifact's murmur.yaml
#   - VERSION= variable in each artifact's package.sh (when present)
# Then regenerates artifacts-index.json from artifacts.toml + each murmur.yaml,
# including each entry's `platforms` — derived from artifacts.toml's
# native_platforms and the artifact's own `implementation:` (see platforms_for
# below), never from a list kept here.
#
# Usage: ./scripts/apply-versions.sh
#
# No external dependencies required beyond bash, grep, sed, and awk.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACTS_TOML="$REPO_ROOT/artifacts.toml"

if [ ! -f "$ARTIFACTS_TOML" ]; then
    echo "error: artifacts.toml not found at $ARTIFACTS_TOML" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Parse workspace_version
# ---------------------------------------------------------------------------
workspace_version=$(grep '^workspace_version' "$ARTIFACTS_TOML" \
    | sed 's/workspace_version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/')

if [ -z "$workspace_version" ]; then
    echo "error: could not parse workspace_version from artifacts.toml" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Parse native_platforms — the platforms a release publishes a native payload
# for. artifacts.toml is the only home for this fact; the index entries below
# derive their `platforms` from it, and scripts/check-platform-coverage.sh holds
# it to build.yml's build-native `platform:` matrix.
# ---------------------------------------------------------------------------
native_platforms=$(sed -n 's/^native_platforms[[:space:]]*=[[:space:]]*\[\(.*\)\].*/\1/p' "$ARTIFACTS_TOML" \
    | tr ',' '\n' | tr -d '"' | tr -d '[:blank:]' | grep . || true)

if [ -z "$native_platforms" ]; then
    echo "error: could not parse a non-empty native_platforms list from artifacts.toml" >&2
    echo "       expected a top-level line such as: native_platforms = [\"darwin-aarch64\", \"linux-x86_64\"]" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Apply workspace_version to [workspace.package] in root Cargo.toml
# ---------------------------------------------------------------------------
CARGO_TOML="$REPO_ROOT/Cargo.toml"

if [ ! -f "$CARGO_TOML" ]; then
    echo "error: Cargo.toml not found at $CARGO_TOML" >&2
    exit 1
fi

tmp=$(mktemp)
awk -v ver="$workspace_version" '
    $0 == "[workspace.package]" { in_section=1 }
    /^\[/ && $0 != "[workspace.package]" { in_section=0 }
    in_section && /^version[[:space:]]*=/ { print "version = \"" ver "\""; next }
    { print }
' "$CARGO_TOML" > "$tmp"
mv "$tmp" "$CARGO_TOML"
echo "Updated: Cargo.toml  [workspace.package] version = \"$workspace_version\""

# ---------------------------------------------------------------------------
# apply_artifact <name> <path> <version>
# Updates murmur.yaml (required) and package.sh (if present).
# ---------------------------------------------------------------------------
apply_artifact() {
    local name="$1" rel_path="$2" version="$3"
    local artifact_dir="$REPO_ROOT/$rel_path"
    local manifest="$artifact_dir/murmur.yaml"
    local package_sh="$artifact_dir/package.sh"

    if [ ! -f "$manifest" ]; then
        echo "error: murmur.yaml not found for '$name' at $manifest" >&2
        exit 1
    fi

    if ! grep -q '^version:' "$manifest"; then
        echo "error: no 'version:' field in $manifest" >&2
        exit 1
    fi

    tmp=$(mktemp)
    sed "s/^version:.*$/version: $version/" "$manifest" > "$tmp"
    mv "$tmp" "$manifest"
    echo "Updated: $rel_path/murmur.yaml  version: $version"

    if [ -f "$package_sh" ]; then
        if ! grep -q '^VERSION=' "$package_sh"; then
            echo "error: no 'VERSION=' line in $package_sh" >&2
            exit 1
        fi
        tmp=$(mktemp)
        sed "s/^VERSION=.*/VERSION=\"$version\"/" "$package_sh" > "$tmp"
        mv "$tmp" "$package_sh"
        chmod +x "$package_sh"
        echo "Updated: $rel_path/package.sh  VERSION=\"$version\""
    fi

    # If the artifact has a Cargo.toml with a pinned version (not version.workspace = true),
    # update it too so the Rust package version stays in sync with murmur.yaml.
    local cargo_toml="$artifact_dir/Cargo.toml"
    if [ -f "$cargo_toml" ] && grep -q '^version[[:space:]]*=' "$cargo_toml"; then
        tmp=$(mktemp)
        sed "s/^version[[:space:]]*=.*/version = \"$version\"/" "$cargo_toml" > "$tmp"
        mv "$tmp" "$cargo_toml"
        echo "Updated: $rel_path/Cargo.toml  version = \"$version\""
    fi
}

# ---------------------------------------------------------------------------
# Parse [[artifact]] blocks and apply each one
# ---------------------------------------------------------------------------
tmpdata=$(mktemp)
trap 'rm -f "$tmpdata"' EXIT

current_name="" current_path="" current_version=""

while IFS= read -r line; do
    if [[ "$line" =~ ^\[\[artifact\]\] ]]; then
        if [ -n "$current_name" ]; then
            printf '%s\t%s\t%s\n' "$current_name" "$current_path" "$current_version" >> "$tmpdata"
        fi
        current_name="" current_path="" current_version=""
    elif [[ "$line" =~ ^name[[:space:]]*= ]]; then
        current_name=$(printf '%s' "$line" | sed 's/name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/')
    elif [[ "$line" =~ ^path[[:space:]]*= ]]; then
        current_path=$(printf '%s' "$line" | sed 's/path[[:space:]]*=[[:space:]]*"\(.*\)"/\1/')
    elif [[ "$line" =~ ^version[[:space:]]*= ]]; then
        current_version=$(printf '%s' "$line" | sed 's/version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/')
    fi
done < "$ARTIFACTS_TOML"

if [ -n "$current_name" ]; then
    printf '%s\t%s\t%s\n' "$current_name" "$current_path" "$current_version" >> "$tmpdata"
fi

if [ ! -s "$tmpdata" ]; then
    echo "error: no [[artifact]] entries found in artifacts.toml" >&2
    exit 1
fi

while IFS=$'\t' read -r name path version; do
    apply_artifact "$name" "$path" "$version"
done < "$tmpdata"

# ---------------------------------------------------------------------------
# Regenerate artifacts-index.json
# ---------------------------------------------------------------------------
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
python3 - "$REPO_ROOT" "$TIMESTAMP" "$tmpdata" $native_platforms > "$REPO_ROOT/artifacts-index.json" <<'PYEOF'
import sys, json, re
from pathlib import Path

repo_root, timestamp, tmpdata_path = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
# artifacts.toml's native_platforms, one per argument.
native_platforms = sys.argv[4:]

RUNTIME_TAGS = {
    "driver": ["driver", "inference"],
    "hook":   ["hook"],
    "tool":   ["tool"],
    "skill":  ["skill"],
}


def first_sentence(s):
    s = s.strip().strip('"')
    m = re.match(r'^(.*?[.!?]) ', s + ' ')
    return m.group(1) if m else s.split('\n')[0].strip()


def get_description(text):
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if line.startswith('description:'):
            val = line[len('description:'):].strip().strip('"')
            if val in ('|', '>'):
                # YAML block scalar — first non-empty indented line
                for j in range(i + 1, len(lines)):
                    if lines[j].startswith(' ') or lines[j].startswith('\t'):
                        content = lines[j].strip()
                        if content:
                            return first_sentence(content)
                return ''
            return first_sentence(val)
    return ''


def get_field(name, text):
    for line in text.splitlines():
        if line.startswith(name + ':'):
            return line[len(name) + 1:].strip().strip('"')
    return ''


def platforms_for(text):
    """The platform set an artifact's payloads are tagged with.

    Derived from the artifact's own `implementation:`, using the same
    absent-means-wasm rule as scripts/classify-crates.sh: a native tool ships one
    platform-tagged .mur.zip per platform in artifacts.toml's native_platforms,
    and every wasm artifact and skill ships a single untagged payload — so it
    declares no platform, which is what the registry records for an untagged
    payload in the local store.
    """
    implementation = get_field('implementation', text) or 'wasm'
    return list(native_platforms) if implementation == 'native' else []


def name_tags(name, runtime, base_tags):
    # Extract keywords from the artifact name beyond the murmur- prefix and runtime type
    parts = name.replace('murmur-', '').split('-')
    extras = [p for p in parts if p and p != runtime and p not in base_tags]
    return base_tags + extras


artifacts = []
with open(tmpdata_path) as f:
    for line in f:
        parts = line.rstrip('\n').split('\t')
        if len(parts) != 3:
            continue
        name, rel_path, version = parts
        text = (repo_root / rel_path / 'murmur.yaml').read_text()
        runtime = get_field('runtime', text) or 'tool'
        description = get_description(text)
        platforms = platforms_for(text)
        base_tags = list(RUNTIME_TAGS.get(runtime, ["tool"]))
        tags = name_tags(name, runtime, base_tags)
        artifacts.append({
            "name": name,
            "version": version,
            "runtime": runtime,
            "description": description,
            "tags": tags,
            "platforms": platforms,
        })

print(json.dumps({"schema_version": "1", "updated_at": timestamp, "artifacts": artifacts}, indent=2))
PYEOF
echo "Generated: artifacts-index.json  updated_at=$TIMESTAMP"

echo ""
echo "Done. All version surfaces updated from artifacts.toml."
