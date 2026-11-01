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

# Claude Code subscription recording, 2.1.314

`11-subscription-rate-limit` is real `claude` 2.1.314 output, recorded 2026-10-31 on a real
Claude subscription login: no mock and no API key. `init.apiKeySource` reads `none`. The model
reply is real.

The recorder ran outside this repository, in a fresh empty scratch working directory. Its
environment held only `HOME` (the real one, where the login lives) and `PATH`, which is what a
capsule with `capabilities.env.allow: [HOME, PATH]` passes. The argv is exactly what the driver
builds for a no-bridge turn with `prompt_suggestions: true`:

    claude --print --output-format stream-json --verbose --input-format stream-json
           --include-partial-messages --setting-sources "" --prompt-suggestions true
           --model haiku --session-id 2946b2e2-95cf-4b20-8f06-6e90af26501a --tools ""
           --system-prompt "You are a Murmur capsule."

The task was one stdin line:
`{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Reply with the single word ok."}]}}`.
Stdin was closed as soon as the `result` line was read, as the runtime closes it, and stdout was
read to EOF.

Redactions, and nothing else changed:

| Recorded | Committed |
|---|---|
| the scratch working dir, in `cwd` and `original_cwd` | `/work` |
| the same dir encoded in the `memory_paths` project slug | `-work` |
| the HOME | `/home/operator` |

The recording carries no email, organization or account id. `stderr` was empty and the exit
code was `0`. Every `rate_limit_event` value is as printed:

    {"status":"allowed","resetsAt":1793818800,"rateLimitType":"seven_day","overageStatus":"rejected",
     "overageDisabledReason":"org_level_disabled","isUsingOverage":false,
     "unifiedWindows":{"seven_day":{"utilization":0.41,"resetsAt":1793818800,"status":"allowed"}}}

This login's line names only `seven_day` in `unifiedWindows`, not `five_hour` as well, and has no
`overageResetsAt`. `resetsAt` is epoch seconds: 1793818800 is 2026-11-04 UTC.

Times are from a monotonic clock in the recorder:

| Run | Turns | `rate_limit_event` | `prompt_suggestion` | last `result` → exit |
|---|---|---|---|---|
| 1, committed as `11-subscription-rate-limit` | 1 | yes, 26 ms before `result` | no | 823 ms |
| 2 | 2 | yes, turn 1 only | no | 1,883 ms |
| 3 | 3 | yes, turn 1 only | no | 2,003 ms |

**No run printed a `prompt_suggestion`.** claude 2.1.314's bundle suppresses the suggestion for
a turn before the conversation holds two assistant messages (`early_conversation`), so a
session's first turn never gets one. In run 2, turn 2 read nothing from the cache
(`cache_read_input_tokens: 0`): the login's claude.ai connector tools joined the tool list after
turn 1, which changed the prompt prefix, and the bundle suppresses a cold-cache turn
(`cache_cold`). Run 3 added a third turn, which read 4,414 cached tokens and still printed no
suggestion. The bundle's suggestion prompt allows the model to answer with nothing, which is
suppressed as `empty`. So no `result` → `prompt_suggestion` delay is measured. When stdin closes,
`claude` waits up to 30 s for the suggestion call. murmur reads the harness's output for only 3 s
after the terminal event, so a suggestion slower than that is lost. Runs 2 and 3 are not
committed, because their later `init` lines list the login's own claude.ai connectors.
