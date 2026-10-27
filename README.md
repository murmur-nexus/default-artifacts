![](https://murmur-static.s3.eu-north-1.amazonaws.com/assets/logo-default-transparent-white-32.png)

# default-artifacts

Default Murmur artifacts shipped with the runtime.

Every artifact here is published under scope `murmur`: the artifact in `tools/tool-editor/` is
`murmur/tool-editor`, which is how a capsule's `artifacts:` names it and how it installs:

```bash
mur install -g murmur/tool-editor@0.11.0
```

```yaml
artifacts:
  - name: murmur/tool-editor
    version: "0.11.0"
```

Installing by scoped name needs `mur` 0.7.0 or later. `mur` v0.6.0 and earlier have no
`murmur/<name>` form and do not read the release's `artifacts-index.json`.

The tables below give each artifact's bare name, which is also the tool name the model calls.
Each one's version is the `version:` in its own `murmur.yaml`; `./scripts/list-artifacts.sh`
lists them all. A release publishes every artifact and the `artifacts-index.json` that
`mur install` resolves them from — see [BUILD.md](./BUILD.md).

## Inference drivers

WASM components (`runtime: driver`). Export `murmur:tool/run` (`world driver`) and translate between the Murmur canonical inference format and each provider's native API. These are the drivers a `transport: http` capsule uses; a process driver is also `runtime: driver` but exports a different interface — see [Process drivers](#process-drivers).

| Artifact | Location | Description |
|---|---|---|
| `driver-anthropic` | `drivers/driver-anthropic/` | Anthropic Messages API, with extended thinking and prompt-cache breakpoints; caching is tuned by a `config:` block on its manifest entry |
| `driver-baseten` | `drivers/driver-baseten/` | Baseten Model APIs Chat Completions — `zai-org/GLM-5.3` only, which always reasons; reasoning effort is tuned by `inference.driver.config`, which rejects any key it does not read; the reasoning goes back on later turns unless the capsule sets `replay_reasoning: false`, and a task's turns share Baseten's session affinity |
| `driver-deepseek` | `drivers/driver-deepseek/` | DeepSeek API — `deepseek-flash` and `deepseek-v4-pro`, with thinking mode; any other model is refused |
| `driver-moonshotai` | `drivers/driver-moonshotai/` | Moonshot AI Chat Completions API — the K3 models `kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed` and `kimi-k3`, and the K2 models `kimi-k2.7-code`, `kimi-k2.7-code-highspeed` and `kimi-k2.6`, which Moonshot retires by the end of December 2026; reasoning effort (K3), the thinking switch (`kimi-k2.6`) and `json_schema` structured output are tuned by `inference.driver.config`, which rejects any key it does not read or the model cannot honour |
| `driver-openai` | `drivers/driver-openai/` | OpenAI API — sends each model it lists to the Responses or Chat Completions endpoint that model's page lists, and refuses any other model; reasoning summaries, effort and verbosity are tuned by `inference.driver.config`, which rejects any key it does not read |
| `driver-zai` | `drivers/driver-zai/` | Z.ai Chat Completions API — `glm-5.3` only, which always reasons; reasoning effort is tuned by `inference.driver.config`, which rejects any key it does not read; the reasoning goes back to Z.ai on every later turn unless the capsule sets `replay_reasoning: false` |

All six report the provider's own token counts on an optional top-level `usage` object of the response — `input_tokens`, `output_tokens`, `cached_tokens`, `cache_write_tokens` — which Murmur carries onto the `inference` trace event. `input_tokens` counts only the request tokens neither read from nor written to the prompt cache, so `input_tokens + cached_tokens + cache_write_tokens` is the provider's total request count, whichever way the provider itself splits it. Each member is independently optional and is omitted, never zero-filled, when the provider did not report it. Murmur also sends a `prompt_cache_key` routing hint: `driver-openai` forwards it into the provider body, `driver-baseten` sends a SHA-256 of it as Baseten's `x-session-affinity` header, and the other four drop it, because none of those provider APIs defines a cache-key field. See each driver's own README for the per-provider field mapping.

All six also take the endpoint they talk to from the host, in
`MURMUR_INFERENCE_ENDPOINT`, and none of them carries a provider URL of its own.
The capsule manifest therefore names the provider as `gateway.endpoint` on the
driver's `artifacts:` entry, with the key as `gateway.api_key`; the runtime hands
the driver its own loopback gateway plus that endpoint's path, and attaches the
key to each request it sends on. An endpoint that is absent, empty or
whitespace-only is refused before any request is built, never replaced with a
provider default.

All six stream text and thinking, and report each tool call while the model is still writing
it, through `murmur:stream/events@0.1.0`: the call's start, with the id and name of the
`tool_call` the driver will return, then a rising UTF-8 byte count of its arguments, never the
arguments themselves. On `transport: http` these become `tool-call-started` and
`tool-call-progress` frames ahead of the call's `artifact`. murmur v0.6.0 is the first release to
serve that interface. A `mur` that can install these artifacts by scoped name (0.7.0 or later,
see above) refuses a driver release built before it at launch, with `E-WIT-LEVEL-001`:

| Artifact | First release on `murmur:stream/events` |
|---|---|
| `driver-anthropic` | 0.10.0 |
| `driver-baseten` | 0.1.0 |
| `driver-deepseek` | 0.8.0 |
| `driver-moonshotai` | 0.7.0 |
| `driver-openai` | 0.9.0 |
| `driver-zai` | 0.1.0 |

See each driver's "Tool-call progress" section for the provider event each report is taken from.

All six hold at most 1024 tool calls per streamed turn, numbered 0 through 1023 by the provider's
own `index` (`driver-anthropic` counts content blocks; `driver-openai` applies it on Chat
Completions only). A stream naming index 1024 or more fails the turn with a `driver:` error that
names the index and the limit, before anything is stored for it. Each driver's README gives the
exact message.

All six declare the output cap and context window of the models they serve, under `models:` in
their `murmur.yaml`, and `max_input_tokens` for a model whose provider documents an input ceiling
below the window. Of the six providers, only OpenAI documents one. A murmur release after v0.7.0
takes a capsule's `inference.max_tokens` and `context.max_tokens` from that table when the capsule
leaves them unset, and compacts against the input budget,
`min(max_input_tokens, context_window − output cap)`. A model a driver does not list still needs
`inference.max_tokens`, or the launch is refused with `E-RUN-052`, and needs `context.max_tokens`
for compaction to run; `driver-baseten`, `driver-deepseek`, `driver-moonshotai`, `driver-openai`
and `driver-zai` then refuse it on the first inference call. Each driver's README lists its models, the numbers and their sources,
and the models it accepts without declaring or refuses:

| Artifact | First release with `models:` | README section |
|---|---|---|
| `driver-anthropic` | 0.14.0 | "Model limits", "Models without declared limits" |
| `driver-baseten` | 0.1.0 | "Model scope" |
| `driver-deepseek` | 0.13.0 | "Model limits", "Models this driver refuses" |
| `driver-moonshotai` | 0.12.0 | "Model limits", "Models this driver refuses" |
| `driver-openai` | 0.14.0 | "Endpoints", "Model limits", "Models this driver refuses" |
| `driver-zai` | 0.1.0 | "Model scope" |

## Hooks

WASM components (`runtime: hook`) that attach to lifecycle events. Each hook declares its `binding`, `execution_mode`, and `commit_policy` in its own `murmur.yaml`; where a field is omitted the runtime defaults apply (all events, async, no commit).

| Artifact | Location | Binding | Mode | Commit policy | Description |
|---|---|---|---|---|---|
| `hook-debug` | `hooks/hook-debug/` | *(all events)* | async | none | Writes a JSONL event log to `hook-debug.jsonl` in the session records directory's `hooks/` |
| `hook-compact` | `hooks/hook-compact/` | `on-compaction` | blocking | replace-context | Compacts conversation history when the session token threshold is reached |
| `hook-diff-summary` | `hooks/hook-diff-summary/` | *(all events)* | blocking | none | Snapshots files before each editor tool call and emits a structured unified-diff summary at end of turn; requires `tool-editor` 0.3.0 or later |
| `hook-memory` | `hooks/hook-memory/` | `on-task-start` | blocking | seed-context | Seeds a task with the relevant part of the conversation that came before it, read from the runtime's durable conversation record |
| `hook-shell-desc` | `hooks/hook-shell-desc/` | `on-stage` | blocking | write-manifests | Returns tool manifests for common non-interpreter shell binaries at staging time, stating what each takes and returns; shell interpreters keep the runtime's own description |
| `hook-eval` | `hooks/hook-eval/` | *(all events)* | async | none | Scores sessions against configured scorers and writes `eval.jsonl` in the session records directory's `hooks/`, where `mur eval` reads it |
| `hook-grafana` | `hooks/hook-grafana/` | *(all events)* | async | none | Exports OTel spans to a Grafana Tempo OTLP/HTTP endpoint |
| `hook-regression-verifier` | `hooks/hook-regression-verifier/` | *(all events)* | blocking | reopen-task | Reopens a task whose edits broke previously-passing tests; appends each verdict to `regression-verifier.jsonl` |

Hooks have no filesystem by default. A hook that writes a record — `hook-debug`, `hook-eval`, `hook-regression-verifier` — is granted `capabilities.filesystem.records: true` on its own `artifacts:` entry, and writes into the `hooks/` directory of the session's records directory (`$MURMUR_RECORDS_DIR/hooks/`, or `~/.murmur/records/<workdir root>/<session-id>/hooks/` on disk), beside the session's `trace.jsonl`. `hook-diff-summary` reads files in the work folder, so it is granted `capabilities.filesystem.scope` instead. The records directory and `records: true` need `mur` 0.7.0 or later; v0.6.0 and earlier ignore `records` with a `W-SEC-019` warning and preopen nothing for the hook.

## Process drivers

WASM components (`runtime: driver`) for `transport: process`, where the capsule drives a
harness CLI — a coding agent with its own loop — instead of calling a model. They export
`murmur:driver/process` (`world process-driver`) and import nothing of Murmur's: the runtime
grants a process driver nothing at all. A process driver holds no credential and declares no
`upstream_auth:` block, because the harness authenticates itself. That is the point of the
transport: a turn spends a subscription rather than an API key.

The exported interface, not the artifact name, is what tells a process driver from an inference
driver.

| Artifact | Location | Description |
|---|---|---|
| `driver-claude-code` | `drivers/driver-claude-code/` | Drives the `claude` CLI (Claude Code): the argv for a turn, the task as one JSON line on stdin, the MCP config pointing the harness at the capsule's tool bridge, and the interrupt control request. Reads the harness's stdout back into events — streamed text and thinking, tool calls and results, retries, and a turn that ended or failed, with a failure read from `is_error` and `api_error_status` rather than `subtype`. Reports the input, output, cache-read, cache-creation and thinking token counts Claude Code prints for a turn, as totals for the run the interface asks for. Reports a tool call as soon as the model starts writing it, then the running byte count of its input, never the input itself. Exports `murmur:driver/process@0.3.0`, so it needs murmur v0.6.0 or later |

## Tools

Tool artifacts (`runtime: tool`) exposed to the agent as callable functions. `tool-corpus`,
`tool-create`, `tool-editor`, `tool-github`, `tool-report`, `tool-request-input`, and
`tool-tavily` are WASM components (`wasm32-wasip2`, exporting `murmur:tool/run`); the
remaining five are native binaries whose C dependencies (SQLite, tree-sitter, TLS) do not
cross-compile to wasm32-wasip2 (see [BUILD.md](./BUILD.md)).

| Artifact | Location | Implementation | Description |
|---|---|---|---|
| `tool-corpus` | `tools/tool-corpus/` | WASM | Append-only record store (`append`, `get`, `read_recent`, `search`, `verify`); requires a `capabilities.state` grant and a `config:` block on its manifest entry declaring the record types |
| `tool-create` | `tools/tool-create/` | WASM | Scaffolds a new artifact directory (`murmur.yaml`, stub payload, README) for a `native` tool, a `wasm` tool, or a `hook` |
| `tool-editor` | `tools/tool-editor/` | WASM | File read/write/patch operations (`read_file`, `write_file`, `replace_in_file`, `find_in_files`); the writing operations take their target as `dest_path`, declared `format: murmur-destination` so a `capabilities.filesystem.read_only` grant is enforced against the declaration rather than guessed from property names. Every path input is workdir-relative: an absolute `path`, `dest_path`, `dir` or `glob` is refused with `error_kind: absolute_path` before any filesystem call. `find_in_files` searches the whole workdir when `dir` is omitted, takes an optional `glob` file filter, and stops at 200 matches, 5000 files walked, 32 MiB read or a 500 KB result, returning what it found with a `capped` note. `read_file` returns at most 2000 lines and 100 KB per call and cuts lines over 2000 characters, with a `truncated` note naming the `start_line` that continues the read; see its README |
| `tool-github` | `tools/tool-github/` | WASM | Pushes a local commit range to one operator-pinned GitHub repo so the remote branch lands on the identical SHA, fetches a branch back, and opens, inspects, lists and merges pull requests on that repo; the GitHub token is bound on the entry as `gateway.api_key` and attached by the runtime, never held by the tool |
| `tool-report` | `tools/tool-report/` | WASM | The capsule states its own conclusion (`report`, `progress`) into `state/reports/<session_id>.json`, one file per session named by `MURMUR_SESSION_ID`, keeping at most 64 and never the caller's own or one written in the last 7 days; requires a `capabilities.state` grant, and takes an optional `config:` block closing the deliverable-kind and note-stage vocabularies |
| `tool-request-input` | `tools/tool-request-input/` | WASM | HITL pause gate — suspends the agent loop and waits for human input via `SendMessage` |
| `tool-tavily` | `tools/tool-tavily/` | WASM | Web search through Tavily, returning a bounded plain-text result; the Tavily key is bound on the entry as `gateway.api_key` and attached by the runtime, never held by the tool, and every cost- and size-bearing lever is in the entry's `config:` block |
| `tool-git` | `tools/tool-git/` | native | Git operations (clone, checkout, status, diff, commit, push, worktree, and more) |
| `tool-code-graph` | `tools/tool-code-graph/` | native | Indexes a Rust and/or Python repo into a SQLite symbol/edge graph; structured queries over stable symbol identities |
| `tool-code-coverage` | `tools/tool-code-coverage/` | native | Spectrum-based fault localization (Ochiai / Tarantula) over per-test LCOV reports |
| `tool-test-report` | `tools/tool-test-report/` | native | Parses raw test-runner output (cargo, pytest, go, jest) into a structured failure list |
| `tool-registry-search` | `tools/tool-registry-search/` | native | Searches a Murmur artifact index for artifacts, listing every published version of each `scope/name` |

## Skills

Documentation artifacts (`runtime: skill`). No binary or WASM component — each contains a
`skill.md` guidance file the runtime installs to `tools/<name>/skill.md` in the session's
runtime directory (`$MURMUR_RUNTIME_DIR`, read-only to the capsule) before the agent loop
starts. The agent reads the file voluntarily; content is never injected into the system prompt.
That is the layout of `mur` 0.7.0 or later; `mur` v0.6.0 and earlier install the file into the
capsule workdir instead.

| Artifact | Location | Description |
|---|---|---|
| `skill-create-manifest` | `skills/skill-create-manifest/` | Complete `murmur.yaml` schema reference — enables an agent to generate valid capsule manifests from plain-language task descriptions |
| `skill-investigation-checkpoint` | `skills/skill-investigation-checkpoint/` | The investigations convention for `checkpoints/decisions.json` — record and reuse investigative verdicts across a session |

## WIT

The `wit/` directory is vendored from `murmur/crates/capsule-runtime/wit/` and synced manually whenever the interfaces change. The mirror is deliberately partial: it carries only the artifact-facing subtrees (`guest/`, `hook/`, `process-driver/`) and omits murmur's host-side world, the dead `runtime/` tree, and murmur's docs-reference top-level copies, none of which any artifact in this repo consumes. The relevant worlds:

- `wit/hook/` — `world hook` — implemented by all hook artifacts
- `wit/guest/` — `world driver { ... export murmur:tool/run; }` — implemented by inference drivers
- `wit/guest/` — `world tool { ... export murmur:tool/run; }` — implemented by tool artifacts
- `wit/process-driver/` — `world process-driver { export process; }` — implemented by process drivers

`world hook` in full, from [wit/hook/worlds.wit](./wit/hook/worlds.wit):

```wit
world hook {
  import inference;
  import tokens;
  import murmur:task-io/read@0.1.0;
  import murmur:conversation/read@0.2.0;
  export murmur:hook/lifecycle@0.9.0;
}
```

Every hook is generated against the whole world, so each import is available to
any hook without a manifest change on the artifact side. What each one provides:

| Import | Provides | Granted by |
|---|---|---|
| `inference` | One LLM completion through the capsule's already-configured inference driver | Always available |
| `tokens` | Token counting against the capsule's model | Always available |
| `murmur:task-io/read@0.1.0` | The task's input and the result text its agent loop produced | `capabilities.task_io.read: true` on the hook's entry in the capsule's `murmur.yaml` |
| `murmur:conversation/read@0.2.0` | Paged, newest-first reads of the runtime's durable conversation record, without a `filesystem` grant | `capabilities.conversation.read: true` on the hook's entry in the capsule's `murmur.yaml` |

`murmur:conversation/read` and `murmur:task-io/read` are both default-deny: an
ungranted hook still links and still runs, and its functions return
`not-granted`. There is no staging-time check for either grant, so a hook that
needs one has to detect the refusal itself and say so.
[`hook-memory`](./hooks/hook-memory/) calls all three of
`murmur:conversation/read`, `murmur:runtime/tokens` and `murmur:task-io/read`.

The `wit-sync` CI job verifies the mirror stays byte-identical to the murmur commit pinned by
`MURMUR_REV` in [.github/workflows/build.yml](./.github/workflows/build.yml); the mirror and the
pin always move together, with `./scripts/murmur-pin.sh --set <commit>`.

See [BUILD.md](./BUILD.md) for exact build and re-pin commands.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
