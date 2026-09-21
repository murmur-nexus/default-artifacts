#!/usr/bin/env bash
# Check that no artifact's published bytes changed while its version in
# artifacts.toml stayed put.
#
# A `.mur.zip` is served by name and version alone. `mur install` writes the
# payload's sha256 into the consumer's murmur.lock, so re-publishing different
# bytes at a version someone already resolved does not upgrade them — it makes
# their next resolve fail with "'<name>' is pinned at sha256 <old>, but <new>
# was resolved", and a store that already holds <name>/<version> never
# re-downloads at all. The fix is always a version bump, and it has to happen
# before the release, not after.
#
# So the rule this enforces is the one BUILD.md states for a WIT bump, applied
# to every input rather than just that one: if a change alters what an artifact
# ships, its version in artifacts.toml moves in the same commit.
#
# What counts as an input to an artifact's bytes:
#
#   own tree        Everything under the artifact's path that is packaged or
#                   compiled — murmur.yaml, src/, package.sh, skill.md.
#                   README.md, tests/, benches/, examples/ and nested crates are
#                   excluded: none of them reach the zip. A nested crate that is
#                   genuinely linked in comes back through `path deps` below.
#
#   path deps       Each `path = "../../libs/x"` dependency, transitively. These
#                   are compiled into the dependent component, so changing one
#                   changes every artifact that links it (BUILD.md, "the crates
#                   under libs/").
#
#   wit mirror      The `wit/<subtree>` each crate names in its bindgen
#                   `path:`, read out of the crate's own source rather than
#                   listed here. A rebuild against a bumped mirror exports a
#                   different interface version, so the bytes differ.
#
#   workspace       `[workspace.package]` and any `[profile.*]`, the
#                   `[workspace.dependencies]` entries this crate declares (a
#                   feature list is not in the lockfile), its own resolved
#                   third-party versions walked out of Cargo.lock, and
#                   rust-toolchain.toml. Narrowed to the one artifact on
#                   purpose: adding an unrelated crate to the workspace, or
#                   bumping workspace_version, changes neither its dependencies
#                   nor its bytes. The workspace crates' own version strings are
#                   dropped for the same reason — a built component does not
#                   embed them. Applies to crate artifacts only; a skill is docs
#                   and compiles nothing.
#
# Usage:
#   ./scripts/check-version-drift.sh [--base <ref>]   compare the working tree
#                                                     against a release
#                                                     (default: the latest v*
#                                                     tag behind HEAD)
#   ./scripts/check-version-drift.sh --audit          walk every v* tag and
#                                                     report each version that
#                                                     shipped twice with
#                                                     different bytes
#
# A dirty working tree is compared as it stands, via `git stash create` — so a
# bump can be checked before it is committed, which is when it is still cheap to
# get right. Untracked files are not in that snapshot: `git add` a newly created
# artifact file before relying on this.
#
# Exit codes:
#   0 — every artifact whose inputs changed also changed version
#   1 — at least one artifact changed its inputs at an unchanged version
#   2 — usage or repository error
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BASE_REF=""
MODE="compare"

while [ $# -gt 0 ]; do
    case "$1" in
        --base)
            [ $# -ge 2 ] || { echo "error: --base needs a ref" >&2; exit 2; }
            BASE_REF="$2"; shift 2 ;;
        --audit)
            MODE="audit"; shift ;;
        -h|--help)
            awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
        *)
            echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

# What gets compared against the release: the working tree when it differs from
# HEAD, otherwise HEAD itself. `git stash create` writes a throwaway commit
# object for the tree and changes nothing else — no stash entry, no index or
# worktree edit — and prints nothing when there is nothing to stash.
HEAD_REF="HEAD"
if [ "$MODE" = "compare" ] && ! git -C "$REPO_ROOT" diff --quiet HEAD 2>/dev/null; then
    snapshot=$(git -C "$REPO_ROOT" stash create 2>/dev/null || true)
    if [ -n "$snapshot" ]; then
        HEAD_REF="$snapshot"
        echo "Comparing the working tree (uncommitted changes included)."
    fi
fi

# The newest v* tag reachable from HEAD that is not the very commit being
# compared. Stepping over that one is what lets this run at tag time, where the
# checkout is the new tag and `git describe` would otherwise compare a release
# against itself. A working-tree snapshot is always a distinct commit, so a
# branch sitting on the last release tag still compares against that tag.
if [ "$MODE" = "compare" ] && [ -z "$BASE_REF" ]; then
    head_commit=$(git -C "$REPO_ROOT" rev-parse "$HEAD_REF^{commit}")
    candidate="HEAD"
    while :; do
        candidate=$(git -C "$REPO_ROOT" describe --tags --abbrev=0 --match 'v*' "$candidate" 2>/dev/null || true)
        [ -n "$candidate" ] || break
        if [ "$(git -C "$REPO_ROOT" rev-parse "$candidate^{commit}")" != "$head_commit" ]; then
            BASE_REF="$candidate"
            break
        fi
        candidate="$candidate^"
    done
    if [ -z "$BASE_REF" ]; then
        echo "error: no v* tag behind HEAD to compare against; pass --base <ref>" >&2
        exit 2
    fi
fi

python3 - "$REPO_ROOT" "$MODE" "$BASE_REF" "$HEAD_REF" <<'PYEOF'
import hashlib
import re
import subprocess
import sys

repo, mode, base_ref, head_ref = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

# --------------------------------------------------------------------------
# git plumbing, with the two caches that make --audit finish in seconds: a ref
# is read many times over, and a blob that did not change between two tags is
# the same object under both.
# --------------------------------------------------------------------------
_tree_cache = {}
_blob_cache = {}


def git(*args):
    return subprocess.run(
        ["git", "-C", repo, *args], capture_output=True, text=True, errors="replace"
    )


def ls_tree(ref, path):
    """{repo-relative path: blob sha} for every blob under `path` at `ref`."""
    key = (ref, path)
    if key in _tree_cache:
        return _tree_cache[key]
    out = git("ls-tree", "-r", "-z", ref, "--", path)
    entries = {}
    if out.returncode == 0:
        for record in out.stdout.split("\0"):
            if not record:
                continue
            meta, name = record.split("\t", 1)
            _mode, kind, sha = meta.split()
            if kind == "blob":
                entries[name] = sha
    _tree_cache[key] = entries
    return entries


def blob(sha):
    if sha not in _blob_cache:
        _blob_cache[sha] = git("cat-file", "blob", sha).stdout
    return _blob_cache[sha]


def show(ref, path):
    out = git("show", f"{ref}:{path}")
    return out.stdout if out.returncode == 0 else None


# --------------------------------------------------------------------------
# artifacts.toml — name, path and version, as apply-versions.sh reads them.
# --------------------------------------------------------------------------
def read_artifacts(ref):
    text = show(ref, "artifacts.toml")
    if text is None:
        return {}
    artifacts, current = {}, {}
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("[[artifact]]"):
            if current.get("name"):
                artifacts[current["name"]] = current
            current = {}
            continue
        match = re.match(r'^(name|path|version)\s*=\s*"([^"]*)"', line)
        if match:
            current[match.group(1)] = match.group(2)
    if current.get("name"):
        artifacts[current["name"]] = current
    return artifacts


# --------------------------------------------------------------------------
# Input collection
# --------------------------------------------------------------------------
EXCLUDED_DIRS = {"tests", "benches", "examples"}
WIT_PATH = re.compile(r'path:\s*"((?:\.\./)+wit/[A-Za-z0-9._-]+)"')
DEP_PATH = re.compile(r'path\s*=\s*"([^"]+)"')


def normalise(path):
    parts = []
    for part in path.split("/"):
        if part in ("", "."):
            continue
        if part == "..":
            if parts:
                parts.pop()
            continue
        parts.append(part)
    return "/".join(parts)


def own_tree(ref, root):
    """The artifact's own blobs, minus what never reaches the zip."""
    entries = ls_tree(ref, root)
    nested = {
        name.rsplit("/", 1)[0]
        for name in entries
        if name.endswith("/Cargo.toml") and name != f"{root}/Cargo.toml"
    }
    kept = {}
    for name, sha in entries.items():
        rest = name[len(root) + 1:]
        parts = rest.split("/")
        if parts[-1] == "README.md" or rest.endswith(".mur.zip"):
            continue
        if any(part in EXCLUDED_DIRS for part in parts[:-1]):
            continue
        if any(name.startswith(d + "/") for d in nested):
            continue
        kept[name] = sha
    return kept


def crate_inputs(ref, root, seen):
    """`root`'s blobs plus, transitively, those of its path dependencies."""
    root = normalise(root)
    if root in seen:
        return {}
    seen.add(root)
    entries = own_tree(ref, root)
    manifest = entries.get(f"{root}/Cargo.toml")
    if manifest:
        for dep in DEP_PATH.findall(blob(manifest)):
            if dep.startswith(("/", "http")):
                continue
            entries.update(crate_inputs(ref, f"{root}/{dep}", seen))
    return entries


def wit_trees(ref, entries):
    """The wit subtrees the collected sources generate bindings from."""
    trees = {}
    for name, sha in entries.items():
        if not name.endswith(".rs") or "/src/" not in name:
            continue
        crate_dir = name.split("/src/", 1)[0]
        for rel in WIT_PATH.findall(blob(sha)):
            trees.update(ls_tree(ref, normalise(f"{crate_dir}/{rel}")))
    return trees


_workspace_cache = {}


def workspace_state(ref):
    """The workspace-wide build inputs at `ref`, parsed once per ref.

    Everything here is reduced to the parts that reach a built component, and
    then narrowed per artifact by `workspace_inputs` below. `[workspace]
    members`, a workspace crate's own Cargo.lock block and
    `[workspace.package] version` are all dropped: they name crates, not code,
    and `wasm-tools metadata show` on a built artifact carries no workspace
    version string. Without that reduction, adding a crate or bumping
    workspace_version would demand a bump of every artifact.
    """
    if ref in _workspace_cache:
        return _workspace_cache[ref]

    # [workspace.package] without its version, plus any [profile.*]: edition,
    # MSRV and codegen settings all change the emitted code.
    cargo_toml = show(ref, "Cargo.toml") or ""
    shared, workspace_deps = [], {}
    section = ""
    for line in cargo_toml.splitlines():
        if line.startswith("["):
            section = line.strip()
            if section in ("[workspace.package]",) or section.startswith("[profile"):
                shared.append(line)
            continue
        if section == "[workspace.package]":
            if not re.match(r"^version\s*=", line):
                shared.append(line)
        elif section == "[workspace.dependencies]":
            match = re.match(r"^([A-Za-z0-9_-]+)\s*=", line)
            if match:
                workspace_deps[match.group(1)] = line

    # Cargo.lock as {(name, version): {"source": str, "deps": [str]}}. Blocks
    # with no `source =` are workspace members — kept as graph nodes so the walk
    # can cross them, and excluded from the digest at the end.
    packages, by_name = {}, {}
    name = version = source = None
    deps, in_deps = [], False
    lock = (show(ref, "Cargo.lock") or "").splitlines()

    def flush():
        if name is not None:
            key = (name, version)
            packages[key] = {"source": source, "deps": list(deps)}
            by_name.setdefault(name, []).append(key)

    for line in lock + ["[[package]]"]:
        if line.startswith("[[package]]"):
            flush()
            name = version = source = None
            deps, in_deps = [], False
            continue
        if in_deps:
            if line.strip().startswith("]"):
                in_deps = False
            else:
                deps.append(line.strip().strip(",").strip('"'))
            continue
        if line.startswith("name = "):
            name = line[7:].strip().strip('"')
        elif line.startswith("version = "):
            version = line[10:].strip().strip('"')
        elif line.startswith("source = "):
            source = line[9:].strip().strip('"')
        elif line.startswith("dependencies = ["):
            in_deps = True

    state = {
        "shared": "\n".join(shared),
        "workspace_deps": workspace_deps,
        "packages": packages,
        "by_name": by_name,
        "toolchain": show(ref, "rust-toolchain.toml") or "",
    }
    _workspace_cache[ref] = state
    return state


def locked_closure(state, crate):
    """Every third-party (name, version) `crate` resolves to, transitively."""
    keys = state["by_name"].get(crate)
    if not keys:
        return None
    seen, stack, external = set(), list(keys), set()
    while stack:
        key = stack.pop()
        if key in seen or key not in state["packages"]:
            continue
        seen.add(key)
        package = state["packages"][key]
        if package["source"]:
            external.add(key)
        for dep in package["deps"]:
            parts = dep.split()
            if len(parts) == 2:
                stack.append((parts[0], parts[1]))
            else:
                stack.extend(state["by_name"].get(parts[0], []))
    return external


def workspace_inputs(ref, crate, declared):
    """The workspace inputs that reach this one artifact: the shared manifest
    settings, the `[workspace.dependencies]` entries it actually declares (their
    feature lists are not in the lockfile), its resolved third-party versions,
    and the toolchain that compiles it."""
    state = workspace_state(ref)
    parts = [state["shared"]]
    parts.append(
        "\n".join(state["workspace_deps"][d] for d in sorted(declared) if d in state["workspace_deps"])
    )
    closure = locked_closure(state, crate)
    parts.append(
        "\n".join(f"{n} {v}" for n, v in sorted(closure)) if closure is not None else "<unlocked>"
    )
    parts.append(state["toolchain"])
    return parts


WORKSPACE_DEP = re.compile(r"^([A-Za-z0-9_-]+)\s*\.\s*workspace\s*=\s*true")


def declared_workspace_deps(entries):
    """The `x.workspace = true` dependency names across the collected crates."""
    names = set()
    for name, sha in entries.items():
        if name.endswith("/Cargo.toml"):
            for line in blob(sha).splitlines():
                match = WORKSPACE_DEP.match(line.strip())
                if match:
                    names.add(match.group(1))
    return names


def fingerprint(ref, artifact):
    """A digest per input class, so a mismatch names what moved."""
    root = normalise(artifact["path"])
    own = own_tree(ref, root)
    if not own:
        return None

    deps = crate_inputs(ref, root, set())
    for name in own:
        deps.pop(name, None)

    is_crate = f"{root}/Cargo.toml" in own
    classes = {
        "own tree": own,
        "path deps": deps,
        "wit mirror": wit_trees(ref, {**own, **deps}) if is_crate else {},
    }

    digests = {}
    for label, entries in classes.items():
        joined = "\n".join(f"{sha} {name}" for name, sha in sorted(entries.items()))
        digests[label] = hashlib.sha256(joined.encode()).hexdigest()
    if is_crate:
        all_entries = {**own, **deps}
        joined = "\0".join(
            workspace_inputs(ref, artifact["name"], declared_workspace_deps(all_entries))
        )
        digests["workspace"] = hashlib.sha256(joined.encode()).hexdigest()
    return digests


def tags():
    out = git("tag", "--list", "v*", "--sort=creatordate")
    return [t for t in out.stdout.split() if t]


# --------------------------------------------------------------------------
# Modes
# --------------------------------------------------------------------------
def compare(base, head):
    base_artifacts = read_artifacts(base)
    head_artifacts = read_artifacts(head)
    if not head_artifacts:
        print(f"error: no [[artifact]] entries at {head}", file=sys.stderr)
        return 2

    drifted, checked = [], 0
    for name, entry in sorted(head_artifacts.items()):
        was = base_artifacts.get(name)
        if was is None:
            continue
        if was["version"] != entry["version"]:
            continue
        before, after = fingerprint(base, was), fingerprint(head, entry)
        if before is None or after is None:
            continue
        checked += 1
        changed = [k for k in after if before.get(k) != after[k]]
        if changed:
            drifted.append((name, entry["version"], changed))

    print(f"Compared {checked} artifact(s) held at the same version since {base}.")
    if not drifted:
        print("No drift: every artifact whose inputs changed also changed version.")
        return 0

    print("")
    print(f"error: {len(drifted)} artifact(s) changed what they ship without a version bump:")
    for name, version, changed in drifted:
        print(f"  {name}@{version} — changed: {', '.join(changed)}")
    print("")
    print("Bump each one's version in artifacts.toml, run ./scripts/apply-versions.sh,")
    print("and commit the result before tagging. Re-publishing different bytes at a")
    print("version consumers already resolved breaks their murmur.lock sha256 pin.")
    return 1


def audit():
    history = {}
    for tag in tags():
        for name, entry in read_artifacts(tag).items():
            digests = fingerprint(tag, entry)
            if digests is None:
                continue
            key = hashlib.sha256(
                "\0".join(f"{k}={v}" for k, v in sorted(digests.items())).encode()
            ).hexdigest()
            history.setdefault((name, entry["version"]), []).append((tag, key, digests))

    findings = 0
    for (name, version), shipped in sorted(history.items()):
        if len({key for _tag, key, _d in shipped}) == 1:
            continue
        findings += 1
        print(f"{name}@{version} shipped different bytes across releases:")
        previous = None
        for tag, key, digests in shipped:
            if previous is None:
                print(f"  {tag}  (first published)")
            elif key == previous[1]:
                print(f"  {tag}  unchanged")
            else:
                changed = [k for k in digests if previous[2].get(k) != digests[k]]
                print(f"  {tag}  CHANGED — {', '.join(changed)}")
            previous = (tag, key, digests)
        print("")

    if findings == 0:
        print("No release published two different payloads at one artifact version.")
        return 0
    print(f"error: {findings} artifact version(s) were published twice with different bytes.")
    return 1


sys.exit(audit() if mode == "audit" else compare(base_ref, head_ref))
PYEOF
