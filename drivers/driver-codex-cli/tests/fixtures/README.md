# Codex CLI stream recordings

Real `codex exec --json` output. Each case has `<name>.stdout.jsonl`, `<name>.stderr.txt` and
`<name>.exit` (the exit code). Codex's own stream format is real; the model replies are canned
(`MOCKREPLY …`) by the mock provider, except in case 15, which reached OpenAI's real endpoint with
no credential.

## `0.154.0/` and `0.167.2/`

Recorded 2026-11-12 by `../live/record.sh`, once per version:

| Directory | Binary |
|---|---|
| `0.154.0/` | the `codex-cli 0.154.0` installed on the recording host |
| `0.167.2/` | `codex-cli 0.167.2`, `codex-x86_64-unknown-linux-musl` from the `openai/codex` GitHub release `rust-v0.167.2` (archive sha256 `8246e846…b15f`, matching the release's published digest) |

Every case ran from `env -i` with only `HOME` (a fresh scratch directory under `/space/scratch/`
holding no `auth.json`), `PATH=/usr/bin:/bin`, `MOCK_KEY=sk-test` and the plan's own
`MURMUR_BRIDGE_TOKEN`. The working directory was a scratch folder. Stderr has the scratch path
written as `<scratch>`. Stdout is unchanged; 0.167.2's code mode `error` items name the folder the
binary was run from.

The argv, `env-set`, `instructions.md` and stdin are exactly what `launch` built, as printed by
`cargo run --example print_plan`, with bridge server `murmur` and the system prompt
`- You are a Murmur capsule recording the codex driver.\n- Answer in one short line.`. The only
addition is the mock provider overrides, inserted right after `exec`:

    -c model_provider=mock -c model_providers.mock.name=mock
    -c model_providers.mock.base_url=http://127.0.0.1:<port>/v1
    -c model_providers.mock.wire_api=responses -c model_providers.mock.env_key=MOCK_KEY
    -c model=gpt-5.4

The mock (`../live/mock.py`) listened on port 18110 for 0.154.0 and 18140 for 0.167.2, and the mock
bridge (`../live/bridge.py`) on 18210 and 18240. A recorded new bridged turn's full argv (0.154.0,
case 05):

    exec <mock overrides> --json --skip-git-repo-check --ignore-user-config --strict-config
    -s read-only -c approval_policy="never" --disable shell_tool --disable unified_exec
    --disable view_image --disable multi_agent --disable goals --disable browser_use
    --disable computer_use --disable image_generation --disable apps --disable plugins
    --disable in_app_browser --disable tool_suggest --disable skill_search --disable sleep_tool
    --disable unbounded_connection_retries -c web_search="disabled" -c project_doc_max_bytes=0
    -c check_for_update_on_startup=false
    -c model_instructions_file="<scratch>/plans/05-bridge-tool-call/files/instructions.md"
    -c mcp_servers.murmur.url="http://127.0.0.1:18210/mcp"
    -c mcp_servers.murmur.bearer_token_env_var="MURMUR_BRIDGE_TOKEN"
    -c mcp_servers.murmur.default_tools_approval_mode="approve" -

Auth is `api-key` through the mock (`MOCK_KEY`) for cases 01–14, and none for case 15.

| Case | Task (stdin) | Extra plan input | What it shows |
|---|---|---|---|
| `01-text-new-session` | `Remember SECRET-4242 for later. Reply briefly.` | new session | `thread.started` with Codex's own id, one `agent_message`, `turn.completed` usage |
| `02-resume-same-session` | `RECALL the secret.` | resume 01's `thread_id` | the same id; the history held the secret (`saw_secret=True`); usage is the thread's total (240) |
| `03-resume-again` | `RECALL it once more.` | resume 01's `thread_id` | the total grows (360) |
| `04-resume-missing-session` | `RECALL the secret.` | resume `01a1cd99-0000-7000-8000-000000000000` | no stdout, exit 1, `no rollout found for thread id …` |
| `05-bridge-tool-call` | `CALL_TOOL=echo_tool` | — | `mcp_tool_call` started then completed, by bare name, on server `murmur` |
| `06-bridge-tool-image` | `CALL_TOOL=picture_tool` | — | a result with a text item and an MCP `image` item (692-byte PNG) |
| `07-bridge-tool-error` | `CALL_TOOL=fail_tool` | — | a tool answering `isError: true` reported as `status: "failed"` with the answer in `result` and no `error` |
| `08-thinking` | `THINK about it.` | config `{"reasoning_summary":"auto"}` | a `reasoning` item before the message |
| `09-auth-401` | `AUTH401` | — | `Reconnecting... 1/5` … `5/5`, an `error` line, `turn.failed` with `401 Unauthorized`, exit 1 |
| `10-quota-429` | `QUOTA429` | — | `exceeded retry limit, last status: 429 Too Many Requests`, exit 1 |
| `11-interrupt-sigint` | `SLOW` | `SIGINT` 4 s in | no terminal line, exit 1 |
| `12-apply-patch-refused` | `CALL_PATCH` | — | no item for the refused `apply_patch`; stderr says why |
| `13-ask-user-refused` | `CALL_ASK` | — | no item for `request_user_input`; stderr `unavailable in Default mode` |
| `14-code-mode-model` | `CALL_TOOL=echo_tool` | `-m gpt-5.6-terra` | a code mode model: no `tools` field, so the mock finds no tool. 0.167.2 adds a code-mode-host `error` item |
| `15-auth-401-unauthenticated-real-endpoint` | `Reply briefly.` | no mock overrides | OpenAI's real endpoint with no credential: WebSocket retries 2–5, the HTTPS fallback item, HTTPS retries 1–5, `turn.failed` with `401 Unauthorized`. Nothing spent |

On 0.154.0 the mock's model `gpt-5.4` is in Codex's catalog, so the bridged tools are deferred
behind Codex's `tool_search`: the mock answers a tool task with a `tool_search_call` first, and the
turn takes three requests (usage 360). On 0.167.2 the model is unknown, every case opens with a
`Model metadata for gpt-5.4 not found` `error` item, and the bridged tools are offered directly.

## `tarball-0.167.2/`

Byte-identical copies of ten recordings from `/space/cowork/Murmur/nexus/grok-codex-recordings.tar.gz`
(`recordings/`), made 2026-11-12 in a cloud sandbox as root with `codex-cli 0.167.2`, the hardened
flag set from its `tools/cflags2.sh` with `developer_instructions` instead of
`model_instructions_file`, the bridge registered as `murmur_bridge`, and the same kind of mocks. Auth
is `api-key` through the mock.

| Case | Read as | What it shows |
|---|---|---|
| `c12-new` | new session | the session the next two resume |
| `c13-resume` | resume | the thread's total, 240 |
| `c16-resume2` | resume | the thread's total, 480 |
| `c17-tool-new` | new session | a bridged call with `default_tools_approval_mode="approve"` |
| `c07-ro-bridge` | new session | a bridged call refused without it: `status: "failed"`, `error.message` set |
| `c15-resume-missing` | new session | no stdout, exit 1 |
| `c18-sigint` | new session | no terminal line after `SIGINT`, exit 1 |
| `c03-gpt-5.6-terra` | new session | a code mode model with no `tools` field |
| `c08-ask` | new session | `request_user_input` refused, no item |
| `c06-patch-readonly` | new session | `apply_patch` refused by the read-only sandbox, no item |
