# murmur-hook-memory

Seeds a task with the relevant part of the conversation that came before it.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-task-start` · mode: blocking · commit policy: `seed-context`.

At task start the hook pages the runtime's durable conversation record through
`murmur:conversation/read`, renders the messages the model was actually shown,
selects a chronological slice that fits the task's seed budget, and returns it as
the head of the new task's context. Each seeded message's `source-id` is the
`msg_` id of the record message it came from, so a seeded turn can always be
traced back to the turn it was drawn from.

**The hook never writes anything.** It creates no file, holds no `filesystem`
grant, and appends nothing to the record — the runtime is the record's only
writer. The hook's whole job is deciding what to load.

It also has no dependency on any other artifact. `murmur-tool-corpus` in
particular is unrelated and need not be installed.

## What gets seeded

Every message the record holds, oldest first, minus the ones below.

| Dropped | Why |
|---|---|
| `system` messages | The capsule's system prompt is sent on every request anyway. |
| Messages that render to nothing but whitespace | Nothing to read. |
| Copies an earlier seed committed | A committed seed is appended to the record like any other message, carrying `source-id`. Seeding it again alongside the message it came from would show the model the same turn twice, and the run after that four times. |

`tool`-role messages are kept, but unwrapped out of the runtime's tool-result
envelope and re-roled to `user` — a tool result seeded on its own, with no
matching tool call ahead of it, is an error for every driver.

Roles in the seed are therefore only ever `user` and `assistant`.

## Configuration

Declare it on your capsule `murmur.yaml`:

```yaml
context:
  max_tokens: 120000
  seed_budget: 0.10

artifacts:
  - name: murmur-hook-memory
    version: 0.6.0
    runtime: hook
    capabilities:
      conversation:
        read: true
      task_io:
        read: true
```

| Key | Required | What it buys |
|---|---|---|
| `capabilities.conversation.read: true` | yes | The conversation record. Without it the hook can read nothing and fails loudly — see below. |
| `capabilities.task_io.read: true` | no | Relevance selection instead of recency: the hook scores each candidate message by how much of the task's wording it shares. Without it, or when the task is not yet in scope, the newest messages are seeded instead. |
| `context.max_tokens` | yes | The hook's seed budget is `context.max_tokens × context.seed_budget`. |
| `context.seed_budget` | no | Fraction of `max_tokens` a seed may occupy; defaults to `0.10`. |

Both capability keys are booleans and are never inferred: a `conversation:` block
that omits `read` fails staging with `E-MAN-003`.

No network capability is required — the hook makes no outbound calls.

**The grant belongs on your capsule manifest, on this hook's own `artifacts:`
entry.** Hook capabilities are per-hook and default-deny. A `capabilities:` block
inside this artifact's bundled `murmur.yaml` is inert and is never consulted for
enforcement — which is what stops a hook from widening its own access — and a
`conversation:` block in the capsule-wide `capabilities:` block reaches nothing
and prints `W-SEC-016`.

## When the hook seeds nothing

Three of these are ordinary and silent: the hook returns no output, and the
runtime writes no `context_seed` line to `trace.jsonl` at all.

| Situation | Why |
|---|---|
| The capsule declares no `context.max_tokens` | The task's `budget-tokens` arrives as `0`, meaning "not computed" rather than "unbounded". A seed proposed against it would be refused anyway, with `reason: "no_budget"`. |
| `lifecycle.conversation: threaded` | The runtime has already reloaded this conversation's history into the task. Seeding on top of it would duplicate it. |
| The record is empty | **Not an error.** The two ordinary causes are a first-ever run — nothing has been recorded yet — and a capsule running `inference.transport: process`, whose CLI owns its own conversation and puts no message list in front of the model, so no record is ever written. `context.record: off` does the same. |

The one loud case is a missing grant. Without
`capabilities.conversation.read: true` the hook still links and still runs, and
`read-messages` returns `not-granted` — which, left unhandled, would be
indistinguishable from an empty record. So the hook fails instead, with an error
naming itself, the missing key, and where the key belongs. Hook errors are
non-fatal: the session continues and the message is written to
`workdir/logs/hook-murmur-hook-memory.log`.

## Confirming it worked

The runtime writes one `context_seed` line to the session's `trace.jsonl` for
every seed it is offered, carrying `hook_name`, `tokens`, `proposed_tokens`,
`budget_tokens`, `message_ids`, and an `outcome` of `seeded`, `trimmed`,
`compacted` or `rejected` (with a `reason` on a rejection). A healthy run records
`seeded`: the hook measures its proposal against the same token counter the host
does and holds back a margin, so trimming is a backstop rather than the routine
path.

See [murmur.yaml](./murmur.yaml) for the full manifest.
