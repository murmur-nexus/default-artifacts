# Murmur Manifest (`murmur.yaml`) — Complete Schema Reference

This skill teaches you to generate a valid `murmur.yaml` for any murmur capsule or artifact
from a plain-language task description.

`murmur.yaml` has two contexts:

- **Artifact manifest** — used by `mur build` to package a `.mur.zip` artifact (tool, driver, hook,
  or skill). Requires `name`, `version`, and `runtime` at the top level.
- **Capsule manifest** — used by `mur run` to launch an agent session. Requires only `name` and
  `version`; the `runtime` field is optional at the capsule level but include `runtime: tool` so
  that `mur build` can also validate the manifest.

**Always include `runtime: tool` at the top level of a capsule manifest.** This lets the manifest
pass `mur build` for syntax validation while remaining fully functional with `mur run`.

---

## 1. Top-Level Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | Unique artifact/capsule identifier, kebab-case |
| `version` | string | yes | — | Semver string, quoted: `"0.1.0"` |
| `runtime` | string | yes* | — | Artifact runtime type (see §2). Use `tool` for capsules. |
| `description` | string | no | — | Human-readable description of purpose |

*Required by `mur build`. `mur run` does not require it at the capsule level.

A top-level `mur_version` key is refused with `E-MAN-003`. To pin the `mur` release a deployment
installs, write it as `deploy.mur_version` (§7b).

---

## 2. Runtime Values

The `runtime:` field declares how the artifact behaves at runtime.

### `tool`

Agent-callable tool. Appears in the LLM tool inventory and in MURMUR.md `## Installed Tools`.
The implementation details (WASM component vs. native binary) are declared inside the artifact's
own `murmur.yaml` via `implementation: native` or `implementation: wasm` (default: wasm).

In the **capsule `artifacts:` block**, always use `runtime: tool` for tools — never
`runtime: native` or `runtime: wasm`. Those values are errors:

```
murmur.yaml: use 'runtime: tool'; implementation is declared in the artifact's own manifest
```

### `driver`

Inference driver, implemented as a WASM component. The runtime calls it to process LLM requests.
Hidden from the agent tool list. Referenced in the `inference.driver.artifact` field.

### `hook`

Event-triggered observer, implemented as a WASM component. The runtime calls it at lifecycle
points (session start/end, after each inference turn, after each tool call, etc.). Hidden from
the agent. Declared in the capsule `artifacts:` block with `runtime: hook`.

### `skill`

Documentation artifact — no binary or WASM component. Its `skill.md` is installed to
`tools/<name>/skill.md` in the session's runtime directory before the agent loop starts. That
directory is read-only: the agent can read the file there and cannot change it. MURMUR.md is in
the same directory. MURMUR.md's `- Runtime directory:` entry gives the directory's path and the
name each of the session's tools opens it by, and its `## Installed Skills` section lists each
skill.

Every skill is also offered to the model as a tool with no input parameters: calling it by name
returns its `skill.md`. The one exception is the skill named in `inference.system_prompt_artifact`
(§7): its content is read once at launch, bound as the system prompt, and not offered as a tool.
No other skill content enters the system prompt.

---

## 3. Artifacts Block

Declares the dependencies a capsule needs. Each entry names an artifact by its full name, gives
its version on a separate `version:` key, and states its runtime type. `mur install` fetches every
entry into the project store.

```yaml
artifacts:
  - name: murmur/tool-read
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-write
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-edit
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-find
    version: "0.1.0"
    runtime: tool

  - name: murmur/driver-anthropic
    version: "0.17.0"
    runtime: driver

  - name: murmur/hook-debug
    version: "0.10.0"
    runtime: hook
    capabilities:
      filesystem:
        records: true          # hook-debug.jsonl goes in the records directory (§4.3a)

  - name: murmur/skill-create-manifest
    version: "0.10.0"
    runtime: skill
```

**Names are `<scope>/<name>`.** The scope says where the artifact comes from:

| Scope | Artifacts | Example |
|---|---|---|
| `murmur` | Murmur's default artifacts (§10), published by `murmur-nexus/default-artifacts` | `murmur/tool-read` |
| `local` | An artifact installed from a `.mur.zip` file (`mur install ./x.mur.zip`) or by `mur publish` | `local/my-tool` |

A bare name such as `tool-read` is refused with `E-MAN-003`, with a hint naming the scoped
forms. The bare part is the tool name the model calls, so two entries may not share one. An entry
that declares `source:` is the one exception: it takes the bare name.

**Version pinning:** Always specify an exact version string on `version:`, never inside the name.
Semver ranges are not supported. The runtime resolves the exact version or fails with a clear
error.

**Valid `runtime` values in the artifacts block:** `tool`, `driver`, `hook`, `skill`.
`native` and `wasm` are rejected — use `tool`.

---

## 4. Capabilities Block

Capabilities control what the capsule agent is allowed to do. Omit a capability entirely to deny
it — the runtime does not grant it if it is not declared.

### 4.1 Shell Capabilities

Grants the agent access to named binaries via the `murmur:shell/execute` interface.
The `allow` list must be non-empty if the `shell:` block is present.

```yaml
capabilities:
  shell:
    allow:
      - bash
      - git
      - make
```

Optional sub-fields:

```yaml
capabilities:
  shell:
    allow:
      - bash
    strip_env:
      - "*_TOKEN"        # glob pattern — strip matching env vars from subprocess env
      - SECRET_KEY
    baseline_env:
      - SSH_AUTH_SOCK    # names of env vars to pass through to subprocesses
      - GOPATH
```

### 4.2 Network Capabilities

Grants the agent (and tool artifacts) outbound network access to listed hosts/URLs.
Exact string matching — prefix the entry with `https://` or `http://` as appropriate.

```yaml
capabilities:
  network:
    allow:
      - https://github.com
      - https://pypi.org
      - http://127.0.0.1:8080
```

**Do not list the inference endpoint in `network.allow`.** The provider's URL goes on the driver's
`gateway.endpoint` (§7), which is the grant for the driver's requests; an allow-list entry naming
it warns `W-SEC-025`.

### 4.3 Filesystem Capabilities

Declares the path scope accessible to the capsule's filesystem operations.
`scope` is resolved relative to the capsule workdir at runtime.

```yaml
capabilities:
  filesystem:
    scope: ./sandbox     # restrict to workdir/sandbox/
```

Omit `filesystem:` to use the default workdir scope. Use `.` to allow the full workdir.
This block does not reach hooks — see §4.3a.

### 4.3a Hook Capabilities

A hook gets no filesystem and no network by default, and the capsule-wide `capabilities:` block
does not reach it. Grant each hook what it needs under `capabilities:` on **that hook's own
`artifacts:` entry**. A `capabilities:` key inside a hook artifact's bundled `murmur.yaml` is
ignored.

| Grant | For | Default artifacts |
|---|---|---|
| `filesystem.records: true` | A hook that writes a score or log. Its current directory becomes the session records directory's `hooks/`, read-write. | `murmur/hook-eval`, `murmur/hook-debug`, `murmur/hook-regression-verifier` |
| `filesystem.scope` | A hook that reads project files. The named workdir directory becomes its current directory. | `murmur/hook-diff-summary` |
| `conversation.read: true` | A hook that reads the capsule's conversation record. | `murmur/hook-memory` |
| `task_io.read: true` | A hook that reads the task's input text and the agent's result text. It grants a host import, `murmur:task-io/read`, not a directory. An ungranted hook still loads and runs, and every read returns `not-granted`. Recognized only on a `runtime: hook` entry. | `murmur/hook-memory`, optionally, for relevance selection instead of recency |
| `network.allow` | A hook that exports to an endpoint. List the same host as `observability.otel_endpoint`. | `murmur/hook-grafana`, `murmur/hook-eval`'s OTel export |

```yaml
artifacts:
  - name: murmur/hook-eval
    version: "0.11.0"
    runtime: hook
    capabilities:
      filesystem:
        records: true          # eval.jsonl goes beside the session's trace
      network:
        allow:
          - http://localhost:4318   # only for OTel export

  - name: murmur/hook-debug
    version: "0.10.0"
    runtime: hook
    capabilities:
      filesystem:
        records: true
```

`murmur/hook-diff-summary` snapshots every file `tool-write` and `tool-edit` touch, so its entry is granted
`filesystem.scope: "."`. Do not give a scoring or logging hook `scope` instead of `records`: it
would write into the work folder, where the capsule's commands can edit the file and `mur eval`
does not look.

`filesystem.records: true` fails with `E-MAN-003` when it is declared beside `filesystem.scope`
on the same entry, on an entry that is not `runtime: hook`, or in the capsule-wide
`capabilities:` block.

**The records directory.** Every record the runtime keeps about a session is in
`~/.murmur/records/<workdir root>/<session-id>/`, where `<workdir root>` is the `--workdir`
directory, otherwise `<manifest-dir>/workdir`. A shell command sees it as `$MURMUR_RECORDS_DIR`.
This is the layout of `mur` 0.7.0 or later. The directory is read-only to the capsule's own
commands and kept after the session ends.

| Path | Holds |
|---|---|
| `trace.jsonl` | The session's trace |
| `blobs/` | Large trace payloads, by content hash |
| `logs/` | `bootstrap.log`, `otel.log`, and `hook-<name>.log` for errors the runtime catches from a hook |
| `completion.json` | A delegated child's outcome |
| `hooks/` | The current directory of every hook granted `filesystem.records: true` |

`mur eval` reads `hooks/eval.jsonl` from each session's records directory.

### 4.4 Mur Capabilities (planned — post-MVP)

Grants the agent access to the `mur` CLI for registry and capsule management operations.
Not yet parsed from the manifest; will be declared as:

```yaml
capabilities:
  mur:
    - list
    - pull
    - run
```

### 4.5 Spawn Capabilities (requires mur-roost)

Grants the capsule permission to spawn named sub-capsules via the mur-roost daemon.
`allow` lists the full names, `<scope>/<name>`, of the capsules it may spawn, and it is the only
key in this block. A bare name is refused with `E-MAN-003`.

```yaml
capabilities:
  spawn:
    allow:
      - local/worker-capsule-a
      - local/worker-capsule-b
```

**Note:** `mur run` parses this block and reports it under `spawn allow` in
`mur run --explain-scope`, but the spawning itself goes through the mur-roost daemon: roost reads
the parent capsule's own `capabilities.spawn.allow` as the per-job allowlist, so a capsule can
only spawn names its own manifest lists. Whether each spawned worker gets an isolated job root is
a roost daemon setting (`--scoped`), not a manifest key.

---

## 5. Lifecycle Block

Controls how the capsule accepts and processes tasks.

| Field | Type | Default | Description |
|---|---|---|---|
| `task_acceptance` | `none` \| `single` \| `queue` | `single` | `none`: reject all tasks. `single`: accept one task, block until done. `queue`: accept tasks into a queue; process one at a time. |
| `after_task` | `exit` \| `sleep` | `exit` | `exit`: terminate after task completes. `sleep`: stay alive waiting for the next task. |
| `queue_depth` | integer | `1` | Max queued tasks when `task_acceptance: queue`. |
| `input_timeout_secs` | integer | — (wait forever) | Seconds to wait for HITL input before failing the task with `input-timeout`. |

```yaml
lifecycle:
  task_acceptance: single
  after_task: exit

# Persistent agent that queues work:
lifecycle:
  task_acceptance: queue
  after_task: sleep
  queue_depth: 5
  input_timeout_secs: 300
```

**Match `task_acceptance` to the actual usage pattern:**
- `single` for one-shot CLI-driven tasks (`mur run --task task.md`)
- `queue` for long-running agents that receive tasks from an orchestrator or API

---

## 6. Context Block

Controls context budget and output truncation.

```yaml
context:
  max_tokens: 100000     # context window; compaction fires at the threshold fraction of the input budget cut from it
```

| Field | Type | Default | Description |
|---|---|---|---|
| `max_tokens` | integer > 0 | the `context_window` the driver's `models:` table declares for the model; with neither, there is no window, compaction does not run and no seed budget applies | The context window. Compaction fires when usage reaches `inference.compaction.threshold` × the input budget, `min(max_input_tokens, window − output cap)`, where `max_input_tokens` is the driver's input ceiling for the model, when it declares one. |

**Runtime-enforced truncation limits** (set by the runtime, not yet manifest fields):
- `context.max_tool_output`: max tokens for a tool-result `data` field before truncation
  (runtime default: 8192 tokens). Truncated output is written to the session log.
- `context.max_shell_output`: max tokens for shell stdout/stderr before truncation
  (runtime default: 4096 tokens).

---

## 7. Inference Block

Configures the LLM inference connection. Required for any capsule that runs an agent loop.

The provider's URL and key are not written here. They go on the driver's own `artifacts:` entry as
a `gateway:` block: the runtime holds the key and attaches it to every request the driver makes,
so the driver never sees it.

```yaml
artifacts:
  - name: murmur/driver-anthropic
    version: "0.17.0"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

inference:
  model: claude-opus-4-8
  driver:
    artifact: murmur/driver-anthropic
```

| Field | Type | Required | Description |
|---|---|---|---|
| `model` | string | yes | Model identifier string passed to the driver |
| `driver.artifact` | string | yes | Full name, `<scope>/<name>`, of an `artifacts:` entry with `runtime: driver`, e.g. `murmur/driver-anthropic`. That entry must carry `gateway:` |
| `driver.config` | mapping | no | Driver-specific configuration passed as JSON (e.g. `temperature`, custom headers) |
| `system_prompt` | string | no | Inline system prompt. |
| `system_prompt_file` | string | no | Path to a `.md` file loaded as the system prompt. |
| `system_prompt_artifact` | string | no | The `name`, as written, of a declared `artifacts:` entry whose content is read once at launch and bound as the system prompt. That entry is a skill by default; any other entry must declare `prompt_payload: true`, or the launch fails with `E-MAN-003`. The bound skill is not offered as a tool. |
| `compaction.threshold` | float (0,1] | no | Fraction of the input budget at which compaction fires. Default: 0.98. |
| `compaction.model` | string | no | Model for compaction inference. Defaults to the primary model. |

At most one of `system_prompt`, `system_prompt_file` and `system_prompt_artifact` may be set; a
second one fails with `E-MAN-003`.

The compaction hook is not named in this block: declare an `artifacts:` entry with
`runtime: hook` whose artifact binds `on-compaction`, such as `murmur/hook-compact`.

On the driver's `artifacts:` entry:

| Field | Type | Required | Description |
|---|---|---|---|
| `gateway.endpoint` | string | yes | Base URL for the inference API. The driver appends its own request path (e.g. `driver-anthropic` appends `/v1/messages`; `driver-openai` appends `/responses` or `/chat/completions`, by model) — include any version segment the provider requires (e.g. `/v1`) in `endpoint` itself. |
| `gateway.api_key` | string | yes, unless `keyless: true` | The key, as a `${NAME}` credential reference. Never inline a literal key. |
| `gateway.keyless` | boolean | no | `true` declares an upstream that takes no key, such as a local server: requests go out with no credential header. Writing it with `api_key` fails with `E-MAN-003`. A gateway with neither a non-blank `api_key` nor `keyless: true` fails with `E-CAP-018`. |

**`${NAME}` references:** only `${UPPER_SNAKE_CASE}` is a reference; anything else is a literal
value. `mur run` resolves it before the session starts, and fails naming the variable when it
cannot.

**Example with driver config:**

```yaml
artifacts:
  - name: murmur/driver-openai
    version: "0.17.0"
    runtime: driver
    gateway:
      endpoint: https://api.openai.com/v1
      api_key: ${OPENAI_API_KEY}

inference:
  model: gpt-5.6-luna
  driver:
    artifact: murmur/driver-openai
    config:
      reasoning_effort: high
```

`driver-openai` accepts only the OpenAI models its README lists, and refuses any other id,
including an OpenAI-compatible provider's. For Z.ai's GLM, use `murmur/driver-zai` with
`gateway.endpoint: https://api.z.ai/api/paas/v4` and `inference.model: glm-5.3`; see its row in
§10. For GLM 5.3 on Baseten's Model APIs, use `murmur/driver-baseten` with
`gateway.endpoint: https://inference.baseten.co/v1` and `inference.model: zai-org/GLM-5.3`; see its
row in §10.

---

## 7a. Observability Block

Optional. Enables OTel telemetry emission and eval scoring.

```yaml
observability:
  otel_endpoint: https://tempo.example.com:4318   # OTLP/HTTP endpoint for OTel spans
  eval:
    scorers:
      - type: exit_ok
        name: completed
      - type: max_turns
        name: concise
        max: 20
```

| Field | Type | Required | Description |
|---|---|---|---|
| `otel_endpoint` | string | no | OTLP/HTTP endpoint for span emission. Required when using `hook-grafana`. |
| `eval.scorers` | list | no | Evaluation scorers run by `hook-eval` at session end. |

`murmur/hook-eval`'s `artifacts:` entry needs `capabilities.filesystem.records: true` (§4.3a).

**Eval scorer types:**

| `type` | Fields | Description |
|---|---|---|
| `exit_ok` | `name` | Passes if the task completed without error |
| `max_turns` | `name`, `max` (integer) | Passes if total inference turns ≤ `max` |
| `max_tokens` | `name`, `max` (integer) | Passes if total tokens used ≤ `max` |

Omit `observability:` entirely for capsules that do not emit telemetry or run evaluations.

---

## 7b. Deploy Block

Optional. Settings read by `mur deploy run` alone; `mur run` and `mur doctor` read nothing in it.

```yaml
deploy:
  mur_version: "1.0.0"
```

| Field | Type | Required | Description |
|---|---|---|---|
| `deploy.mur_version` | string | no | The `mur` release `mur deploy run` installs on the target. Default: the version of the `mur` running `mur deploy run`. |

This is the only place a manifest pins a `mur` version. A top-level `mur_version` key is refused
with `E-MAN-003`, whatever its value.

---

## 8. Worked Examples

### Example 1: Minimal Single-Tool Capsule

A capsule that edits files using the four file tools and exits when done.

```yaml
name: notes-editor
version: "0.1.0"
runtime: tool
description: "Simple file editing capsule — reads and writes files, exits on completion"

artifacts:
  - name: murmur/tool-read
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-write
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-edit
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-find
    version: "0.1.0"
    runtime: tool
  - name: murmur/driver-anthropic
    version: "0.17.0"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

inference:
  model: claude-sonnet-4-6
  driver:
    artifact: murmur/driver-anthropic

lifecycle:
  task_acceptance: single
  after_task: exit

deploy:
  mur_version: "1.0.0"
```

### Example 2: Shell-Capable Coding Capsule

A capsule with shell, git, and the file tools for coding tasks. Grants only the binaries actually needed.

```yaml
name: coding-agent
version: "0.1.0"
runtime: tool
description: "Coding agent with shell, git operations, and file editing"

artifacts:
  - name: murmur/tool-git
    version: "0.6.0"
    runtime: tool
  - name: murmur/tool-read
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-write
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-edit
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-find
    version: "0.1.0"
    runtime: tool
  - name: murmur/driver-anthropic
    version: "0.17.0"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

capabilities:
  shell:
    allow:
      - bash
      - git                    # tool-git runs git itself
  network:
    allow:
      - https://github.com

inference:
  model: claude-opus-4-8
  driver:
    artifact: murmur/driver-anthropic

lifecycle:
  task_acceptance: single
  after_task: exit

context:
  max_tokens: 200000
```

### Example 3: Orchestrator Capsule

A capsule that decomposes tasks into parallel sub-capsules using mur-roost.
`spawn.allow` names every sub-capsule it may spawn. It only reads the work folder to plan, so it
takes `tool-read` and `tool-find`; the sub-capsules do the editing.

```yaml
name: orchestrator
version: "0.1.0"
runtime: tool
description: "Orchestrator that spawns sub-capsules for parallel task execution"

artifacts:
  - name: murmur/tool-read
    version: "0.1.0"
    runtime: tool
  - name: murmur/tool-find
    version: "0.1.0"
    runtime: tool
  - name: murmur/driver-anthropic
    version: "0.17.0"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

capabilities:
  spawn:
    allow:
      - local/worker-capsule

inference:
  model: claude-opus-4-8
  driver:
    artifact: murmur/driver-anthropic

lifecycle:
  task_acceptance: queue
  after_task: sleep
  queue_depth: 10
```

---

## 9. Least-Privilege Guidance

**Do not grant `bash` when only `git` or file access is needed.**
`tool-git` covers git operations as typed tool calls; it needs `git` in `shell.allow`, not `bash`.
`tool-read`, `tool-write`, `tool-edit` and `tool-find` cover reading, writing, editing and
searching files without shell access.
Add `capabilities.shell.allow: [bash]` only when the task genuinely requires arbitrary shell
commands that no structured tool can handle.

**Do not declare network access for local-only workloads.**
Omit `capabilities.network` entirely for capsules that operate only on local files.
For capsules that call external APIs, list only the specific hostnames needed — not `*` or broad ranges.

**Do not over-declare spawn allowlists.**
`capabilities.spawn.allow` must list only the full names (`<scope>/<name>`) of the capsules this
capsule needs to spawn. An empty list or absent block means no spawning is allowed.

**Match `task_acceptance` to the actual usage pattern.**
- `single` (default): one-shot task; capsule exits when done. Correct for `mur run --task task.md`.
- `queue`: persistent agent that accepts tasks from an orchestrator. Requires `after_task: sleep`.
  Do not use `queue` for one-shot tasks — the capsule will not exit and will block the caller.

**Use `${ENV_VAR}` references for all secrets.**
Never write an API key literal in a manifest. Use `gateway.api_key: ${ANTHROPIC_API_KEY}` on the
driver's entry and inject the value via the environment. A manifest with a literal secret will trigger a build warning.

**Omit optional blocks entirely when not needed.**
`capabilities`, `context`, `lifecycle`, and `observability` are all optional. Omit any block
whose defaults are acceptable — fewer declared capabilities is always safer.

---

## 10. Default Artifact Catalog

Use this table when selecting artifacts by name. Each installs as `murmur/<name>`, which is the
name its `artifacts:` entry uses. For live discovery, use `mur search <keyword>`.

| Artifact | Runtime | Description |
|---|---|---|
| `murmur/driver-anthropic` | driver | Anthropic Messages API inference driver. Use with `gateway.endpoint: https://api.anthropic.com` |
| `murmur/driver-baseten` | driver | Baseten Model APIs inference driver: `zai-org/GLM-5.3` only, on the Chat Completions surface, with a `reasoning_effort` config key; its reasoning goes back to Baseten on every later turn, and a task's turns share one `x-session-affinity`. Use with `gateway.endpoint: https://inference.baseten.co/v1` |
| `murmur/driver-claude-code` | driver | Process driver for the `claude` CLI (Claude Code). The harness authenticates itself, so a turn spends a Claude subscription rather than an API key; the driver holds no credential and is granted nothing. Use with `inference.transport: process` and `capabilities.env.allow: [HOME, PATH]`; its entry takes no `gateway:`. |
| `murmur/driver-deepseek` | driver | DeepSeek inference driver: `deepseek-flash` and `deepseek-v4-pro`, with thinking mode; any other model is refused. Use with `gateway.endpoint` set to DeepSeek's API base URL; it appends `/chat/completions`. |
| `murmur/driver-moonshotai` | driver | Moonshot AI inference driver: `kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed` and `kimi-k3` with a `reasoning_effort` config key, and `kimi-k2.7-code`, `kimi-k2.7-code-highspeed` and `kimi-k2.6` (Moonshot retires K2 by the end of December 2026), on the Chat Completions surface; `kimi-k2.6` takes a `thinking` config key, and every model takes `response_format`. Use with `gateway.endpoint: https://api.moonshot.ai/v1` |
| `murmur/driver-openai` | driver | OpenAI inference driver: sends each OpenAI model its README lists to the Responses or Chat Completions endpoint that model's page lists, and refuses any other model, an OpenAI-compatible provider's included. Use with `gateway.endpoint: https://api.openai.com/v1` |
| `murmur/driver-zai` | driver | Z.ai inference driver: `glm-5.3` only, on the Chat Completions surface, with `reasoning_effort` and `thinking` config keys; its reasoning goes back to Z.ai on every later turn. Use with `gateway.endpoint: https://api.z.ai/api/paas/v4` |
| `murmur/hook-compact` | hook | Context compaction hook. Fires when token usage crosses the compaction threshold; replaces conversation history with a summary. Declared as a `runtime: hook` entry; it binds `on-compaction`. |
| `murmur/hook-debug` | hook | Debug event logger. Writes every lifecycle event (session start, inference, tool call, shell, session end) to `hooks/hook-debug.jsonl` in the session records directory. Needs `filesystem.records: true`. |
| `murmur/hook-diff-summary` | hook | Edit summary. Snapshots each file `murmur/tool-write` and `murmur/tool-edit` write and emits a unified-diff summary at end of turn. Needs `filesystem.scope: "."`. Pairs with `murmur/tool-write` and `murmur/tool-edit` 0.1.0 or later. |
| `murmur/hook-eval` | hook | Evaluation scorer. Runs configured scorers (exit_ok, max_turns, tool_sequence, llm_judge) and writes results to `hooks/eval.jsonl` in the session records directory, where `mur eval` reads them. Needs `filesystem.records: true`. |
| `murmur/hook-grafana` | hook | OTel span emitter. Sends structured telemetry to Grafana Tempo. Requires `observability.otel_endpoint` in the capsule manifest. |
| `murmur/hook-memory` | hook | Conversation memory. At `on-task-start`, seeds the task with a budget-bounded slice of the earlier conversation. Needs `conversation.read: true` and a context window: `context.max_tokens`, or the driver's `models:` entry for the model. `task_io.read: true` is optional and selects by relevance instead of recency. |
| `murmur/hook-regression-verifier` | hook | Regression gate. Reopens a task whose edits broke previously-passing tests, judged from the test runs it sees in `on-shell` (`pytest`, `cargo test`, `go test`, jest). Appends each verdict to `hooks/regression-verifier.jsonl` in the session records directory. Needs `filesystem.records: true`. The reopen budget is `lifecycle.max_task_reopens`. |
| `murmur/hook-shell-desc` | hook | Shell tool descriptions. At staging (`on-stage`), returns a tool manifest for each of `cargo`, `curl`, `find`, `git`, `grep`, `jq`, `node`, `python3` and `rustc`, stating what it takes and returns; shell interpreters keep the runtime's own description. Needs no `capabilities:` block. |
| `murmur/tool-code-coverage` | tool | Native binary. Spectrum-based fault localization (Ochiai, Tarantula) over a Rust repository already indexed by `murmur/tool-code-graph`, from per-test LCOV reports the agent produced; writes suspicion scores onto the code-graph database. Never runs tests or coverage itself. Needs no grant beyond the session workdir, which must hold `repo_path` and `coverage_dir`. |
| `murmur/tool-code-graph` | tool | Native binary. Indexes a Rust and/or Python repository into a SQLite symbol and call graph at `<repo_path>/.murmur/code-graph.db`, and answers `find_symbol`, `get_symbol`, `slice_symbol`, `explain_path` and `impact_analysis` by stable symbol identity. Needs no grant beyond the session workdir, which must hold `repo_path`. |
| `murmur/tool-corpus` | tool | Append-only record store at `state/corpus.jsonl`: `append`, `get`, `read_recent`, `search`, `verify`. Needs `capabilities.state: {}` on its entry, and a `config:` block (`config_version: 1`) declaring each record type and the JSON Schema its body must satisfy. |
| `murmur/tool-create` | tool | Scaffolds a new tool or hook artifact directory with `murmur.yaml`, a stub implementation and a README, under `tools/<name>/` in the capsule workdir. |
| `murmur/tool-edit` | tool | Replaces text in a file: `path` and `edits`, a list of exact `{old, new}` replacements applied in order, each matching once, all or nothing. Its `path` is a declared write destination, so `filesystem.read_only` refuses an edit under a read-only entry. Prefer over shell-based file editing. |
| `murmur/tool-find` | tool | Searches file contents for a regex: `pattern`, optional `dir`, `glob`, `recursive`, `context_lines`; stops at 200 matches with a `capped` note. Declares that it writes nothing. |
| `murmur/tool-git` | tool | Native binary. Structured git interface: status, add, diff, commit, branch, checkout, fetch, pull, push, clone, worktree, and more. Needs `git` in `capabilities.shell.allow`; `fetch`, `pull`, `push` and `clone` also need the remote's host in `capabilities.network.allow` and a `capabilities.shell.interpreter_runtime` grant for `git` naming its remote-helper directory. Prefer over granting `bash` for git work. |
| `murmur/tool-github` | tool | Pushes a local commit range to one pinned GitHub repo so the remote branch lands on the identical SHA (a `git clone --depth 1` works), fetches a branch back, and opens, inspects, lists and merges pull requests. Needs `gateway: {endpoint: https://api.github.com, api_key: ${GITHUB_TOKEN}}` on its entry and a `config:` block with `config_version: 1` and `repo: owner/name`; the runtime attaches the token, and without the gateway every call returns `gateway_missing`. Merging needs `allow_merge: true` and `merge_methods` in `config:`. |
| `murmur/tool-read` | tool | Reads a file: `path`, optional `start_line`/`end_line`; at most 2000 lines and 100 KB, with a `truncated` note naming the `start_line` that continues it. Declares that it writes nothing, so it reads under `filesystem.read_only`. |
| `murmur/tool-registry-search` | tool | Native binary. Searches a Murmur artifact index for artifacts matching a keyword; each result is a full `<scope>/<name>` with every published version, newest first. The default index, on the latest default-artifacts release, needs `https://github.com` and `https://release-assets.githubusercontent.com` in `capabilities.network.allow`. |
| `murmur/tool-report` | tool | States the session's own conclusion in `state/reports/<session_id>.json` (host: `~/.murmur/state/<store>/reports/<session_id>.json`, `<session_id>` = `MURMUR_SESSION_ID` verbatim; at most 64 files kept, never the caller's own or one written in the last 7 days): `progress` notes, then one terminal `report` whose outcome is `success`, `rejected`, `blocked` or `failed`. Needs `capabilities.state: {}` on its entry; an optional `config:` block closes the deliverable kinds and note stages. |
| `murmur/tool-request-input` | tool | HITL pause gate. Emits an `input-required` signal; the capsule waits for a human reply before continuing. Use with `lifecycle.input_timeout_secs` to avoid indefinite waits. |
| `murmur/tool-tavily` | tool | Web search through Tavily, returning bounded plain text: a header, an optional answer, then numbered results with title, URL and snippet. Needs `gateway: {endpoint: https://api.tavily.com, api_key: ${TAVILY_API_KEY}}` on its entry; the runtime attaches the key, and without the gateway every call returns `gateway_missing`. |
| `murmur/tool-test-report` | tool | Native binary. Parses a test-runner output file the agent already captured (`cargo_test`, `pytest`, `go_test`, `jest`, or `auto`) into a structured failure list; never runs tests. Needs no grant beyond the session workdir, which must hold `input_path`. |
| `murmur/tool-write` | tool | Creates or replaces a file and any missing directories: `path`, `content`. Its `path` is a declared write destination, so `filesystem.read_only` refuses a write under a read-only entry. |
| `murmur/skill-create-manifest` | skill | This artifact. Provides complete manifest generation guidance. |
| `murmur/skill-investigation-checkpoint` | skill | The investigations convention for `checkpoints/decisions.json`: record and reuse investigative verdicts (ruled-out, confirmed, uncertain) across a session instead of re-deriving them. |

A native tool runs with the capsule-wide grants; a `capabilities:` block on its entry does not
narrow them and warns `W-SEC-008`.

**Using artifacts in a capsule manifest:**

```yaml
artifacts:
  - name: murmur/tool-git
    version: "0.6.0"
    runtime: tool
  - name: murmur/hook-debug
    version: "0.10.0"
    runtime: hook
    capabilities:
      filesystem:
        records: true
```

Always specify an exact `version`. Run `mur list` to see which versions are installed locally, or
`mur install -g murmur/<name>@<version>` to fetch a specific version.
