---
name: skill-investigation-checkpoint
description: "Record and reuse the verdicts of a debugging or root-cause investigation in checkpoints/decisions.json: an investigations array where each lead has a stable_id, a verdict (ruled-out, confirmed or uncertain), a 0.0–1.0 confidence and a rationale, so you, or another agent after a context compaction or handoff, do not re-check leads already settled. Use when chasing a chain of hypotheses, ruling out suspects, or resuming an investigation."
metadata:
  version: "0.6.0"
---
# Investigation Checkpoint Convention — recording what you've ruled out

This skill is for any agent that debugs, root-causes or otherwise investigates a problem over a
long session: Claude Code, Cursor, an agent in a Murmur capsule, or any other agent that reads
Agent Skills. It teaches you an **additive, documentation-only convention** for the file
`checkpoints/decisions.json`: a top-level array, `investigations`, that records the verdicts
of investigative leads you've chased during a session (a hypothesis confirmed, a suspect ruled out,
a lead still uncertain) so that neither you-after-a-compaction nor a later agent re-treads ground
that's already been settled.

Call this skill when you are debugging, root-causing, or otherwise chasing a chain of hypotheses and
want to leave a durable-within-the-session trail of what you've checked and concluded. It is a
**prose convention, not a validated file format** — no tool parses `decisions.json` as JSON (see
§1), so the discipline below is enforced by you following it, not by a schema checker. Read the
whole skill before writing your first `investigations` entry, and read §7 (what a compaction does
to the file) before relying on an entry after one.

---

## 1. The file and its existing shape

`checkpoints/decisions.json` is a file in your working folder that this convention has you create.
**You write it and you read it.** The file has no tooling: nothing creates `checkpoints/`, and
nothing validates, parses, reads or overwrites the file except you and any other agent following
this convention. The file is a JSON object. This convention owns its `investigations` key and
leaves every other top-level key as it found it, such as a free-form `decisions` array of
`{decision, rationale}` pairs:

```json
{
  "decisions": [
    {
      "decision": "Store every timestamp in UTC",
      "rationale": "The service runs in three regions, and local times made the nightly reports disagree"
    }
  ]
}
```

Such a key is untouched by this convention. Everything below **adds a sibling key** next to it; it
never modifies, wraps, or renames the `decisions` array or its entries.

---

## 2. The extension: an `investigations` array

The convention adds an optional, sibling top-level array named `investigations`. Each entry
records one investigative lead and its current verdict. The full shape becomes:

```json
{
  "decisions": [
    {
      "decision": "Store every timestamp in UTC",
      "rationale": "The service runs in three regions, and local times made the nightly reports disagree"
    }
  ],
  "investigations": [
    {
      "stable_id": "src/manifest.rs::validate",
      "verdict": "ruled-out",
      "confidence": 0.9,
      "rationale": "Traced every call path out of validate(): none writes to disk, so it cannot be the source of the corrupted-zip bug."
    }
  ]
}
```

`decisions` and `investigations` are **peers**: both are top-level keys of the same JSON object,
each an array of objects. Neither nests inside the other.

---

## 3. Field reference for an `investigations` entry

Every field below is **required** by this convention. (Because nothing parses the file, "required"
is a discipline you uphold, not an error a tool raises — but write all four every time, or a later
reader can't act on the entry.)

| Field | Type | Required | Meaning |
|---|---|---|---|
| `stable_id` | string | yes | The join key identifying the lead. When the lead is a code symbol your code index identifies, this is **byte-identical** to that index's identifier for it (see §4). Otherwise it's a freeform token you mint once and reuse (see §4). |
| `verdict` | string enum | yes | One of `ruled-out`, `confirmed`, `uncertain` — nothing else. |
| `confidence` | number | yes | Your subjective 0.0–1.0 confidence in `verdict`. Always a number, never a category word such as one a code-analysis tool reports for its own results. |
| `rationale` | string | yes | Prose explaining how you reached the verdict — same name and spirit as a `decisions` entry's `rationale`. Cite the evidence (a tool you ran, a call path you traced, a test you read). |

`verdict` enum values:

| Value | Use when |
|---|---|
| `ruled-out` | You have evidence this lead is *not* the cause / not the answer. Recording this is the highest-value entry — it stops re-investigation. |
| `confirmed` | You have evidence this lead *is* the cause / is the answer. |
| `uncertain` | You investigated but the evidence is inconclusive; a later agent may pick it back up with fresh evidence. |

---

## 4. Sourcing `stable_id`

`stable_id` is the **join key** that lets a later entry supersede an earlier one for the same lead
(see §5). It is *not* required to be unique per write — the opposite: reuse it so successive verdicts
for the same lead collapse onto one another.

**When the lead is a code symbol**, and your code index (a symbol or call graph, a language server,
a tags file) gives that symbol a stable identifier, reuse that identifier **verbatim**. Copy it from
the index's output for the symbol in question; never hand-type or recompute it. Your investigation
entries then line up with everything else keyed on that symbol's identity.

**When the lead isn't tied to an indexed symbol** — a config value, an external service's observed
behavior, an untested hypothesis, or a symbol in a codebase you have no index for — mint a short
freeform token instead and **reuse it consistently** for that lead across the whole session. The
token's job is to be a stable handle for the lead, so a later entry can supersede this one; it is not
required to be globally unique, only stable-per-lead. Prefer a readable slug, e.g.:

```json
{
  "stable_id": "hypothesis:zip-central-directory-offset-overflow",
  "verdict": "uncertain",
  "confidence": 0.4,
  "rationale": "Corruption only reproduces on archives >4GiB, consistent with a 32-bit offset overflow, but I haven't confirmed which writer path emits the bad offset."
}
```

---

## 5. Write / dedup convention

1. **Read before you re-tread.** At the **start of a session, and again right after any compaction**,
   read `checkpoints/decisions.json` and scan its `investigations` array. If a lead you're about to
   chase already has a `ruled-out` or `confirmed` entry, don't re-investigate it from scratch —
   build on the recorded verdict instead.

2. **Overwrite, don't append, for the same lead.** When new evidence *changes* the verdict for a
   lead, **replace the existing entry that has that `stable_id`** rather than appending a second
   entry for it. One `stable_id` → at most one entry in the array. The join key exists precisely so a
   later verdict can supersede an earlier one for the same lead.

   Example — an earlier `uncertain` becomes `ruled-out` after tracing the call path. The single entry
   for that `stable_id` is rewritten in place (not duplicated):

   ```json
   {
     "investigations": [
       {
         "stable_id": "src/manifest.rs::validate",
         "verdict": "ruled-out",
         "confidence": 0.95,
         "rationale": "Upgraded from uncertain: every caller and callee of validate() is read-only, so it cannot touch the zip writer."
       }
     ]
   }
   ```

3. **Distinct leads get distinct entries.** Two genuinely different leads keep two entries, even if
   related — dedup is per `stable_id`, not per topic.

---

## 6. Additivity — `investigations` is optional

`investigations` is an **optional sibling key**. A `decisions.json` that contains only the
`decisions` array, with **no** `investigations` key at all, is fully valid and unchanged by this
convention:

```json
{
  "decisions": [
    {
      "decision": "Keep the release version only in the top-level VERSION file",
      "rationale": "Every build step reads it from there, so a second copy elsewhere could only disagree with what ships"
    }
  ]
}
```

The example above is valid **with no `investigations` key present** — that is the whole point of the
additive design. A reader that does not know `investigations` simply never sees the key; any
existing free-form `decisions` entry is untouched. There is no version bump, no migration, and no
parser change anywhere — this is additive **by construction**, because (as §1 notes) no tool reads
this file's JSON structure at all.

---

## 7. What a compaction does to the file

A compaction, where your agent replaces the conversation so far with a summary to free context,
replaces the conversation, not files. What you wrote to `checkpoints/decisions.json` is still in the
file after a compaction unless something in the session changed it, such as a command you ran or
another agent writing the same path.

The compaction summary that replaces the conversation may not mention the file or its entries. The
file, not your memory of the conversation, holds the verdicts you recorded: right after a
compaction, re-read `checkpoints/decisions.json` and act on what it actually holds.

---

## 8. Quick checklist

- [ ] At session start / post-compaction, read `checkpoints/decisions.json` and scan `investigations`.
- [ ] For each settled lead, write an entry with all four fields: `stable_id`, `verdict`, `confidence`, `rationale`.
- [ ] `verdict` is exactly one of `ruled-out` | `confirmed` | `uncertain`.
- [ ] `confidence` is a number `0.0`–`1.0` — never a category word (§3).
- [ ] `stable_id` reuses your code index's identifier verbatim for an indexed symbol; otherwise a stable freeform token reused per lead (§4).
- [ ] Overwrite the existing entry for a `stable_id` when its verdict changes — don't append a duplicate (§5).
- [ ] Keep the `decisions` array intact; `investigations` is a peer, and it's optional (§6).
- [ ] After a compaction, re-read the file: it holds what you wrote, and the summary may not mention it (§7).

---

## In a Murmur capsule

This section applies only when this skill runs inside a Murmur capsule; an agent anywhere else can
skip it.

**Nothing in the runtime touches the file.** The Murmur runtime does not create `checkpoints/`, and
no runtime component reads or writes `checkpoints/decisions.json`. A Murmur compaction replaces the
conversation only, so §7 holds as written.

**`stable_id` for a symbol indexed by `tool-code-graph`.** When the capsule has `tool-code-graph`
and the lead is a Rust or Python symbol it indexed, the code index of §4 is `tool-code-graph`, and
the identifier to reuse verbatim is its `symbol_id`. Its format is:

```
<scheme>://<package>/<module>#<qualified_name>(<signature_body>)
```

where `<scheme>` is `rust` or `python`. `tool-code-graph` stores it as the `symbols.symbol_id`
column in `<repo_path>/.murmur/code-graph.db`. Real examples of that exact format:

```
rust://fixture_crate/#add(i64,i64->i64)
rust://fixture_crate/#Widget::doubled(&self->i64)
rust://murmur-core/manifest#validate(&Manifest->Result<(),ManifestError>)
```

This is already the join key between Murmur's tools: `tool-test-report` resolves a failing test's
`stable_id` by reading code-graph's `symbols.symbol_id` column *verbatim, never recomputing it*.
Use the same value, so your entries line up with its failure lists. Don't hand-type these — copy
them from `tool-code-graph`'s output for the symbol in question.

**Two unrelated fields named `confidence`.** `tool-code-graph` has its own `confidence`, on its
call-graph edges. Do not conflate the two:

| | This skill's `investigations[].confidence` | code-graph's `edges.confidence` |
|---|---|---|
| Type | number, `0.0`–`1.0` | string enum |
| Values | any float in range, e.g. `0.9` | `definite` \| `possible` \| `heuristic` \| `unresolved` |
| Where | `checkpoints/decisions.json` (this convention) | `edges` table in `<repo_path>/.murmur/code-graph.db` |
| Meaning | *Your* subjective judgment in a verdict | Static-analysis certainty that a call edge is real |

`edges.confidence` is a property of an automatically-derived call-graph edge (how sure the parser is
that A calls B). This skill's `confidence` is a property of *your* reasoning about a lead. Never
write one of code-graph's enum strings (`definite`, etc.) into an `investigations` entry's
`confidence`, and never expect a numeric value there — they are different fields in different files
with different types.
