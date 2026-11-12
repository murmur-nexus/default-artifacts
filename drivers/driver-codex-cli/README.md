# driver-codex-cli

The process driver for OpenAI's [Codex CLI](https://developers.openai.com/codex). It tells Murmur
how to drive `codex exec --json` for one turn: the argument list, the system prompt as a private
instructions file, the `-c mcp_servers.<server>.*` overrides that point the harness at the
capsule's tool bridge, the bridge token as one environment variable, and the task on stdin. It
reads the JSONL Codex prints back into murmur's events.

A turn spends the ChatGPT subscription `codex` is logged into, not an API key. The model is offered
the capsule's own tools through murmur's tool bridge; Codex's own shell, patch-writing, web,
browser, image, agent and plugin tools are switched off or refused by a read-only sandbox, and the
operator's own Codex config is kept out.

Every Codex fact lives in this crate, as a named constant at the top of `src/lib.rs`. The runtime
runs *a* process driver; it never learns it is running the Codex one.

**The driver is granted nothing** — no network, no filesystem, no environment, no Murmur host
import. It runs on an empty WASI context and is pure translation from a launch request to a launch
plan. It holds no credential and declares no auth block: the harness authenticates itself.

## Which murmur this needs

This artifact exports `murmur:driver/process@0.6.0`, which murmur serves from v0.11.0. The host
accepts exactly one version of that interface, so `murmur/driver-codex-cli` needs `mur` v0.11.0 or
later. An older `mur` refuses it at launch with `E-WIT-LEVEL-001`.

## The manifest an operator writes

```yaml
artifacts:
  - name: murmur/driver-codex-cli
    version: "0.1.0"
    runtime: driver

inference:
  transport: process
  model: gpt-5.5                 # optional; blank leaves the choice to codex
  system_prompt_file: ./instructions.md
  driver:
    artifact: murmur/driver-codex-cli
    config:                      # optional; see "Driver config" below
      reasoning_effort: high
      reasoning_summary: auto
  # command: /usr/local/bin/codex   # optional; overrides the `codex` this driver names

lifecycle:
  conversation: threaded         # optional; continues Codex's thread across tasks

capabilities:
  env:
    allow: [HOME, PATH]
  harness:
    paths:
      - { path: ~/.codex, access: read-write }
  network:
    allow: [chatgpt.com, auth.openai.com]
  shell:
    allow: [true]                # see "The TLS trust store" below
```

| Field | Why |
|---|---|
| `capabilities.env.allow: [HOME, PATH]` | The two variables `describe()` requires; a capsule without either is refused with `E-CAP-019` |
| `capabilities.harness.paths` `~/.codex` | The login store and Codex's state, which `describe()` declares as a harness need; without it the launch is refused with `E-CAP-021` |
| `capabilities.network.allow` | The two hosts `describe()` declares; same refusal |
| `capabilities.shell.allow` | Not a need of the driver: under mur 0.11.0 it is what hands the harness the host's TLS trust store |

### The TLS trust store

Under mur 0.11.0 a process harness reads the host's certificate store only when
`capabilities.shell.allow` is non-empty. Codex verifies HTTPS against that store. Without it, every
request fails with `invalid peer certificate: UnknownIssuer`, and the turn fails with
`E-RUN-033 … (harness-error): Connection failed: error sending request`.

`shell.allow: [true]` is the smallest grant that fixes it. Every `shell.allow` entry is also
offered to the model as a tool through the bridge, so the model can run `/usr/bin/true`, which does
nothing. The guest runs below show both cases. When murmur grants a harness the trust store by
itself, the entry can go.

## Driver config

`inference.driver.config` accepts two keys. Each value must be a JSON string that is exactly one of
the names listed:

| Key | Passed as | Accepted values |
|---|---|---|
| `reasoning_effort` | `-c model_reasoning_effort="<value>"` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`, `persistent` |
| `reasoning_summary` | `-c model_reasoning_summary="<value>"` | `auto`, `concise`, `detailed`, `none` |

The effort list is Codex's own: `impl FromStr for ReasoningEffort` in
`codex-rs/protocol/src/openai_models.rs` at tag `rust-v0.154.0` (commit `6b9826e3`), the same
names both tested binaries hold. The
[config reference](https://developers.openai.com/codex/config-reference) gives no closed list. It
says "such as `low`, `medium`, `high`, `xhigh`, `max`, or `ultra`", and that the levels available
depend on the model. Which level a model honours is up to the model.
`codex` itself accepts any effort string at config load, so this driver's list is what catches a
typo. The summary list is what `codex` names when it refuses an unknown value.

Absent, `{}` or whitespace means neither flag is passed. Anything else is refused before `codex` is
started:

| Config | Refusal |
|---|---|
| Not JSON | `driver: inference.driver.config is not valid JSON: <error>` |
| Not an object | `driver: inference.driver.config must be a JSON object` |
| An unknown key | `driver: unrecognised inference.driver.config key(s): <keys, sorted>. Accepted keys: reasoning_effort, reasoning_summary` |
| A value not in the list (`"high "`, `"HIGH"`, `3`, `null`) | `driver: inference.driver.config '<key>' must be one of <values>, got <value as JSON>` |

## Login check and harness needs

| Field | Value |
|---|---|
| `login-check` | `command(["login", "status"])` |
| `harness-needs.paths` | `~/.codex`, `read-write`: the login store `auth.json` and its token refresh, the `sessions/` rollouts `resume` reads, the log and state databases, and the managed install under `~/.codex/packages` |
| `harness-needs.hosts` | `chatgpt.com` (the ChatGPT backend that serves a subscription turn) and `auth.openai.com` (token refresh). A live ChatGPT run has not confirmed them yet |

`codex login status` writes its answer to stderr. `read-login` reads only the exit code and the
first non-empty line, from stderr, or from stdout when stderr is empty:

| Output | Reading |
|---|---|
| `Not logged in`, whatever the exit code | logged out: ``codex is not signed in: run `codex login` as the user mur runs as`` |
| Any other line, exit `0` | logged in |
| Any other line, another exit code | unknown: `` `codex login status` exited <n> with output this driver does not recognise `` |
| Nothing, another exit code | unknown: `` `codex login status` exited <n> without printing anything `` |
| Ended by a signal | unknown: `` `codex login status` was ended by a signal `` |

No reason quotes the output. An API-key login prints a masked key.

**An API-key login reads as logged in.** `codex login --with-api-key` stores the key in the same
`auth.json`, and every turn then bills that key rather than a ChatGPT plan. The runtime cannot tell
the two apart: see [`session-info.auth`](#session-infoauth-is-unknown).

**`CODEX_HOME` outside `~/.codex` is not covered.** The declared need is `~/.codex`. A Codex
pointed elsewhere with `CODEX_HOME` keeps its login store where the harness cannot reach it under
`scoped`.

## Required variables, and why each one stays

Both `HOME` and `PATH` are in `describe().required_env`. These probes ran on 2026-11-12, each from
`env -i` with a scratch `HOME` holding no `auth.json`, and the turns against the mock provider:

| Probe | 0.154.0 | 0.167.2 |
|---|---|---|
| `env -i codex --version` | exit `0`, `codex-cli 0.154.0` | exit `0`, `codex-cli 0.167.2` |
| only a scratch `HOME` and `PATH=/usr/bin:/bin` | exit `0`, the same line | exit `0`, the same line |
| a mock turn with `HOME` dropped | exit `0`, `{"type":"thread.started",…}` | exit `0`, the same (run with `CODEX_HOME` set to the scratch store) |
| a mock turn with `PATH` dropped | exit `0`, `{"type":"thread.started",…}` | exit `0`, the same |

Neither variable was needed by a mock turn, and both stay:

- **`HOME` survives being dropped because `codex` falls back to the passwd entry's home.** Without
  `HOME` it opened that home's `~/.codex`. A ChatGPT login lives in `~/.codex/auth.json`, and the
  harness need is granted only as `~/.codex` under the `HOME` the capsule passes.
- **`PATH` survives being dropped because `codex` is one static binary.** Murmur's own resolution
  of a bare `codex` walks `PATH` whenever the manifest sets no `command:`.

## Optional variables an operator may want to allow

| Variable | Why |
|---|---|
| `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` (and lowercase) | reaching OpenAI through a corporate proxy |
| `CODEX_HOME` | moving Codex's state off `~/.codex`; see the caveat above |

`OPENAI_API_KEY` cannot be allowed: murmur refuses it with
`error[E-CAP-016]: capabilities.env.allow names 'OPENAI_API_KEY' (credential backstop pattern 'OPENAI_API_KEY')`.

## Running as root

Codex has no root guard. As uid 0 in a user namespace (`unshare -r`), a text turn and a bridged
tool turn both exit `0` on 0.154.0 and 0.167.2, with the same events as an ordinary user. The
recordings in the tarball were made as root. `classify-exit` adds no root line.

## The argv this driver produces

One `codex exec --json` run per turn, every flag before `resume`:

```
exec --json --skip-git-repo-check --ignore-user-config --strict-config
-s read-only -c approval_policy="never"
--disable shell_tool --disable unified_exec --disable view_image --disable multi_agent
--disable goals --disable browser_use --disable computer_use --disable image_generation
--disable apps --disable plugins --disable in_app_browser --disable tool_suggest
--disable skill_search --disable sleep_tool --disable unbounded_connection_retries
-c web_search="disabled" -c project_doc_max_bytes=0 -c check_for_update_on_startup=false
-c model_instructions_file="{files_dir}/instructions.md"
-c mcp_servers.<server>.url="<url>"                                   (bridge only)
-c mcp_servers.<server>.bearer_token_env_var="MURMUR_BRIDGE_TOKEN"    (bridge only)
-c mcp_servers.<server>.default_tools_approval_mode="approve"        (bridge only)
-c model_reasoning_effort="<level>"                                  (configured only)
-c model_reasoning_summary="<mode>"                                  (configured only)
-m <model>                                                           (non-blank model only)
resume <session id>                                                  (resume only)
-
```

| Flag | Why |
|---|---|
| `exec --json` | One non-interactive turn, one JSON event per line |
| `--skip-git-repo-check` | Runs in any working directory, git or not |
| `--ignore-user-config` | Keeps `$CODEX_HOME/config.toml` out, its MCP servers and hooks included, and keeps the stored login |
| `--strict-config` | Refuses an unknown `-c` key before any request, so a renamed key fails the turn instead of doing nothing |
| `-s read-only` | Refuses any write by Codex's own tools; `apply_patch` answers `patch rejected: writing is blocked by read-only sandbox` |
| `approval_policy="never"` | A headless turn never stops to ask |
| `--disable` × 14 | Each feature offers the model a tool of Codex's own: the shell, `exec`/`write_stdin`, `view_image` (which reads any image on the host), sub-agents, goals, browser and computer use, image generation, connectors, plugins, the in-app browser, tool and skill suggestion, and sleep |
| `--disable unbounded_connection_retries` | With it on, a connection failure is retried forever, with `Reconnecting... waiting for network` at least once a minute, so the runtime's 600 s inactivity limit never ends the turn. Off, it is retried five times and the turn fails |
| `web_search="disabled"` | Removes the hosted web search tool |
| `project_doc_max_bytes=0` | Keeps an `AGENTS.md` in the working directory out |
| `check_for_update_on_startup=false` | Keeps the version check off the network |
| `model_instructions_file` | Makes the system prompt Codex's base prompt, replacing its own. `developer_instructions` would only add a message beside Codex's prompt |
| `mcp_servers.<server>.*` | Registers the bridge under the name murmur hands the driver. `default_tools_approval_mode="approve"` is needed under `approval_policy="never"`: without it every bridged call fails with `MCP tool call requires approval, but approval policy is never` |
| `-m` | The model, trimmed; a blank model is left to Codex |
| `resume <id> -` | Continues the thread Codex reported on `thread.started`. A new session puts no id in argv: Codex mints its own |
| `-` | Reads the task from stdin |

Every `-c` value is a TOML basic string written with `serde_json` string quoting, so a quote or a
backslash in a URL escapes rather than breaking the override. The server name becomes part of a key
path, so the driver refuses one that is empty or holds anything but ASCII letters, digits, `_` and
`-`. It refuses a resume whose id is blank or starts with `-` too, because Codex would read that
id as a flag. The driver never passes `--dangerously-bypass-approvals-and-sandbox` (under it Codex
wrote outside the work folder), `--approve-for-me`, `--dangerously-bypass-hook-trust`,
`--ephemeral`, `--oss`, `-p`/`--profile` or `forced_login_method`.

### Why the token is in `env-set`

`env-set` is exactly `[("MURMUR_BRIDGE_TOKEN", <bearer token>)]` with a bridge, and empty without
one. Codex reads the token through `bearer_token_env_var`, so it is never in argv, which any process
on the host can read, and never in a file. The driver sets no other variable.

`files` is exactly `instructions.md`, holding the system prompt byte for byte. `stdin` is the task
byte for byte, with `keep-stdin-open: false` and no `interrupt-stdin`: the turn is interrupted with
`SIGINT`.

## Reading what the harness prints

| Line | Events |
|---|---|
| blank or whitespace-only | none |
| not JSON, not an object, or an absent or unknown `type` | one `note`: `unreadable stdout line: <line>` |
| `thread.started` with a non-empty `thread_id` | `session-started { id: thread_id, auth: "unknown", model: none }` |
| `thread.started` with an empty or absent `thread_id` | none |
| `turn.started`, `item.updated` | none |
| `item.started`/`item.completed` of a capsule call not yet announced | `tool-call-started { id, name }`, then `tool-call { id, name, input }`, the arguments as compact JSON (`{}` when absent or null) |
| `item.started` of a capsule call already announced | none |
| `item.completed` of a capsule call | after the two above if not yet announced: `tool-result { id, output, is_error }` |
| `item.started` of an `mcp_tool_call` that is not a capsule call | one `note` |
| `item.completed` of an `mcp_tool_call` that is not a capsule call | one `note` |
| `item.started` of any other item | none |
| `item.completed` `agent_message` | `text` when non-empty; it becomes the turn's result |
| `item.completed` `reasoning` | `thinking` when non-empty |
| `item.completed` `error` | one `note` |
| `item.completed` of any other item, or of one with no readable `type` | one `note`, or `unreadable stdout line` |
| `error` whose message is `Reconnecting... <n>/<m> …` (or with `…`) | `retry { attempt: n, reason }`, the reason inside the outermost parentheses, else the rest of the line, else `unknown` |
| any other `error` | one `note` |
| `turn.completed`, new session | `usage` when any count was read, then `turn-end(<last agent message>)` |
| `turn.completed`, resumed session | a `note` with the running total when any count was read, then `turn-end(<last agent message>)` |
| `turn.failed` | `turn-failed { kind, message }` |

`parse` never fails and never panics. The same lines in any batch partition give the same events.
What one batch leaves for the next is the set of announced calls, at most 1,024, oldest forgotten
first, and the last agent message.

### Capsule calls and notes

A **capsule call** is an `mcp_tool_call` item with a non-empty `id`, a `server` equal to the bridge's
server name, and a non-empty `tool`. Codex reports `tool` under the name the bridge listed, which is
the bare artifact name (`session-status` is offered to the model as `session_status` but reported as
`session-status`), so murmur matches the call to the bridge's own report. With no bridge, no call
is a capsule call. The driver hardcodes no server name.

Codex restarts its item ids (`item_0`, `item_1`, …) in every process. The runtime pairs a call with
its result within one run, so that is enough.

Every note that carries Codex's text cuts each piece at 512 characters plus `…`. No note carries a
call's arguments:

| Source | Note |
|---|---|
| an unreadable line | `unreadable stdout line: <line>` |
| another server's call starting | `codex started a call to '<tool>' on MCP server '<server>', which is not one of the capsule's tools (call <id>)` |
| another server's call ending | `codex finished a call to '<tool>' on MCP server '<server>', which is not one of the capsule's tools (call <id>, status <status>)` |
| an `error` item | `codex error item: <message>` |
| one of Codex's own items | `codex reported its own '<type>' item (<id>)`, plus `, status <status>` when it carries one |
| an `error` line that is not a numbered retry | `codex error: <message>` |
| a resumed turn's counts | `codex reports a resumed thread's running total, not this turn's spend, so no usage is reported: <usage as JSON>` |

### Results and images

`is_error` is true when the call's `status` is not `completed`, or its result carries a JSON `true`
under `isError` or `is_error`. Codex reports two kinds of failed call, both as `status: "failed"`:

| Recording | What Codex printed | Output |
|---|---|---|
| tarball `c07` | a call Codex never made: `error.message` set, no `result` | the message |
| `07-bridge-tool-error` | a tool that answered `isError: true`: no `error`, the answer in `result`, the flag dropped | the answer |

So a failed call's output is its `error.message`, else its result, else `codex reported the call
failed with no message`. A result's output is its `content` items, one per line: text as itself,
an image as `[image: <media type>, <n> bytes]`, any other item as compact JSON. When the content is
empty, the output is `structured_content` as JSON.

The image line names the image and never carries its base64. The size comes from the base64 length,
with nothing decoded; a missing type reads `unknown media type`, unreadable data `size unknown`.

In recording `06-bridge-tool-image`, the bridge answered the way murmur's does: a text item, then an
MCP `image` item holding a 692-byte PNG. Codex printed the base64 on its `item.completed` line. The
feed carries `[image: image/png, 692 bytes]`. In the request after the call, Codex passed the model
the image itself: an `input_image` with `detail: "high"` whose bytes have the PNG's sha256
(`22b99df3…`), on both 0.154.0 and 0.167.2.

### How `turn.failed` is read

Codex describes a failure only in text, with no status field, so the kind is read from the status
or limit message the text quotes, in this order:

| Kind | The message contains |
|---|---|
| `auth` | `401 Unauthorized`, `403 Forbidden`, `status 401` or `status 403` |
| `quota` | `429 Too Many Requests`, `status 429`, `You've hit your usage limit` (with an ASCII or a typographic apostrophe) or `Quota exceeded` |
| `harness-error` | anything else |

A bare number never counts. The message is `error.message`, or `the harness reported a failure with
no message`. A mock `usage_limit_reached` 429 made 0.154.0 print `You've hit your usage limit.
Upgrade to Pro (…), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try
again at <time>.` and 0.167.2 the same with `You’ve`. The real text on a ChatGPT plan is still to
be seen.

### Tokens

`turn.completed.usage` holds `input_tokens` (cache reads and writes included), `cached_input_tokens`,
`cache_write_input_tokens`, `output_tokens` (reasoning included) and `reasoning_output_tokens`. The
driver reports `input = input_tokens − cached − cache_write` (saturating at 0), `cache_read`,
`cache_creation`, `output` and `thinking`. Each count is absent when missing or not a non-negative
integer; a reported zero is zero.

That `input_tokens` folds in cache writes is from Codex's source: `codex-api` copies the provider's
`input_tokens` and its `input_tokens_details` verbatim, and its own test reads `input_tokens: 100`
with 40 cached and 60 written.

The counts are **the thread's running total**, not the turn's. One thread read 120, 240, 360 and 480
over four one-request turns. On a new session that total is the run's total, which is what the
interface asks for, so it is reported verbatim. On a resumed turn it includes every earlier turn,
and murmur would charge the whole history to this one, so **a resumed turn reports no usage**. It
writes a note with the total instead.

On a threaded capsule this means `inference.max_session_tokens` counts only the turns that start a
session: the first task of a context, and any task after `--forget-session`. Later tasks in the
context spend without being counted.

### `session-info.auth` is `unknown`

`codex exec --json` does not say whether a turn bills a ChatGPT plan or an API key, so every session
reports `auth: "unknown"`, and murmur warns `W-SEC-031` on every session. To confirm the billing,
run `codex login status` as the user mur runs as: `Logged in using ChatGPT` is a plan.

### `classify-exit`

For a run that printed no `turn.completed` or `turn.failed`:

| Exit | Event |
|---|---|
| interrupted, whatever the code | `turn-failed(canceled)`, `codex was interrupted before it reported a result` |
| code `0` | `turn-failed(harness-error)`, `codex exited without a result` |
| any other | `turn-failed(harness-error)`, `codex exited with code <n>` / `codex was killed by signal <n>` / `codex exited without a status`, plus `: ` and the stderr tail |

The tail drops every line holding `fail to delete session` (murmur's bridge answers the MCP session
`DELETE` with 400, which Codex logs after every run) or `could not create PATH aliases` (Codex
refuses helper binaries under a temporary `CODEX_HOME`). When nothing else is left, the whole
trimmed tail is kept. A missing thread reads `codex exited with code 1: Error: thread/resume:
thread/resume failed: no rollout found for thread id <id> (code -32600)`. `SIGINT` gives exit 1
with no terminal line.

Codex keeps running for a few seconds after `turn.completed`. Murmur ends it after its 3-second
exit grace, and the trace shows `harness_exit` with `signal: 9, cause: "terminal"`. The turn is
unaffected.

## What Codex still does that Claude Code does not

| driver-claude-code | driver-codex-cli |
|---|---|
| Granted nothing, holds no credential | Same |
| Operator config kept out (`--setting-sources ""`, `--strict-mcp-config`) | `--ignore-user-config`, `project_doc_max_bytes=0`. Global `AGENTS.md`, the system skills list, the permissions text and the environment context are declared below |
| Only bridge tools | The disable list, `web_search="disabled"` and the read-only sandbox. Leftovers declared below |
| Token in a private file, not argv | Token in `env-set`, not argv |
| Session new and resume with the runtime's id | Codex's own id, reported in `thread.started` and stored by the runtime |
| Stdin control interrupt | `SIGINT`; `classify-exit` reports canceled |
| Driver config, strict refusals by name | Same rules; keys `reasoning_effort`, `reasoning_summary` |
| Login check that never echoes the output | `codex login status`, read by exit code and the logged-out line |
| `harness-needs` paths and hosts | `~/.codex` read-write; `chatgpt.com`, `auth.openai.com` until the live run confirms them |
| `required-env` HOME, PATH with probes recorded | Same, probes recorded for Codex |
| Text and thinking deltas | Not possible; complete items only, `streams-text: false` |
| `tool-call-started` and byte progress, never input | `tool-call-started` only; Codex reports no progress |
| Usage summed per run, absent is not zero | New sessions only; resumed turns report no usage |
| Quota windows from `rate_limit_event` | None: `codex exec --json` prints nothing for Codex's rate-limit headers or its `codex.rate_limits` event |
| Prompt suggestions | Not possible |
| Image placeholder in tool results | Same, from the MCP result content |
| Failure kinds from structured fields | From message text |
| Root guard message | No root guard |
| Recordings, golden tests, partition invariance | Same |
| Tested versions, `W-RUN-002` outside them | Same |

The gaps, plainly:

- **Codex's own tools that stay offered.** The three MCP resource tools (`list_mcp_resources`,
  `list_mcp_resource_templates`, `read_mcp_resource`) reach only the bridge. `request_user_input`
  stays offered and answers `request_user_input is unavailable in Default mode`. `apply_patch`
  stays offered on models whose metadata asks for it (0.154.0 offers it to `gpt-5.4`), and the
  read-only sandbox refuses it. On models with search support (0.154.0 and `gpt-5.4`), Codex hides
  the bridged tools behind its client-side `tool_search`, which prints nothing on stdout: the model
  searches first, then calls the tool, and the turn takes one request more.
- **Context that still loads.** Beside the system prompt, Codex sends a developer message
  listing its system skills (`imagegen`, `openai-docs`, `plugin-creator` on 0.154.0; `imagegen`,
  `openai-docs`, `skill-creator`, `skill-installer` on 0.167.2), a developer message with the
  read-only permissions text, the global `~/.codex/AGENTS.md` as a user message, and an
  `<environment_context>` user message with the working directory, shell, date, timezone and
  filesystem policy. 0.154.0 sends the system prompt as the request's `instructions`; 0.167.2 sends
  it as the first developer message. An `AGENTS.md` in the working directory and a
  `.codex/config.toml` there (its developer instructions, an MCP server and a hook) did not reach
  the run.
- **Code mode models** (the gpt-5.6 and gpt-6 families) send no `tools` field. Their tools come in
  an `additional_tools` developer item: `exec` (JavaScript run in Codex's local code-mode host),
  `wait`, `request_user_input`, and a `collaboration` namespace (`spawn_agent`, `send_message`,
  `followup_task`, `wait_agent`, `interrupt_agent`, `list_agents`) that `--disable multi_agent` does
  not remove. 0.167.2 adds `Deferred tool namespaces: - mcp__<server>`; 0.154.0 does not name the
  bridge. Both say `node_repl_disabled: false`. `--disable code_mode_host` changes none of that:
  it only adds an `error` item, `Code Mode is unavailable because code-mode host is disabled. Code
  mode will fail closed`. The 0.167.2 release binary alone prints the same kind of item because
  `codex-code-mode-host` is not beside it. The host runs inside the capsule's containment. Whether
  a code mode model reaches the bridge, and what else its host can do, is unknown until the live
  run. The driver does not disable `code_mode_host`.
- **No text deltas and no tool progress.** Codex prints messages and reasoning complete.
- **No usage on a resumed turn**, as above.
- **`auth` is `unknown`**, so `W-SEC-031` is on every session.
- **Failure kinds are read from text.**
- **Every run logs `fail to delete session … DELETE returned HTTP 400`** on stderr. It is harmless:
  murmur's bridge answers `DELETE` with 400, and the turn is unaffected.
- **A Codex release that drops a disabled feature's name refuses every turn** with
  `Error: Unknown feature flag: <name>`, before any request. A release outside the tested ones is
  warned as `W-RUN-002`; check its `codex features list` before relying on it.

## The recordings this is tested against

`tests/fixtures/README.md` maps every fixture to its source. In short:

| Directory | Codex | Made with |
|---|---|---|
| `0.154.0/` | the installed `codex-cli 0.154.0` | exactly the plan `launch` builds, with bridge server `murmur`, plus the mock provider overrides |
| `0.167.2/` | `codex-cli 0.167.2`, the musl binary from the `openai/codex` release `rust-v0.167.2` | the same |
| `tarball-0.167.2/` | `codex-cli 0.167.2`, byte-identical copies from the grok/codex recordings tarball | the hardened 0.167.2 flag set with `developer_instructions`, server `murmur_bridge` |

`TESTED_VERSIONS` is `["0.154.0", "0.167.2"]`, the two directories recorded with the exact plan.
`tests/golden.rs` asserts every recording's complete event list under three batch partitions, and
`classify-exit`'s answer for each one with no terminal line. `tests/live/README.md` records the
probes, the guest runs and the handoff for a live ChatGPT run.

## Building it

```
cargo build -p driver-codex-cli --target wasm32-wasip2 --release
./scripts/validate-component.sh target/wasm32-wasip2/release/driver_codex_cli.wasm
cargo test -p driver-codex-cli
./scripts/local-install.sh driver-codex-cli <capsule-dir>
```

The component exports `murmur:driver/process@0.6.0` and imports no `murmur:*` interface. A local
install is pinned as `local/driver-codex-cli`.
