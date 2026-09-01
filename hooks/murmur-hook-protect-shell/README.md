# murmur-hook-protect-shell

Refuses a shell call whose recognized write form targets a protected path, with a
reason naming the path, the write form and the pattern that refused it.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-shell` · mode: blocking · commit policy: `deny`.

Its sibling, [`murmur-hook-protect-tool`](../murmur-hook-protect-tool), applies the
same protected-path list to tool calls. The two share a matcher at build time and
are independent at runtime: neither requires the other to be installed, neither
reads the other's config, and either alone is a complete gate for its own event.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-protect-shell
    version: 0.1.0
    runtime: hook
    config:
      protect:
        - "tests/"
        - "conftest.py"
      allow:
        - "tests/fixtures/"
      shell_write_binaries:
        - "black"
```

No `capabilities:` block is required — the hook reads no file and makes no
network call. It judges paths as text, and holds no filesystem grant to
canonicalize them with.

An entry with **no** `config:` key runs the defaults below. That is not the same
as `config: {}` in meaning, but it produces the same policy.

| Key | Type | Required | Default |
|---|---|---|---|
| `protect` | list of glob strings | optional, at least one entry when present | `["tests/", "test_*", "*_test.*", "spec/", "conftest.py"]` |
| `allow` | list of glob strings, checked first; a match here is never refused | optional | `[]` |
| `shell_write_binaries` | list of extra binary basenames whose non-flag argv entries are write targets | optional | `[]` |

`tools` belongs to the tool half and is an unknown key here.

## Write forms recognized

Decided on `binary`, `argv` and `script` — never on `command`, which the runtime
clips to 200 characters and marks display only. A `-c` script is tokenized into
commands on `;`, `&&`, `||`, `|` and newline, and into words on whitespace
respecting single and double quotes.

| Form | Write target |
|---|---|
| `sed -i` / `--in-place` | each file operand (the script operand is not a file) |
| `>`, `>>`, `N>`, `N>>`, `&>`, `>\|` redirection | the target word (`2>&1` names a descriptor, not a file) |
| `tee` | each operand |
| `patch` | the file operand, or `-o`'s value |
| `cp`, `mv`, `install`, `ln` | the destination — the last non-flag argument, or `-t`'s value; a destination written as a directory also covers each source's basename beneath it |
| `rm` | each operand |
| `truncate` | each operand |
| `dd` | the value of `of=` |
| `git checkout -- <paths>` | each path after the `--` |
| `git restore <paths>` | each path operand |
| `git apply` | unreadable — see below |
| any basename in `shell_write_binaries` | each non-flag operand |

A leading `VAR=value` assignment or a leading `env` is stripped, so
`FOO=1 sed -i s/a/b/ tests/x.py` is still recognized.

**Two forms name their targets inside data this hook cannot read**: `git apply`
(without `--check`/`--stat`/`--numstat`/`--summary`) and a `patch` invocation with
no file operand. Both are refused, on the same fail-closed reasoning as an
escaping path: a write whose target the policy cannot see is one it cannot judge.
Apply such a change with the editor tool, or name the file on the command line.

## Glob semantics

| Pattern shape | Meaning | Example |
|---|---|---|
| No `/` at all | matches the **basename** only | `conftest.py`, `test_*`, `*_test.*` |
| Trailing `/`, no internal `/` | matches any path with that **directory component** anywhere | `tests/` matches `tests/a.py` and `pkg/tests/a.py` |
| Contains an internal `/` | **anchored at the workdir root** | `src/tests/*.py` |
| Internal `/` and a trailing `/` | anchored, and covers everything beneath | `src/tests/` matches `src/tests/deep/a.py` |

`*` matches within one component and never crosses `/`. `**` is a whole component
and matches zero or more components. `?` matches one non-`/` character. There is
no escaping and no character class. The matcher is hand-rolled and
non-backtracking — a regex engine here would be a fail-closed hazard, since a
pattern that outran the hook's epoch deadline would refuse every call for the rest
of the run.

## Anchoring, escape, and the absolute-path limit

Paths are judged lexically. Normalization splits on `/`, drops empty and `.`
components, and pops on `..`; a relative path is read as relative to the workdir
root, which is what root-anchored patterns anchor to.

A path whose `..` popping rises above its own root **escapes**, and an escaping
path is refused outright — a target the policy cannot anchor is one it cannot
judge, and the fail-closed rule decides it. The escape check runs before `allow`,
because with no anchor there is no path for an `allow` pattern to match.

**An absolute path does not escape: it is matched on basename and
directory-component patterns, and a root-anchored pattern misses it.** The hook
cannot know where the workdir sits in the host filesystem, so `/w/tests/a.py` is
still caught by `tests/` and by `test_*`, but never by `src/tests/*.py`.

## Refusal reasons

The reason is pinned verbatim into what the model is shown, so it is written for
an agent to adapt to rather than retry.

| Case | Reason |
|---|---|
| Protected path | `murmur-hook-protect-shell: 'sed -i' would write 'tests/test_auth.py', which the protected-path pattern 'tests/' refuses. Change the code under test, not the test.` |
| Escaping path | `murmur-hook-protect-shell: 'rm' would write '../../etc/passwd', which escapes the capsule workdir. A path the policy cannot anchor is one it cannot judge, so it is refused. Use a path inside the workdir.` |
| Unusable config | `murmur-hook-protect-shell: configuration error — unknown key 'protet'; expected one of: allow, protect, shell_write_binaries. Every gated call is refused until this artifact's config: block is fixed. This is a configuration fault, not a protected-path match.` |
| Unreadable target | `murmur-hook-protect-shell: 'git apply' writes files this hook cannot name — the files it writes are named inside the patch, which this hook cannot read. A write whose target the policy cannot read is refused. Make the edit with the editor tool, or name the file on the command line.` |

An unknown key, a value of the wrong type, an empty `protect`, or a malformed
pattern makes the config unusable, and **every** gated call is then refused with
the configuration reason — including calls that name no protected path. Refusing
at stage time instead would be louder and cheaper, but it is not reachable from an
artifact: the runtime dispatches `on-stage` only to hooks bound `on-stage` or
`all`, a `deny` policy requires an explicit `on-tool-call`/`on-shell` binding, and
an `on-stage` error is logged and discarded rather than fatal. Denying at the
first gated call is the loudest behaviour available here.

## What this hook does not do

**This half is best-effort and cannot be made airtight.** It is a tokenizer, not a
shell parser, and no sandbox is built out of string matching.

- **An interpreter in `shell.allow` defeats it.** With `shell.allow: [python3]`,
  python is a general-purpose write primitive, and `on-shell` sees the script text
  of a `-c` invocation but never the body of a script *file*.
- **A build-tool recipe is not resolved into its body**, so `make test` reaches the
  hook as `make test`, and any recipe indirection bypasses the policy. The runtime
  states the same thing about `argv`: `make <target>`, `just <recipe>` and
  `npm run <script>` arrive unresolved.
- A write form outside the table above is not a write as far as this hook is
  concerned. So is a heredoc body, and so is a command reached through `xargs`,
  `find -exec`, a shell function or a subshell whose body it did not tokenize as a
  command.
- **The capability grant is the seal and this hook is the guard rail.** For a
  benchmark where the result must be trustworthy, the grant is what you tighten,
  and an operator who installs this believing it is airtight is worse off than one
  who knows it is not.
- It keeps no state across calls: there is no three-strikes rule, and the same
  refused call is refused identically every time.
- It only narrows. There is no permit arm, and nothing here widens what the
  capsule manifest granted.

## Cost

A call touching no protected path produces the same result, the same trace and the
same shell output as with neither hook installed, and the hook does no I/O and
makes no host call on that path. Installing any policy hook does make the runtime
resolve each call and dispatch to the hook — that cost is inherent to the seam.

See [murmur.yaml](./murmur.yaml) for the full manifest.
