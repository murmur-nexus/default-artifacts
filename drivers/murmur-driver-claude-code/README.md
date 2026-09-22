# murmur-driver-claude-code

The process driver for [Claude Code](https://docs.claude.com/en/docs/claude-code). It tells
Murmur how to drive the `claude` CLI for one turn: the argument list, the JSON line the task
goes out on, the MCP config that points the harness at the capsule's tool bridge, and the
control request that interrupts a turn.

It is the first artifact in this repo that exports `murmur:driver/process@0.2.0` rather than
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

## Which murmur this needs

This artifact exports `murmur:driver/process@0.2.0`, and **the host accepts exactly one version
of that interface and carries no fallback for an earlier one.** So it needs a murmur whose
process-driver runner is at `@0.2.0`; one still at `@0.1.0` refuses it at load, naming both
versions and telling you to rebuild:

```
error[E-RUN-029]: the transport: process driver does not export the process driver interface
  murmur-driver-claude-code@0.2.0 exports murmur:driver/process@0.2.0
  this runtime expects murmur:driver/process@0.1.0
```

The refusal is deliberate and runs the other way too: `murmur-driver-claude-code@0.1.0` no
longer loads against a murmur at `@0.2.0`. Install `0.2.0` — a rebuild is the intended cost of
the bump.

`mur --version` does not distinguish the two: it reports `murmur-cli 0.3.0` both before and
after the interface moved, because the CLI version string was not bumped with it. If you see
`E-RUN-029`, no setting in this artifact will change it; build `mur` from a murmur that carries
the `@0.2.0` runner, or wait for the release that does.

`transport: process` with an `inference.driver` is also newer than any published murmur release.
A murmur without the runner at all refuses the manifest before the driver is loaded, with
`E-MAN-003` naming `inference.driver.artifact`.

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
    version: "0.2.0"
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

## Reading what the harness prints

`parse` takes a batch of complete stdout lines and returns the events they carry. Every line
`claude` prints is self-contained — a `tool_result` carries its own `tool_use_id` — so the
parser holds nothing across lines, and the same lines split into different batches produce the
same events. The only thing carried from one call to the next is the bridge's tool prefix, which
`launch` records so `parse` can strip it off again.

| Line | Events |
|---|---|
| `system` `init` | `session-started` |
| `system` `api_retry` | `retry` |
| `system` anything else (`status`, `thinking_tokens`, …) | none |
| `control_response` | none |
| `stream_event` whose `event.delta.type` is `text_delta` | `text-delta` |
| `stream_event` whose `event.delta.type` is `thinking_delta` | `thinking-delta` |
| `stream_event` anything else (message and block framing, `input_json_delta`, `signature_delta`) | none |
| `assistant` | at most one `thinking`, then at most one `text`, then one `tool-call` per `tool_use` block |
| `user` `tool_result` block | `tool-result` |
| `user` any other block, such as the `[Request interrupted by user]` marker | none |
| `result` | one `usage`, then `turn-end` or `turn-failed` — see [The tokens a turn spent](#the-tokens-a-turn-spent) |
| a blank or whitespace-only line | none |
| anything else | exactly one `note` |

An `assistant` message emits **one** `text` event, its text blocks joined, which is what makes
the interface's rule — a `text` replaces the `text-delta`s streamed for that message rather than
being appended to them — well defined. A message with no text of its own, such as a
thinking-only one, emits no `text` event at all.

A line the driver cannot read — not JSON, not a JSON object, or an object whose `type` is absent
or unrecognised — becomes exactly one `note` reading `unreadable stdout line: <line>`, truncated
at 512 characters with `…` appended. `parse` never returns an error and never panics, whatever
it is handed.

### `is_error` decides a `result` line, never `subtype`

```
is_error == false                        -> turn-end
is_error == true, or absent              -> turn-failed, kind decided in this order:
    api_error_status 401 or 403          -> auth
    api_error_status 429                 -> quota
    terminal_reason "aborted_streaming"  -> canceled
    subtype "error_max_turns"            -> max-turns
    anything else                        -> harness-error
```

`claude` 2.1.278 reports an authentication failure and a quota failure as `subtype: "success"`
**alongside** `is_error: true` — both are in the recordings under `tests/fixtures/`. A reader
that trusts `subtype` therefore hands a 401 to the runtime as the model's answer. This driver
reads `is_error` and `api_error_status`, and `subtype` is consulted last and only for
`error_max_turns`, the one failure it is the sole witness to. A `result` line with no readable
`is_error` is a failure, not a success: a turn nobody can confirm succeeded is not reported as
one that did.

`turn-end` carries the line's `result` text, or `""` when it wrote none. A `turn-failed` carries
`result` when it is a non-empty string, and otherwise composes the failure from the fields the
line does say — `subtype`, `terminal_reason: <value>` and `api_error_status: <value>`, joined
with `", "`. An interrupted turn writes no `result`, so it reads
`error_during_execution, terminal_reason: aborted_streaming`.

### The tokens a turn spent

The `result` line that ends a turn also carries a `usage` block, and the driver reads five
counts out of it. That block is the only place Claude's spelling of a count appears; the
runtime is handed the interface's generic names.

| Count | Claude's field on the `result` line |
|---|---|
| input | `usage.input_tokens` |
| output | `usage.output_tokens` |
| cache read | `usage.cache_read_input_tokens` |
| cache creation | `usage.cache_creation_input_tokens` |
| thinking | `usage.output_tokens_details.thinking_tokens` |

**Absent is not zero.** Each count is read as a non-negative integer; a value that is absent,
`null`, a string, a float or negative reads as *absent*, never as zero. A harness that does not
report a count and a turn that spent none of it are different facts, and a ceiling weighed
against a number nobody measured is a ceiling against nothing. The converse holds too: a zero
Claude did write is a measurement and is reported as zero, not dropped. A line that names none
of the five — no `usage` key, `"usage": null`, `"usage": {}`, or a `usage` whose counts are all
unreadable — reports nothing at all, one representation for "this line said nothing about
tokens".

**One reading per `result` line, and none from any other line.** `claude` also writes a `usage`
block on its `assistant` lines and on its `message_delta` stream events, and neither is read.
Both count one *message* rather than the turn: in the `03-bridge-tool-call` recording the two
`message_delta` lines each report the same five output tokens, so summing them reports the
turn's output a second time, and adding them to the `result` line's total of `10` reports `20`.
The `result` line's block is the only per-turn statement the harness makes, so it is the only
one this driver reads.

These are **the harness's own reported numbers** for a subscription turn — what `claude` says it
spent — rather than anything Murmur metered. Murmur sees no request on this path; the harness
authenticates and bills itself. `total_cost_usd`, `costUSD` and `contextWindow` are deliberately
not read: a dollar figure a harness produced against its own price table is a different claim
from a token count and has no equivalent on the http path. Neither are `server_tool_use`,
`service_tier`, `cache_creation`, `inference_geo`, `iterations` or `speed`.

#### What goes out is the run's total, not the turn's

`murmur:driver/process@0.2.0` takes every member of `usage` as **cumulative for the harness
run**, and the runtime attributes to each turn only the growth of a member over what it has
already attributed. `claude` reports the opposite — each `result` line carries that turn's own
spend and nothing earlier — so the driver adds each turn onto the totals it has already
reported and sends the sum.

| Turn's `result` line says | Driver reports | Runtime attributes to that turn |
|---|---|---|
| `input 2, output 10` | `input 2, output 10` | `input 2, output 10` |
| `input 1, output 5` | `input 3, output 15` | `input 1, output 5` |

Reporting the second turn's `1`/`5` verbatim would have the runtime attribute **nothing** to
it, because `1` is not growth over the `2` already counted. A member no turn has reported stays
absent; a member one turn omits keeps the total an earlier turn set, because silence is the
harness declining to repeat itself rather than retracting what it said.

The totals belong to one process. A new `launch` starts them over, since the next process's
first turn is the run's first turn again.

The `usage` event is emitted **immediately before** the `turn-end` or `turn-failed` of the same
`result` line: the runtime attributes a reading to the turn open when it arrives, and both
terminal events close that turn.

### The other events

- **`session-started`** takes its id from `session_id` and its model from `model`. `auth` is
  `subscription` only when the harness reports `apiKeySource: "none"`; every other value reads
  as `api-key`, **absent included**. The runtime warns on anything that is not `subscription`,
  so the ambiguous case fails towards being warned about rather than towards a quiet
  subscription claim. Every recording in `tests/fixtures/` authenticated with an API key, so
  every one reads `api-key`; the subscription reading is covered by a unit test.
- **`retry`** takes `attempt` from the line and reads its reason as `<error> (<error_status>)`,
  falling back to whichever of the two the line carries, and to `unknown` with neither.
- **`tool-call`** strips the prefix `launch` added, and **only** that prefix. A generic
  `mcp__<anything>__` strip would also rename the tools of an MCP server this driver never
  registered; with no bridge recorded, a tool name passes through untouched.
- **`tool-result`** pairs by `tool_use_id`. Its output is the block's `content` when that is a
  string, and otherwise its text blocks joined with newlines, with any non-text block keeping
  its JSON rather than being dropped.

### `classify-exit`

The runtime calls it only for a run whose output ended without a terminal event, and it only
ever answers `turn-failed`:

| Condition | Result |
|---|---|
| `interrupted` and no terminal event | `canceled`, `claude was interrupted before it reported a result` |
| exit code `0` and no terminal event | `harness-error`, `claude exited without a result` |
| anything else | `harness-error`, `claude exited with code <n>` / `claude was killed by signal <n>` / `claude exited without a status`, with a non-empty stderr tail appended after `": "` |

The interrupt is checked before the exit code is read at all, which is the rule the interface
holds every driver to: `claude` exits `0` on a turn stopped by SIGINT, so the code cannot tell a
canceled turn from a finished one.

## The recordings this is tested against

`tests/fixtures/` holds ten recordings of real `claude` 2.1.278 output, byte-identical copies of
the roadmap's, each with its stdout, its stderr and its exit code. `tests/golden.rs` asserts the
**complete** event list for every one, and asserts across all ten that no recording produces a
`note`, that every one ends with a terminal event — so the runtime never reaches `classify-exit`
for any of them, including the two that exit `1` — and that no recording whose `result` line
says `is_error: true` ends the turn successfully.

Each recording is parsed three ways: all lines in one batch, one line per batch, and a two-batch
split whose boundary falls between a message's deltas and the `assistant` line carrying its full
text. All three must produce the same events.

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
so the tests run on the host, with the recordings and their golden event lists under `tests/`.
Code behind the wasm gate does not exist for the host target, so logic written there would
report a green `cargo test` having executed none of its lines.
