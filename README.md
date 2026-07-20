![](https://murmur-static.s3.eu-north-1.amazonaws.com/assets/logo-default-transparent-white-32.png)

# default-artifacts

Default Murmur artifacts shipped with the runtime.

## Inference drivers

WASM components (`runtime: driver`). Export `murmur:tool/run` (`world driver`) and translate between the Murmur canonical inference format and each provider's native API.

| Artifact | Location | Description |
|---|---|---|
| `murmur-driver-anthropic` | `drivers/murmur-driver-anthropic/` | Anthropic Messages API |
| `murmur-driver-deepseek` | `drivers/murmur-driver-deepseek/` | DeepSeek API — `deepseek-v4-flash` and `deepseek-v4-pro`, with thinking mode |
| `murmur-driver-openai` | `drivers/murmur-driver-openai/` | OpenAI-compatible Chat Completions API, with Responses API for `gpt-5` and later models |

## Hooks

WASM components (`runtime: hook`) that attach to lifecycle events. Each hook declares its `binding`, `execution_mode`, and `commit_policy` in its own `murmur.yaml`; where a field is omitted the runtime defaults apply (all events, async, no commit).

| Artifact | Location | Binding | Mode | Commit policy | Description |
|---|---|---|---|---|---|
| `murmur-hook-debug` | `hooks/murmur-hook-debug/` | *(all events)* | async | none | Writes a JSONL event log to `workdir/hook-debug.jsonl` |
| `murmur-hook-compact` | `hooks/murmur-hook-compact/` | `on-compaction` | blocking | replace-context | Compacts conversation history when the session token threshold is reached |
| `murmur-hook-diff-summary` | `hooks/murmur-hook-diff-summary/` | *(all events)* | blocking | none | Snapshots files before each editor tool call and emits a structured unified-diff summary at end of turn |
| `murmur-hook-memory-jsonl` | `hooks/murmur-hook-memory-jsonl/` | *(all events)* | blocking | replace-context | Durable per-Turn Memory Log — appends each Turn to a JSONL file, reloads prior Turns at task start |
| `murmur-hook-shell-desc` | `hooks/murmur-hook-shell-desc/` | `on-stage` | blocking | write-manifests | Returns enriched tool manifests for common shell binaries at staging time |
| `murmur-hook-eval` | `hooks/murmur-hook-eval/` | *(all events)* | async | none | Scores sessions against configured scorers and writes `eval.jsonl` |
| `murmur-hook-grafana` | `hooks/murmur-hook-grafana/` | *(all events)* | async | none | Exports OTel spans to a Grafana Tempo OTLP/HTTP endpoint |

## Tools

Tool artifacts (`runtime: tool`) exposed to the agent as callable functions. `murmur-tool-create`,
`murmur-tool-editor`, and `murmur-tool-request-input` are WASM components (`wasm32-wasip2`,
exporting `murmur:tool/run`); the remaining five are native binaries whose C dependencies
(SQLite, tree-sitter, TLS) do not cross-compile to wasm32-wasip2 (see [BUILD.md](./BUILD.md)).

| Artifact | Location | Implementation | Description |
|---|---|---|---|
| `murmur-tool-create` | `tools/murmur-tool-create/` | WASM | Scaffolds new tool artifact directories |
| `murmur-tool-editor` | `tools/murmur-tool-editor/` | WASM | File read/write/patch operations (`read_file`, `write_file`, `replace_in_file`, `find_in_files`) |
| `murmur-tool-request-input` | `tools/murmur-tool-request-input/` | WASM | HITL pause gate — suspends the agent loop and waits for human input via `message/send` |
| `murmur-tool-git` | `tools/murmur-tool-git/` | native | Git operations (clone, checkout, status, diff, commit, push, worktree, and more) |
| `murmur-tool-code-graph` | `tools/murmur-tool-code-graph/` | native | Indexes a Rust repo into a SQLite symbol/edge graph; structured queries over stable symbol identities |
| `murmur-tool-code-coverage` | `tools/murmur-tool-code-coverage/` | native | Spectrum-based fault localization (Ochiai / Tarantula) over per-test LCOV reports |
| `murmur-tool-test-report` | `tools/murmur-tool-test-report/` | native | Parses raw test-runner output (cargo, pytest, go, jest) into a structured failure list |
| `murmur-tool-registry-search` | `tools/murmur-tool-registry-search/` | native | Searches the Murmur artifact registry for artifacts matching a keyword |

## Skills

Documentation artifacts (`runtime: skill`). No binary or WASM component — each contains a
`skill.md` guidance file the runtime installs to `workdir/tools/<name>/skill.md` before the
agent loop starts. The agent reads the file voluntarily; content is never injected into the
system prompt.

| Artifact | Location | Description |
|---|---|---|
| `murmur-skill-create-manifest` | `skills/murmur-skill-create-manifest/` | Complete `murmur.yaml` schema reference — enables an agent to generate valid capsule manifests from plain-language task descriptions |
| `murmur-skill-investigation-checkpoint` | `skills/murmur-skill-investigation-checkpoint/` | The investigations convention for `checkpoints/decisions.json` — record and reuse investigative verdicts across a session |

## WIT

The `wit/` directory is vendored from `murmur/crates/capsule-runtime/wit/` and synced manually whenever the interfaces change. The mirror is deliberately partial: it carries only the artifact-facing subtrees (`guest/`, `hook/`) and omits murmur's host-side world, the dead `runtime/` tree, and murmur's docs-reference top-level copies, none of which any artifact in this repo consumes. The relevant worlds:

- `wit/hook/` — `world hook { export murmur:hook/lifecycle; }` — implemented by all hook artifacts
- `wit/guest/` — `world driver { ... export murmur:tool/run; }` — implemented by inference drivers
- `wit/guest/` — `world tool { ... export murmur:tool/run; }` — implemented by tool artifacts

The `wit-sync` CI job verifies the mirror stays byte-identical to the murmur commit pinned in
[.github/workflows/ci.yml](./.github/workflows/ci.yml); the mirror and the pin always move together.

See [BUILD.md](./BUILD.md) for exact build and copy commands.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
