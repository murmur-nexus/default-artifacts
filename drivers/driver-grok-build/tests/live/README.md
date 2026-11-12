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

The fixture was recorded on 2026-11-12, after the agent file's body became the system prompt,
with `S=/space/scratch/sp101a`, the mock on 28731 and the bridge on 28732 (token
`rec12-bridge-token`):

```bash
cargo build -q -p driver-grok-build --example write_plan
./target/debug/examples/write_plan \
  $S/rec12/files $S/rec12/plan.json murmur http://127.0.0.1:28732/mcp rec12-bridge-token \
  echo_tool,session-status,image_tool new 019a7c3e-5b10-7a4c-9e21-3f6d2b8a1c05 \
  '{"reasoning_effort":"low"}' - '- You are a Murmur capsule. Answer briefly.' \
  'CALL_TOOL=use_tool:murmur__image_tool'
python3 -I $L/run_plan.py $S/rec12/plan.json /space/scratch/grok-101a/bin/grok $S/rec12/home \
  /usr/bin:/bin http://127.0.0.1:28731/v1 $S/rec12/work $S/rec12/12-bridge-image-result
```

The first recording, on 2026-11-11 under `/space/scratch/grok-101a/rec12`, ran the same plan with
the agent body a fixed line, `You are a Murmur capsule.`. On a new session the override replaces
the body, and the two streams are the same line for line once `uuid`, `cwd`, message ids and
durations are set aside.

The plan's argv, with `{files_dir}` replaced:

```
--prompt-file /space/scratch/sp101a/rec12/files/task.md
--output-format streaming-messages-json
--include-partial-messages
--agent /space/scratch/sp101a/rec12/files/capsule-agent.md
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
- You are a Murmur capsule. Answer briefly.
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

## The system prompt on a resumed turn

Measured on 2026-11-12 with `grok` 1.0.77, each case a new session (`-s`) then a resume (`-r`) of
the same id in the same work folder, the driver's flags and 18 switches, and the mock logging every
request body (`FULLBODY`). The scripts, kept outside the repo under `/space/scratch/sp101a/scripts/`,
are `turn.sh` (one turn from `env -i` with a scratch `HOME`, given an agent file, an override
text and extra args) and `two_turns.sh` (`examples/write_plan` for a new and a resume plan, each run
by `run_plan.py`). "Custom model" means a scratch `~/.grok/config.toml` holding:

```toml
[model.mock]
model = "mock-model"
base_url = "http://127.0.0.1:28941/v1"
api_backend = "responses"
api_key = "xai-test"
```

and `-m mock`. "Built-in model" means `grok-4.6` through `GROK_XAI_API_BASE_URL`, with no `-m` or
with `-m grok-4.6`.

| Case | Override | Agent body | New session's system message | Resumed session's system message |
|---|---|---|---|---|
| built-in, no `-m` (`runs/b`) | `PROMPT-LINE-A override only.` | `PROMPT-LINE-B body only.` | the override alone (28 chars) | the override alone (28 chars) |
| built-in, `-m grok-4.6` (`runs/d`) | the same | the same | the override alone | the override alone |
| custom, `-m mock` (`runs/c`) | the same | the same | the override alone | **`grok`'s own prompt ("You are Grok released by xAI. …"), then the body** (3,452 chars); the override is absent |
| custom, the driver's plan (`runs/e`): a prompt starting `---`, holding `tools: read_file, run_command`, `mcpServers: []` and a later `---` line | the prompt | the prompt | the prompt alone, byte for byte | `grok`'s own prompt, then the prompt byte for byte at the end |
| built-in, the driver's plan (`runs/g`) | `- CAPSULE-PROMPT for a built-in model.` | the same | the prompt alone | the prompt alone, once |
| custom, the driver's plan with an empty prompt (`runs/f`) | empty | empty | `grok`'s own prompt | `grok`'s own prompt |

In every case `init.tools` was `search_tool`, `use_tool` and, with the driver's plan, `init` listed
the `murmur` server and the MCP reminder said `murmur (3 tools)`: a body holding `---` lines and
frontmatter-looking keys leaves the real frontmatter in force. The session's `prompt_context.json`
records `"prompt_mode": "extend"` and the body as `prompt_body`, and its `system_prompt_label` is
`Grok 4.6` on the built-in model and `Grok` on the custom one.

So the driver writes the system prompt as the agent file's body as well as passing the override:
the model gets the capsule's prompt on every turn, alone wherever the override holds, and after
`grok`'s own prompt on a resumed custom-model session. The review of this card also measured
`--system-prompt=<p>` (the same as the override on resume) and agent frontmatter
`prompt_mode: replace`, `full` and `override` (no effect); none removes `grok`'s prompt there.

### Side model calls

`batch.sh base 20` ran 20 two-turn custom-model sessions under the driver's switches and counted
the mock's requests: every session made exactly three, a forced-tool `session_title` call on the new
session and one model request per turn. None made a title refresh or a "dashboard line" call. The
review of this card saw both, in 2 of about 10 runs. `GROK_TITLE_REFRESH=false` and
`GROK_SESSION_RECAP=false` were not added to the driver's switches: with no extra call to suppress,
no effect could be measured.

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

### The guest run again, with the prompt in the agent body

Repeated on 2026-11-12 after the agent file's body became the system prompt, with the same capsule
and `config.toml` in a fresh `G=/space/scratch/sp101a/guest` (the mock on 28731, its log
`/space/scratch/sp101a/logs12/mock.log`), on **`scoped`**:

```bash
/space/scratch/sp101a/scripts/install.sh      # the two local-install.sh commands above, HOME=$G/home
cd $G/capsule
HOME=$G/home mur run --containment scoped --workdir $G/work --context grok-guest-fix --task $G/task1.md
HOME=$G/home mur run --containment scoped --workdir $G/work --context grok-guest-fix --task $G/task2.md
HOME=$G/home mur trace show ses_01a1cfc988797ab188f010088ed40d96
HOME=$G/home-empty mur doctor     # $G/home-empty/.grok is empty
mur doctor                        # the operator's HOME: grok version and grok models only
```

| Record | First task (`ses_01a1cfc980377303bb524c78d75d3fb0`) | Second task (`ses_01a1cfc988797ab188f010088ed40d96`) |
|---|---|---|
| status | `ok` | `ok` |
| `harness_start` | `harness: grok`, `binary: /space/scratch/grok-101a/bin/grok`, `version_tested: true`, `session_mode: new`, `harness_session_id: 01a1cfc9-829b-7c63-ab36-a9af7db194a7`, `files: [task.md, capsule-agent.md]`, `system_prompt_sha256: 3dc471456c53…` | the same with `session_mode: resume`, the same id and the same `system_prompt_sha256` |
| `harness_session` | `auth: api-key`, `model: mock`, `W-SEC-031` | the same |
| `tool_call` | `tool_name: tool-read`, `input: {"path":"hello.txt"}`, `status: ok` | none |
| usage | `inference` turn 1: 200/30, cached 40; `task_end` 200/30 | `inference` turn 0: 100/15, cached 20 |

`harness-session.json` holds `01a1cfc9-829b-7c63-ab36-a9af7db194a7`, the id `init` reported. No
`E-RUN-032`, `E-RUN-036`, `E-CAP-019` or `E-CAP-021` appeared.

The mock received four requests: a `session_title` side call, the tool turn, the turn after the
tool result, and the second task. **The system message of the second task's request is 4,131
characters: `grok`'s own prompt, then the 703 characters of murmur's prompt** (sha256 prefix
`3dc471456c53`, the `system_prompt_sha256` murmur traced), ending the message and appearing once.
murmur's prompt includes the capsule's `- You are a Murmur capsule under test. Answer briefly.`
The first task's requests carried murmur's prompt alone. Before this change the second task's
system message was `grok`'s own prompt alone, with the capsule prompt absent.

`mur doctor`: `✗ error[E-RUN-064]: grok is not logged in for this user: grok is not signed in: run
\`grok login\` as the user mur runs as` with the empty scratch `HOME`, and `✓ login logged in`,
`✓ version grok 1.0.77 (50ca579fa996) [stable], tested` with the operator's.

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
- After the second task, `system_prompt.txt` in that session's folder under
  `~/.grok/sessions/<url-encoded work folder>/<session id>/` holds murmur's prompt for the capsule
  (it starts `Work folder:` and includes the manifest's `system_prompt`), not `grok`'s own `You are
  Grok released by xAI. …`. That is what a built-in model keeps on a resume.

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
5. The first line of the resumed session's `system_prompt.txt` after the second task: murmur's
   prompt, or `grok`'s own. If it is `grok`'s own, the model `grok` chose behaves like a custom
   model on resume, and the capsule's prompt reaches it only after `grok`'s.
