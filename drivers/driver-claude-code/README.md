# driver-claude-code

The process driver for [Claude Code](https://docs.claude.com/en/docs/claude-code). It tells
Murmur how to drive the `claude` CLI for one turn: the argument list, the JSON line the task
goes out on, the MCP config that points the harness at the capsule's tool bridge, and the
control request that interrupts a turn.

It is the first artifact in this repo that exports `murmur:driver/process@0.4.0` rather than
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

This artifact exports `murmur:driver/process@0.4.0`, which murmur serves from v0.9.0, and **the
host accepts exactly one version of that interface and carries no fallback for an earlier one.**
Installing `murmur/driver-claude-code` by scoped name needs `mur` 0.9.0 or later.

That `mur` refuses `0.7.0` and earlier at launch with `E-WIT-LEVEL-001`, with the clause
`it exports murmur:driver/process@0.3.0, and level 2 has murmur:driver@0.4.0`. Install `0.8.0` or
later — a rebuild is the intended cost of the bump. The refusal runs the other way too: a murmur
at `@0.3.0` refuses this release. Pick the driver by the murmur you run:

| murmur | Its process-driver interface | Driver release to pin |
|---|---|---|
| `v0.3.0` and earlier | none | none: `transport: process` is not supported |
| `v0.4.0`, `v0.5.0` | `murmur:driver/process@0.2.0` | `0.3.0` |
| `v0.6.0` to `v0.8.0` | `murmur:driver/process@0.3.0` | `0.4.0` to `0.7.0` |
| `v0.9.0` and later | `murmur:driver/process@0.4.0` | `0.8.0` and later |

A murmur without the runner at all refuses the manifest before the driver is loaded, with
`E-MAN-003` naming `inference.driver.artifact`.

## The manifest an operator writes

```yaml
inference:
  transport: process
  model: ""                      # optional; empty leaves the choice to the harness
  system_prompt_file: ./instructions.md
  driver:
    artifact: murmur/driver-claude-code
    config:                      # optional; see "Driver config" below
      prompt_suggestions: true
  # command: /usr/local/bin/claude   # optional; overrides the `claude` this driver names

capabilities:
  env:
    allow: [HOME, PATH]

artifacts:
  - name: murmur/driver-claude-code
    runtime: driver
    version: "0.9.0"
```

The harness starts from an **empty** environment and sees exactly the variables
`capabilities.env.allow` declares. That is what keeps the subscription the thing being spent: an
API key sitting in the operator's shell reaches `claude` only if the operator writes it down
here.

## Driver config

`inference.driver.config` is optional. murmur hands it to the driver as JSON, and the driver
reads it once per turn, before it plans anything.

| Key | Type | Default | What it does |
|---|---|---|---|
| `prompt_suggestions` | boolean | `false` | Starts `claude` with `--prompt-suggestions true`, and reads each `prompt_suggestion` line it prints as a `suggested-prompt` event |

No config, `{}` or a whitespace-only value leaves every key at its default. Anything else the
driver cannot read refuses the turn, with a message naming the problem:

| Config | Refusal |
|---|---|
| not JSON | `driver: inference.driver.config is not valid JSON: <error>` |
| JSON that is not an object | `driver: inference.driver.config must be a JSON object` |
| a key not in the table | `driver: unrecognised inference.driver.config key(s): <keys, sorted>. Accepted keys: prompt_suggestions` |
| `prompt_suggestions` that is not a JSON boolean, such as `"yes"`, `"true"`, `1` or `null` | `driver: inference.driver.config 'prompt_suggestions' must be a boolean, got <value>` |

**A suggestion costs one more model call on the subscription after each turn.** `claude` makes
it as a second, forked call once the turn's `result` line is printed, so the suggestion always
arrives after `result`. murmur reads the harness's output after the terminal event only until
the harness exits or its 3 s exit grace runs out. A suggestion that takes longer than that is
lost. In the three recorded runs, `claude` exited 823 ms, 1,883 ms and 2,003 ms after the last
`result`. None of them printed a suggestion, so no suggestion delay is measured yet: see
[The recordings this is tested against](#the-recordings-this-is-tested-against).

`claude` also skips the suggestion on its own:

- for a turn before the conversation holds two assistant messages, so a session's first turn
  never gets one;
- when the prompt cache is cold;
- when the model answers with no suggestion at all.

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
       [--prompt-suggestions true]
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
- `--prompt-suggestions true` appears only when `prompt_suggestions` is `true` in the
  [driver config](#driver-config). Without it the argv is exactly what it was before the key
  existed.
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

`parse` takes a batch of complete stdout lines and returns the events they carry. Two things
are carried from one `parse` call to the next: what `launch` decided, and the open tool calls of
the message being streamed, which tie an `input_json_delta` to the call its block started.
`launch` records the bridge's tool prefix, so `parse` can strip it off again, and whether
suggestions are on. It records them only for a config it accepted. The run's token totals are
carried the same way. `launch` starts the open calls and the totals afresh, because a new
process is a new stream. Because what one line leaves for a later one is carried rather than dropped at a batch
boundary, the same lines split into different batches still produce the same events.

| Line | Events |
|---|---|
| `system` `init` | `session-started` |
| `system` `api_retry` | `retry` |
| `system` anything else (`status`, `thinking_tokens`, …) | none |
| `control_response` | none |
| `stream_event` whose `event.delta.type` is `text_delta` | `text-delta` when its `text` is non-empty; none when it is empty |
| `stream_event` whose `event.delta.type` is `thinking_delta` | `thinking-delta` when its `thinking` is non-empty; none when it is empty |
| `stream_event` `message_start` | none; forgets the previous message's tool calls |
| `stream_event` `content_block_start` whose `content_block.type` is `tool_use` | `tool-call-started` with the block's `id` and its name, stripped as `tool-call` strips it; none when its `id` is empty |
| `stream_event` whose `event.delta.type` is `input_json_delta` | `tool-call-progress` with the running UTF-8 byte total of the call's input so far; none when no `tool_use` block in this message opened its `index`, or its `partial_json` is empty |
| `stream_event` anything else (`content_block_stop`, `message_delta`, `message_stop`, `signature_delta`, other block starts) | none |
| `assistant` | at most one `thinking`, then at most one `text`, then one `tool-call` per `tool_use` block |
| `user` `tool_result` block | `tool-result`; an `image` block in it becomes `[image: <media type>, <n> bytes]` — see [Images in a tool result](#images-in-a-tool-result) |
| `user` any other block, such as the `[Request interrupted by user]` marker | none |
| `result` | one `usage`, then `turn-end` or `turn-failed` — see [The tokens a turn spent](#the-tokens-a-turn-spent) |
| `rate_limit_event` | one `quota` — see [Subscription windows](#subscription-windows) |
| `rate_limit_event` whose `rate_limit_info` is missing, is not an object, or names no window | exactly one `note` |
| `prompt_suggestion` | `suggested-prompt` when `prompt_suggestions` is on; one `note` when it is off or the line has no string `suggestion` |
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

### A tool call being written

`claude` streams a tool call while the model writes it: a `content_block_start` names the call,
then `input_json_delta` lines carry its input a fragment at a time, and the complete call comes
on the `assistant` line only once the input is finished. A call writing a large file can take
many seconds to get there. So the driver reports the call as soon as its block starts, as
`tool-call-started`, and then how much of its input has been written, as `tool-call-progress`.

**It reports a size and never the input.** No event but `tool-call` carries any byte of a call's
input, and no `note` quotes it either:

- the A2A stream has never carried tool input, and these frames are written to it;
- `stream/watch` can be held as a read-only scope, which should not become a way to read what a
  tool is about to be handed;
- a hook that denies a call cannot unsend fragments already streamed.

**The size is a running total for the call**, never the size of one fragment: the total after a
given line is the same however the lines are batched, so the report does not depend on where a
batch boundary fell. Each `tool-call-progress` is larger than the last for its call; an empty
fragment reports nothing.

**The size counts the bytes the model streamed**, the UTF-8 length of each decoded
`partial_json`. That can differ from the length of the `tool-call`'s `input`, which is the
complete block re-serialized compactly: the `03-bridge-tool-call` recording streams
`{"text": "hi"}`, 14 bytes, and its `tool-call` carries `{"text":"hi"}`, 13.

`tool-call-started` and `tool-call` carry the same id and the same name for one call, and
`tool-call-started` may arrive with no progress at all: a call whose fragments are all empty
reports none.

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

`input` is the prompt tokens neither read from nor written to the cache; `cache-read` and
`cache-creation` sit beside it, so `input + cache-read + cache-creation` is the turn's total
request count. Claude's `usage` block is Anthropic-shaped and its `input_tokens` already
excludes both cache counts, so each count is relayed unchanged.

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

`murmur:driver/process@0.4.0` takes every member of `usage` as **cumulative for the harness
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

### Subscription windows

A subscription run of `claude` prints a `rate_limit_event` line at its first API response, and
again whenever a window's rounded percentage, reset time, status or threshold moves. An API-key
run never prints one. The driver turns each line into **exactly one `quota` event**, built from
the line's `rate_limit_info`. That mapping is the only place Claude's spelling of a window
appears.

| quota-window | From claude |
|---|---|
| `name` | the `unifiedWindows` key, or the top-level `rateLimitType`, lowercased with `_` written as `-`: `five_hour` → `five-hour`, `seven_day` → `seven-day`, `seven_day_opus` → `seven-day-opus`. The overage fields give the window `overage` |
| `used` | `utilization`. A value above 1 is reported as `1`; a negative value, or one that is not a number, is absent |
| `resets-at` | `resetsAt` × 1000, so epoch seconds become epoch milliseconds. `overageResetsAt` × 1000 for `overage`. A value that is not a non-negative integer, or overflows as milliseconds, is absent |
| `window-minutes` | `300` for `five_hour` and `10080` for `seven_day`. Absent for every other window: `claude` never sends a length, and the driver states one only for the two it knows |
| `state` | `status`, or `overageStatus` for `overage`: `allowed` → `ok`, `allowed_warning` → `warning`, `rejected` → `exhausted`. Any other value is absent |

The windows come in this order:

1. **One per `unifiedWindows` entry** whose value is an object, in key order. Each window's
   members come from that entry.
2. **The window the top-level `rateLimitType` names.** The top-level `status`, `resetsAt` and
   `utilization` describe that window. When step 1 produced it, they fill only the members it
   left absent. Otherwise they make a window of their own, after the step-1 windows. With no
   `rateLimitType` they name no window and are not read.
3. **`overage`, last.** It appears when `overageStatus` or `overageResetsAt` is present and not
   `null`, or when `rateLimitType` is `overage`. In that last case the top-level fields fill the
   members the overage fields left absent.

Every member but `name` is optional, and **absent means the line did not carry it**, never zero.
A line that produces no window becomes one `note`, never an empty `quota`. The driver does not
check names against the runtime's rule; the runtime notes a window whose name it refuses.

`used` is capped at 1 because `claude` reports utilization above 1 when use runs past a
window's cap, and the runtime refuses a whole window whose fraction is above 1.

Not read, because the interface has no member for them: `surpassedThreshold`,
`overageDisabledReason`, `isUsingOverage`, `overageInUse`, `overageEnabled`, `limitScope`,
`rateLimitGraceActive`, `uuid`, `session_id` and any other field.

**Each line is sent whole, and nothing is carried from one line to the next.** The runtime
replaces a window of the same name whole and leaves the others standing. So a line naming only
`seven_day` updates `seven-day` and leaves an earlier `five-hour` as it was. The runtime records
the windows, shows them in `session-status`, `mur trace show` and the task stream, and never
acts on them.

The verbatim line from a subscription run, and the windows it gives:

```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1790900400,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.01,"resetsAt":1790900400},"seven_day":{"utilization":0.11,"resetsAt":1791399600}}},"uuid":"…","session_id":"…"}
```

| name | used | resets-at | window-minutes | state |
|---|---|---|---|---|
| `five-hour` | `0.01` | `1790900400000` | `300` | `ok`, from the top-level `status` |
| `seven-day` | `0.11` | `1791399600000` | `10080` | absent |
| `overage` | absent | absent | absent | `exhausted` |

### The suggested next prompt

With `prompt_suggestions` on, `claude` prints a `prompt_suggestion` line after a turn's
`result`. Its `suggestion` becomes one `suggested-prompt`, verbatim and even when empty; the
runtime drops an empty one itself. With the key off, the driver never asks for suggestions, and
a `prompt_suggestion` line is noted like any other line it does not read.

### The other events

- **`session-started`** takes its id from `session_id` and its model from `model`. `auth` is
  `subscription` only when the harness reports `apiKeySource: "none"`; every other value reads
  as `api-key`, **absent included**. The runtime warns on anything that is not `subscription`,
  so the ambiguous case fails towards being warned about rather than towards a quiet
  subscription claim. The eleven mock recordings in `tests/fixtures/` authenticated with an API
  key and read `api-key`. `11-subscription-rate-limit` ran on a subscription login and reads
  `subscription`.
- **`retry`** takes `attempt` from the line and reads its reason as `<error> (<error_status>)`,
  falling back to whichever of the two the line carries, and to `unknown` with neither.
- **`tool-call`** strips the prefix `launch` added, and **only** that prefix. A generic
  `mcp__<anything>__` strip would also rename the tools of an MCP server this driver never
  registered; with no bridge recorded, a tool name passes through untouched.
- **`tool-result`** pairs by `tool_use_id`. Its output is the block's `content` when that is a
  string, and otherwise its blocks joined with newlines: a text block gives its text, an `image`
  block gives one reference line (below), and any other block keeps its JSON rather than being
  dropped. A `content` that is a single `image` object gives the line on its own.

### Images in a tool result

An image block never reaches the event feed as its bytes. In its place, the `tool-result` output
carries one line:

    [image: <media type>, <n> bytes]

The driver reads both shapes an image block arrives in. `claude` writes the Anthropic one.

| Field | Read from, first match wins |
|---|---|
| media type | `source.media_type` (when `source` is an object), `mimeType`, `media_type`; the first non-blank string |
| data | `source.data` (when `source` is an object), `data`; the first string |

`<n>` is the size the base64 decodes to, worked out from its length without decoding it.
ASCII whitespace is ignored. Any character outside `A–Z a–z 0–9 + /` makes the size unknown,
and so does `=` anywhere but the last one or two places, padded data that is not a whole number
of 4-character groups, and unpadded data that ends on a single character. The fallbacks:

| Block | Line |
|---|---|
| no media type | `[image: unknown media type, 19 bytes]` |
| no data string (a `source.type: "url"` image, `"source":"s"`) or data that is not base64 | `[image: image/png, size unknown]` |
| neither | `[image: unknown media type, size unknown]` |

**The size is what `claude` reported, not necessarily what the tool attached.** `claude` passes a
small image to its model byte for byte. It re-encodes a large one. In the mock recording below, a
12,005,713-byte 2000×2000 PNG went to the model as a 1932×1932 JPEG of 429,284 bytes, and the
line then reads `[image: image/jpeg, 429284 bytes]`.

**The line is murmur's own trace placeholder without the hash.** murmur writes
`[image: <media type>, <n> bytes, sha256 <hex>]` where a tool result's output repeats an image's
base64. With this driver the output never does, so the hash is found in the trace's `tool_call`
record, in its `attachments` entry `{"path","media_type","bytes","sha256"}` for the same call.

**Claude Code shows its model an image a murmur tool returns.** `claude` 2.1.316 was checked two ways:

- **Mock recording `12-bridge-image-result`.** The request `claude` sent its model after the
  tool call holds the bridge's image as an `image` block, `source.type: "base64"`, with the
  bridge's exact bytes (same sha256). The block sits between the bridge's text item and a text
  block `claude` adds itself, `[Image: source: <path>]`. That path names the copy `claude` saves
  under `$HOME/.claude/projects/<project>/<session>/tool-results/`. The `user` line on stdout
  carries the same three blocks, base64 included, in `message.content` and again in
  `tool_use_result`.
- **A live subscription run of `tests/live/image-check.sh`**, on a `mur` built at `4f65bb98`.
  On `--model haiku`, the model read the number and the colour off a picture it could see only
  in the tool result, and the script printed `PASS`. That was the implementing agent's own
  run. The maintainer's run is pending, with the command in
  [`tests/live/README.md`](tests/live/README.md#the-live-check).

**Image items reach the harness only on a murmur with `4f65bb98` or later.** That commit makes the
tool bridge answer a bridged call with an MCP `image` item per accepted attachment. On an
earlier murmur, the tool result carries only text, and there is no image for this driver to
report.

**murmur can pair attachments with the wrong call.** murmur pairs a tool's `AttachmentReport`
with the driver's `tool-result` by tool name and arguments. It first looks for a report whose
base64 appears verbatim in the output. This driver never puts base64 in the output, so murmur
always falls back to the oldest report with the same name and arguments. Two concurrent calls
with the same name and arguments, returning different images and reported out of order, can
have their `attachments` swapped in the trace. Neither murmur nor this driver works around it.

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

`tests/fixtures/` holds twelve recordings of real `claude` output, each with its stdout, its
stderr and its exit code:

| Recordings | claude | Auth | Launched with |
|---|---|---|---|
| `01` to `10` | 2.1.278, against a mock endpoint and a mock bridge; byte-identical copies of the roadmap's | API key | the bridge `claude_bridge`, no config |
| `11-subscription-rate-limit` | 2.1.314, a real subscription turn on `--model haiku` | subscription | no bridge, `prompt_suggestions: true` |
| `12-bridge-image-result` | 2.1.316, against a mock endpoint and a mock bridge that answers with murmur's text item, then an MCP `image` item | API key | the bridge `claude_bridge`, no config |

`tests/golden.rs` asserts the **complete** event list for every one. It also asserts, across
every recording:

- no recording produces a `note`;
- every one ends with a terminal event, so the runtime never reaches `classify-exit` for any of
  them, including the two that exit `1`;
- no recording whose `result` line says `is_error: true` ends the turn successfully;
- each `rate_limit_event` line gives one `quota`, and only the subscription recording has one.

`12-bridge-image-result` gives one `tool-result` with three lines: the bridge's text, the
reference line `[image: image/png, 692 bytes]`, and the `[Image: source: …]` text block `claude`
adds. No 16-character run of the recorded base64 appears in it.

`11-subscription-rate-limit` gives `session-started` with `auth: "subscription"` and one `quota`.
Its line names only `seven_day` in `unifiedWindows`. Read with suggestions off, it gives the same
events, with each `suggested-prompt` replaced by one `note`.

**No recorded run printed a `prompt_suggestion`.** Three runs were made, of one, two and three
turns. `claude` skips a session's first turn by rule. The second turn of the two-turn run had a
cold cache, because the login's claude.ai connector tools reached the tool list between turns.
The third turn of the three-turn run read a warm cache and still printed none. The
`suggested-prompt` unit tests use a line in the shape claude 2.1.314's bundle declares. The
three-turn run is not committed, because its later `init` lines list the login's own connectors.

Each recording is parsed four ways, each partition with its own fresh parse state carried
across its batches: all lines in one batch, one line per batch, a two-batch split whose boundary
falls between a message's deltas and the `assistant` line carrying its full text, and a
two-batch split between a tool call's `content_block_start` and its first `input_json_delta`.
All four must produce the same events. Only `03-bridge-tool-call` and `12-bridge-image-result`
stream tool input, so they are the only recordings the fourth partition applies to; the others
are checked to stream none.

## Building it

```bash
cargo test -p driver-claude-code
cargo build -p driver-claude-code --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/driver_claude_code.wasm
./scripts/local-install.sh driver-claude-code <capsule-dir>
```

The crate is three layers, the shape every artifact in this repo uses: the pure logic at the
crate root, a `#[cfg(target_arch = "wasm32")] mod wasm_driver` that converts WIT records to the
crate-root mirrors and back and decides nothing, and `#[cfg(test)] mod tests` at the crate root
so the tests run on the host, with the recordings and their golden event lists under `tests/`.
Code behind the wasm gate does not exist for the host target, so logic written there would
report a green `cargo test` having executed none of its lines.
