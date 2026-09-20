# murmur-driver-claude-code

The process driver for [Claude Code](https://docs.claude.com/en/docs/claude-code). It tells
Murmur how to drive the `claude` CLI for one turn: the argument list, the JSON line the task
goes out on, the MCP config that points the harness at the capsule's tool bridge, and the
control request that interrupts a turn.

It is the first artifact in this repo that exports `murmur:driver/process@0.1.0` rather than
`murmur:tool/run`. That export is how the runtime tells a process driver from an HTTP driver —
both are `runtime: driver` artifacts, and the name decides nothing.

Every Claude Code fact lives in this crate: its flags, how it spells a bridged tool, the shape
of its MCP config, the stdin format, the interrupt message, and the environment variables it
cannot run without. The runtime runs *a* process driver; it never learns it is running the
Claude one.

**The driver is granted nothing** — no network, no filesystem, no environment, no Murmur host
import. It runs on an empty WASI context and is pure translation from a launch request to a
launch plan. It holds no credential and declares no auth block: the harness authenticates
itself, which is the whole point of process transport. A turn spends a Claude subscription
rather than an API key.

## The manifest an operator writes

```yaml
inference:
  transport: process
  model: ""                      # optional; empty leaves the choice to the harness
  system_prompt_file: ./instructions.md
  driver:
    artifact: murmur-driver-claude-code
  # command: /usr/local/bin/claude   # optional; overrides the `claude` this driver names

capabilities:
  env:
    allow: [HOME, PATH]

artifacts:
  - name: murmur-driver-claude-code
    runtime: driver
    version: "0.1.0"
```

The harness starts from an **empty** environment and sees exactly the variables
`capabilities.env.allow` declares. That is what keeps the subscription the thing being spent: an
API key sitting in the operator's shell reaches `claude` only if the operator writes it down
here.

## Required variables, and why each one stays

Both `HOME` and `PATH` are in `describe().required_env`, so a capsule that omits either is
refused at load with an error naming the variable.

Five probes were run against real `claude` 2.1.278 on 2026-09-20. **All five exited `0`** — the
requirement is kept in spite of that, not because of it, for the reasons below.

| Probe | Exit | First line of output |
|---|---|---|
| `env -i $(command -v claude) --version` | `0` | `2.1.278 (Claude Code)` |
| the same with only `HOME` (a scratch dir) and a minimal `PATH` | `0` | `2.1.278 (Claude Code)` |
| a full turn against a mock endpoint and a mock bridge | `0` | `{"type":"system","subtype":"init",…,"mcp_servers":[{"name":"claude_bridge","status":"connected"}]}` |
| the same turn with `HOME` dropped | `0` | the same `system`/`init` line |
| the same turn with `PATH` dropped | `0` | the same `system`/`init` line |

Two things mask the requirement rather than removing it:

- **`HOME` survived being dropped** because glibc falls back to the passwd entry's home
  directory when `HOME` is unset, and because the mock turn authenticated with an API key —
  the API-key path never opens the login store under `~/.claude`. A
  **subscription** run does open it, and that run is the only reason `transport: process`
  exists. `HOME` stays.
- **`PATH` survived being dropped** because the `claude` on the probe machine is a native
  single-binary install (an ELF file on the `PATH`, no interpreter). The npm install shape is a
  launcher that starts `node` off `PATH` and cannot exec without it. Murmur's own binary
  resolution also walks `PATH` whenever the manifest sets no `command:`. `PATH` stays.

Drop either variable only once a **subscription** run is shown to succeed without it.

## Optional variables an operator may want to allow

| Variable | Why |
|---|---|
| `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` (and `https_proxy`, `http_proxy`, `no_proxy`) | reaching Anthropic through a corporate proxy |
| `CLAUDE_CONFIG_DIR` | putting the harness's own config and login store somewhere other than `~/.claude` |

> **Do not declare `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` in `capabilities.env.allow`
> unless you mean to bill the API instead of the subscription.** Either one makes `claude`
> authenticate as an API client and charge per token instead of drawing on the subscription the
> transport exists to spend.

The two are not equally easy to write by mistake, which is worth knowing before you go looking
for the mistake in the wrong place:

| Variable | What the runtime does | Measured |
|---|---|---|
| `ANTHROPIC_API_KEY` | refuses the capsule — it matches murmur's built-in credential backstop, which no manifest setting exempts a name from | `error[E-CAP-016]: capabilities.env.allow names 'ANTHROPIC_API_KEY' (credential backstop pattern 'ANTHROPIC_API_KEY')` |
| `ANTHROPIC_AUTH_TOKEN` | allows it, and warns | `warning[W-SEC-024]: capabilities.env.allow names 'ANTHROPIC_AUTH_TOKEN', a credential-shaped variable the credential backstop does not drop` |

So `ANTHROPIC_AUTH_TOKEN` is the one that can quietly move a run onto API billing; `W-SEC-024`
is the only thing that says so. Declaring it together with `ANTHROPIC_BASE_URL` is also how to
point a turn at a mock endpoint for testing, which is how this driver was exercised end to end.

## The argv this driver produces

```
claude --print --output-format stream-json --verbose
       --input-format stream-json --include-partial-messages
       --setting-sources ""
       [--model <model>]
       --session-id <id> | --resume <id>
       --tools mcp__<server>__<tool>,…  --mcp-config {files_dir}/mcp-config.json
         --strict-mcp-config --permission-mode bypassPermissions
       --system-prompt <text>
```

- `--print` with `--output-format stream-json --verbose` is the non-interactive streaming mode.
  `--input-format stream-json` is what makes the task a JSON line on stdin and makes the
  interrupt control request possible.
- `--setting-sources ""` is what stops the operator's own `CLAUDE.md`, settings and hooks
  loading into a capsule's turn. The capsule's manifest is the only configuration a run answers
  to. (`--safe-mode` and `--bare` were tested upstream and rejected for this job; do not
  reach for them.)
- `--model` appears only when the manifest names a non-blank model, carrying it trimmed. A blank
  model leaves the choice to the harness rather than passing `--model ""`, which `claude`
  rejects.
- `--session-id` starts a session, `--resume` continues one; the id passes through verbatim.
- With no bridge the tool list is `--tools ""` and none of the four MCP flags appear. An absent
  `--tools` would leave the harness its own built-in tools, which a capsule exposing none did
  not ask for.
- The tool names are built from the bridge's own `server-name`. Nothing here hardcodes one.
- `--permission-mode bypassPermissions` is safe because the bridge already enforces the
  capsule's capabilities on every tool call; a second interactive approval inside the harness
  would only deadlock a headless run.

The task goes out as one newline-terminated JSON line on stdin:

```json
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]}}
```

Stdin stays open so the interrupt can be written to it mid-turn:

```json
{"type":"control_request","request_id":"murmur-interrupt-<session id>","request":{"subtype":"interrupt"}}
```

### Why `--mcp-config` points at a file

`claude` accepts both a file path and inline JSON, and both were verified working against 2.1.278
on 2026-09-20 — each reaches `system`/`init` with the bridge `connected` and the bridged tool
listed. The file is chosen for one reason: the config carries the bridge's bearer token, a live
credential for an endpoint that executes the capsule's tools, and argv is readable by any process
on the host through `/proc/<pid>/cmdline`. The runtime already creates a private `0700` directory
for driver files, so the token goes there. `{files_dir}` is the token the runtime replaces with
that directory's path.

## What is not here at 0.1.0

`parse` and `classify_exit` are stubs. The driver plans a turn; it does not yet turn the
harness's output back into events, so a run produces no streaming text, no tool feed and no
failure reason.

- `parse` returns no events. The interface allows a call to return none, so this is a legal
  answer rather than a claim about what the harness said.
- `classify_exit` keeps the one rule every driver must keep — a run the runtime interrupted,
  with no terminal event seen, is `canceled` whatever the exit code says — and answers every
  other input with a failure naming the parser that does not exist yet.

## Building it

```bash
cargo test -p murmur-driver-claude-code
cargo build -p murmur-driver-claude-code --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/murmur_driver_claude_code.wasm
./scripts/local-install.sh murmur-driver-claude-code <capsule-dir>
```

The crate is three layers, the shape every artifact in this repo uses: the pure logic at the
crate root, a `#[cfg(target_arch = "wasm32")] mod wasm_driver` that converts WIT records to the
crate-root mirrors and back and decides nothing, and `#[cfg(test)] mod tests` at the crate root
so the tests run on the host. Code behind the wasm gate does not exist for the host target, so
logic written there would report a green `cargo test` having executed none of its lines.
