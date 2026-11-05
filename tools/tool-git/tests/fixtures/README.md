# Fixtures

## `native-0.6.0-results.json`

What the native tool-git 0.6.0 binary answered for every call of `tests/common/sequence.rs`, run
over a fresh `tests/common/fixture.rs` work folder, and the argv of every git command it ran. Each
answer is reduced as murmur's native dispatch reduced the binary's stdout (`status`, `summary`, and
`data` as a string or compact JSON) and normalised by `tests/common/normalise.rs`: the work folder
becomes `<WORK>`, each 40-hex hash `<C1>`, `<C2>`, … in first-seen order with its abbreviations
`<Cn:len>`, an abbreviation of an object whose full hash never appears `<H1>`, …, and each
ISO-8601 date-time `<DATE>`.

`cargo test -p tool-git --test parity` compares the component's answers and argv with it. Never edit
it by hand; re-record it:

```sh
# 1. The native binary, built from the last native commit of this repository.
mkdir -p /space/scratch/tool-git-native
git archive 95e17e1 | tar -x -C /space/scratch/tool-git-native
find /space/scratch/tool-git-native -exec touch {} +   # so cargo rebuilds everything
(cd /space/scratch/tool-git-native && cargo build --release -p tool-git)

# 2. Record. The test runs the binary with the environment the test host gives git
#    (PATH, an empty HOME, GIT_CONFIG_NOSYSTEM=1), puts a `git` that logs its argv first on
#    PATH, and writes this file.
TOOL_GIT_NATIVE_BIN=/space/scratch/tool-git-native/target/release/tool-git \
  cargo test -p tool-git --test parity record_native -- --ignored
```

## The native zip for `mur_run_parity`

`tests/mur_run_parity.rs` runs the native build beside the component under the same `mur`. Pack it
from the same scratch tree, with a `mur` at WIT level 4 first on `PATH`:

```sh
cd /space/scratch/tool-git-native
PATH=<dir of a WIT level 4 mur>:$PATH ./scripts/package-artifact.sh tools/tool-git /space/scratch/zips
# → /space/scratch/zips/tool-git-0.6.0-<platform>.mur.zip, passed as TOOL_GIT_NATIVE_ZIP
```
