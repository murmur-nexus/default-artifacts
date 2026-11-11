# tool-transit-board-relay

A capsule's way onto a Transit board. The tool is a thin relay to the board tool that
`nexus transit serve` answers: it knows the envelope and never the verbs. Each call sends the
model's input, this entry's grant, the capsule's name and the session id to serve in one POST,
and hands serve's answer back unchanged. nexus checks the grant, runs the verb and records the
change under the capsule's name.

The relay holds no verb list and no board logic, so a new verb needs a nexus release and no
relay release. It holds no key either: the runtime attaches it.

WASM component (`wasm32-wasip2`), exporting `murmur:tool/run` and importing no `murmur:*`
interface. It needs a `mur` with the per-entry credential gateway and `upstream_auth:`, which
the murmur commit this repository pins has.

## The manifest entry

```yaml
artifacts:
  - name: murmur/tool-transit-board-relay
    version: "0.1.0"
    runtime: tool
    gateway:
      endpoint: http://127.0.0.1:<port of nexus transit serve>
      api_key: ${NEXUS_TRANSIT_TOOL_KEY}
    config:
      board: /abs/path/to/the/project
      verbs: [ls, show, add, dep]
      protect: [Plan, Design, Done]
```

| Field | Value |
|---|---|
| `gateway.endpoint` | `http://127.0.0.1:<port>` with no path: the address `nexus transit serve --port <p>` prints |
| `gateway.api_key` | `${NEXUS_TRANSIT_TOOL_KEY}`, resolved from `credentials.NEXUS_TRANSIT_TOOL_KEY` |
| `config.board` | The board's project folder, an absolute path |
| `config.verbs` | The verbs this capsule may run, or the groups `read`, `act` and `author` |
| `config.protect` | Stages no move by this capsule may land in |

The grant lives in this entry's `config:` block and nowhere else. The relay forwards it
uninspected and nexus checks it on every call, so a grant nexus refuses comes back as a
`bad-grant` answer.

## The key

serve keeps its key in `~/.nexus/transit/tool.key` on the host running it: 64 hex digits, mode
0600. Store it once:

```bash
mur config set -g credentials.NEXUS_TRANSIT_TOOL_KEY "$(cat ~/.nexus/transit/tool.key)"
```

The bundled `murmur.yaml` declares how serve takes the key:

```yaml
upstream_auth:
  header: Authorization
  value: "Bearer {key}"
```

The runtime gives the relay one address, `MURMUR_GATEWAY_ENDPOINT`, which carries no key. It
readdresses the relay's request to `gateway.endpoint`, removes any `Authorization` header and
attaches exactly one, rendered from the key. The credential is re-read while the capsule runs,
so a key replaced with `mur config set -g` is used on the next call.

The relay reads exactly four variables, all set by the runtime:

| Variable | Sent as |
|---|---|
| `MURMUR_GATEWAY_ENDPOINT` | The address the call goes to, plus `/v1/call` |
| `MURMUR_ARTIFACT_CONFIG` | `config`; `null` when the entry has no `config:` |
| `MURMUR_CAPSULE_NAME` | `capsule`: the actor nexus records a change under |
| `MURMUR_SESSION_ID` | `session` |

## Calling it

The model sends `{"verb": "...", "reason"?: "...", ...the verb's fields}`. The tool's
description tells it to call `{"verb": "help"}` first. `help` lists only the verbs the grant
holds, each with its fields, and the protected stages. Every verb that changes the board needs
`reason`, recorded with the change.

The input is forwarded as is. An object encoded twice as a JSON string is unwrapped once; any
other JSON value goes to nexus, which answers it.

## What the model gets back

serve's answer body, byte for byte:

```json
{"ok": true, "verb": "...", "effect": "read", "result": ..., "output": ..., "logged": [...]}
{"ok": false, "verb": "...", "effect": "none", "error": {"code": "...", "message": "..."}}
```

| Field | Value |
|---|---|
| `status` | `passed` when `ok` is `true`, `failed` when it is `false` |
| `data` | The answer body, unchanged |
| `summary` | `transit <verb>: ok (<effect>)`, or `transit <verb>: refused <code>: <message>` with the message on one line, cut to 300 characters |
| `metadata` | `state_effect: read` or `state_effect: mutate` from the answer's `effect`; nothing for `none`. Never `resource_id` |

A refused verb changed nothing. The runtime fences the text as untrusted content; the relay
passes it through unaltered, an `</untrusted-content>` inside it included.

## Relay lines

When the call never reached the board, the result is one plain line starting
`tool-transit-board-relay: `, as both `summary` and `data`, with no metadata. `<msg>` is serve's
`error.message`, else the body cut to 300 characters, else `(no body)`, always on one line.

| Case | `status` | Line, after the prefix |
|---|---|---|
| No gateway | `error` | `this entry has no gateway, so there is no board tool to call: add gateway: { endpoint: http://127.0.0.1:<port>, api_key: ${NEXUS_TRANSIT_TOOL_KEY} } to the murmur/tool-transit-board-relay entry in the capsule manifest, with the port nexus transit serve printed` |
| No capsule name or session id | `error` | `the runtime set no <VAR>, so the call cannot say which capsule and session made it; run this tool in a capsule launched by mur run` |
| `config:` not JSON | `error` | `MURMUR_ARTIFACT_CONFIG is not JSON (<error>); the runtime writes this entry's config: block there` |
| Input not JSON | `failed` | `the input is not JSON; send an object with "verb", such as {"verb": "help"}, which lists the verbs this capsule holds and the fields each takes` |
| `401` | `error` | `nexus transit serve refused this entry's key (401): store the key from ~/.nexus/transit/tool.key on the host running serve with mur config set -g credentials.NEXUS_TRANSIT_TOOL_KEY "$(cat ~/.nexus/transit/tool.key)" and name it on this entry as gateway.api_key: ${NEXUS_TRANSIT_TOOL_KEY}; the next call uses the stored key` |
| `400` | `error` | `nexus transit serve refused the envelope (400): <msg>` |
| `404` | `error` | `nothing answers <path> at the gateway (404): gateway.endpoint must be http://127.0.0.1:<port> with no path, the address nexus transit serve printed; serve said: <msg>` |
| `405` | `error` | `the gateway refused POST <path> (405): <msg>` |
| `413` | `error` | `the call is larger than nexus transit serve accepts (413): <msg>; send shorter fields` |
| `503` | `error` | `nexus transit serve is shutting down (503); call again once it is serving` |
| Any other non-2xx, `3xx` included | `error` | `the gateway answered HTTP <status>: <msg>` |
| No answer | `error` | `no answer through the gateway (<reason>); check that nexus transit serve is running on the port this entry's gateway.endpoint names` |
| 2xx that is not an answer | `error` | `the gateway answered <status> with a body that is not a board tool answer, an object with a boolean "ok": <body>` |

Every check runs before the request, so a refused call sends nothing. Each call sends at most
one request and never retries.

## Cost

murmur brokers the call but does not meter it. Every launch warns `W-SEC-030` for this entry.

## Development

| Item | Location |
|---|---|
| Transport | `wasm_tool` in `src/lib.rs`: one blocking POST over `wasi:http`, and the crate's only four environment reads |
| The envelope, URL and headers | `src/envelope.rs` |
| Judging a reply, and every relay line | `src/answer.rs` |
| The call, in order | `src/ops.rs` |

A new verb needs no change here. To map a new serve status, add an arm to
`answer::http_refused`, a row to the table above, and a case to `tests/relay_ops.rs`.

```bash
cargo test -p tool-transit-board-relay
MUR_BIN=/path/to/mur cargo test -p tool-transit-board-relay --test mur_run_gateway -- --ignored
```
