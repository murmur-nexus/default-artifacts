# Grok Build stream recordings, 1.0.77

Real `grok` 1.0.77 output in `--output-format streaming-messages-json`. Each case is
`<name>.stdout.jsonl`, `<name>.stderr.txt` and `<name>.exit` (the exit code). The model replies are
canned by a mock (`MOCKREPLY …`); `grok`'s own stream format is real. Every case authenticated with
an API key pointed at the mock, so every `init` reads `apiKeySource: "user"` and the driver reports
`auth: "api-key"`. None ran on a grok.com login.

## 01 to 11: byte-identical copies

Recorded 2026-11-12 in a cloud sandbox, as root, against a minimal OpenAI Responses SSE mock and a
copy of murmur's tool bridge serving `echo_tool` and `session-status` under the server name
`murmur_bridge`. Copies of those mocks, extended, are in [`../live/`](../live/). Each run started
`grok` from `env -i` with a scratch `HOME`, `XAI_API_KEY=xai-test`,
`GROK_XAI_API_BASE_URL=http://127.0.0.1:18100/v1` and `GROK_DISABLE_AUTOUPDATER=1`, with an agent
file listing `tools: search_tool, use_tool` and the bridge under `mcpServers`. **The exact argv of
each run was not kept.** The files are copied byte for byte from the recordings tarball.

| Fixture | Source | grok | Auth | How it was launched, and what it shows |
|---|---|---|---|---|
| `01-text-new-session` | `g06-new` | 1.0.77 | api-key | `-s e09e81d3-…`, partial messages: one text delta, `assistant`, `result` success |
| `02-resume-same-session` | `g07-resume` | 1.0.77 | api-key | `-r` of `01`'s session: the model saw the first message (`saw_secret=True`), three text deltas |
| `03-new-session-id-in-use` | `g09-new-existing` | 1.0.77 | api-key | `-s` with `01`'s id again: `init` with an empty `session_id`, `result` `errors: ["Session ID … is already in use."]`, exit 1 |
| `04-resume-missing-session` | `g10-resume-missing` | 1.0.77 | api-key | `-r` of an id `grok` does not hold: an empty `session_id`, a failed restore from `auth.x.ai`, exit 1 |
| `05-bridge-tool-call` | `g17-tool-partial` | 1.0.77 | api-key | partial messages: a `use_tool` block start, one 67-byte `input_json_delta` naming `murmur_bridge__echo_tool`, the bridged result as an `MCP` JSON string |
| `06-operator-tool-refused` | `g05-allowlist-operator_srv__echo_tool` | 1.0.77 | api-key | default permission mode, `--allow "MCPTool(murmur_bridge__*)"`, an operator server `operator_srv`: the call is refused, `result` `stop_reason: cancelled`, exit 0 |
| `07-unqualified-tool-name` | `g03-tool-echo_tool` | 1.0.77 | api-key | `bypassPermissions`: `use_tool` naming the bare `echo_tool` is refused by `grok` with an `{"error","message"}` result |
| `08-thinking` | `g12-THINK` | 1.0.77 | api-key | partial messages: two thinking deltas, a text delta, a thinking block and a text block |
| `09-auth-401` | `g12-AUTH401` | 1.0.77 | api-key | the mock answers 401: `errors[0]` holds `Unauthorized (401)` and `"http_status": 401`, all-zero usage, exit 1 |
| `10-quota-429` | `g12-QUOTA429` | 1.0.77 | api-key | the mock answers 429: `errors: ["rate_limit_exceeded: mock 429"]`, all-zero usage, exit 1 |
| `11-interrupt-sigint` | `g11-sigint` | 1.0.77 | api-key | a slow reply, `SIGINT` after twelve deltas: a closing `result` `stop_reason: cancelled`, all-zero usage, exit 130 |

`06`, `07` and `03`/`04` were not run with `--include-partial-messages`, so they carry no
`stream_event` lines.

## 12: recorded for this driver

`12-bridge-image-result` was recorded on 2026-11-12 on the build host, as a normal user, with
`grok` 1.0.77, by the commands in [`../live/README.md`](../live/README.md#recording-12). Its argv,
environment switches and files are exactly what `launch` planned for a request with bridge server
`murmur` (tools `echo_tool`, `session-status`, `image_tool`), config `{"reasoning_effort":"low"}`,
system prompt `- You are a Murmur capsule. Answer briefly.`, no model, task
`CALL_TOOL=use_tool:murmur__image_tool` and new session `019a7c3e-5b10-7a4c-9e21-3f6d2b8a1c05`,
written to disk by `examples/write_plan.rs`. The mock is `../live/mock.py` and the bridge
`../live/bridge.py`, whose `image_tool` answers with a text item and an MCP `image` item holding an
88-byte PNG. Auth was `XAI_API_KEY=xai-test` with a scratch `HOME` holding no `auth.json`. Paths in
`init.cwd` are the scratch work folder as recorded; nothing is redacted. The agent file's body is
that system prompt. A first recording on 2026-11-11, made when the body was a fixed line, is the
same stream line for line once `uuid`, `cwd`, message ids and durations are set aside.

| Fixture | Source | grok | Auth | What it shows |
|---|---|---|---|---|
| `12-bridge-image-result` | recorded here | 1.0.77 | api-key | `init.tools` lists the three bridged tools; one 61-byte `input_json_delta`; the result's `OkayOutput` is one string holding the bridge's text and the image as a `data:image/png;base64,…` line; stderr empty, exit 0 |
