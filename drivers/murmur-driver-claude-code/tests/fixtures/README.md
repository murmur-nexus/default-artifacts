# Claude Code stream recordings, 2.1.278

Real `claude` 2.1.278 output, recorded 2026-09-19 against a local mock Anthropic endpoint
(`../tools/mock.py`) and a mock MCP server standing in for Murmur's bridge (`../tools/bridge.py`).
Claude Code's own stream format is real; the model replies are canned (`MOCKREPLY ...`).
Auth was an API key pointed at the mock, so `init.apiKeySource` reads `ANTHROPIC_API_KEY`.
On a subscription login it reads `none`; that case is NOT recorded here.

Each case has `<name>.stdout.jsonl`, `<name>.stderr.txt` and `<name>.exit` (exit code).
Paths are redacted: `/work` is the working dir, `/home/operator` the HOME, `/scratch` the rest.

Base argv for every case (the flags the process driver is expected to produce):

    claude --print --output-format stream-json --verbose --input-format stream-json
           --include-partial-messages --setting-sources ""
           --tools mcp__claude_bridge__echo_tool --mcp-config <bridge json> --strict-mcp-config
           --permission-mode bypassPermissions --system-prompt "You are a Murmur capsule."

The task is one JSON line on stdin:
`{"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}`

| Case | Extra argv / env | What it shows |
|---|---|---|
| 01-text-new-session | `--session-id aaaaaaaa-…` | init carries the caller's session id; text deltas then final `assistant` then `result` success |
| 02-resume-same-session | `--resume aaaaaaaa-…` | same session id; the model saw the first message (`saw_secret=True`) |
| 03-bridge-tool-call | — | `tool_use` named `mcp__claude_bridge__echo_tool`, `input_json_delta`, `user` `tool_result` paired by `tool_use_id` |
| 04-thinking | — | `thinking_delta`, a thinking-only `assistant` event, then the text event |
| 05-auth-401 | `CLAUDE_CODE_MAX_RETRIES=0` | `result` with `subtype: success` **and** `is_error: true`, `api_error_status: 401`, exit 1 |
| 06-quota-429 | `CLAUDE_CODE_MAX_RETRIES=0` | same shape, `api_error_status: 429`, exit 1 |
| 07-quota-429-after-retries | `CLAUDE_CODE_MAX_RETRIES=2` | `system` `api_retry` events before the failing `result` |
| 08-interrupt-control-request | long-lived stdin, `{"type":"control_request","request_id":"int-1","request":{"subtype":"interrupt"}}` after 4 s, then a second message | `control_response`, `result` `error_during_execution`, then the second message answered in the same process |
| 09-interrupt-sigint | SIGINT after 5 s, one-shot | `result` `error_during_execution`, `terminal_reason: aborted_streaming`, exit 0 |
| 10-resume-after-sigint | `--resume cccccccc-…` | a session interrupted by SIGINT resumes normally |

Default retry behaviour (not recorded, observed): with no `CLAUDE_CODE_MAX_RETRIES`, a 401 or 429
is retried up to 10 times with backoff, each visible as `system`/`api_retry`, before the final result.
