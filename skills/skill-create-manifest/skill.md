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

Documentation artifact — no binary or WASM component. Contains a `skill.md` file that the
runtime installs to `tools/<name>/skill.md` in the capsule workdir before the agent loop starts.
Skills are listed in MURMUR.md `## Installed Skills` with their install path. The runtime does
**not** inject skill content into the system prompt; the agent reads the file voluntarily via the
editor tool or filesystem access.

---

## 3. Artifacts Block

Declares the dependencies a capsule needs. Each entry names an artifact by its full name, gives
its version on a separate `version:` key, and states its runtime type. `mur install` fetches every
entry into the project store.

```yaml
artifacts:
  - name: murmur/tool-editor
    version: "0.6.0"
    runtime: tool

  - name: murmur/driver-anthropic
    version: "0.10.0"
    runtime: driver

  - name: murmur/hook-debug
    version: "0.7.0"
    runtime: hook

  - name: murmur/skill-create-manifest
    version: "0.1.0"
    runtime: skill
```

**Names are `<scope>/<name>`.** The scope says where the artifact comes from:

| Scope | Artifacts | Example |
|---|---|---|
| `murmur` | Murmur's default artifacts (§10), published by `murmur-nexus/default-artifacts` | `murmur/tool-editor` |
| `local` | An artifact installed from a `.mur.zip` file (`mur install ./x.mur.zip`) or by `mur publish` | `local/my-tool` |

A bare name such as `tool-editor` is refused with `E-MAN-003`, with a hint naming the scoped
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
- `single` for one-shot CLI-driven tasks (`mur run --input task.md`)
- `queue` for long-running agents that receive tasks from an orchestrator or API

---

## 6. Context Block

Controls context budget and output truncation.

```yaml
context:
  max_tokens: 100000     # token budget; triggers compaction when threshold is reached
```

| Field | Type | Default | Description |
|---|---|---|---|
| `max_tokens` | integer > 0 | — (no limit) | Token budget for the session. When usage approaches the compaction threshold (default 98% of this value), the runtime fires the compaction hook. |

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
    version: "0.10.0"
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
| `system_prompt` | string | no | Inline system prompt. Mutually exclusive with `system_prompt_file`. |
| `system_prompt_file` | string | no | Path to a `.md` file loaded as the system prompt. Mutually exclusive with `system_prompt`. |
| `compaction.threshold` | float (0,1] | no | Fraction of `context.max_tokens` at which compaction fires. Default: 0.98. |
| `compaction.model` | string | no | Model for compaction inference. Defaults to the primary model. |

The compaction hook is not named in this block: declare an `artifacts:` entry with
`runtime: hook` whose artifact binds `on-compaction`, such as `murmur/hook-compact`.

On the driver's `artifacts:` entry:

| Field | Type | Required | Description |
|---|---|---|---|
| `gateway.endpoint` | string | yes | Base URL for the inference API. The driver appends its own request path (e.g. `driver-anthropic` appends `/v1/messages`; `driver-openai` appends `/chat/completions`) — include any version segment the provider requires (e.g. `/v1`) in `endpoint` itself. |
| `gateway.api_key` | string | yes | The key, as a `${NAME}` credential reference. Never inline a literal key. |

**`${NAME}` references:** only `${UPPER_SNAKE_CASE}` is a reference; anything else is a literal
value. `mur run` resolves it before the session starts, and fails naming the variable when it
cannot.

**Example with driver config:**

```yaml
artifacts:
  - name: murmur/driver-openai
    version: "0.9.0"
    runtime: driver
    gateway:
      endpoint: https://api.openai.com/v1
      api_key: ${OPENAI_API_KEY}

inference:
  model: gpt-4o
  driver:
    artifact: murmur/driver-openai
    config:
      temperature: 0.2
```

`driver-openai` implements the OpenAI Chat Completions protocol, so it also works with
any OpenAI-compatible provider (e.g. GLM/Zhipu) — just point `gateway.endpoint` at that
provider's own base URL, including whatever version segment it requires. For example, GLM's base
URL is `https://open.bigmodel.cn/api/paas/v4` (no `/v1`).

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

A capsule that edits files using `tool-editor` and exits when done.

```yaml
name: notes-editor
version: "0.1.0"
runtime: tool
description: "Simple file editing capsule — reads and writes files, exits on completion"

artifacts:
  - name: murmur/tool-editor
    version: "0.6.0"
    runtime: tool
  - name: murmur/driver-anthropic
    version: "0.10.0"
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

A capsule with shell, git, and editor for coding tasks. Grants only the binaries actually needed.

```yaml
name: coding-agent
version: "0.1.0"
runtime: tool
description: "Coding agent with shell, git operations, and file editing"

artifacts:
  - name: murmur/tool-git
    version: "0.2.0"
    runtime: tool
  - name: murmur/tool-editor
    version: "0.6.0"
    runtime: tool
  - name: murmur/driver-anthropic
    version: "0.10.0"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

capabilities:
  shell:
    allow:
      - bash
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
`spawn.allow` names every sub-capsule it may spawn.

```yaml
name: orchestrator
version: "0.1.0"
runtime: tool
description: "Orchestrator that spawns sub-capsules for parallel task execution"

artifacts:
  - name: murmur/tool-editor
    version: "0.6.0"
    runtime: tool
  - name: murmur/driver-anthropic
    version: "0.10.0"
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

**Do not grant `bash` when only `git` or `editor` is needed.**
`tool-git` covers all git operations as typed tool calls without shell access.
`tool-editor` covers file read/write/patch without shell access.
Add `capabilities.shell.allow: [bash]` only when the task genuinely requires arbitrary shell
commands that no structured tool can handle.

**Do not declare network access for local-only workloads.**
Omit `capabilities.network` entirely for capsules that operate only on local files.
For capsules that call external APIs, list only the specific hostnames needed — not `*` or broad ranges.

**Do not over-declare spawn allowlists.**
`capabilities.spawn.allow` must list only the full names (`<scope>/<name>`) of the capsules this
capsule needs to spawn. An empty list or absent block means no spawning is allowed.

**Match `task_acceptance` to the actual usage pattern.**
- `single` (default): one-shot task; capsule exits when done. Correct for `mur run --input task.md`.
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
| `murmur/driver-openai` | driver | OpenAI Chat Completions inference driver (also works with OpenAI-compatible providers). Use with `gateway.endpoint: https://api.openai.com/v1` |
| `murmur/tool-git` | tool | Structured git interface: status, add, commit, push, pull, branch, clone, worktree, and more. Prefer over granting `git` in `shell.allow`. |
| `murmur/tool-editor` | tool | File read/write/patch operations: read_file, write_file, replace_in_file, find_in_files. Prefer over shell-based file editing. |
| `murmur/tool-request-input` | tool | HITL pause gate. Emits an `input-required` signal; the capsule waits for a human reply before continuing. Use with `lifecycle.input_timeout_secs` to avoid indefinite waits. |
| `murmur/hook-compact` | hook | Context compaction hook. Fires when token usage crosses the compaction threshold; replaces conversation history with a summary. Declared as a `runtime: hook` entry; it binds `on-compaction`. |
| `murmur/hook-debug` | hook | Debug event logger. Writes every lifecycle event (session start, inference, tool call, shell, session end) to `hook-debug.jsonl` in the capsule workdir. |
| `murmur/hook-grafana` | hook | OTel span emitter. Sends structured telemetry to Grafana Tempo. Requires `observability.otel_endpoint` in the capsule manifest. |
| `murmur/hook-eval` | hook | Evaluation scorer. Runs configured scorers (exit_ok, max_turns, tool_sequence, llm_judge) and writes results to `eval.jsonl`. |
| `murmur/skill-create-manifest` | skill | This artifact. Provides complete manifest generation guidance. |

**Using artifacts in a capsule manifest:**

```yaml
artifacts:
  - name: murmur/tool-git
    version: "0.2.0"
    runtime: tool
  - name: murmur/hook-debug
    version: "0.7.0"
    runtime: hook
```

Always specify an exact `version`. Run `mur list` to see which versions are installed locally, or
`mur install -g murmur/<name>@<version>` to fetch a specific version.
