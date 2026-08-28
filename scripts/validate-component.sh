#!/usr/bin/env bash
#
# validate-component.sh — post-build hygiene check for built WASM artifacts.
#
# For each built `.wasm`, this runs:
#   1. `wasm-tools validate`          — confirms it is a well-formed component.
#   2. `wasm-tools component wit`     — confirms its world-level imports/exports
#                                       match the expected shape for its category,
#                                       and that the exported murmur interface is
#                                       the exact version the vendored WIT declares.
#
# Category is derived from the artifact's crate/file name (hyphen- and
# underscore-insensitive), NOT from an argument, so the same mapping is enforced
# identically in both `ci.yml` and `build.yml` (which calls this once per matrix
# entry):
#
#   murmur-hook-*                      -> HOOK   category
#   murmur-driver-*                    -> TOOL   category
#   murmur-tool-request-input          -> TOOL   category
#   murmur-tool-{create,editor,corpus} -> TOOL   category (ported to wasm32-wasip2
#                                               components that export murmur:tool/run;
#                                               they import zero murmur:* interfaces)
#   murmur-tool-{git,git-validate,     -> SKIP   (still native bin crates cross-compiled
#     registry-search}                          to wasm by the workspace build;
#                                               shipped as native binaries, they
#                                               export wasi:cli/run and are NOT
#                                               murmur guest components)
#   anything else                      -> ERROR  (fail closed: an unrecognised
#                                               artifact must not silently skip)
#
# Expected shape per category (versions on wasi:* imports are deliberately NOT
# gated — allowing any wasi:*@x.y.z is the whole point of this slice; pinning the
# toolchain is what stabilises those versions, and gating on murmur:* namespace
# membership is what catches an artifact importing something the host won't link):
#
#   HOOK: export set == { murmur:hook/lifecycle@<hook-wit-version> }; ZERO murmur:* imports.
#   TOOL: export set == { murmur:tool/run@<tool-wit-version> };       murmur:* imports subset of
#         { murmur:text/chunks, murmur:task/task }.
#
# The exported interface's version is read from the vendored WIT that the
# components are built against ($HOOK_WIT / $TOOL_WIT below), never hardcoded, so
# a WIT package bump propagates here with no edit to this script. The host links
# exactly one version of each interface, so a component built against a stale
# vendored WIT is unloadable at `mur run` — this check is what catches it at
# build time instead.
#
# Usage:   scripts/validate-component.sh [<path-to-.wasm>]
#          With a path: validate that one artifact.
#          With no argument: validate every .wasm under
#          target/wasm32-wasip2/release/ (this is what CI runs).
# Exit:    0 = pass (or a deliberately-skipped native artifact)
#          1 = validation/shape/version failure (message names the artifact + the
#              specific unexpected import/export/version)
#          2 = usage error / unrecognised artifact name / no components to validate
#
# Requires `wasm-tools` on PATH (CI pins an exact version; see the workflows).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$REPO_ROOT/target/wasm32-wasip2/release"

# The vendored WIT each category's components are generated from. These are the
# files `check-wit-sync.sh` keeps byte-identical to murmur's own copies, so the
# version read here is the version the host will link.
HOOK_WIT="$REPO_ROOT/wit/hook/deps/murmur-hook/lifecycle.wit"
TOOL_WIT="$REPO_ROOT/wit/guest/deps/murmur-tool/tool.wit"

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [<path-to-.wasm>]" >&2
  exit 2
fi

# ---- expected interface versions --------------------------------------------
# Prints the version from a vendored WIT's `package <name>@<version>;` line, or
# nothing if that package carries no version (in which case the export check
# falls back to comparing the interface name alone). A missing file or a package
# line naming a different package is a hard error: silently dropping the version
# assertion is exactly the blind spot this check exists to close.
wit_package_version() {
  local file="$1" pkg="$2" decl
  if [ ! -f "$file" ]; then
    echo "FAIL: vendored WIT not found: $file (expected it to declare 'package $pkg@X.Y.Z;')" >&2
    exit 2
  fi
  decl="$(awk '$1=="package"{sub(/;$/,"",$2); print $2; exit}' "$file")"
  case "$decl" in
    "$pkg"@*) printf '%s' "${decl#*@}" ;;
    "$pkg")   printf '' ;;
    *)
      echo "FAIL: $file declares package '$decl'; expected '$pkg'" >&2
      exit 2
      ;;
  esac
}

hook_version="$(wit_package_version "$HOOK_WIT" "murmur:hook")"
tool_version="$(wit_package_version "$TOOL_WIT" "murmur:tool")"

# ---- single-artifact validation ----------------------------------------------
# Returns 0 pass / 1 validation failure / 2 unrecognised artifact name.
validate_one() {
  local wasm="$1"
  local base name category expected_export expected_version
  base="$(basename "$wasm" .wasm)"
  # Normalise underscores (cdylib output) and hyphens (bin output) to a single form.
  name="${base//_/-}"

  # ---- category resolution ---------------------------------------------------
  case "$name" in
    murmur-tool-git|murmur-tool-git-validate|murmur-tool-registry-search)
      echo "skip: $base is a native command component (exports wasi:cli/run), not a murmur guest component — not validated"
      return 0
      ;;
    murmur-hook-*)
      category="hook"
      expected_export="murmur:hook/lifecycle"
      expected_version="$hook_version"
      ;;
    murmur-driver-*|murmur-tool-request-input|murmur-tool-create|murmur-tool-editor|murmur-tool-corpus)
      category="tool"
      expected_export="murmur:tool/run"
      expected_version="$tool_version"
      ;;
    *)
      echo "FAIL: $base — unrecognised artifact name; refusing to skip validation (add it to the category map in scripts/validate-component.sh)" >&2
      return 2
      ;;
  esac

  echo "== validating $base (category: $category) =="

  # ---- 1. structural validation ----------------------------------------------
  if ! wasm-tools validate "$wasm"; then
    echo "FAIL: $base — wasm-tools validate rejected the component (see error above)" >&2
    return 1
  fi
  echo "  wasm-tools validate: OK"

  # ---- 2. world-level import/export extraction -------------------------------
  # `wasm-tools component wit` prints the component's own `world root { ... }` block
  # first, followed by the referenced package/interface definitions. We only want
  # the world-level `import`/`export` lines from that first block. The block has no
  # nested braces (each import/export is a single `... ;` line), so the first `}`
  # after `world root {` closes it.
  local wit world imports exports exports_versioned murmur_imports murmur_exports
  wit="$(wasm-tools component wit "$wasm")"
  world="$(printf '%s\n' "$wit" | awk '/^world root \{/{f=1;next} f&&/^\}/{f=0} f')"

  # Interface ids with the trailing `;` stripped. Exports keep their @version
  # suffix — that is what the version assertion below compares. Imports have it
  # stripped, since import versions are intentionally not gated. awk (not sed) for
  # portability: BSD sed treats `\+` as a literal `+`, so a sed-based extraction
  # silently matches nothing on macOS.
  imports="$(printf '%s\n' "$world" | awk '$1=="import"{sub(/;$/,"",$2); sub(/@.*/,"",$2); print $2}')"
  exports_versioned="$(printf '%s\n' "$world" | awk '$1=="export"{sub(/;$/,"",$2); print $2}')"
  exports="$(printf '%s\n' "$exports_versioned" | awk '{sub(/@.*/,""); print}')"

  murmur_imports="$(printf '%s\n' "$imports" | grep '^murmur:' || true)"
  murmur_exports="$(printf '%s\n' "$exports_versioned" | grep '^murmur:' || true)"

  local fail=0

  # ---- 3. export check -------------------------------------------------------
  # Exactly the one expected murmur export, at exactly the vendored WIT's version,
  # and no other murmur export.
  if [ "$(printf '%s\n' "$murmur_exports" | grep -c .)" -eq 0 ]; then
    echo "FAIL: $base ($category): missing expected export '$expected_export' (component exports: ${exports_versioned//$'\n'/, })" >&2
    fail=1
  else
    local e e_name e_version
    while IFS= read -r e; do
      [ -n "$e" ] || continue
      e_name="${e%%@*}"
      if [ "$e_name" = "$e" ]; then e_version=""; else e_version="${e#*@}"; fi
      if [ "$e_name" != "$expected_export" ]; then
        echo "FAIL: $base ($category): unexpected export '$e' — a $category component must export exactly '$expected_export'" >&2
        fail=1
      elif [ -n "$expected_version" ] && [ "$e_version" != "$expected_version" ]; then
        echo "FAIL: $base ($category): exports '$expected_export@${e_version:-<unversioned>}' but the vendored WIT declares '$expected_export@$expected_version' — the component was built against a stale WIT and must be rebuilt" >&2
        fail=1
      fi
    done <<< "$murmur_exports"
    if ! printf '%s\n' "$murmur_exports" | awk '{sub(/@.*/,""); print}' | grep -qx "$expected_export"; then
      echo "FAIL: $base ($category): expected export '$expected_export' not present" >&2
      fail=1
    fi
  fi

  # ---- 4. murmur:* import check ----------------------------------------------
  local i
  if [ "$category" = "hook" ]; then
    # Hooks may import only murmur:runtime/inference (the one-completion capability a
    # compaction hook uses); most hooks import no murmur:* interface at all.
    # murmur:hook/lifecycle rides along as a *type-only* instance whenever inference is
    # imported, because inference.wit does `use murmur:hook/lifecycle.{message}`. It
    # carries no functions, so wasmtime never asks the linker to satisfy it.
    while IFS= read -r i; do
      [ -n "$i" ] || continue
      case "$i" in
        murmur:runtime/inference|murmur:hook/lifecycle) ;;
        *)
          echo "FAIL: $base (hook): unexpected import '$i' — hook components may import only murmur:runtime/inference" >&2
          fail=1
          ;;
      esac
    done <<< "$murmur_imports"
  else
    # Tools/drivers may import only murmur:text/chunks and/or murmur:task/task.
    while IFS= read -r i; do
      [ -n "$i" ] || continue
      case "$i" in
        murmur:text/chunks|murmur:task/task) ;;
        *)
          echo "FAIL: $base (tool): unexpected import '$i' — tool/driver components may import only murmur:text/chunks and/or murmur:task/task" >&2
          fail=1
          ;;
      esac
    done <<< "$murmur_imports"
  fi

  if [ "$fail" -ne 0 ]; then
    return 1
  fi

  local n_wasi n_murmur
  n_wasi="$(printf '%s\n' "$imports" | grep -c '^wasi:' || true)"
  n_murmur="$(printf '%s\n' "$murmur_imports" | grep -c . || true)"
  echo "  export: $expected_export${expected_version:+@$expected_version} — OK"
  echo "  imports: $n_wasi wasi:*, $n_murmur murmur:* — OK"
  echo "PASS: $base"
  return 0
}

# ---- driver ------------------------------------------------------------------
if [ "$#" -eq 1 ]; then
  validate_one "$1" || exit $?
  exit 0
fi

# No argument: validate the whole build output. This is the single definition of
# "validate everything" — ci.yml calls it bare rather than re-inlining the loop.
shopt -s nullglob
wasms=("$BUILD_DIR"/*.wasm)
shopt -u nullglob

if [ "${#wasms[@]}" -eq 0 ]; then
  echo "FAIL: no .wasm found in $BUILD_DIR — run 'cargo build --workspace --target wasm32-wasip2 --release' first" >&2
  exit 2
fi

failed=()
unrecognised=0
for wasm in "${wasms[@]}"; do
  rc=0
  validate_one "$wasm" || rc=$?
  if [ "$rc" -ne 0 ]; then
    failed+=("$(basename "$wasm")")
    [ "$rc" -eq 2 ] && unrecognised=1
  fi
done

echo
echo "== validated ${#wasms[@]} components: $(( ${#wasms[@]} - ${#failed[@]} )) passed, ${#failed[@]} failed =="
if [ "${#failed[@]}" -ne 0 ]; then
  echo "failed: ${failed[*]}" >&2
  # An unrecognised name is a usage error (exit 2), not a shape failure, and
  # outranks it: the category map needs updating before anything else is trusted.
  [ "$unrecognised" -eq 1 ] && exit 2
  exit 1
fi
