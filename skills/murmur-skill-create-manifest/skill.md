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
| `mur_version` | string | no | running mur | Pins the required mur runtime version |

*Required by `mur build`. `mur run` does not require it at the capsule level.

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

Declares the dependencies a capsule needs. Each entry specifies an artifact by name, version,
and runtime type. The runtime resolves each from the local artifact index (`~/.murmur/artifacts/`).

```yaml
artifacts:
  - name: murmur-tool-editor
    version: "0.1.0"
    runtime: tool

  - name: murmur-driver-anthropic
    version: "0.1.0"
    runtime: driver

  - name: murmur-hook-debug
    version: "0.1.0"
    runtime: hook

  - name: murmur-skill-create-manifest
    version: "0.1.0"
    runtime: skill
```

**Version pinning:** Always specify an exact version string. Semver ranges are not supported.
The runtime resolves the exact version or fails with a clear error.

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
      - https://api.anthropic.com
      - https://api.openai.com
      - http://127.0.0.1:8080
      - https://github.com
```

**Always include the inference endpoint in `network.allow`** — e.g. `https://api.anthropic.com`
for Anthropic or `https://api.openai.com` for OpenAI.

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
`allow` lists the artifact names the capsule may spawn. `scoped: true` gives each spawned
sub-capsule an isolated workdir subdirectory under the caller's workdir.

```yaml
capabilities:
  spawn:
    allow:
      - worker-capsule-a
      - worker-capsule-b
    scoped: true
```

**Note:** Spawn capabilities require the mur-roost daemon. `mur run` does not currently parse
`capabilities.spawn` from the manifest — spawn allowlists are configured in mur-roost. This
YAML syntax is the intended declaration for future direct-run support.

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

```yaml
inference:
  endpoint: https://api.anthropic.com
  model: claude-opus-4-8
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
```

| Field | Type | Required | Description |
|---|---|---|---|
| `endpoint` | string | yes | Base URL for the inference API. The driver appends its own request path (e.g. `murmur-driver-anthropic` appends `/v1/messages`; `murmur-driver-openai` appends `/chat/completions`) — include any version segment the provider requires (e.g. `/v1`) in `endpoint` itself. |
| `model` | string | yes | Model identifier string passed to the driver |
| `api_key` | string | no | API key. Use `${ENV_VAR}` syntax to read from environment. Never inline a literal key. |
| `driver.artifact` | string | yes | Name of the driver artifact declared in `artifacts:` |
| `driver.config` | mapping | no | Driver-specific configuration passed as JSON (e.g. `temperature`, custom headers) |
| `system_prompt` | string | no | Inline system prompt. Mutually exclusive with `system_prompt_file`. |
| `system_prompt_file` | string | no | Path to a `.md` file loaded as the system prompt. Mutually exclusive with `system_prompt`. |
| `compaction.threshold` | float (0,1] | no | Fraction of `context.max_tokens` at which compaction fires. Default: 0.98. |
| `compaction.model` | string | no | Model for compaction inference. Defaults to the primary model. |
| `compaction.artifact` | string | no | Compaction hook artifact name. Default: `murmur-hook-compact`. |

**`${ENV_VAR}` substitution:** `api_key: ${ANTHROPIC_API_KEY}` is resolved at manifest load time.
The variable name must be uppercase letters, digits, and underscores. If the variable is not set,
`mur run` fails immediately with a clear error naming the missing variable.

**Example with driver config:**

```yaml
inference:
  endpoint: https://api.openai.com/v1
  model: gpt-4o
  api_key: ${OPENAI_API_KEY}
  driver:
    artifact: murmur-driver-openai
    config:
      temperature: 0.2
      max_tokens: 4096
```

`murmur-driver-openai` implements the OpenAI Chat Completions protocol, so it also works with
any OpenAI-compatible provider (e.g. GLM/Zhipu) — just point `endpoint` at that provider's own
base URL, including whatever version segment it requires. For example, GLM's base URL is
`https://open.bigmodel.cn/api/paas/v4` (no `/v1`).

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
| `otel_endpoint` | string | no | OTLP/HTTP endpoint for span emission. Required when using `murmur-hook-grafana`. |
| `eval.scorers` | list | no | Evaluation scorers run by `murmur-hook-eval` at session end. |

**Eval scorer types:**

| `type` | Fields | Description |
|---|---|---|
| `exit_ok` | `name` | Passes if the task completed without error |
| `max_turns` | `name`, `max` (integer) | Passes if total inference turns ≤ `max` |
| `max_tokens` | `name`, `max` (integer) | Passes if total tokens used ≤ `max` |

Omit `observability:` entirely for capsules that do not emit telemetry or run evaluations.

---

## 8. Worked Examples

### Example 1: Minimal Single-Tool Capsule

A capsule that edits files using `murmur-tool-editor` and exits when done.

```yaml
name: notes-editor
version: "0.1.0"
runtime: tool
description: "Simple file editing capsule — reads and writes files, exits on completion"

artifacts:
  - name: murmur-tool-editor
    version: "0.1.0"
    runtime: tool
  - name: murmur-driver-anthropic
    version: "0.1.0"
    runtime: driver

capabilities:
  network:
    allow:
      - https://api.anthropic.com

inference:
  endpoint: https://api.anthropic.com
  model: claude-sonnet-4-6
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic

lifecycle:
  task_acceptance: single
  after_task: exit
```

### Example 2: Shell-Capable Coding Capsule

A capsule with shell, git, and editor for coding tasks. Grants only the binaries actually needed.

```yaml
name: coding-agent
version: "0.1.0"
runtime: tool
description: "Coding agent with shell, git operations, and file editing"

artifacts:
  - name: murmur-tool-git
    version: "0.1.0"
    runtime: tool
  - name: murmur-tool-editor
    version: "0.1.0"
    runtime: tool
  - name: murmur-driver-anthropic
    version: "0.1.0"
    runtime: driver

capabilities:
  shell:
    allow:
      - bash
  network:
    allow:
      - https://api.anthropic.com
      - https://github.com

inference:
  endpoint: https://api.anthropic.com
  model: claude-opus-4-8
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic

lifecycle:
  task_acceptance: single
  after_task: exit

context:
  max_tokens: 200000
```

### Example 3: Orchestrator Capsule

A capsule that decomposes tasks into parallel sub-capsules using mur-roost.
`spawn.scoped: true` gives each sub-capsule an isolated workdir subdirectory.

```yaml
name: orchestrator
version: "0.1.0"
runtime: tool
description: "Orchestrator that spawns scoped sub-capsules for parallel task execution"

artifacts:
  - name: murmur-tool-editor
    version: "0.1.0"
    runtime: tool
  - name: murmur-driver-anthropic
    version: "0.1.0"
    runtime: driver

capabilities:
  network:
    allow:
      - https://api.anthropic.com
  spawn:
    allow:
      - worker-capsule
    scoped: true

inference:
  endpoint: https://api.anthropic.com
  model: claude-opus-4-8
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic

lifecycle:
  task_acceptance: queue
  after_task: sleep
  queue_depth: 10
```

---

## 9. Least-Privilege Guidance

**Do not grant `bash` when only `git` or `editor` is needed.**
`murmur-tool-git` covers all git operations as typed tool calls without shell access.
`murmur-tool-editor` covers file read/write/patch without shell access.
Add `capabilities.shell.allow: [bash]` only when the task genuinely requires arbitrary shell
commands that no structured tool can handle.

**Do not declare network access for local-only workloads.**
Omit `capabilities.network` entirely for capsules that operate only on local files.
For capsules that call external APIs, list only the specific hostnames needed — not `*` or broad ranges.

**Do not over-declare spawn allowlists.**
`capabilities.spawn.allow` must list only the specific capsule artifact names this capsule needs
to spawn. An empty list or absent block means no spawning is allowed.

**Match `task_acceptance` to the actual usage pattern.**
- `single` (default): one-shot task; capsule exits when done. Correct for `mur run --input task.md`.
- `queue`: persistent agent that accepts tasks from an orchestrator. Requires `after_task: sleep`.
  Do not use `queue` for one-shot tasks — the capsule will not exit and will block the caller.

**Use `${ENV_VAR}` references for all secrets.**
Never write an API key literal in a manifest. Use `api_key: ${ANTHROPIC_API_KEY}` and inject
the value via the environment. A manifest with a literal secret will trigger a build warning.

**Omit optional blocks entirely when not needed.**
`capabilities`, `context`, `lifecycle`, and `observability` are all optional. Omit any block
whose defaults are acceptable — fewer declared capabilities is always safer.

---

## 10. Default Artifact Catalog

Use this table when selecting artifacts by name. For live discovery, use
`murmur-tool-registry-search` (coming in a future slice).

| Artifact | Runtime | Description |
|---|---|---|
| `murmur-driver-anthropic` | driver | Anthropic Messages API inference driver. Use with `endpoint: https://api.anthropic.com` |
| `murmur-driver-openai` | driver | OpenAI Chat Completions inference driver (also works with OpenAI-compatible providers). Use with `endpoint: https://api.openai.com/v1` |
| `murmur-tool-git` | tool | Structured git interface: status, add, commit, push, pull, branch, clone, worktree, and more. Prefer over granting `git` in `shell.allow`. |
| `murmur-tool-editor` | tool | File read/write/patch operations: read_file, write_file, replace_in_file, find_in_files. Prefer over shell-based file editing. |
| `murmur-tool-request-input` | tool | HITL pause gate. Emits an `input-required` signal; the capsule waits for a human reply before continuing. Use with `lifecycle.input_timeout_secs` to avoid indefinite waits. |
| `murmur-hook-compact` | hook | Context compaction hook. Fires when token usage crosses the compaction threshold; replaces conversation history with a summary. Referenced via `inference.compaction.artifact`. |
| `murmur-hook-debug` | hook | Debug event logger. Writes every lifecycle event (session start, inference, tool call, shell, session end) to `hook-debug.jsonl` in the capsule workdir. |
| `murmur-hook-grafana` | hook | OTel span emitter. Sends structured telemetry to Grafana Tempo. Requires `observability.otel_endpoint` in the capsule manifest. |
| `murmur-hook-eval` | hook | Evaluation scorer. Runs configured scorers (exit_ok, max_turns, tool_sequence, llm_judge) and writes results to `eval.jsonl`. |
| `murmur-skill-create-manifest` | skill | This artifact. Provides complete manifest generation guidance. |

**Using artifacts in a capsule manifest:**

```yaml
artifacts:
  - name: murmur-tool-git
    version: "0.1.0"
    runtime: tool
  - name: murmur-hook-debug
    version: "0.1.0"
    runtime: hook
```

Always specify the exact `version` from `mur list` output. Run `mur list` to see which versions
are installed locally, or `mur pull <name>` to fetch a specific version.
