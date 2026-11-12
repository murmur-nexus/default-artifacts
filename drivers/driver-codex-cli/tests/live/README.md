# Recording, probing and running `codex` by hand

Nothing here runs under `cargo test`: Cargo ignores this directory. Every script uses only bash and
the Python standard library, runs Python with `python3 -I`, and refuses a scratch directory outside
`/space/scratch/`. None of them reads the operator's `~/.codex`: `codex` always gets a scratch `HOME`
with no `auth.json`.

| File | What it is |
|---|---|
| `mock.py` | A minimal OpenAI Responses SSE mock, from the grok/codex recordings tarball, extended: it writes every request body to a directory, can answer with Codex's rate-limit headers and `codex.rate_limits` event (`RATELIMIT`) or a `usage_limit_reached` 429 (`USAGELIMIT`), drives Codex's client-side `tool_search` before calling a deferred tool, and takes `ARGS=<json>` for a call's arguments |
| `bridge.py` | A copy of murmur's tool bridge from the tarball (`DELETE` answered 400 as murmur does), serving `echo_tool`, `session-status`, `picture_tool` (a text item, then an MCP `image` item with a real PNG) and `fail_tool` (`isError: true`) |
| `picture.py` | Draws the PNG `picture_tool` returns (copied from driver-claude-code) |
| `record.sh` | Records the fixture set for one `codex` binary with the exact plan `launch` builds |
| `probes.sh` | The probes below |
| `request_summary.py` | Prints a captured request's tools, instructions and messages, with image data replaced by its sha256 |
| `guest-run.sh` | The guest run: the compiled component under a real `mur run` with the mock provider |
| `guest-unauthenticated.sh` | The operator's capsule shape against OpenAI's real endpoint with no credential |

Each script builds the plan with `cargo run -p driver-codex-cli --example print_plan`, never by hand.

## Recording

```
./record.sh <codex-binary> <out-dir> /space/scratch/<fresh-dir> [case ...]
```

The fixtures were recorded with:

```
./record.sh ~/.codex/packages/standalone/releases/0.154.0-x86_64-unknown-linux-musl/bin/codex \
    /space/scratch/a6975964/rec2-154/out /space/scratch/a6975964/rec2-154
MOCK_PORT=18140 BRIDGE_PORT=18240 ./record.sh /space/scratch/a6975964/codex-0.167.2/bin/codex \
    /space/scratch/a6975964/rec2-167/out /space/scratch/a6975964/rec2-167
```

The 0.167.2 binary was fetched with
`gh release download rust-v0.167.2 -R openai/codex -p codex-x86_64-unknown-linux-musl.tar.gz`
into a fresh scratch directory. Its sha256 `8246e846c550ea21197fcc4067bc270dca895e94ea42ed6a282e6f4e0125b15f`
matched the digest GitHub publishes for the asset. `../fixtures/README.md` describes each case.

## Probes

```
./probes.sh <codex-binary> /space/scratch/<fresh-dir> strict features context ratelimit slash codemode env root
```

Run 2026-11-12 against `codex-cli 0.154.0` (the installed binary) and `0.167.2` (the release binary),
each with the plan `launch` builds plus the mock provider overrides.

### Flags under `--strict-config`

Every flag of the plan is accepted by both versions, `check_for_update_on_startup=false` included:
every recording with the full plan exits `0`. Neither version refuses anything, so nothing was
dropped. What a refusal looks like, on both:

| Probe | Exit | First stderr line |
|---|---|---|
| `-c no_such_key_for_probe=1` | `1` | ``Error loading config.toml: unknown configuration field `no_such_key_for_probe` in -c/--config override`` |
| `--disable no_such_feature_for_probe` | `1` | `Error: Unknown feature flag: no_such_feature_for_probe` |

### Features

All fifteen disabled names exist in both versions' `codex features list` (stable, on by default).

Every other stable, on-by-default feature was disabled one at a time, and the captured request's
tool list was compared with the plan's own. On neither version did disabling one take a tool away,
so none of them offers the model a tool:

| Version | Tools in the plan's request (mock model `gpt-5.4`) |
|---|---|
| 0.154.0 | `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource`, `request_user_input`, `apply_patch` (custom), `tool_search` (the bridged tools are behind it) |
| 0.167.2 | `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource`, `request_user_input`, `mcp__murmur/echo_tool`, `mcp__murmur/fail_tool`, `mcp__murmur/picture_tool`, `mcp__murmur/session_status` |

The features compared:

- **0.154.0.** `auth_elicitation`, `browser_use_external`, `browser_use_full_cdp_access`,
  `compaction_image_budget`, `content_item_kinds`, `enable_request_compression`, `fast_mode`,
  `guardian_approval`, `hooks`, `in_app_chat`, `in_app_dictation`, `in_app_local_automation`,
  `in_app_updates`, `mentions_v2`, `personality`, `plugin_sharing`, `remote_compaction_v2`,
  `remote_plugin`, `shell_snapshot`, `skill_mcp_dependency_install`, `tool_call_mcp_elicitation`,
  `unbounded_connection_retries`, `unified_exec_tty`, `workspace_dependencies`.
- **0.167.2.** The same, less `personality` (removed in 0.167.2), plus
  `api_key_cyber_access_programs`, `api_key_model_discovery`, `bedrock_setup_wizard`,
  `browser_annotation_api`, `cli_daybreak`, `code_mode_interrupt`, `daemon_auto_start`,
  `deferred_tool_world_state`, `guardian_reuse_parent_compaction`, `in_app_voice`,
  `incremental_tools`, `incremental_tools_legacy_gcp`, `mcp_oauth_refresh_coordination`,
  `plugin_discovery_prefetch`, `prediction_preserve_parent_tools`, `reasoning_effort_override`,
  `system_proxy_fallback`, `ultrafast_mode`, `write_stdin_approval`.

One exception: on 0.167.2, `--disable mcp_oauth_refresh_coordination` removed the **bridge's** tools.
That breaks the bridge rather than closing a leak, so it stays on.

`unbounded_connection_retries` offers no tool. It is disabled anyway, in its own named list
(`HEADLESS_DISABLED_FEATURES`). The second guest run showed why: a harness that cannot reach its
provider printed `Reconnecting... waiting for network` with a growing delay, capped at 60 s in
Codex's `core/src/responses_retry.rs`, so the turn never ended until murmur was stopped.
Disabled, the same capsule fails in seconds.

### Instructions, developer and user messages Codex adds

With a global `~/.codex/AGENTS.md` placed in the scratch home, the first request held:

| Role | Content | 0.154.0 | 0.167.2 |
|---|---|---|---|
| `instructions` | the system prompt | yes | `null` |
| developer | the system prompt | — | yes, first |
| developer | `<skills_instructions>`: the system skills | `imagegen`, `openai-docs`, `plugin-creator` | `imagegen`, `openai-docs`, `skill-creator`, `skill-installer` |
| developer | `<permissions instructions>`: `sandbox_mode` is `read-only` | yes | yes |
| user | `# AGENTS.md instructions` with the global `AGENTS.md` | yes | yes |
| user | `<environment_context>`: cwd, shell, date, timezone, filesystem policy | yes | yes |
| user | the task | yes | yes |

No personality text appears: `model_instructions_file` replaces the base prompt that carries it.

An `AGENTS.md` in the working directory (`project_doc_max_bytes=0`) and a `.codex/config.toml`
there did not reach the run on either version. That config held `developer_instructions`, an MCP
server `cwd_srv` and a `SessionStart` hook: no marker in the request, no `cwd_srv` request at the
bridge, and the hook did not run.

### Rate limits

| Mock answer | `codex exec --json` printed |
|---|---|
| 200 with `x-codex-primary-*`/`x-codex-secondary-*` headers (header names from `codex-api/src/rate_limits.rs` at `rust-v0.154.0`) and a `codex.rate_limits` SSE event | nothing about them: the same four lines as a plain turn, on both versions |
| 429 `usage_limit_reached` with the same headers | an `error` line and `turn.failed`, both `You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at May 17th, 2033 10:33 PM.` (0.167.2: `You’ve`, typographic, and `…/settings/usage`) |

So the driver emits no `quota`; the usage-limit text reads as `quota` through its marker.
`exec`'s JSONL has no event type for rate limits at all (`codex-rs/exec/src/exec_events.rs`).

### A task starting with `/`

`/status` and `/review` reach the model as plain user text, the request's last user message, and
the turn ends normally on both versions.

### The image after a tool call

After `picture_tool`, the next request's `function_call_output` held `Wall time: …`, the bridge's
text item, and an `input_image` with `detail: "high"` whose data has the PNG's sha256
`22b99df39a225a34bac4485a768b0ef792f43b07f586b4a3c0cd9a8d0e4cbd1b`: the exact image, on both
versions.

### Code mode and `code_mode_host`

`-m gpt-5.6-terra` with `CALL_TOOL=echo_tool`, with and without `--disable code_mode_host`:

| | `code_mode_host` on (the plan) | `--disable code_mode_host` |
|---|---|---|
| `tools` field | absent | absent |
| `additional_tools` developer item | `functions/exec`, `functions/wait`, `functions/request_user_input`, `collaboration/{followup_task, interrupt_agent, list_agents, send_message, spawn_agent, wait_agent}` | identical |
| `node_repl_disabled` in the turn metadata | `false` | `false` |
| `Deferred tool namespaces` | 0.167.2: `- mcp__murmur`; 0.154.0: none | identical |
| stdout | 0.154.0: nothing extra. 0.167.2 release binary: `Code Mode is unavailable because failed to spawn code-mode host …/codex-code-mode-host: host executable was not found` | `Code Mode is unavailable because code-mode host is disabled. Code mode will fail closed; enable features.code_mode_host and install codex-code-mode-host.` |

Disabling the host changes no tool the model is offered; it only makes code mode fail closed.
`--disable multi_agent` does not remove the `collaboration` namespace on a code mode model. Left on
for the live run to decide.

### `cache_write_input_tokens`

`codex-api/src/sse/responses.rs` at `rust-v0.154.0` copies the provider's `input_tokens` and
`input_tokens_details.{cached_tokens, cache_write_tokens}` verbatim. Its test
`parses_cache_write_token_usage` reads `input_tokens: 100` with 40 cached and 60 written, so
`input_tokens` includes both. The driver subtracts both.

### Required variables

| Probe | 0.154.0 | 0.167.2 |
|---|---|---|
| `env -i codex --version` | exit `0`, `codex-cli 0.154.0` | exit `0`, `codex-cli 0.167.2` |
| scratch `HOME`, `PATH=/usr/bin:/bin` | exit `0`, `codex-cli 0.154.0` | exit `0`, `codex-cli 0.167.2` |
| mock turn, `HOME` dropped | exit `0`, `{"type":"thread.started",…}`, and Codex used the passwd entry's home | exit `0`, `{"type":"thread.started",…}` |
| mock turn, `PATH` dropped | exit `0`, `{"type":"thread.started",…}` | exit `0`, `{"type":"thread.started",…}` |

Without `HOME`, `codex` takes its home from the passwd entry and opens that home's `~/.codex`. The
`HOME`-dropped probe therefore sets `CODEX_HOME` to the scratch store (the 0.167.2 row), so it
cannot touch the operator's own.

### Running as root

`probes.sh … root` runs a text turn and a bridged tool turn under `unshare -r` (uid 0 in a user
namespace). Both exit `0` on both versions, with the same events as an ordinary user.

## The guest runs

Unit tests never reach `mod wasm_driver`, so the compiled component ran under the installed
`mur 0.11.0` (commit `0105492330320938ae704b79fe177b0187b68a0a`, WIT level 6). Every run used a
scratch `HOME` for `mur` and `codex`.

### Two tasks against the mock

```
./guest-run.sh /space/scratch/a6975964/guest5
```

The script copies the installed `codex` into the scratch directory and writes a `#!/bin/sh` wrapper
there as `inference.command`. The wrapper exports `MOCK_KEY=sk-test`, appends its argv to
`harness-argv.log` in the work folder, and runs `exec <scratch>/bin/codex "$sub" <mock overrides> "$@"`.
It then writes the capsule, installs `tool-read` and the driver with `./scripts/local-install.sh`,
and runs:

```
cd <scratch>/capsule
HOME=<scratch>/home PATH=<scratch>/bin:$PATH mur run --task task1.md --context ctx-codex-guest --workdir <scratch>/capsule
HOME=<scratch>/home PATH=<scratch>/bin:$PATH mur run --task task2.md --context ctx-codex-guest --workdir <scratch>/capsule
HOME=<scratch>/home PATH=<scratch>/bin:$PATH mur doctor
```

`task1.md` is `CALL_TOOL=tool_read ARGS={"path":"notes.txt"} Read the notes.`, `task2.md` is
`Now answer in one line.` The capsule:

```yaml
name: codex-guest-run
version: 0.1.0
artifacts:
  - { name: local/driver-codex-cli, version: "0.1.0", runtime: driver }
  - { name: local/tool-read, version: "0.5.0", runtime: tool }
capabilities:
  containment: scoped
  env: { allow: [HOME, PATH] }
  shell: { allow: [codex] }        # the wrapper may exec only an allowed binary
  harness: { paths: [{ path: ~/.codex, access: read-write }] }
  network: { allow: [chatgpt.com, auth.openai.com, "127.0.0.1:18150"] }
trace: { capture: content }
lifecycle: { conversation: threaded }
inference:
  transport: process
  driver: { artifact: local/driver-codex-cli }
  command: <scratch>/bin/codex-mock
```

It ran on `scoped` (`containment_achieved: scoped`, harness tier `landlock+seccomp`): the harness
reached the loopback mock through its `127.0.0.1:18150` allow entry. Both runs exited `0`.

| Check | Seen |
|---|---|
| `harness_start` | `harness: "codex"`, `harness_version: "codex-cli 0.154.0"`, `version_tested: true`, `env_names` with `MURMUR_BRIDGE_TOKEN`, `files: ["instructions.md"]`; `session_mode: "new"`, then `"resume"` |
| The argv | run 1 ends `… default_tools_approval_mode="approve" -`; run 2 ends `… resume 01a1cea2-6d93-79f2-89e4-52a8ffcdfb74 -` |
| The thread | run 1's `harness_session` reports `01a1cea2-6d93-79f2-89e4-52a8ffcdfb74`; `~/.murmur/conversations/codex-guest-run/ctx-codex-guest/harness-session.json` stores it; run 2's `harness_start` resumes it |
| `tool_call` | `tool_name: "tool-read"`, `input: {"path": "notes.txt"}`, `status: "ok"`; `mur trace show`: `turn 0  tool-read 180ms ✓  {"path":"notes.txt"}` |
| Usage | run 1's `inference`: `input_tokens: 300`, `output_tokens: 45`, `cached_tokens: 60`, `cache_write_tokens: 0`, `thinking_tokens: 15`. Run 2: none, and the `harness_note` `codex reports a resumed thread's running total, … {"cache_write_input_tokens":0,"cached_input_tokens":80,"input_tokens":480,…}` |
| Auth | `harness_session` `auth: "unknown"` and `harness_warning` `W-SEC-031` on both runs |
| Refusals | no `E-RUN-032`, `E-CAP-019` or `E-CAP-021` in the output or the traces |
| The bridge's `DELETE` | Codex logs `DELETE returned HTTP 400`; the turn is unaffected |
| Exit | `harness_exit` `signal: 9, cause: "terminal"`: Codex outlived murmur's 3 s exit grace after `turn.completed` and was ended; `task_end` `exit_status: "ok"` |

`mur doctor`'s login check, with the operator's `HOME` running only `codex login status`:

| `HOME` | Reading |
|---|---|
| scratch | ``✗ error[E-RUN-064]: codex is not logged in for this user: codex is not signed in: run `codex login` as the user mur runs as`` |
| the operator's (`cd <scratch>/capsule && PATH=<scratch>/bin:$PATH mur doctor`) | `✓ login    logged in` |

`shell.allow: [codex]` also put a shell tool named `codex` in `tools_declared`. A capsule that names
`codex` itself as `command:` needs no such entry.

### The operator's capsule shape, unauthenticated

```
./guest-unauthenticated.sh /space/scratch/a6975964/guest-unauth2
SHELL_ALLOW=true ./guest-unauthenticated.sh /space/scratch/a6975964/guest-unauth-true
```

No wrapper: `inference.command` is the copied `codex`, `network.allow` is
`[chatgpt.com, auth.openai.com, api.openai.com]`, and the task goes to OpenAI's real endpoint with
no credential, so nothing is spent.

| `capabilities.shell.allow` | Result |
|---|---|
| absent | `invalid peer certificate: UnknownIssuer` on every WebSocket retry, then `error[E-RUN-033]: the harness turn failed (harness-error): Connection failed: error sending request`. Before `unbounded_connection_retries` was disabled, this capsule never ended |
| `[true]` | `error[E-RUN-033]: the harness turn failed (auth): unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, …`; `tools_declared: ["session-status", "true"]` |

Under mur 0.11.0 the harness gets the host's TLS trust store only with a non-empty
`capabilities.shell.allow`. Hence `shell: allow: [true]` in the README's manifest.

## Handoff: a live run on a ChatGPT login

For Emanuel. This spends a few turns of the ChatGPT plan `codex` is logged into. Run it as the user
whose `codex login status` says `Logged in using ChatGPT`, with the normal `HOME`.

### The capsule

```
mkdir -p ~/codex-live && cd ~/codex-live
printf 'LIVE-NOTES: the capsule read this file through tool-read.\n' > notes.txt
cat > task1.md <<'EOF'
Call the tool tool-read exactly once with path notes.txt. Then answer with the first line it returned, verbatim.
EOF
cat > task2.md <<'EOF'
Which file did you read in the previous task, and what did its first line say? Answer in one line.
EOF
cat > murmur.yaml <<'EOF'
name: codex-live
version: 0.1.0
artifacts:
  - { name: local/driver-codex-cli, version: "0.1.0", runtime: driver }
  - { name: local/tool-read, version: "0.5.0", runtime: tool }
capabilities:
  containment: scoped
  env: { allow: [HOME, PATH] }
  harness: { paths: [{ path: ~/.codex, access: read-write }] }
  network: { allow: [chatgpt.com, auth.openai.com] }
  shell: { allow: [true] }
trace: { capture: content }
lifecycle: { conversation: threaded }
inference:
  transport: process
  model: gpt-5.5
  driver: { artifact: local/driver-codex-cli }
EOF
cd <default-artifacts checkout>
./scripts/local-install.sh tool-read ~/codex-live
./scripts/local-install.sh driver-codex-cli ~/codex-live
```

For the code mode model, copy it: `cp -r ~/codex-live ~/codex-live-terra` and set
`model: gpt-5.6-terra` and `name: codex-live-terra` in its `murmur.yaml`.

### The runs

```
cd ~/codex-live
mur doctor
mur run --task task1.md --context ctx-live --workdir ~/codex-live
mur run --task task2.md --context ctx-live --workdir ~/codex-live
mur trace show --workdir ~/codex-live

cd ~/codex-live-terra
mur doctor
mur run --task task1.md --context ctx-live-terra --workdir ~/codex-live-terra
mur run --task task2.md --context ctx-live-terra --workdir ~/codex-live-terra
mur trace show --workdir ~/codex-live-terra
```

`codex` resolves to `~/.local/bin/codex`, a link into `~/.codex/packages/standalone/`. If the launch
refuses that binary, copy it out (`cp "$(readlink -f "$(command -v codex)")" ~/bin/codex`) and add
`command: ~/bin/codex` under `inference:`, written as an absolute path.

### The trace lines to expect

In each session's `trace.jsonl` (under `~/.murmur/records/…/ses_*/`):

- `harness_start` with `harness: "codex"`, `binary` naming the resolved `codex`,
  `harness_version: "codex-cli 0.154.0"`, `version_tested: true`, `tier: "landlock+seccomp"`;
  `session_mode: "new"` for task 1 and `"resume"` for task 2.
- `harness_session` with `auth: "unknown"`, then `harness_warning` `W-SEC-031`, on every session.
- `~/.murmur/conversations/codex-live/ctx-live/harness-session.json` holding the `thread_id` of
  task 1's `harness_session`, and task 2's `harness_start` carrying that same `harness_session_id`.
- `tool_call` with `tool_name: "tool-read"`, `input: {"path": "notes.txt"}`, `status: "ok"`.
- task 1's last `inference` with `input_tokens`, `output_tokens`, `cached_tokens`; task 2's with
  none, and a `harness_note` `codex reports a resumed thread's running total, …`.
- `mur doctor`: `Process harness (local/driver-codex-cli@0.1.0, harness codex)` with
  `✓ login    logged in`.
- No quota line: Codex prints no rate-limit event, so the driver sends none.
- `harness_exit` with `signal: 9, cause: "terminal"` after a finished turn is normal.

### A host the harness needed but the capsule did not allow

Under `scoped` a name outside `network.allow` gets a refused DNS answer, with no trace record of its
own. It shows up as Codex's retries: `harness_retry` records whose `reason` names the host, or
`harness_note` records `codex error: …` / `codex error item: Falling back from WebSockets …`, and
finally `E-RUN-033 … (harness-error)` with the error text. A missing host fails within about
a minute. `mur run --explain-scope` lists the hosts the capsule allows.

### What to report back

- **The hosts.** Whether `chatgpt.com` and `auth.openai.com` were enough, and every other host that
  appeared in a failure. These become `describe().harness-needs.hosts`.
- **The usage-limit text**, verbatim, if a turn hits the plan's limit, with the `turn-failed` kind
  murmur recorded (it should be `quota`).
- **Any quota line**: none is expected; any `rate_limits` text in a note would be new.
- **What `gpt-5.6-terra` did.** Whether `tool_call` records for `tool-read` appeared, which
  `codex reported its own '<type>' item` notes appeared (`collab_tool_call` would be the
  sub-agent tools), whether a `Code Mode is unavailable` note appeared, and what the answer to
  task 1 was.
