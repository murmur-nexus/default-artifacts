# tool-create

Scaffolds a new tool or hook artifact directory with `murmur.yaml`, a stub
implementation, and a README.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`). Input arrives on the stdin envelope; the scaffold
is written under `tools/<name>/` in the capsule workdir.

See [murmur.yaml](./murmur.yaml) for the full manifest.

## Request

```json
{"type": "tool", "name": "csv-parser", "runtime": "wasm"}
```

| Field | Required | Value |
|---|---|---|
| `type` | yes | `tool` |
| `name` | yes | Directory name under `tools/`, and the artifact's `name:` |
| `runtime` | no (default `wasm`) | `wasm` or `hook` |

Any other `runtime` is rejected with
`unknown runtime '<value>'; expected 'wasm' or 'hook'` and nothing is written to
disk.

## What each arm generates

`{snake}` is `name` with `-` replaced by `_` — the cdylib filename cargo
produces.

| `runtime` | manifest `runtime:` | manifest `implementation:` | `requires_files:` | payload written |
|---|---|---|---|---|
| `wasm` | `tool` | `wasm` | `{snake}.wasm` | `component.wat` |
| `hook` | `hook` | `wasm` | `{snake}.wasm` | `Cargo.toml` + `src/lib.rs` |

`runtime:` is the artifact's role and `implementation:` is how its payload is
built. `mur` derives the published classification from the pair, so both are
always emitted — a manifest carrying only one of them publishes as the wrong
runtime and is rejected when a capsule manifest names it.

Every generated `murmur.yaml` declares `version: 0.1.0`, the new artifact's own
starting version. It is unrelated to this generator's version, which appears in
the generated README's attribution line.

`requires_files:` names the payload `mur build` packs beside `murmur.yaml`.
Without it the built `.mur.zip` would hold the manifest alone.

Both arms declare their payload before it exists — it is
produced by a cargo build you run afterwards. `mur build` requires the file to
be present, so build the component before packaging.

### Tool arm (`wasm`)

```yaml
name: csv-parser
version: 0.1.0
runtime: tool
implementation: wasm
description: |
  TODO: describe what csv-parser does
input_schema: |
  {"type":"object","properties":{}}
output_schema: |
  {"type":"object","properties":{}}
requires_files:
  - csv_parser.wasm
```

### Hook arm

A hook is dispatched with a lifecycle event rather than a tool payload, so no
schemas are emitted; it gains the dispatch fields instead.

```yaml
name: event-sink
version: 0.1.0
runtime: hook
implementation: wasm
execution_mode: async
commit_policy: none
description: |
  TODO: describe what event-sink does
requires_files:
  - event_sink.wasm
```

The generated `src/lib.rs` has four top-level items, in this order:

| Item | Holds |
|---|---|
| crate doc comment | the split, why it exists, and the reference hooks to read |
| `pub mod logic` | plain mirrors of every `murmur:hook@0.9.0` record, `Event`, `Decision`, and `decide` |
| `#[cfg(target_arch = "wasm32")] mod wasm_hook` | the WIT bindings, all nine `Guest` methods, the conversions, `export!` |
| `#[cfg(test)] mod tests` | one passing host test over `logic::decide` |

`logic::decide(&Event) -> Result<Decision, String>` is the single entry point all
nine dispatches route through, and mirrors the WIT `result<hook-output, string>`.
The `logic` module names no WIT type and carries no `cfg`, so `cargo test` on the
host compiles and runs it. The adapter converts and nothing else: every
`Guest` method builds an `Event`, calls `decide`, and maps the returned
`Decision` onto the `HookOutput` case of the same name.

`Guest` defaults none of its nine methods — `on_stage`, `on_session_start`,
`on_task_start`, `on_inference`, `on_tool_call`, `on_shell`, `on_compaction`,
`on_task_end`, `on_session_end` — so a stub short of any one does not compile.

`ToolEvent::outcome` and `ShellEvent::outcome` are mirrored as `Option`, and the
generated doc comments say what the two dispatches mean: `None` is the decision
point, the one dispatch at which a returned `Decision::Deny` is honoured;
`Some(..)` is the post-call observation, where nothing can be refused. Logic that
ignores the field runs twice per call.

The generated `Cargo.toml` declares `[dev-dependencies] tempfile = "3"`. It is
unused by the generated test on purpose: the usual hook side effect is appending
to a file under the workdir, and a host test for that needs a scratch directory,
so the section is there before the author needs it.

The generated README's checklist points step 2 at the `logic` module and step 3
at `cargo test -p <crate name>`, which passes before a line of behaviour is
written. See `BUILD.md`'s "Add one" for the convention this follows.
