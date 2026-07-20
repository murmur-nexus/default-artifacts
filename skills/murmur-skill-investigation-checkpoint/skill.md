# Investigation Checkpoint Convention — recording what you've ruled out

This skill teaches you an **additive, documentation-only convention** for the checkpoint file
`checkpoints/decisions.json`: a second top-level array, `investigations`, that records the verdicts
of investigative leads you've chased during a session (a hypothesis confirmed, a suspect ruled out,
a lead still uncertain) so that neither you-after-a-compaction nor a later agent re-treads ground
that's already been settled.

Call this skill when you are debugging, root-causing, or otherwise chasing a chain of hypotheses and
want to leave a durable-within-the-session trail of what you've checked and concluded. It is a
**prose convention, not a validated file format** — nothing in the murmur runtime parses
`decisions.json` as JSON (see §6), so the discipline below is enforced by you following it, not by a
schema checker. Read the whole skill before writing your first `investigations` entry, and read §7
(the compaction-survival limitation) so you don't mistake this for a persistence guarantee.

---

## 1. The file and its existing shape

`checkpoints/decisions.json` is one of three checkpoint files (`summary.md`, `plan.json`,
`decisions.json`) that the runtime HMAC-signs and verifies as **opaque byte blobs** — it never
parses their contents. Today it carries exactly one documented top-level array, `decisions`, holding
free-form key-decision/rationale pairs:

```json
{
  "decisions": [
    {
      "decision": "Adopt wasm32-wasip2 as the sole build target for default-artifacts",
      "rationale": "wasi:http and component model are only stable on the p2 target"
    }
  ]
}
```

This existing shape is untouched by this convention. Everything below **adds a sibling key** next to
`decisions`; it never modifies, wraps, or renames the `decisions` array or its entries.

---

## 2. The extension: an `investigations` array

The convention adds a second, optional, sibling top-level array named `investigations`. Each entry
records one investigative lead and its current verdict. The full shape becomes:

```json
{
  "decisions": [
    {
      "decision": "Adopt wasm32-wasip2 as the sole build target for default-artifacts",
      "rationale": "wasi:http and component model are only stable on the p2 target"
    }
  ],
  "investigations": [
    {
      "stable_id": "rust://murmur-core/manifest#validate(&Manifest->Result<(),ManifestError>)",
      "verdict": "ruled-out",
      "confidence": 0.9,
      "rationale": "Traced call path with murmur-tool-code-graph's impact_analysis; validate() has zero persistence_operations edges, so it cannot be the source of the corrupted-zip bug."
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
| `stable_id` | string | yes | The join key identifying the lead. When the lead is an indexed Rust symbol, this is **byte-identical** to `murmur-tool-code-graph`'s `symbol_id` (see §4). Otherwise it's a freeform token you mint once and reuse (see §4). |
| `verdict` | string enum | yes | One of `ruled-out`, `confirmed`, `uncertain` — nothing else. |
| `confidence` | number | yes | Your subjective 0.0–1.0 confidence in `verdict`. **Not** code-graph's categorical edge confidence — see §5. |
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
(see §6). It is *not* required to be unique per write — the opposite: reuse it so successive verdicts
for the same lead collapse onto one another.

**When the lead is an indexed Rust symbol**, reuse `murmur-tool-code-graph`'s `symbol_id` verbatim.
That identifier's format is:

```
rust://<package>/<module>#<qualified_name>(<signature_body>)
```

minted by `parse::make_symbol_id` in `tools/murmur-tool-code-graph/src/parse.rs` and stored as the
`symbols.symbol_id` column in `.murmur/code-graph.sqlite3`. Real examples of that exact format:

```
rust://fixture_crate/#add(i64,i64->i64)
rust://fixture_crate/#Widget::doubled(&self->i64)
rust://murmur-core/manifest#validate(&Manifest->Result<(),ManifestError>)
```

This is already the cross-tool join key in this codebase: `murmur-tool-test-report`'s
`resolve::resolve_stable_ids` resolves a failing test's `stable_id` by reading code-graph's
`symbols.symbol_id` column *verbatim, never recomputing it*. Your investigation entries must use the
same value so they line up with everything else keyed on symbol identity. Don't hand-type these — get
them from code-graph's output for the symbol in question.

**When the lead isn't tied to an indexed symbol** — a config value, an external service's observed
behavior, an untested hypothesis — mint a short freeform token instead and **reuse it consistently**
for that lead across the whole session. The token's job is to be a stable handle for the lead, so a
later entry can supersede this one; it is not required to be globally unique, only stable-per-lead.
Prefer a readable slug, e.g.:

```json
{
  "stable_id": "hypothesis:zip-central-directory-offset-overflow",
  "verdict": "uncertain",
  "confidence": 0.4,
  "rationale": "Corruption only reproduces on archives >4GiB, consistent with a 32-bit offset overflow, but I haven't confirmed which writer path emits the bad offset."
}
```

---

## 5. Naming collision — `confidence` here vs. code-graph's `confidence`

There are **two unrelated fields named `confidence` in this codebase**. Do not conflate them:

| | This skill's `investigations[].confidence` | code-graph's `edges.confidence` |
|---|---|---|
| Type | number, `0.0`–`1.0` | string enum |
| Values | any float in range, e.g. `0.9` | `definite` \| `possible` \| `heuristic` \| `unresolved` |
| Where | `checkpoints/decisions.json` (this convention) | `edges` table in `.murmur/code-graph.sqlite3`, assigned by `db::assign_confidence` in `tools/murmur-tool-code-graph/src/db.rs` |
| Meaning | *Your* subjective judgment in a verdict | Static-analysis certainty that a call edge is real |

`edges.confidence` is a property of an automatically-derived call-graph edge (how sure the parser is
that A calls B). This skill's `confidence` is a property of *your* reasoning about a lead. Never
write one of code-graph's enum strings (`definite`, etc.) into an `investigations` entry's
`confidence`, and never expect a numeric value there — they are different fields in different files
with different types.

---

## 6. Write / dedup convention

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
         "stable_id": "rust://murmur-core/manifest#validate(&Manifest->Result<(),ManifestError>)",
         "verdict": "ruled-out",
         "confidence": 0.95,
         "rationale": "Upgraded from uncertain: impact_analysis confirms zero persistence_operations edges out of validate(), so it cannot touch the zip writer."
       }
     ]
   }
   ```

3. **Distinct leads get distinct entries.** Two genuinely different leads keep two entries, even if
   related — dedup is per `stable_id`, not per topic.

---

## 7. Additivity — `investigations` is optional

`investigations` is an **optional sibling key**. A `decisions.json` that contains only the
`decisions` array, with **no** `investigations` key at all, is fully valid and unchanged by this
convention:

```json
{
  "decisions": [
    {
      "decision": "Sign checkpoint files as opaque bytes, not parsed JSON",
      "rationale": "Keeps signing schema-agnostic; the convention can evolve without touching checkpoint_sign.rs"
    }
  ]
}
```

The example above is valid **with no `investigations` key present** — that is the whole point of the
additive design. Any consumer that only knows the old shape simply never sees the new key; any
existing free-form `decisions` entry is untouched. There is no version bump, no migration, and no
parser change anywhere — this is additive **by construction**, because (as §1 notes) nothing in the
runtime reads this file's JSON structure at all: `checkpoint_sign.rs` signs and verifies all three
checkpoint files as opaque byte blobs.

---

## 8. Known limitation — compaction clobbers `decisions.json`

**Read this before relying on anything you write here surviving.** The compaction hook
(`hooks/murmur-hook-compact`) currently, on **every compaction**, unconditionally overwrites
`checkpoints/decisions.json` with the empty stub:

```json
{"decisions": []}
```

It does **not** read or merge the file's prior contents first — per its own source comment, the file
is a placeholder to be "populated by LLM-powered compaction once the runtime supports wasi:http in
hooks." The practical consequence: **anything you write to `decisions.json` — including
`investigations` entries under this convention — survives only until the next compaction fires, at
which point it is reset to `{"decisions": []}`.**

So treat this convention as a **within-a-session working memory** that you re-read right after each
compaction (which is also when the file was just cleared — meaning after a compaction you're starting
from an empty array again for now). Making these entries durable *across* compactions requires a
future change to `murmur-hook-compact` to read-merge-then-write instead of stubbing; that is
explicitly out of scope for the convention this skill documents. Don't mistake this skill's silence
elsewhere for a persistence guarantee — there isn't one yet.

---

## 9. Quick checklist

- [ ] At session start / post-compaction, read `checkpoints/decisions.json` and scan `investigations`.
- [ ] For each settled lead, write an entry with all four fields: `stable_id`, `verdict`, `confidence`, `rationale`.
- [ ] `verdict` is exactly one of `ruled-out` | `confirmed` | `uncertain`.
- [ ] `confidence` is a number `0.0`–`1.0` — never one of code-graph's enum strings (§5).
- [ ] `stable_id` reuses code-graph's `symbol_id` verbatim for indexed symbols; otherwise a stable freeform token reused per lead (§4).
- [ ] Overwrite the existing entry for a `stable_id` when its verdict changes — don't append a duplicate (§6).
- [ ] Keep the `decisions` array intact; `investigations` is a peer, and it's optional (§7).
- [ ] Remember it doesn't survive the next compaction yet (§8).
