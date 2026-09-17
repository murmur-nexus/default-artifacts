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

Every message the record holds, oldest first, minus the ones below and minus
any role `config.seed_roles` leaves out.

| Dropped | Why |
|---|---|
| `system` messages | The capsule's system prompt is sent on every request anyway. |
| Messages that render to nothing but whitespace | Nothing to read. |
| Copies an earlier seed committed | A committed seed is appended to the record like any other message, carrying `source-id`. Seeding it again alongside the message it came from would show the model the same turn twice, and the run after that four times. |

`tool`-role messages are kept, but unwrapped out of the runtime's tool-result
envelope and re-roled to `user` — a tool result seeded on its own, with no
matching tool call ahead of it, is an error for every driver.

Every other role — `developer`, or any role the record holds that is not
`system` or `assistant` — is re-roled to `user` the same way. The seed only ever
holds `user` and `assistant` messages, but without `seed_roles` a `user` seed
message may have been a tool result or any other non-assistant role. Set
`seed_roles: [user, assistant]` to make a seeded `user` turn mean a user turn.

The role filter reads each record message's own role, after copies an earlier
seed committed are dropped and before re-roling. The order is what keeps a tool
result out: an earlier unfiltered seed's `user`-roled copy of it is recognised as
a copy while its `tool` original is still in the walk.

**Known limit.** A `user`-roled copy that an earlier *unfiltered* seed committed,
whose original has since fallen outside the 2,000-message scan window, cannot be
told apart from a user turn by its role alone, so `seed_roles: [user, assistant]`
still seeds it. Excluding it needs provenance the record does not yet carry.

## Configuration

Declare it on your capsule `murmur.yaml`:

```yaml
context:
  max_tokens: 120000
  seed_budget: 0.10

artifacts:
  - name: murmur-hook-memory
    version: 0.8.0
    runtime: hook
    config:
      seed_roles: [user, assistant]
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
| `config.seed_roles` | no | Which record roles may be seeded. See below. |

Both capability keys are booleans and are never inferred: a `conversation:` block
that omits `read` fails staging with `E-MAN-003`.

No network capability is required — the hook makes no outbound calls.

**The grant belongs on your capsule manifest, on this hook's own `artifacts:`
entry.** Hook capabilities are per-hook and default-deny. A `capabilities:` block
inside this artifact's bundled `murmur.yaml` is inert and is never consulted for
enforcement — which is what stops a hook from widening its own access — and a
`conversation:` block in the capsule-wide `capabilities:` block reaches nothing
and prints `W-SEC-016`.

### `config.seed_roles`

`seed_roles` is the only key the hook reads from `config:`, and it is optional.

| Value | Effect |
|---|---|
| unset, or no `config:` block | Every role except `system` is seeded; non-`assistant` roles arrive as `user`. |
| a list of role names, such as `[user, assistant]` | Only record messages whose role is on the list are seeded. |

Role names match the record's role exactly and case-sensitively. Any string is
accepted, so `[user, assistant, developer]` admits `developer` messages, and
`tool` re-admits tool results (still unwrapped and seeded as `user`). A
misspelled role can only narrow the seed, never widen it. Listing a role twice
is harmless.

The block is read fail-closed: anything the hook cannot fully honour is an
error, and the hook seeds nothing rather than falling back to the unfiltered
default.

| Written | Error |
|---|---|
| Any key other than `seed_roles`, such as `seed_role` | Names the key and says `seed_roles` is the only key accepted. |
| `seed_roles` that is not a list | `config.seed_roles` must be a list of role names. |
| `seed_roles: []` | Would seed nothing; list a role or remove the hook. |
| An entry that is not a string, or is empty or blank | Every entry must be a non-empty role name. |
| `system` in the list | `system` messages are never seeded. |

Every error names `murmur-hook-memory` and lands in
`workdir/logs/hook-murmur-hook-memory.log`; the session continues unseeded.

## When the hook seeds nothing

Three of these are ordinary and silent: the hook returns no output, and the
runtime writes no `context_seed` line to `trace.jsonl` at all.

| Situation | Why |
|---|---|
| The capsule declares no `context.max_tokens` | The task's `budget-tokens` arrives as `0`, meaning "not computed" rather than "unbounded". A seed proposed against it would be refused anyway, with `reason: "no_budget"`. |
| `lifecycle.conversation: threaded` | The runtime has already reloaded this conversation's history into the task. Seeding on top of it would duplicate it. |
| The record is empty | **Not an error.** The two ordinary causes are a first-ever run — nothing has been recorded yet — and a capsule running `inference.transport: process`, whose CLI owns its own conversation and puts no message list in front of the model, so no record is ever written. `context.record: off` does the same. |

Two cases are loud. The first is a `config:` block the hook cannot honour — see
[`config.seed_roles`](#configseed_roles). The second is a missing grant. Without
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
