# murmur-hook-protect-tool

Refuses a tool call that would write a protected path, with a reason naming the
path and the pattern that refused it.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-tool-call` · mode: blocking · commit policy: `deny`.

Its sibling, [`murmur-hook-protect-shell`](../murmur-hook-protect-shell), applies
the same protected-path list to shell calls. The two share a matcher at build
time and are independent at runtime: neither requires the other to be installed,
neither reads the other's config, and either alone is a complete gate for its own
event.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-protect-tool
    version: 0.1.0
    runtime: hook
    config:
      protect:
        - "tests/"
        - "conftest.py"
      allow:
        - "tests/fixtures/"
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
| `tools` | list of `{ match, path_keys, write_when? }` rules | optional | the two rules below |

`shell_write_binaries` belongs to the shell half and is an unknown key here.

### Tool rules

```yaml
tools:
  - match: murmur-tool-editor
    path_keys: [path]
    write_when: { key: operation, any_of: [write_file, replace_in_file] }
  - match: murmur-tool-create
    path_keys: [name, path]
```

`match` is a tool name, exact or glob, and must not contain a `/`. `path_keys`
names the input keys whose string values are write targets. `write_when` names an
input key and the values that mean "this call writes"; with no `write_when`, every
`path_keys` hit is a write.

**A tool matching no rule is not gated at all** — it behaves exactly as with this
hook uninstalled. To gate a different editing tool, add a rule naming its tool
name and the input key that carries the path.

Reads are never refused. `murmur-tool-editor`'s `read_file` and `find_in_files`
are outside the default rule's `write_when`, because refusing an agent's reads of
a test file would break every capsule that installs this and protect nothing. The
rule is about writing the test, not seeing it.

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
| Protected path | `murmur-hook-protect-tool: 'murmur-tool-editor' would write 'tests/test_auth.py', which the protected-path pattern 'tests/' refuses. Change the code under test, not the test.` |
| Escaping path | `murmur-hook-protect-tool: 'murmur-tool-editor' would write '../../etc/passwd', which escapes the capsule workdir. A path the policy cannot anchor is one it cannot judge, so it is refused. Use a path inside the workdir.` |
| Unusable config | `murmur-hook-protect-tool: configuration error — unknown key 'protet'; expected one of: allow, protect, tools. Every gated call is refused until this artifact's config: block is fixed. This is a configuration fault, not a protected-path match.` |
| Unreadable input | `murmur-hook-protect-tool: 'murmur-tool-editor' is gated by a protected-path rule, but its input is not valid JSON (…), so the write target cannot be read. A call whose write target the policy cannot read is refused.` |

An unknown key, a value of the wrong type, an empty `protect`, or a malformed
pattern makes the config unusable, and **every** gated call is then refused with
the configuration reason — including calls that name no protected path. Refusing
at stage time instead would be louder and cheaper, but it is not reachable from an
artifact: the runtime dispatches `on-stage` only to hooks bound `on-stage` or
`all`, a `deny` policy requires an explicit `on-tool-call`/`on-shell` binding, and
an `on-stage` error is logged and discarded rather than fatal. Denying at the
first gated call is the loudest behaviour available here.

## What this hook does not do

- **The capability grant is the seal and this hook is the guard rail.** For a
  benchmark where the result must be trustworthy, the grant is what you tighten,
  and an operator who installs this believing it is airtight is worse off than one
  who knows it is not.
- This half gates tool calls only. A capsule that also grants `shell` needs
  [`murmur-hook-protect-shell`](../murmur-hook-protect-shell) as well — and **that
  half is best-effort and cannot be made airtight**:
  - **An interpreter in `shell.allow` defeats it.** With `shell.allow: [python3]`,
    python is a general-purpose write primitive, and `on-shell` sees the script
    text of a `-c` invocation but never the body of a script *file*.
  - **A build-tool recipe is not resolved into its body**, so `make test` reaches
    the hook as `make test`, and any recipe indirection bypasses the policy.
- It keeps no state across calls: there is no three-strikes rule, and the same
  refused call is refused identically every time.
- It only narrows. There is no permit arm, and nothing here widens what the
  capsule manifest granted.

## Cost

A call touching no protected path produces the same result, the same trace and the
same tool output as with neither hook installed, and the hook does no I/O and
makes no host call on that path. Installing any policy hook does make the runtime
resolve each call and dispatch to the hook — that cost is inherent to the seam.

See [murmur.yaml](./murmur.yaml) for the full manifest.
