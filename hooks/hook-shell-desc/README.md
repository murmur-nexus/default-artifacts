# hook-shell-desc

Returns tool manifests for common non-interpreter shell binaries at staging
time, stating what each takes and returns. Shell interpreters keep the
runtime's own description.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-stage` · mode: blocking · commit policy: `write-manifests`.

## Profiled binaries

| Binary | Identity |
| --- | --- |
| `cargo` | the Rust package manager and build tool |
| `curl` | transfers data to and from URLs |
| `find` | walks directory trees and lists the files that match its tests |
| `git` | the Git version control client |
| `grep` | prints the lines of files that match a pattern |
| `jq` | a command-line JSON processor |
| `node` | the Node.js JavaScript runtime |
| `python3` | the Python 3 interpreter |
| `rustc` | the Rust compiler |

Each profile states, after the identity sentence:

- `command` is the argument list the binary receives, without the binary name
  (`node` and `python3` add that it is not source code).
- The runtime splits `command` at whitespace; `'...'` and `"..."` group an
  argument and lose their quotes; a backslash makes the next character literal.
- No shell runs the command, so `|`, `>`, `<`, `&&`, `;`, globs, `~` and
  `$VAR` reach the binary as literal arguments.
- The result is the exit code, stdout and stderr, cut short past the runtime's
  size limit with a note saying so.

The hook writes no manifest for `bash`, `sh`, `zsh`, `fish`, `dash` or `ksh`,
so the runtime's own description ("The `command` field is passed to
&lt;interpreter&gt; via -c.") stands. The runtime decides how `command` reaches an
interpreter and states it; a second copy here could only agree with it or
drift from it.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur/hook-shell-desc
    version: 0.8.0
    runtime: hook
```

No `capabilities:` block is required — this hook touches neither the network
nor the filesystem, and runs fully confined under the runtime's default-deny
capability model.

The manifests are returned as its `HookOutput`; the host — not the
hook — writes them to `tools/<binary>/murmur.yaml` in the session's runtime
directory (`$MURMUR_RUNTIME_DIR`) under the `write-manifests` commit policy.
`mur` v0.6.0 and earlier write them to `tools/<binary>/murmur.yaml` in the
capsule workdir instead.

See [murmur.yaml](./murmur.yaml) for the full manifest.
