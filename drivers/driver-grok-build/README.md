# driver-grok-build

The process driver for [Grok Build](https://x.ai), xAI's `grok` CLI. It tells Murmur how to
drive `grok` for one headless turn: the argument list, the task file the prompt is read from, the
agent file that points the harness at the capsule's tool bridge, and the environment switches that
keep the operator's other agents' configuration out of the turn. A turn is interrupted with
`SIGINT`.

It exports `murmur:driver/process@0.6.0`, not `murmur:tool/run`. That export is how the runtime
tells a process driver from an HTTP driver — both are `runtime: driver` artifacts, and the name
decides nothing.

Every Grok fact lives in this crate: its flags, how it names and calls an MCP tool, the shape of
its agent file, the switches it reads, how it prints a session, a tool call and a failure, and the
variables it cannot run without. The runtime runs *a* process driver; it never learns it is
running the Grok one.

**The driver is granted nothing** — no network, no filesystem, no environment, no Murmur host
import. It runs on an empty WASI context and is pure translation from a launch request to a launch
plan and from `grok`'s stdout to events. It holds no credential and declares no auth block: the
harness authenticates itself, which is the whole point of process transport. A turn spends the
SuperGrok subscription `grok` is logged into rather than an API key.

## Which murmur this needs

This artifact exports `murmur:driver/process@0.6.0`, which murmur serves from v0.11.0, and **the
host accepts exactly one version of that interface and carries no fallback for another.** Running
it needs `mur` 0.11.0 or later; a `mur` at another level refuses it at launch with
`E-WIT-LEVEL-001`.

## The manifest an operator writes

```yaml
inference:
  transport: process
  model: ""                      # optional; empty leaves the choice to grok
  system_prompt_file: ./instructions.md
  driver:
    artifact: murmur/driver-grok-build
    config:                      # optional; see "Driver config" below
      reasoning_effort: high
  # command: /usr/local/bin/grok   # optional; overrides the `grok` this driver names

capabilities:
  env:
    allow: [HOME, PATH]
  harness:
    paths:
      - path: ~/.grok
        access: read-write
  network:
    allow: [api.x.ai, auth.x.ai]
  shell:
    allow: [cat]                 # see "The TLS trust store" below

artifacts:
  - name: murmur/driver-grok-build
    runtime: driver
    version: "0.1.0"
```

The harness starts from an **empty** environment and sees exactly the variables
`capabilities.env.allow` declares, then the driver's switches. That is what keeps the
subscription the thing being spent: an API key sitting in the operator's shell reaches `grok` only
if the operator writes it down here, and murmur refuses the one `grok` reads (see
[Variables](#required-variables-and-why-each-one-stays)).

### The TLS trust store

On a `scoped` or `sealed` capsule, `grok` must be able to read the host's TLS trust store, or it
cannot build its MCP client: the bridge is reported `failed`, and every capsule tool call comes
back `Tool not found: <server>__<tool>`, even though the bridge itself is plain `http`. grok 1.0.77
logs `Error loading CA root certificate: … Permission denied … at '/usr/lib/ssl/cert.pem'`, then
`MCP server init failed … Failed to build HTTP client: builder error`.

murmur v0.11.0 grants a contained command the trust store only when the capsule's
`capabilities.shell.allow` **and** `capabilities.network.allow` are both non-empty. It refuses a
harness need under `/etc`, so this driver cannot ask for the trust store itself. A capsule on
this driver therefore declares a `shell.allow` entry. `cat` is a small one, but it is a real grant:
murmur offers the model a shell tool that can run it in the work folder. This was measured on the
guest run in [`tests/live/README.md`](tests/live/README.md#the-guest-run). Without the entry, the
turn still runs, with no capsule tool.

## Driver config

`inference.driver.config` is optional. murmur hands it to the driver as JSON, and the driver reads
it once per turn, before it plans anything.

| Key | Type | Default | What it does |
|---|---|---|---|
| `reasoning_effort` | string | absent: the model's default | Starts `grok` with `--reasoning-effort <level>`. One of Grok's canonical levels: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. A model accepts only the levels its menu advertises |

No config, `{}` or a whitespace-only value leaves every key at its default. Anything else the
driver cannot read refuses the turn, with a message naming the problem:

| Config | Refusal |
|---|---|
| not JSON | `driver: inference.driver.config is not valid JSON: <error>` |
| JSON that is not an object | `driver: inference.driver.config must be a JSON object` |
| a key not in the table | `driver: unrecognised inference.driver.config key(s): <keys, sorted>. Accepted keys: reasoning_effort` |
| a `reasoning_effort` that is not exactly a canonical level, such as `"deep"`, `"high "`, `"HIGH"`, `3` or `null` | `driver: inference.driver.config 'reasoning_effort' must be one of none, minimal, low, medium, high, xhigh, max, got <value as JSON>` |

`grok` also takes per-model menu ids such as `deep`. They mean nothing across models, so the
driver refuses them.

## Login check and harness needs

| Field | Value |
|---|---|
| `login-check` | `command(["models"])`. `read-login` reads only the first non-empty line `grok models` prints, trimmed. No reason quotes the output, which goes on to list the account's models |
| `harness-needs.paths` | `~/.grok`, `read-write`: the login store `auth.json`, sessions, logs and the managed install under `~/.grok/bin` |
| `harness-needs.hosts` | `api.x.ai`, `auth.x.ai` |

| First line of `grok models` (grok 1.0.77, observed 2026-11-11) | Reading |
|---|---|
| `You are logged in with grok.com.` | logged in |
| `You are using XAI_API_KEY.` | logged in |
| `You are not authenticated.` | logged out: ``grok is not signed in: run `grok login` as the user mur runs as`` |
| `Model '<name>' is using its own API key.`, printed when `~/.grok/config.toml` gives a custom model an inline `api_key`, and any other line | unknown: ``grok models` printed a first line this driver does not recognise`` |
| nothing | unknown: ``grok models` exited <code> without printing anything``, or ``… was ended by a signal without printing anything`` |

A capsule allows the needs with `capabilities.harness.paths` and `capabilities.network.allow`, or
the launch is refused with `E-CAP-021`. The host list is what the recordings and the guest run
showed: `api.x.ai` for inference and `auth.x.ai` for a session restore. A live SuperGrok run has
not confirmed it yet: see the [handoff](tests/live/README.md#handoff-a-live-supergrok-run).

## Required variables, and why each one stays

`HOME` and `PATH` are in `describe().required-env`, so a capsule that omits either is refused at
load with an error naming the variable. These probes ran against real `grok` 1.0.77 on 2026-11-11.
Every one exited `0`. The requirement stays in spite of that, for the reasons below.

| Probe | Exit | First line |
|---|---|---|
| `env -i grok version` | `0` | `grok 1.0.77 (50ca579fa996) [stable]` |
| `grok version` with only a scratch `HOME` and `PATH=/usr/bin:/bin` | `0` | `grok 1.0.77 (50ca579fa996)` |
| a mock turn with `HOME` dropped | `0` | the `system`/`init` line |
| a mock turn with `PATH` dropped | `0` | the `system`/`init` line |

- **`HOME` survived being dropped** because `grok` falls back to the account's passwd home when
  `HOME` is unset (the probe ran in a user namespace with empty folders mounted over `~/.grok` and
  `/root`, so no real login could be read), and because the mock turn authenticated with an API
  key. A subscription run reads its login from `~/.grok/auth.json`. `HOME` stays.
- **`PATH` survived being dropped** because the `grok` on the probe machine is a static binary.
  An npm-shaped install is a launcher that starts `node` off `PATH`, and murmur resolves a bare
  `grok` on `PATH` whenever the manifest sets no `command:`. `PATH` stays.

### Optional variables an operator may want to allow

| Variable | Why |
|---|---|
| `GROK_HOME` | putting `grok`'s config and login store somewhere other than `~/.grok`; the harness need still names `~/.grok` |
| `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` (and lowercase) | reaching xAI through a proxy |

> **`XAI_API_KEY` is refused.** murmur's credential backstop matches `*_API_KEY`, so
> `capabilities.env.allow: [XAI_API_KEY]` fails at load with `E-CAP-016`. A stored grok.com login
> in `~/.grok/auth.json` outranks an API key in any case, so a logged-in `grok` spends the
> subscription even with a key in its environment.

## Root

It works as root. `grok` has no root guard, and the driver never asks it to bypass its permission
prompts: it starts `grok` in the default permission mode with one allow rule for the bridge's
tools. The mock recordings ran as root in a sandbox.

## The argv this driver produces

```
grok --prompt-file {files_dir}/task.md
     --output-format streaming-messages-json
     --include-partial-messages
     --agent {files_dir}/capsule-agent.md
     --system-prompt-override=<system prompt>
     [--allow MCPTool(<server>__*)]
     --no-auto-update
     --disable-web-search
     --no-subagents
     [--reasoning-effort <level>]
     [-m <model>]
     -s <session id> | -r <session id>
```

Each flag and each value is one argv entry.

| Flag | Why |
|---|---|
| `--prompt-file {files_dir}/task.md` | The task, byte for byte, from a file. Headless `grok` never reads stdin |
| `--output-format streaming-messages-json` | One JSON line per message, in Claude Code's `stream-json` shape |
| `--include-partial-messages` | Adds the `stream_event` deltas that make `text-delta`, `thinking-delta` and tool-call progress possible |
| `--agent {files_dir}/capsule-agent.md` | The agent file below: two meta-tools and the bridge |
| `--system-prompt-override=<prompt>` | Replaces `grok`'s system prompt with the runtime's. The `=` form keeps a prompt that starts with `-` one value |
| `--allow MCPTool(<server>__*)` | Lets the bridge's tools run without asking, which a headless run cannot do. Only with a bridge. Any other MCP tool is refused |
| `--no-auto-update` | No update check for the turn. grok 1.0.77 accepts it, although `grok --help` does not list it |
| `--disable-web-search` | No web search or fetch tool |
| `--no-subagents` | No subagents, so no background work holds the closing line |
| `--reasoning-effort <level>` | Only when [configured](#driver-config) |
| `-m <model>` | Only for a non-blank `inference.model`, trimmed |
| `-s <id>` / `-r <id>` | Starts a session with the runtime's id, or resumes it. `-s` with an id already in use fails, so the runtime's new id is always fresh |

The driver never passes `--permission-mode bypassPermissions`, `--yolo`, `--always-approve` or
`--trust`. With `bypassPermissions` a call to one of the operator's own MCP tools runs; in the
default mode it is refused.

### The agent file, and why the token is in a file

```
---
name: "murmur-capsule"
description: "Murmur capsule"
tools: search_tool, use_tool
mcpServers:
  - <server>:
      url: "<bridge url>"
      headers:
        Authorization: "Bearer <bridge token>"
---
You are a Murmur capsule.
```

- `tools: search_tool, use_tool` leaves the model exactly `grok`'s two MCP meta-tools and none of
  its built-in tools: no shell, no file edits, no web.
- Every value is written as a JSON string, which is a YAML double-quoted scalar, so a URL or token
  holding a quote, a colon or a backslash cannot break the frontmatter. The server key goes bare:
  `launch` has already held it to letters, digits, `_` and `-`.
- Without a bridge, `mcpServers` and `--allow` are left out and the two meta-tools stay, so any MCP
  call is refused. Every murmur from v0.10.0 gives every capsule `session-status`, so a real run
  always has a bridge.
- The body is never sent: `--system-prompt-override` replaces it.

The bearer token is a live credential for an endpoint that runs the capsule's tools, and argv is
readable by any process on the host through `/proc/<pid>/cmdline`. So it goes only into this file,
in the runtime's private `0700` directory, and never into argv.

### The bridge names grok admits

`grok` skips an MCP tool it cannot admit into its catalog, with only a log line, so the model
would never see it. `launch` refuses such a bridge instead, naming the server or each tool:

| Part | Rule |
|---|---|
| Server name | Starts with an ASCII letter or `_`, uses only ASCII letters, digits, `_` and `-`, and does not end with `_` |
| Tool name | Non-empty, the same characters, no `__`, and does not start with `_` |
| `<server>__<tool>` | At most 256 characters |

The refusal reads `driver: grok cannot admit the tool bridge's server name "<name>": …` or
`driver: grok cannot admit these capsule tool names: "<tool>" (<why>), …`, and both end
`grok skips tools it cannot admit, so the model would never see them`. murmur's own names —
`murmur`, `tool-read`, `session-status`, `tool-transit-board-relay`, `git` — all pass.

### The environment switches

`env-set` is exactly these, in this order, and nothing from the request:

| Variable | Value | What it keeps out |
|---|---|---|
| `GROK_CLAUDE_{MCPS,SKILLS,RULES,AGENTS,HOOKS}_ENABLED` | `false` | the operator's Claude Code MCP servers, skills, rules, agents and hooks |
| `GROK_CURSOR_{MCPS,SKILLS,RULES,AGENTS,HOOKS}_ENABLED` | `false` | the same from Cursor |
| `GROK_CODEX_{MCPS,SKILLS,RULES,AGENTS,HOOKS}_ENABLED` | `false` | the same from Codex |
| `GROK_TURN_SUMMARY` | `0` | the summary model call `grok` makes after each turn |
| `GROK_DISABLE_AUTOUPDATER` | `1` | a self-update mid-run |
| `GROK_MEMORY` | `0` | `grok`'s cross-session memory |

The fifteen compat switches are written out one per variable in `env-set`; the table groups them.

## Reading what the harness prints

`parse` takes a batch of complete stdout lines and returns the events they carry. Three things are
carried from one `parse` call to the next: the run's token totals, the `use_tool` calls the
current message has opened, and the ids of calls noted as not the capsule's, waiting for their
results. `launch` starts all three afresh. Because they are carried rather than dropped at a batch
boundary, the same lines split into different batches produce the same events.

| Line | Events |
|---|---|
| blank or whitespace-only | none |
| not JSON, not an object, or an absent or unknown `type` | one `note`: `unreadable stdout line: <line>`, cut at 512 characters with `…` |
| `system` `init` with a non-empty `session_id` | `session-started`: `auth` is `subscription` when `apiKeySource` is `oauth`, otherwise `api-key` (absent included); `model` unless empty or `unknown`. Then one `note` when `mcp_servers` lists the bridge's server as `failed`, `needs-auth` or `disabled`: `grok reports the capsule's tool bridge as <status>` |
| `system` `init` with an empty or absent `session_id` | none: the runtime never sees an empty id |
| `system`, any other subtype | none |
| `stream_event` `message_start` | none; forgets the open calls |
| `stream_event` `content_block_start` | none; forgets the call at that index. A `tool_use` named `use_tool` with an id opens a call whose tool is not yet known |
| `stream_event` `input_json_delta` at an open call | See [A tool call being written](#a-tool-call-being-written) |
| `stream_event` `text_delta` / `thinking_delta` | `text-delta` / `thinking-delta` when non-empty |
| `stream_event`, anything else | none |
| `assistant` | at most one `thinking`, at most one joined `text`, then per `tool_use` block in order: a [capsule call](#capsule-calls-and-notes) becomes `tool-call`, anything else one `note` |
| `user` `tool_result` for a noted call | one `note`: `grok answered its own call <id>: <n> bytes, is_error <bool>`. The content is never quoted |
| `user` `tool_result`, any other | `tool-result` with the [unwrapped](#unwrapping-results-and-images) content and the block's `is_error`, default `false` |
| `user`, any other block | none |
| `result` | one `usage` when it reports a count, then `turn-end` or `turn-failed` |

`parse` never returns an error and never panics, whatever it is handed. The noted ids are bounded
at 1,024, the oldest forgotten first.

### Capsule calls and notes

The model reaches every MCP tool through `use_tool`, naming it `<server>__<tool>` in `tool_name`
and passing its arguments in `tool_input`. A **capsule call** is a `tool_use` named `use_tool`
whose `input` is an object, whose `tool_name` starts with the prefix `launch` built from the
bridge's server name followed by a non-empty rest, and which carries neither `file` nor
`tool_input_file`. Its `tool-call` carries the bare name and `tool_input` serialized compactly
(`{}` when absent or `null`), which is exactly what the bridge receives, so murmur pairs its
report with the call. Only that prefix comes off; the driver hardcodes no server name.

Everything else is one `note` naming the call and the tool it asked for, and nothing else of its
input:

| Call | Note |
|---|---|
| `use_tool` naming a tool that is not the capsule's | `grok called use_tool for '<tool_name>', which is not one of the capsule's tools (call <id>)` |
| `use_tool` with `tool_input_file` | `grok called use_tool for '<tool_name>' with its arguments in a file, which is never read as a capsule call (call <id>)` |
| `use_tool` with `file` | `grok called use_tool with its whole call in a file, which is never read as a capsule call (call <id>)` |
| `use_tool` with no string `tool_name` | `grok called 'use_tool', which is not one of the capsule's tools (call <id>)` |
| `search_tool`, or any other tool | `grok called '<name>', which is not one of the capsule's tools (call <id>)` |

With no bridge, nothing is a capsule call. A quoted name is cut at 512 characters.

### A tool call being written

`grok` names a call `use_tool` on its block start and sends its whole input as one
`input_json_delta`, so the capsule tool is known only from the input. The driver adds each
fragment's UTF-8 length to the call's running total and holds the fragments, up to 65,536 bytes,
trying them as a JSON object after each one. The first time they parse to a capsule call, it sends
`tool-call-started` with the id and bare name, then `tool-call-progress` with the total, and drops
what it held. If they parse to anything else, or pass the cap first, nothing more is sent for that
call. After the start, each non-empty fragment sends the new total. An empty fragment sends
nothing.

**It reports a size and never the input**: no event but `tool-call` carries any byte of a call's
input. In recording `05` the one fragment is
`{"tool_name":"murmur_bridge__echo_tool","tool_input":{"text":"hi"}}`, 67 bytes, and the
`tool-call` input is `{"text":"hi"}`.

### Unwrapping results and images

`grok` writes a bridged call's result as a JSON **string**:
`{"type":"MCP","tool_name":"<tool>","server_name":"<server>","output":{"OkayOutput":<value>}}`.

| Content | Output |
|---|---|
| a string that is JSON with `type: "MCP"` and an `output` object of exactly one key | that key's value, rendered as below |
| a string that is JSON with string `error` and `message` | `<error>: <message>` |
| any other string | itself |
| a list of blocks | each block on its own line: a `text` block's text, an `image` block's image line, anything else its JSON |
| absent or `null` | `""` |

Rendering the value: a string is itself, a list gives one line per item (text, image line, or
JSON), a single image object gives its image line, anything else is its JSON. **Every
`data:<media type>;base64,<data>` URL in a rendered string becomes its image line**, so no byte of
an image's base64 reaches an event:

    [image: <media type>, <n> bytes]

`<n>` is the size the base64 decodes to, worked out from its length. The fallbacks are
`unknown media type` and `size unknown`.

**What grok passes its model for an image.** murmur's bridge answers a call whose tool returned an
image with a text item, then an MCP `image` item. In recording `12`, grok 1.0.77 wrote the image
item into the `OkayOutput` string as a third line, `data:image/png;base64,<data>`. It sent its
model that whole string as the call's `function_call_output`: 208 characters of text, with the
88-byte PNG inline as base64 (sha256 `51b9d2ba…13a`, the bridge's exact bytes). The model gets the
image **as text, not as an image**. A large image costs its base64 length in prompt tokens, and a
model reads it only as far as it can decode base64 in text. The driver's output for that call reads:

```
IMAGE-TOOL-RAN
[image attached: picture.png, image/png, 88 bytes]
[image: image/png, 88 bytes]
```

### How a `result` line is read

```
is_error == false                                   -> turn-end(result, or "")
anything else, absent included                      -> turn-failed, kind in this order:
    stop_reason "cancelled"                         -> canceled
    subtype "error_max_turns"                       -> max-turns
    errors + result text holds (401) (403)
      "http_status": 401 / 403 (with or without the space)  -> auth
    errors + result text holds rate_limit_exceeded
      (429) "http_status": 429 (with or without the space)  -> quota
    anything else                                   -> harness-error
```

`grok`'s `result` line carries no status field. A 401 arrives as
`Internal error: {"message": "Unauthorized (401) from …", "http_status": 401}` in `errors[0]`, and
a 429 as `rate_limit_exceeded: <provider message>`. **The kind is read from that text**, because
it is the only place the status appears. A bare `429` is never matched, since it is as likely a
count or a port. The message is the non-empty `errors` joined with `"; "`, else a non-empty
`result`, else `subtype` and `stop_reason: <value>` joined with `", "`, else
`the harness reported a failure with no message`.

### Tokens

| Count | `grok`'s field on the `result` line |
|---|---|
| input | `usage.input_tokens` (uncached) |
| output | `usage.output_tokens` |
| cache read | `usage.cache_read_input_tokens` |
| cache creation | `usage.cache_creation_input_tokens` |
| thinking | never reported: `grok` prints no thinking count |

- **All zero means unknown.** `grok` writes all four as `0` when it knows none of them: a turn a
  signal stopped, one its provider refused, a session that never started. A block whose present
  counts are all zero reports nothing. A count that is absent, `null` or not a non-negative
  integer reads as absent.
- **One reading per `result` line**, summed over every model call of the run. The `usage` on
  `message_delta` and `assistant` lines counts one message and is not read; neither are
  `modelUsage`, `total_cost_usd`, `server_tool_use` or the `duration_*` fields.
- The reading goes out as the process's running total, immediately before the terminal event. A
  headless `grok` prints one `result` per process, so the total is that line.
- **Not counted**: the title call `grok` makes once per new session; it is not in `usage`. The
  per-turn summary call is switched off with `GROK_TURN_SUMMARY=0`.

### `classify-exit`

The runtime calls it only for a run whose output ended without a terminal event, and it only ever
answers `turn-failed`:

| Condition | Result |
|---|---|
| interrupted and no terminal event | `canceled`, `grok was interrupted before it reported a result`, whatever the exit code |
| exit code `0` and no terminal event | `harness-error`, `grok exited without a result` |
| anything else | `harness-error`, `grok exited with code <n>` / `grok was killed by signal <n>` / `grok exited without a status`, with a non-empty stderr tail appended after `": "` |

`grok` normally reaches none of these: it closes a turn stopped by `SIGINT` with its own
`result` (exit 130), and a failed turn with a failing `result` (exit 1).

## Parity with driver-claude-code

| driver-claude-code | driver-grok-build |
|---|---|
| Granted nothing, holds no credential | Same |
| Operator config kept out by `--setting-sources ""` | The compat env switches, `GROK_MEMORY=0`, and the permission allowlist for MCP. Grok's injected context is declared |
| Only bridge tools | Agent `tools` list plus the allowlist |
| Token in a private file | Same, in the agent file |
| Session new and resume with the runtime's id | Same (`-s`, `-r`) |
| Stdin control interrupt | `SIGINT`, which still produces a closing `result` |
| Driver config, strict refusals by name | Same rules; the only key is `reasoning_effort` |
| Login check that never echoes | `grok models`, first line only |
| Harness needs | `~/.grok` read-write; `api.x.ai`, `auth.x.ai` until the live run confirms them |
| Text and thinking deltas | Same |
| `tool-call-started` and byte progress | Adapted: started once the streamed input names a capsule tool |
| Usage summed per run | Same |
| Quota windows | Not in the stream; absent |
| Prompt suggestions | None in Grok |
| Image placeholder | Same line, on the unwrapped output |
| Failure kinds | From `stop_reason`, `subtype` and `errors` text |
| Root guard message | None needed |
| Recordings, golden tests, partition invariance | Same |
| Tested versions, `W-RUN-002` | Same |

## Where Grok cannot do what Claude Code does

- **Two meta tools stand between the model and the capsule's tools.** The model sees
  `search_tool` and `use_tool`, not the tools themselves. Each capsule call costs a `use_tool`
  round. A `search_tool` call is a note, not a trace `tool_call`. Through `search_tool` the model
  can also see the names of the operator's own MCP tools from `~/.grok/config.toml`, plugins and
  `.mcp.json`.
- **Calling one of the operator's tools ends the turn as `canceled`, `cancelled`.** A headless run
  cannot ask for approval, so `grok` refuses the call and stops. stderr names the tool:
  `permission required: <server>__<tool> was not run, as a headless run cannot ask for approval`.
- **Grok injects its own context**, which the capsule's system prompt does not replace. It adds a
  `<user_info>` block (OS, shell, work folder, date), a built-in `user_rules` line, a workflow
  reminder (`deep-research`) and an MCP reminder as user messages. Every rule file in
  `~/.grok/rules/` reaches the model, as do `[paths] extra_rule_dirs`. The work folder's
  `AGENTS.md` and `.grok/rules/`, up to its repo root, load only when `grok` trusts the folder (it
  records trusted folders in `~/.grok/trusted_folders.toml`). The driver never passes `--trust`.
  The compat switches do not stop a trusted folder's `AGENTS.md`: measured in
  [`tests/live/README.md`](tests/live/README.md#probes).
- **A task that starts with one of `grok`'s slash commands runs the command instead of reaching the
  model.** `/session-info …` printed the session's details, and `/goal …` started a goal, with no
  model call. `--verbatim` does not change this, so the driver does not pass it. `init` lists
  `compact`, `always-approve`, `context`, `session-info`, `feedback` and `goal`. `/always-approve`
  ends the run with no model turn, and a resumed turn is back in the default permission mode. Don't
  start a task with one of these words.
- **The operator's own `[permission]` allow rules in `~/.grok/config.toml` still apply.**
- **One title model call per new session spends subscription and is not counted.**
- **No quota windows and no prompt suggestions** are in the stream.
- **Failure kinds are read from text.**
- **A missing session makes `grok` try a restore from xAI (`auth.x.ai`) before failing.** murmur
  reports a resumed turn that fails without a session as `E-RUN-036`.
- **`grok` may hold its closing line up to 10 s after `SIGINT`** while it stops background work.
  With no subagents or workflows there is none, and murmur's interrupt grace is 10 s.
- **The `use_tool` `file` and `tool_input_file` forms are notes, never capsule calls, yet they run
  the capsule's tool.** `grok` 1.0.77 reads the file itself, with no Read tool and no approval,
  and calls the bridge with its contents. The bridge still enforces the capsule's capabilities,
  and murmur traces the call it ran. The driver's events show only the note and the noted result.
- **An image reaches the model as base64 text**, not as an image: see
  [Unwrapping results and images](#unwrapping-results-and-images).
- **`init.tools` may list only the meta-tools.** `grok` writes `init` at its first output line,
  which can come before the bridge's `tools/list` answers. The tools are still callable once the
  bridge is up.
- **The TLS trust store needs a `shell.allow` entry** on a contained capsule: see
  [The TLS trust store](#the-tls-trust-store).

## The recordings this is tested against

`tests/fixtures/` holds twelve recordings of real `grok` 1.0.77 output, each with its stdout, its
stderr and its exit code. They are described in [`tests/fixtures/README.md`](tests/fixtures/README.md).

| Recordings | grok | Auth | Launched with |
|---|---|---|---|
| `01` to `11` | 1.0.77, against a mock Responses endpoint and a mock bridge `murmur_bridge`; byte-identical copies | API key | the exact argv was not kept |
| `12-bridge-image-result` | 1.0.77, with exactly the argv, switches and files `launch` plans, against the mock and a bridge `murmur` whose tool answers with a text item then an MCP `image` item | API key | bridge `murmur`, `reasoning_effort: low`, a system prompt starting with `- `, a new session |

| grok | Evidence |
|---|---|
| 1.0.77 | recordings `01` to `12`, the probes, and the guest run under `mur` 0.11.0 |

A `grok` not in this list still runs: murmur prints `W-RUN-002` naming the version and the tested
list, and refuses nothing.

`tests/golden.rs` asserts the **complete** event list for every recording. It also asserts, across
every recording:

- notes appear only in `06` and `07`, exactly two each;
- every recording ends with exactly one terminal event, so the runtime never reaches
  `classify-exit` for any of them;
- no recording whose `result` says `is_error: true` ends the turn successfully;
- `03` and `04`, whose sessions never started, emit no `session-started`;
- `12`'s image reaches the feed as its line, and no 16-character run of its base64 does.

Each recording is parsed four ways, each with its own fresh parse state: one batch, one line per
batch, a split between a message's deltas and its `assistant` line (or before `result` for a
recording that streams none), and, for `05` and `12`, a split between the `use_tool` block start
and its `input_json_delta`. All four give the same events.

## Building it

```bash
cargo test -p driver-grok-build
cargo build -p driver-grok-build --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/driver_grok_build.wasm
./scripts/local-install.sh driver-grok-build <capsule-dir>
```

The crate is three layers. The logic and the Grok facts sit at the crate root. A
`#[cfg(target_arch = "wasm32")] mod wasm_driver` converts WIT records to the crate-root mirrors
and back, and decides nothing. `#[cfg(test)] mod tests` sits at the crate root, so the tests run
on the host; the recordings and their golden event lists are under `tests/`.
`examples/write_plan.rs` writes what `launch` plans for one request to disk, and
`tests/live/run_plan.py` runs `grok` with exactly that plan. That is how recording `12` was made.
