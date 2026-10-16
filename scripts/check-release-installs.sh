#!/usr/bin/env bash
# Install every version a release adds, through a real `mur`, before it is uploaded.
#
# An index can be well-formed and still unusable: an asset the index names but the
# release does not carry, a sha256 that does not match, a payload whose packed name
# or version differs from its entry, or a version no `mur` will install. Each of
# those surfaces only when a consumer runs `mur install`, after the release is
# public. So this runs that install first, against <release-dir> served exactly as
# GitHub would serve it.
#
# A loopback fake of the GitHub REST API (Python 3 standard library, below) serves
# the release as the latest release of murmur-nexus/default-artifacts:
#
#   /repos/murmur-nexus/default-artifacts                    the owner id mur checks
#   /repos/murmur-nexus/default-artifacts/releases/latest    <tag>, one asset per file
#   /repos/murmur-nexus/default-artifacts/releases/tags/<tag>   the same
#   /repos/murmur-nexus/default-artifacts/releases/assets/<id>  that file's bytes
#
# Then, with a scratch HOME and MUR_GITHUB_API_BASE pointed at the fake, every
# version in <release-dir>/artifacts-index.json whose `release` is <tag> is
# installed with `mur install -g murmur/<name>@<version>` — `--all-platforms` for a
# native tool, so each platform's payload is fetched and checked, not only the
# host's — and each asset file is asserted under
# $HOME/.murmur/artifacts/murmur/<name>/<version>/. Versions an earlier release
# published are not installed: their assets are on that release, not in this
# directory.
#
# Usage:   scripts/check-release-installs.sh <release-dir> --release <tag>
# Output:  one line per install.
# Exit:    0 — every version installed
#          1 — an install failed; mur's output is shown. With no
#              artifacts-index.json in <release-dir> one install is still
#              attempted, so the failure is the one a consumer would see
#              (E-IDX-002)
#          2 — usage error
#
# `mur` is $MUR_BIN, else `mur` on PATH, and must have the `index` subcommand.
# Requires `jq` and `python3`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

OWNER_ID=190196424
REPO="murmur-nexus/default-artifacts"

usage() {
    echo "usage: $0 <release-dir> --release <tag>" >&2
    exit 2
}

RELEASE_DIR="" TAG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --release) [ $# -ge 2 ] || usage; TAG="$2"; shift 2 ;;
        -*) echo "error: unknown option '$1'" >&2; usage ;;
        *) [ -z "$RELEASE_DIR" ] || usage; RELEASE_DIR="$1"; shift ;;
    esac
done
[ -n "$RELEASE_DIR" ] && [ -n "$TAG" ] || usage

require_mur
MUR="$(cd "$(dirname "$MUR")" && pwd)/$(basename "$MUR")"
command -v jq >/dev/null || { echo "error: jq is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

if [ ! -d "$RELEASE_DIR" ]; then
    echo "error: $RELEASE_DIR is not a directory" >&2
    exit 1
fi
RELEASE_DIR="$(cd "$RELEASE_DIR" && pwd)"
INDEX="$RELEASE_DIR/artifacts-index.json"

WORK="$(mktemp -d)"
FAKE_PID=""
cleanup() {
    [ -z "$FAKE_PID" ] || kill "$FAKE_PID" 2>/dev/null || true
    [ -z "$FAKE_PID" ] || wait "$FAKE_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

cat > "$WORK/fake_github.py" <<'PY'
"""A loopback stand-in for the part of the GitHub REST API `mur install` reads."""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

release_dir, tag, repo, owner_id, port_file = sys.argv[1:6]
files = sorted(
    name for name in os.listdir(release_dir)
    if os.path.isfile(os.path.join(release_dir, name))
)
assets = [{"id": i + 1, "name": name} for i, name in enumerate(files)]
by_id = {str(asset["id"]): asset["name"] for asset in assets}
release = json.dumps({"tag_name": tag, "assets": assets}).encode()
prefix = f"/repos/{repo}"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == prefix:
            return self.send(200, json.dumps({"owner": {"id": int(owner_id)}}).encode())
        if path in (f"{prefix}/releases/latest", f"{prefix}/releases/tags/{tag}"):
            return self.send(200, release)
        if path.startswith(f"{prefix}/releases/assets/"):
            name = by_id.get(path.rsplit("/", 1)[1])
            if name is not None:
                with open(os.path.join(release_dir, name), "rb") as f:
                    return self.send(200, f.read(), "application/octet-stream")
        return self.send(404, b'{"message":"Not Found"}')

    def send(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file + ".tmp", "w") as f:
    f.write(str(server.server_address[1]))
os.rename(port_file + ".tmp", port_file)
server.serve_forever()
PY

python3 -I "$WORK/fake_github.py" "$RELEASE_DIR" "$TAG" "$REPO" "$OWNER_ID" "$WORK/port" &
FAKE_PID=$!
for _ in $(seq 1 100); do
    [ -f "$WORK/port" ] && break
    kill -0 "$FAKE_PID" 2>/dev/null || { echo "error: the fake GitHub API exited before listening" >&2; exit 1; }
    sleep 0.1
done
[ -f "$WORK/port" ] || { echo "error: the fake GitHub API did not start listening" >&2; exit 1; }

export HOME="$WORK/home"
mkdir -p "$HOME"
port="$(cat "$WORK/port")"
export MUR_GITHUB_API_BASE="http://127.0.0.1:$port"
unset GITHUB_TOKEN GH_TOKEN

# What to install: <name>\t<version>\t<native 0|1>\t<file> per asset of each version
# this release adds. With no index there is nothing to enumerate, so the first zip's
# name and version stand in, and the install below fails the way a consumer's would.
if [ -f "$INDEX" ]; then
    plan="$(jq -r --arg tag "$TAG" '
        .artifacts[] as $a | $a.versions[] | select(.release == $tag) as $v
        | ($v.assets | any(has("platform"))) as $native
        | $v.assets[] | [$a.name, $v.version, (if $native then 1 else 0 end), .file] | @tsv
    ' "$INDEX")"
    if [ -z "$plan" ]; then
        echo "error: $INDEX lists no version released as $TAG" >&2
        exit 1
    fi
else
    echo "warning: no artifacts-index.json in $RELEASE_DIR; attempting one install so mur reports what a consumer would see" >&2
    first="$(find "$RELEASE_DIR" -maxdepth 1 -name '*.mur.zip' -exec basename {} \; | LC_ALL=C sort | head -1)"
    if [[ ! "$first" =~ ^(.+)-([0-9]+\.[0-9]+\.[0-9]+)(-(darwin|linux)-(aarch64|x86_64))?\.mur\.zip$ ]]; then
        echo "error: no artifacts-index.json and no <name>-<version>.mur.zip in $RELEASE_DIR" >&2
        exit 1
    fi
    plan="$(printf '%s\t%s\t0\t%s' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "$first")"
fi

count=0
for key in $(printf '%s\n' "$plan" | awk -F'\t' '{ print $1 "@" $2 }' | LC_ALL=C sort -u); do
    name="${key%@*}" version="${key#*@}"
    native="$(printf '%s\n' "$plan" | awk -F'\t' -v n="$name" -v v="$version" '$1 == n && $2 == v { print $3; exit }')"
    flags=(-g)
    [ "$native" = "1" ] && flags+=(--all-platforms)
    if ! output="$(cd "$WORK" && "$MUR" install "${flags[@]}" "murmur/$name@$version" 2>&1)"; then
        echo "FAIL  murmur/$name@$version — mur install ${flags[*]} exited non-zero:" >&2
        printf '%s\n' "$output" | sed 's/^/    /' >&2
        exit 1
    fi
    store="$HOME/.murmur/artifacts/murmur/$name/$version"
    while IFS=$'\t' read -r _n _v _native file; do
        if [ ! -f "$store/$file" ]; then
            echo "FAIL  murmur/$name@$version — installed, but $store/$file is absent. mur printed:" >&2
            printf '%s\n' "$output" | sed 's/^/    /' >&2
            exit 1
        fi
    done < <(printf '%s\n' "$plan" | awk -F'\t' -v n="$name" -v v="$version" '$1 == n && $2 == v')
    files="$(printf '%s\n' "$plan" | awk -F'\t' -v n="$name" -v v="$version" '$1 == n && $2 == v { printf "%s%s", sep, $4; sep = ", " }')"
    echo "ok    murmur/$name@$version ($files)"
    count=$((count + 1))
done

echo "Installed $count version(s) released as $TAG through $("$MUR" version | head -1)."
