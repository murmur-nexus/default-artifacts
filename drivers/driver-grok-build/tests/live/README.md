# Live checks for driver-grok-build

What was run against the real `grok` 1.0.77 to record `12-bridge-image-result`, the probes behind
the driver's flags and its README, and the guest run under a real `mur`. Last comes the handoff for
a live SuperGrok run, which this card did not make.

**Every run here used a scratch `HOME` under `/space/scratch/` with no `auth.json`.** A stored
grok.com login outranks `XAI_API_KEY` and would have sent the turn to xAI on the subscription. The
operator's real `HOME` was used only for `grok version`, `grok models` and `mur doctor` (which runs
those two), none of which spends a turn. `grok --help` was also run once with the real `HOME`; it
prints and exits.

## The files here

| File | What it is |
|---|---|
| `mock.py` | A minimal OpenAI Responses SSE mock, copied from the grok/codex recordings and extended: `TOOL_INPUT=<json>` sets a `use_tool` call's `tool_input`, `CALL_FILE_FORM=tool_input_file:<server__tool>` or `CALL_FILE_FORM=file` with `FORM_PATH=<path>` makes a file-form `use_tool` call, and every `function_call_output` is logged as a `TOOLOUTPUT` line with the sha256 of every base64 image in it, a data URL written into text included |
| `bridge.py` | A copy of murmur's tool bridge behaviour serving `echo_tool` and `session-status`, extended with `image_tool`: a text item `IMAGE-TOOL-RAN\n[image attached: picture.png, image/png, 88 bytes]`, then an MCP `image` item holding an 88-byte PNG it draws itself (sha256 `51b9d2ba7bccb8df170a166c194c8698b282c5471d03e0b398f657afb2deb13a`) |
| `run_plan.py` | Runs `grok` with exactly the argv and `env-set` a plan from `examples/write_plan.rs` holds, from an empty environment plus `HOME`, `PATH`, `XAI_API_KEY=xai-test` and `GROK_XAI_API_BASE_URL`. Refuses a `HOME` outside `/space/scratch/` |

Run the Python with `python3 -I`. The scratch layout used below:

```bash
S=/space/scratch/grok-101a
L=drivers/driver-grok-build/tests/live            # from the repo root
cp ~/.grok/downloads/grok-linux-x86_64 $S/bin/grok   # the same 1.0.77 binary, outside ~/.grok
python3 -I $L/mock.py 28731 $S/logs/mock.log &
python3 -I $L/bridge.py 28732 <token> $S/logs/bridge.log &
```

The probes ran `grok` through a small runner equivalent to:

```bash
cd <work dir> && env -i HOME=<scratch home> PATH=$S/bin:/usr/bin:/bin XAI_API_KEY=xai-test \
  GROK_XAI_API_BASE_URL=http://127.0.0.1:28731/v1 <the driver's 18 env switches> \
  timeout 60 grok <args>
```

with an agent file shaped exactly as `launch` writes it (server `murmur`, the bridge URL and
`Bearer <token>` as JSON strings) and the driver's flags: `--prompt-file <task> --output-format
streaming-messages-json --include-partial-messages --agent <agent file>
"--system-prompt-override=- You are a probe." --allow "MCPTool(murmur__*)" --no-auto-update
--disable-web-search --no-subagents -s <uuid>`.

## Recording 12

```bash
cargo run -q -p driver-grok-build --example write_plan -- \
  $S/rec12/files $S/rec12/plan.json murmur http://127.0.0.1:28732/mcp rec12-bridge-token \
  echo_tool,session-status,image_tool new 019a7c3e-5b10-7a4c-9e21-3f6d2b8a1c05 \
  '{"reasoning_effort":"low"}' - '- You are a Murmur capsule. Answer briefly.' \
  'CALL_TOOL=use_tool:murmur__image_tool'
python3 -I $L/run_plan.py $S/rec12/plan.json $S/bin/grok $S/rec12/home2 /usr/bin:/bin \
  http://127.0.0.1:28731/v1 $S/rec12/work $S/rec12/final/12-bridge-image-result
```

The plan's argv, with `{files_dir}` replaced:

```
--prompt-file /space/scratch/grok-101a/rec12/files/task.md
--output-format streaming-messages-json
--include-partial-messages
--agent /space/scratch/grok-101a/rec12/files/capsule-agent.md
--system-prompt-override=- You are a Murmur capsule. Answer briefly.
--allow MCPTool(murmur__*)
--no-auto-update
--disable-web-search
--no-subagents
--reasoning-effort low
-s 019a7c3e-5b10-7a4c-9e21-3f6d2b8a1c05
```

Its `env-set` was the driver's 18 switches, and its files were `task.md` (the task) and
`capsule-agent.md`:

```
---
name: "murmur-capsule"
description: "Murmur capsule"
tools: search_tool, use_tool
mcpServers:
  - murmur:
      url: "http://127.0.0.1:28732/mcp"
      headers:
        Authorization: "Bearer rec12-bridge-token"
---
You are a Murmur capsule.
```

`grok` accepted every flag. It exited `0` with an empty stderr.

**What grok sent the mock for the image.** The request after the tool call carried the result as
a `function_call_output` whose `output` is a **string** of 208 characters:

```
IMAGE-TOOL-RAN
[image attached: picture.png, image/png, 88 bytes]
data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABgAAAAQCAIAAACDRijCAAAAH0lEQVR42mM4ISdHEMnZnCCIGEYNGjVo1KBRgygzCACqlZyQfrF9rQAAAABJRU5ErkJggg==
```

The mock logged `TOOLOUTPUT {"call_id": "call_1", "output_type": "str", "shape": "<string of 208
chars>", "images": [["image/png (as text)", "51b9d2ba7bccb8df170a166c194c8698b282c5471d03e0b398f657afb2deb13a"]]}`:
the bridge's exact bytes, **as text** inside the tool output, not as an image input.

## Probes

| Probe | Result |
|---|---|
| `--no-auto-update` | **Accepted**: the full driver argv with it exited `0` and the turn completed. It stays in `launch` |
| The quoted agent file | **Works**: the bridge log shows `server/discover`, then `initialize`, `notifications/initialized`, a `GET` (405) and `tools/list`, every request carrying the right bearer. A tool-calling turn's `init.tools` listed `murmur__echo_tool`, `murmur__session-status` and `murmur__image_tool`, and the call returned `echoed: hi`. A plain text turn's `init.tools` listed only `search_tool` and `use_tool`: `init` is written at the first output line, which can precede `tools/list` |
| A task `/session-info please` via `--prompt-file` | **Run as a slash command**: the reply was the session's id, work folder, model, turn and context use, with no model request |
| A task `/goal finish the probe` | **Run as a slash command**: `Goal paused. No plan was produced. Retry with /goal resume.`, no model request |
| The same two with `--verbatim` after `--prompt-file`'s pair | **Still run as slash commands**, identically. `--verbatim` does not make them text, so `launch` does not pass it |
| A task `/always-approve` | Run as a command: an empty `result`, no model request. A turn resumed from that session (`-r`) that called an operator MCP tool was **refused** as in `06`: the mode did not carry over |
| `use_tool` with `tool_input_file` naming `murmur__echo_tool` and a file in the work folder | **Not refused**: `grok` read the file with no Read tool and no approval, called the bridge with `{"text":"from-args-file"}`, and returned `echoed: from-args-file` |
| `use_tool` with `file` naming a whole-call file | **Not refused** either: the bridge got `{"text":"from-call-file"}` |
| `AGENTS.md` in the work folder, plain folder and git repo | **Not loaded**, with or without the compat switches |
| The same `AGENTS.md` with `--trust` | **Loaded**, under the full driver argv and switches, in a git repo and in a plain folder. `grok` records trusted folders in `~/.grok/trusted_folders.toml`; the driver never passes `--trust` |
| `~/.grok/rules/x.md` in the scratch `HOME` | **Loaded**, under `--system-prompt-override`, as a `<system-reminder>` user message naming the file |

The model's request in the rules probe held, in order: the system message (the override text
alone), a user message with `<user_info>` (OS, shell, work folder, date) and a built-in
`<user_rules>` line, the home rule as a `<system-reminder>`, a workflow reminder listing
`deep-research`, the task in `<user_query>`, and an MCP reminder naming the connected server.

### Required variables

| Probe | Exit | First line |
|---|---|---|
| `env -i grok version` | `0` | `grok 1.0.77 (50ca579fa996) [stable]` |
| `env -i HOME=<scratch> PATH=/usr/bin:/bin grok version` | `0` | `grok 1.0.77 (50ca579fa996)` |
| a mock turn with `PATH` dropped (`env -i HOME=<scratch> XAI_API_KEY=… GROK_XAI_API_BASE_URL=… <switches> grok …`) | `0` | the `system`/`init` line |
| a mock turn with `HOME` dropped | `0` | the `system`/`init` line |

The `HOME`-dropped turn ran as `unshare -rm sh -c 'mount -t tmpfs none ~/.grok && mount -t tmpfs
none /root && exec env -i PATH=/usr/bin:/bin XAI_API_KEY=xai-test GROK_XAI_API_BASE_URL=… <switches>
grok …'`, so whatever home `grok` fell back to was an empty folder and the operator's login could
not be read.

## The guest run

The compiled component under the installed `mur` 0.11.0 (commit `01054923`), on **`scoped`**, with
a scratch `HOME` for both `mur` and `grok`.

The capsule, `$G/capsule/murmur.yaml` with `G=/space/scratch/grok-101a/guest`:

```yaml
name: grok-guest
version: 0.1.0

inference:
  transport: process
  model: mock
  system_prompt: "- You are a Murmur capsule under test. Answer briefly."
  driver:
    artifact: local/driver-grok-build
    config:
      reasoning_effort: low
  command: /space/scratch/grok-101a/bin/grok

capabilities:
  env:
    allow: [HOME, PATH]
  harness:
    paths:
      - path: ~/.grok
        access: read-write
  shell:
    allow: [cat]
  network:
    allow: [api.x.ai, auth.x.ai, "127.0.0.1:28731"]

trace:
  capture: content

lifecycle:
  conversation: threaded

artifacts:
  - name: local/driver-grok-build
    runtime: driver
    version: "0.1.0"
  - name: local/tool-read
    runtime: tool
    version: "0.5.0"
```

and `$G/home/.grok/config.toml`, which points a custom model at the mock with a fake inline key:

```toml
[model.mock]
model = "mock-model"
base_url = "http://127.0.0.1:28731/v1"
api_backend = "responses"
api_key = "xai-test"
```

```bash
# install (the real toolchain homes, so rustup does not reinstall into the scratch HOME)
HOME=$G/home RUSTUP_HOME=~/.rustup CARGO_HOME=~/.cargo \
  WASI_SDK_CACHE=~/.cache/murmur-default-artifacts \
  ./scripts/local-install.sh driver-grok-build $G/capsule
HOME=$G/home RUSTUP_HOME=~/.rustup CARGO_HOME=~/.cargo \
  WASI_SDK_CACHE=~/.cache/murmur-default-artifacts \
  ./scripts/local-install.sh tool-read $G/capsule
mkdir -p $G/home/.grok $G/work && echo "hello from the guest run" > $G/work/hello.txt
printf 'CALL_TOOL=use_tool:murmur__tool-read TOOL_INPUT={"path":"hello.txt"}' > $G/task1.md
printf 'Reply with a plain sentence.' > $G/task2.md

cd $G/capsule
HOME=$G/home mur run --containment scoped --workdir $G/work --context grok-guest-final --task $G/task1.md
HOME=$G/home mur run --containment scoped --workdir $G/work --context grok-guest-final --task $G/task2.md
HOME=$G/home mur trace show ses_01a1cdf47cd87ec18ea48d0850e710c1
```

Both runs ended `status: ok`. From `trace.jsonl`:

| Record | First task | Second task |
|---|---|---|
| `harness_start` | `harness: grok`, `binary: /space/scratch/grok-101a/bin/grok`, `binary_source: inference.command`, `harness_version: grok 1.0.77 (50ca579fa996)`, `version_tested: true`, `session_mode: new`, `harness_session_id: 01a1cdf4-7ecd-7503-8952-40bc762b29aa`, `files: [task.md, capsule-agent.md]`, `bridge_tools: [cat, session-status, tool-read]`, `stdin_bytes: 0` | the same, `session_mode: resume`, the same `harness_session_id` |
| `harness_session` | `auth: api-key`, `model: mock`, then `W-SEC-031` | the same |
| `tool_call` | `tool_name: tool-read`, `input: {"path":"hello.txt"}`, `output` the fenced `{"content":"hello from the guest run\n","total_lines":1}`, `status: ok` | none |
| usage | `inference` turn 1: `input_tokens 200`, `output_tokens 30`, `cached_tokens 40`; `task_end` 200/30 | `inference` turn 0: 100/15, cached 20 |
| `harness_exit` | `code 0`, `cause terminal` | `code 0`, `cause terminal` |

No `E-RUN-032`, `E-CAP-019` or `E-CAP-021` appeared. `harness-session.json` under
`$G/home/.murmur/conversations/grok-guest/grok-guest-final/` holds the id `init` reported, and the
mock's third request (the second task) carried the first task's `function_call`,
`function_call_output` and `assistant` items: `-r` resumed it.

What it took to get there, each one measured:

- **A wrapper script as `command:` cannot `exec` grok.** The harness may execute its `command:`
  and its interpreter, and nothing a harness need grants, so `exec` of the grok binary from a
  `#!/bin/sh` wrapper failed with `Permission denied` (`E-RUN-033`, exit 126). The guest capsule
  therefore names the grok binary itself and gives the mock's URL and key through the scratch
  `config.toml` above, which also keeps `XAI_API_KEY` out of the capsule.
- **The mock needs a `host:port` allow entry.** The harness runs in the session's network
  namespace, where `127.0.0.1` is the namespace's own. A bare `127.0.0.1` entry opens only ports
  80 and 443; `"127.0.0.1:28731"` makes the egress proxy forward that port to the host's mock.
  Without it every request failed `tcp connect error: Connection refused` and `grok` retried
  until murmur interrupted it, which ended `canceled`/`cancelled` (exit by `SIGINT`, then
  `SIGKILL`).
- **The bridge needs the TLS trust store, which needs `shell.allow`.** With no `shell.allow`,
  `grok` logged `Error loading CA root certificate: … Permission denied (os error 13) at
  '/usr/lib/ssl/cert.pem'` and `MCP server init failed … Failed to build HTTP client: builder
  error`. The capsule tool call came back `tool_execution_failed: Tool not found: murmur__tool-read`
  (`tool_call` `status: error`), and the next turn's `init` gave the note `grok reports the
  capsule's tool bridge as failed`. A `/etc/ssl` harness need is refused by murmur (`lies under
  /etc, which the runtime replaces with its own copy in a composed root`). Adding `shell.allow:
  [cat]` made murmur grant the trust store: `Loaded 121 CA root certificates from the system`, and
  the call succeeded.
- **A bad config refuses the turn through the guest.** With `reasoning_effort: deep`:
  `error[E-RUN-034]: process driver 'local/driver-grok-build@0.1.0' failed in launch: driver:
  inference.driver.config 'reasoning_effort' must be one of none, minimal, low, medium, high,
  xhigh, max, got "deep"`.

`mur doctor` in the same capsule:

| `HOME` | Login check |
|---|---|
| a scratch `HOME` with an empty `~/.grok` | ``✗ error[E-RUN-064]: grok is not logged in for this user: grok is not signed in: run `grok login` as the user mur runs as`` |
| the scratch `HOME` above, whose `config.toml` gives `mock` an inline key | ``⚠ warning[W-RUN-013]: login not checked — `grok models` printed a first line this driver does not recognise`` (the line is `Model 'mock' is using its own API key.`) |
| the operator's own `HOME` (`mur doctor` with no `HOME` override; runs only `grok version` and `grok models`) | `✓ login logged in`, `✓ version grok 1.0.77 (50ca579fa996) [stable], tested` |

## Handoff: a live SuperGrok run

This card made no subscription turn. To check the driver on a real SuperGrok login:

**Prerequisites.** `grok` 1.0.77 on `PATH`, logged in with `grok login` as the user `mur` runs as,
so `grok models` prints `You are logged in with grok.com.` first. No `XAI_API_KEY` in the
shell (murmur would not pass it anyway). `mur` 0.11.0 or later. A checkout of this repo at the
branch that holds the driver.

**The capsule.**

```bash
mkdir -p ~/grok-supergrok/work && cd ~/grok-supergrok
cat > murmur.yaml <<'EOF'
name: grok-supergrok
version: 0.1.0

inference:
  transport: process
  model: ""
  system_prompt: "You are a Murmur capsule. Use the tools you are given when a task needs them."
  driver:
    artifact: local/driver-grok-build

capabilities:
  env:
    allow: [HOME, PATH]
  harness:
    paths:
      - path: ~/.grok
        access: read-write
  shell:
    allow: [cat]
  network:
    allow: [api.x.ai, auth.x.ai]

trace:
  capture: content

lifecycle:
  conversation: threaded

artifacts:
  - name: local/driver-grok-build
    runtime: driver
    version: "0.1.0"
  - name: local/tool-read
    runtime: tool
    version: "0.5.0"
EOF
echo "The secret word is plum." > work/notes.txt
echo "Use the tool-read tool to read notes.txt and tell me the secret word." > task1.md
echo "Without calling any tool, what was the secret word you read in the previous task? One word." > task2.md
echo "Read notes.txt again with tool-read, then tell me how many words it has." > task3.md
```

**Install and run**, from the repo checkout and then the capsule:

```bash
./scripts/local-install.sh driver-grok-build ~/grok-supergrok
./scripts/local-install.sh tool-read ~/grok-supergrok
cd ~/grok-supergrok
mur doctor
mur run --containment scoped --workdir ./work --context supergrok-1 --task task1.md
mur run --containment scoped --workdir ./work --context supergrok-1 --task task2.md
mur run --containment scoped --workdir ./work --context supergrok-1 --task task3.md
mur trace show @3
mur trace show @2
mur trace show @1
```

**The trace lines to expect.**

- `mur doctor`: `✓ version grok 1.0.77 …, tested` and `✓ login logged in` (its first line of
  `grok models` is `You are logged in with grok.com.`).
- `harness_start` with `harness: grok`, `binary` the resolved `grok` path, `binary_source` saying it
  was found on `PATH`, `version_tested: true`, `session_mode: new` for the first task and `resume`
  for the next two, all with the same `harness_session_id`.
- `harness_session` with `auth: subscription` and the model `grok` chose, and **no** `W-SEC-031`
  warning beside it: `apiKeySource` reads `oauth` on a grok.com login.
- `~/.murmur/conversations/grok-supergrok/supergrok-1/harness-session.json` holding that session id,
  resumed by the second and third tasks; the second task's answer is `plum` with no tool call.
- A `tool_call` with `tool_name: tool-read` and `status: ok` in the first and third tasks, under
  the bare name.
- `inference` records with input and output token counts for each turn, and `task_end` totals.

**How to spot a host the harness needed but the capsule did not allow.** murmur's resolver
answers a name outside `capabilities.network.allow` with DNS `REFUSED` and writes no trace record.
`grok` then fails or retries with `error sending request for url (https://<host>/…)`. Look in:

- the `E-RUN-033` message `mur run` prints, which carries `grok`'s `errors` text;
- `~/.grok/logs/unified.jsonl`, for `inference_retry`, `model catalog refresh failed` and any
  `error sending request for url` line, each naming the URL.

**What to report back.**

1. Every host `grok` tried that `api.x.ai` and `auth.x.ai` did not cover (from the places above),
   and whether the turn needed it. That list becomes `describe().harness-needs.hosts`.
2. The real 401 text, if you can trigger one (for example by `grok logout` in a scratch copy of
   `~/.grok`, or an expired login): the `errors[0]` of the `result` line. And the real quota text,
   if a turn is ever rate-limited: does it hold `rate_limit_exceeded`, `(429)` or `"http_status":
   429`?
3. Anything refused under `scoped`: an `E-CAP-*` at launch, a `Permission denied` in
   `~/.grok/logs/unified.jsonl`, or a `grok reports the capsule's tool bridge as …` note.
4. The `harness_session` `auth` and `model`, and the per-turn token counts, so the README's
   subscription reading is measured rather than inferred.
