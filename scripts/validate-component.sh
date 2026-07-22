#!/usr/bin/env bash
#
# validate-component.sh — post-build hygiene check for a single WASM artifact.
#
# For one built `.wasm`, this runs:
#   1. `wasm-tools validate`          — confirms it is a well-formed component.
#   2. `wasm-tools component wit`     — confirms its world-level imports/exports
#                                       match the expected shape for its category.
#
# Category is derived from the artifact's crate/file name (hyphen- and
# underscore-insensitive), NOT from an argument, so the same mapping is enforced
# identically in both `ci.yml` (which loops over every `.wasm`) and `build.yml`
# (which calls this once per matrix entry):
#
#   murmur-hook-*                      -> HOOK   category
#   murmur-driver-*                    -> TOOL   category
#   murmur-tool-request-input          -> TOOL   category
#   murmur-tool-{create,editor}        -> TOOL   category (ported to wasm32-wasip2
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
#   HOOK: export set == { murmur:hook/lifecycle }; ZERO murmur:* imports.
#   TOOL: export set == { murmur:tool/run };       murmur:* imports subset of
#         { murmur:text/chunks, murmur:task/task }.
#
# Usage:   scripts/validate-component.sh <path-to-.wasm>
# Exit:    0 = pass (or a deliberately-skipped native artifact)
#          1 = validation/shape failure (message names the artifact + the
#              specific unexpected import/export)
#          2 = usage error / unrecognised artifact name
#
# Requires `wasm-tools` on PATH (CI pins an exact version; see the workflows).

set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <path-to-.wasm>" >&2
  exit 2
fi

wasm="$1"
base="$(basename "$wasm" .wasm)"
# Normalise underscores (cdylib output) and hyphens (bin output) to a single form.
name="${base//_/-}"

# ---- category resolution -----------------------------------------------------
case "$name" in
  murmur-tool-git|murmur-tool-git-validate|murmur-tool-registry-search)
    echo "skip: $base is a native command component (exports wasi:cli/run), not a murmur guest component — not validated"
    exit 0
    ;;
  murmur-hook-*)
    category="hook"
    expected_export="murmur:hook/lifecycle"
    ;;
  murmur-driver-*|murmur-tool-request-input|murmur-tool-create|murmur-tool-editor)
    category="tool"
    expected_export="murmur:tool/run"
    ;;
  *)
    echo "FAIL: $base — unrecognised artifact name; refusing to skip validation (add it to the category map in scripts/validate-component.sh)" >&2
    exit 2
    ;;
esac

echo "== validating $base (category: $category) =="

# ---- 1. structural validation ------------------------------------------------
if ! wasm-tools validate "$wasm"; then
  echo "FAIL: $base — wasm-tools validate rejected the component (see error above)" >&2
  exit 1
fi
echo "  wasm-tools validate: OK"

# ---- 2. world-level import/export extraction ---------------------------------
# `wasm-tools component wit` prints the component's own `world root { ... }` block
# first, followed by the referenced package/interface definitions. We only want
# the world-level `import`/`export` lines from that first block. The block has no
# nested braces (each import/export is a single `... ;` line), so the first `}`
# after `world root {` closes it.
wit="$(wasm-tools component wit "$wasm")"
world="$(printf '%s\n' "$wit" | awk '/^world root \{/{f=1;next} f&&/^\}/{f=0} f')"

# Interface ids with the @version suffix and trailing `;` stripped (versions are
# intentionally not gated). awk (not sed) for portability: BSD sed treats `\+` as
# a literal `+`, so a sed-based extraction silently matches nothing on macOS.
imports="$(printf '%s\n' "$world" | awk '$1=="import"{sub(/;$/,"",$2); sub(/@.*/,"",$2); print $2}')"
exports="$(printf '%s\n' "$world" | awk '$1=="export"{sub(/;$/,"",$2); sub(/@.*/,"",$2); print $2}')"

murmur_imports="$(printf '%s\n' "$imports" | grep '^murmur:' || true)"
murmur_exports="$(printf '%s\n' "$exports" | grep '^murmur:' || true)"

fail=0

# ---- 3. export check ---------------------------------------------------------
# Exactly the one expected murmur export, and no other murmur export.
if [ "$(printf '%s\n' "$murmur_exports" | grep -c .)" -eq 0 ]; then
  echo "FAIL: $base ($category): missing expected export '$expected_export' (component exports: ${exports//$'\n'/, })" >&2
  fail=1
else
  while IFS= read -r e; do
    [ -n "$e" ] || continue
    if [ "$e" != "$expected_export" ]; then
      echo "FAIL: $base ($category): unexpected export '$e' — a $category component must export exactly '$expected_export'" >&2
      fail=1
    fi
  done <<< "$murmur_exports"
  if ! printf '%s\n' "$murmur_exports" | grep -qx "$expected_export"; then
    echo "FAIL: $base ($category): expected export '$expected_export' not present" >&2
    fail=1
  fi
fi

# ---- 4. murmur:* import check ------------------------------------------------
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
  exit 1
fi

n_wasi="$(printf '%s\n' "$imports" | grep -c '^wasi:' || true)"
n_murmur="$(printf '%s\n' "$murmur_imports" | grep -c . || true)"
echo "  export: $expected_export — OK"
echo "  imports: $n_wasi wasi:*, $n_murmur murmur:* — OK"
echo "PASS: $base"
