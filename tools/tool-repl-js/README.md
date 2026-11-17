# tool-repl-js

## What it is

`tool-repl-js` gives a capsule's model code mode: one tool that runs JavaScript in a single QuickJS
context that lasts for the session. From that context a program lists the capsule's tools, calls
one or several at once, runs an allowlisted program, and prints. Only what it prints, plus the value
of its last expression, goes back to the model.

It is a WASM component (`wasm32-wasip2`), one zip for every platform. It is built against murmur's
`code-mode` world: it exports `murmur:tool/run@0.2.0` and imports `murmur:bus/bus@0.1.0` and
`murmur:process/process@0.2.0`. QuickJS is compiled into the component from its C sources, through
`rquickjs` with default features off.

## The manifest

```yaml
artifacts:
  - name: murmur/tool-repl-js
    version: 0.1.0
    runtime: tool
    config:
      output_limit_bytes: 16384   # optional; unset cuts nothing
code_mode:
  repl: murmur/tool-repl-js
  detach_after_secs: 120          # optional
  tools_visible: false            # optional
```

The artifact must be the one `code_mode.repl` names. murmur refuses at launch, with `E-CAP-022`,
any artifact importing `murmur:bus` that `code_mode.repl` does not name.

murmur names every WASM tool after its artifact, so the model calls this tool `tool-repl-js`. Its
input is `{"code": "<JavaScript source>"}`.

## What a call does

`code` runs as a global script (not a module) in the session's one context, so top-level `var`,
function and class declarations, `let`/`const` and `globalThis` properties one call defines are
there in the next. Top-level `await` is a `SyntaxError`, and declaring a top-level `let`, `const` or
class name a second time throws. Promise jobs a call queues run before the call returns.

| Outcome | Status | Text |
|---|---|---|
| The script ran | `passed` | What `print` wrote, then `=> <value>` of the last expression on its own final line, unless the value is `undefined`. No newline follows the value line. With nothing printed and an `undefined` value, the text is empty |
| An `Error` was thrown | `failed` | `Uncaught <name>: <message>`, a newline, the error's stack, then what was printed before the throw |
| Any other value was thrown | `failed` | `Uncaught <value>`, a newline, then what was printed before the throw |
| The input is not an object with a string `code`, or the config is invalid | `error` | A line starting `tool-repl-js: ` that names the problem. No JavaScript runs |

A thrown exception leaves the context and its values as they were.

How a value reads, in `print` and on the `=> ` line:

| Value | Text |
|---|---|
| string | as is in `print`; in JSON quotes on the `=> ` line |
| `undefined` | `undefined` |
| number, boolean, `null` | what `JSON.stringify` writes (`NaN` and `Infinity` read `null`) |
| bigint | `<digits>n` |
| symbol | `Symbol(<description>)` |
| function | `[function <name>]`, or `[function]` when it has no name |
| `Error` | `<name>: <message>` |
| any other object or array | compact `JSON.stringify(v)`, or `String(v)` when that throws or returns `undefined` |

For example, `print("before"); function f() { throw new Error("boom") } f()` returns, with status
`failed`:

```text
Uncaught Error: boom
    at f (eval_script:1:43)
    at <eval> (eval_script:1:58)
before
```

## The globals

The globals and their shapes are this artifact's contract. Every global returns its value
directly, never a `Promise`. Each call sets them again, so a program that replaced one gets it back
on its next call.

| Global | Returns | Throws |
|---|---|---|
| `tools()` | `[{name, description, inputSchema}]`, one per operation a program may call. `inputSchema` is the parsed schema, or its raw text when it does not parse | An `Error` with `kind: "refused"` and the bus's text when the bus refuses to list |
| `call(name, input)` | `{status, summary, data, dataPath, truncated, metadata, attachments}`: `status` is `"passed"`, `"failed"` or `"error"`; `summary`, `data` and `dataPath` are `null` when absent; `metadata` is an object (the first value of a repeated key); `attachments` is `[{mediaType, path}]` | See below |
| `callAll([[name, input], ...])` | One entry per pair, in input order: the envelope `call` would return, or the `Error` `call` would throw, unthrown. `callAll([])` returns `[]` and calls nothing | A `TypeError`, before any call, when `pairs` is not an array of `[name, input?]` arrays or a pair fails `call`'s checks |
| `run(program, args, options)` | `{exitCode, timedOut, stdout, stdoutTruncated, stderr, stderrTruncated}`, with stdout and stderr decoded as UTF-8 (an invalid byte reads U+FFFD) | See below |
| `print(...values)` | `undefined`. Appends the values, separated by spaces, and a newline to this call's text | Nothing |

`call(name, input)`:

- `name` must be a string. `input` defaults to `{}` and is sent as `JSON.stringify(input)`. A
  non-string name, or an input that does not serialize to JSON (a function, a symbol, a bigint, a
  cycle), throws a `TypeError` and calls nothing.
- `data` is the text the model would receive for that call, inside its
  `<untrusted-content source=tool:NAME>` fence; `summary` is empty for a fenced result.
- It throws an `Error` whose message is the bus's text, with `kind`:

| `kind` | When |
|---|---|
| `"refused"` | The call did not run: the name is not an operation this program may call (`'<name>' is not an operation this program may call; list() returns every operation it may call`), or a rule refused it |
| `"failed"` | The call ran and failed in the runtime |
| `"error"` | The tool returned status `error`. The envelope is on `error.result`; the message is its `data`, else its `summary`, else `tool returned status error` |

`run(program, args, options)`:

- `program` is a string. `args` defaults to `[]` and must be an array of strings. `options` may
  hold only `env` (an object of string values), `stdin` (a string, sent as UTF-8 bytes) and
  `timeoutMs` (a non-negative integer). Any other shape throws a `TypeError` and runs nothing.
- It keeps the first 1 MiB of each stream.
- It throws an `Error` with `kind`:

| `kind` | Also on the error | Message |
|---|---|---|
| `"refused"` | `rule`, such as `capabilities.shell.allow`, `on-shell` or `capabilities.filesystem.read_only` | `<rule>: <reason>` |
| `"unknown-handle"` | | `unknown handle` |
| `"not-running"` | | `the process is not running` |
| `"timed-out"` | | `the program timed out` |
| `"failed"` | | The runtime's text |

## The config key

| Key | Value | Effect |
|---|---|---|
| `output_limit_bytes` | An integer from 1 to 4294967295 | The text is cut to its longest prefix of at most that many bytes ending on a UTF-8 boundary, followed on its own line by `[output cut: <total> bytes, the first <kept> shown]`, and the result is marked `truncated`. The mark does not count toward the limit |

Unset, the tool cuts nothing; murmur has no byte bound of its own on a WASM tool's result beyond
its 256 MiB worker frame. Any other key, or a value outside the range, makes every call return
status `error` naming the key and the rule.

## What the capsule must grant

| For | Grant |
|---|---|
| `tools`, `call`, `callAll` | Nothing. Each bus call is checked against the grants and hooks of the tool it names, exactly as the model's own call to that tool would be |
| `run` | The program in `capabilities.shell.allow`. Each entry there is also a shell tool, which `tools()` lists |

The bus answers only while the model's own call runs in the foreground on
`inference.transport: http`.

## How long the context lives

The instance starts at the model's first call to the tool and lives for the session. It runs one
call at a time. It ends, losing every value, on any of:

- the session's end;
- `background-command stop`;
- a trap;
- `capabilities.limits.deadline_seconds`;
- a memory or table cap;
- a cancel.

There is no poll tool. With `code_mode.detach_after_secs` set, murmur's `background-command`
reports on a detached call, waits for it, or stops it. The trace records
`code_mode_instance_start`, `code_mode_instance_end` and `code_mode_detach`, and each bus call as a
`tool_call` with `origin: "code_mode"` and the `repl_call_id` of the model's call.

## What is not there

- No modules or module loader, and no QuickJS `std` or `os` module.
- No npm packages and no TypeScript.
- No file or network objects: a program reaches files and the network only through the tools the
  capsule grants.

## WIT level

The component imports `murmur:bus@0.1.0` and `murmur:process@0.2.0`, so it needs a `mur` whose
`mur version` reports WIT level 8 or higher. Level 8 is murmur `d95c68b4` (#423); the first murmur
release to serve it is the release after v0.11.0, which is not tagged yet. An older `mur` refuses
the capsule with `E-WIT-LEVEL-001`.

## Tests

```bash
cargo test -p tool-repl-js
```

runs the engine against a fake host (`tests/engine.rs`): state across calls, every rendering rule,
the output formats, the cut, the config and input checks, and each global's results, `TypeError`s
and `Error.kind`s; and checks the crate depends on `rquickjs`, `wit-bindgen` and the std-only
`murmur-process-host` only.

One suite is ignored by default, because it drives a real `mur`:

```bash
# Real `mur run` through a fake provider on loopback, with tool-read as the fixture tool: state
# across calls, tools(), call, callAll, run, the cut, an exception, and the code-mode trace records.
MUR_BIN=<mur at WIT level 8> cargo test -p tool-repl-js --test mur_run_repl -- --ignored --nocapture
```
