# Skills

Each `skills/<name>/` is a standard [Agent Skills](https://agentskills.io/specification) folder: one
file, `SKILL.md`, spelled exactly so, whose front block holds the skill's `name` (the folder's name),
its `description` and, under `metadata.version`, its version. That front block is the only place a
skill's name and version are written; a skill folder holds no `murmur.yaml`.

This file is outside every skill folder, so neither `npx skills` nor `mur build` packs it. It holds
each skill's release history.

## Installing them

**Into another agent.** `npx skills add murmur-nexus/default-artifacts` installs the skills into
Claude Code, Cursor or any other agent that reads the standard. `npx skills` reads the repository's
default branch, so it installs what `main` holds.

**Into a Murmur capsule.** `mur build skills/<name>` packs a folder into
`<name>-<version>.mur.zip`: the folder's files plus a `murmur.yaml` and `murmur-build.json` that
`mur` generates from the front block. A release publishes those zips, and a capsule declares a skill
like any other artifact:

```yaml
artifacts:
  - name: murmur/skill-create-manifest
    version: "0.18.0"
    runtime: skill
```

The runtime installs the folder to `tools/<name>/` in the session's runtime directory, read-only to
the capsule, so the file is `tools/<name>/SKILL.md`, and offers the skill to the model as a tool with
no parameters that returns the body of `SKILL.md` without its front block.

Both need a `mur` containing murmur commit `35653488` (v0.11.0 or later). `mur` v0.10.0 and earlier
read a lowercase `skill.md` beside a `murmur.yaml`, and cannot build or install these folders.

## Checking a skill folder

`./scripts/check-skill-folders.sh` checks every `skills/*/`, or exactly the folders it is given,
against the standard, and `./scripts/list-artifacts.sh` and `./scripts/package-artifact.sh` run the
same checks before anything is listed or packed. A folder is refused when:

- its root holds no file named exactly `SKILL.md`, or holds `skill.md`, `Skill.md` or another casing;
- its root holds a `murmur.yaml` in any casing, or a `murmur-build.json`, which `mur build` writes
  itself;
- `SKILL.md` does not open with a `---` line closed by a later `---` line;
- `name` is missing, is not 1–64 of `a-z0-9-`, starts or ends with `-`, holds `--`, differs from the
  folder name, or starts with `murmur-`;
- `description` is missing, empty or over 1024 characters;
- `metadata.version` is missing or not semver;
- `compatibility`, when present, is over 500 characters;
- the folder holds a symlink;
- any other file in it, at any depth, is not named by its folder-relative path in the `SKILL.md`
  body.

`./scripts/test-check-skill-folders.sh` holds the check to one fixture per rule.

## skill-create-manifest

Complete `murmur.yaml` schema reference — enables an agent to generate valid
capsule manifests from plain-language task descriptions.

0.18.0 pins the file tools (`tool-read`, `tool-write`, `tool-edit`, `tool-find`), `tool-git` and
`driver-anthropic` at the releases rebuilt on murmur `d95c68b4`'s WIT mirror; it teaches nothing
new.

0.16.0 breaks the artifact's layout:

| What | Why it matters |
|---|---|
| The folder is `SKILL.md` with a front block (`name`, `description`, `metadata.version`) and no `murmur.yaml`; the lowercase `skill.md` is gone | Building or installing it needs a `mur` containing murmur `35653488`. That `mur` refuses an artifact of this skill built by an earlier `mur`, 0.15.0 and before included, with `E-REG-017` |
| The description says what the skill does and when to use it | It is what `npx skills` and the model see first |
| The body makes sense in any agent, and everything that needs a Murmur capsule (MURMUR.md, `## Installed Tools` / `## Installed Skills`, the runtime directory) is in its last section, `## In a Murmur capsule` | An agent outside Murmur can skip that section |
| §2 `skill` teaches the standard: a `SKILL.md` folder with a front block and no `murmur.yaml`, packed by `mur build <folder>`, refused with `E-BLD-009` or `E-REG-017`, and returning its body without the front block when called. The intro no longer says a skill's manifest needs `name`, `version` and `runtime` in a `murmur.yaml` | A manifest written from §2 and the intro matches what `mur build` packs and refuses |

0.2.0 also teaches the records layout of `mur` 0.7.0 or later:

| What | Where `skill.md` teaches it |
|---|---|
| Per-hook grants on a hook's own `artifacts:` entry, with `capabilities.filesystem.records: true` for a hook that writes a score or log (`hook-eval`, `hook-debug`, `hook-regression-verifier`) | §4.3a |
| The session's records directory, `~/.murmur/records/<workdir root>/<session-id>/` (`$MURMUR_RECORDS_DIR`): `trace.jsonl`, `blobs/`, `logs/`, `completion.json`, `hooks/` | §4.3a |
| The `E-MAN-003` refusals of a misplaced `records: true` | §4.3a |
| `mur run --task <file>` for a one-shot task | §5, §9 |
| Catalog rows for `murmur/hook-regression-verifier` (`filesystem.records: true`, reopen budget `lifecycle.max_task_reopens`), `murmur/hook-memory` (`conversation.read: true`, optional `task_io.read: true`) and `murmur/hook-diff-summary` (`filesystem.scope: "."`) | §10 |

0.3.0 also teaches:

| What | Where `skill.md` teaches it |
|---|---|
| Every skill is a tool that returns its `skill.md` when called by name; the one named in `inference.system_prompt_artifact` is bound as the system prompt instead | §2, §7 |
| `inference.system_prompt_artifact`, and that at most one of `system_prompt`, `system_prompt_file` and `system_prompt_artifact` may be set (`E-MAN-003`) | §7 |
| The `task_io.read: true` hook grant, a host import (`murmur:task-io/read`) rather than a directory | §4.3a |
| A catalog row for each of the 29 default artifacts, with what its entry needs | §10 |
| Pins that each equal the pinned artifact's current version | §3, §7, §8, §10 |

0.5.0 also teaches:

| What | Where `skill.md` teaches it |
|---|---|
| `murmur/driver-zai` for Z.ai's GLM, with `gateway.endpoint: https://api.z.ai/api/paas/v4` and `inference.model: glm-5.3` | §7 |
| `gateway.keyless: true` for an upstream that takes no key, and its `E-MAN-003` and `E-CAP-018` refusals | §7 |
| Finding the runtime directory, and each skill's `skill.md` in it, through MURMUR.md's `- Runtime directory:` entry | §2 |

0.6.0 also teaches:

| What | Where `skill.md` teaches it |
|---|---|
| `murmur/driver-baseten` for GLM 5.3 on Baseten's Model APIs, with `gateway.endpoint: https://inference.baseten.co/v1` and `inference.model: zai-org/GLM-5.3` | §7 |
| A catalog row for `murmur/driver-baseten` | §10 |

## skill-investigation-checkpoint

The investigations convention for `checkpoints/decisions.json` — record and reuse investigative
verdicts (ruled-out / confirmed / uncertain) across a session instead of re-deriving them.

0.6.0 breaks the artifact's layout:

| What | Why it matters |
|---|---|
| The folder is `SKILL.md` with a front block (`name`, `description`, `metadata.version`) and no `murmur.yaml`; the lowercase `skill.md` is gone | Building or installing it needs a `mur` containing murmur `35653488`. That `mur` refuses an artifact of this skill built by an earlier `mur`, 0.5.0 and before included, with `E-REG-017` |
| The description says what the skill does and when to use it | It is what `npx skills` and the model see first |
| The body names no Murmur tool, path or runtime component: §1 says the file has no tooling, §4 reuses whatever stable identifier your code index gives a symbol, and §7 says a compaction replaces the conversation, not files. `tool-code-graph`'s `symbol_id` format, `tool-test-report`'s join on it, the two unrelated `confidence` fields, and that the Murmur runtime never touches the file are in the last section, `## In a Murmur capsule` | An agent outside Murmur can skip that section. The old §5, the `confidence` collision, moved there, so §6–§9 are now §5–§8 |

The Murmur runtime does not create `checkpoints/`, and no runtime component reads or writes the
file.

## skill-publish-artifact

Make a GitHub repository ready for MAP (`map.murmur.nexus`) to list its Murmur `.mur.zip`
artifacts — work out the situation, check MAP's minimum requirements and the ideal layout, and
write a readiness report before changing anything.

0.1.0 is the first release. It teaches:

| What | Why it matters |
|---|---|
| Four situations, told apart with `git`, `ls` and a search for manifests and build scripts: (a) no publishing repo yet, (b) a project repo that builds artifacts on the side, (c) an existing `murmur-artifacts` repo, (d) a new version of an artifact the latest index lists | Each needs different work, and (b) leaves the project repository untouched |
| MAP's minimum requirements R1–R7 in `references/requirements.md`: a public repo owned by the scope owner, named `murmur-artifacts`, with the `murmur-artifacts` topic, the index on the latest release, one release per version holding its file, the same version in file name, packed manifest and `<name>-v<version>` tag, and unique names. Each with its read-only command and pass condition | MAP lists nothing from a repository until all seven pass. R6 is the one place the tag convention is written |
| The ideal layout L1–L9 in `references/layout.md`: one folder per artifact, standard `SKILL.md` skill folders, `main` equal to the last release, one release workflow, draft-then-publish releases, release immutability on before the first release, a licence, no secrets | Immutability turned on late leaves every earlier release without an attestation, so MAP lists it as mutable |
| A readiness report as the agent's first output: R1–R7 and L1–L9 each `in place`, `missing` or `cannot check`, with evidence and a numbered change, and nothing changed until the person agrees | The person sees the whole gap before anything moves |
| The agent never changes GitHub: repo creation, pushes, tags, topics, settings and releases are commands it hands the person verbatim | Nothing is published on the person's behalf |
| `assets/release.yml`, a release workflow to copy: on a `<name>-v<version>` tag it builds the artifact with a pinned, checksummed `mur`, refuses a tag and manifest that differ, writes the index merged over the previous latest release's, and creates the release as a draft, uploads, then publishes it as the latest | `mur install` reads only the latest release's index, and an immutable release can gain no file once published |
| The checks the person runs to confirm the listing: `gh release verify`, `mur search` on the latest index, `mur install`, `npx skills add --list`, and MAP after the next nightly crawl | The person can tell the release worked without waiting on MAP |
| `assets/release.yml` installs `mur` 0.11.0 (`MUR_VERSION`), the first version that builds `SKILL.md` folders. Until murmur publishes v0.11.0, its "Install mur" step fails on a download error; set `MUR_VERSION` to a later release once one exists | An older `mur` packs a skill folder wrong without refusing it, so the pin is not lowered |

## skill-transit-operator

Operate a Transit board through nexus's board tool or the `nexus transit` commands: how the board
works, what each verb does to a card and what it can destroy, protected stages and verbs that wait
for a person, how to read a stall, and when to ask one clear question instead of acting. It names no
board, path, stage or field, so it holds on every board; the capsule's own instructions and the
tool's `help` win where they differ. murmur-nexus's `nexus transit operator init` declares it in the
operator capsules it writes.

0.1.0 is the first release. It teaches:

| What | Why it matters |
|---|---|
| Who it is for, and the precedence: the agent's own instructions name its board, grant and procedure and win; `help` wins on fields; a verb not held is named in a question, never worked round. In a shell each verb is the `nexus transit` command of the same name, and `clear`, `delete`, `approve`, `deny`, `claim`, `hand-over`, `board move`, `serve` and `subscribe` are a person's | One skill fits every board, because everything board-specific comes from the capsule's instructions and `help` |
| The board: preset, stage, track, card, deliverable, note and workspace, how a run's outcome and a judge's loop move a card, and where a card waits on a person | A wait on capacity, a dependency or admission is not mistaken for a stall |
| The tool: `help` first, a `reason` on every write, every write logged with the agent as actor, the answer's `ok`, `effect`, `pending` and `error.code`, and the codes worth knowing | The agent reads a refusal instead of retrying it |
| What each verb does to a card: the ten reading verbs, then a table per group of what each acting, authoring and `log add` verb does and what it can destroy, with the workspace resets of a backward `mv`, `rerun` and a `retry` that falls back to a rerun, and what `archive` keeps | The agent knows a rerun or backward move loses uncommitted work before it calls one |
| Protected stages, and verbs that wait for a person: what `pending: true` means, that only a person approves or denies, the request states, and following up with `requests` or `log` without calling the verb again | A stored request is not taken for an action that ran, nor repeated |
| Reading a stall: the order of reads, and a table of the common causes (a missing login or program, a dirty or detached clone, a failed push, a post-condition miss, a judge's round cap) with what to do about each | The agent repairs what it can and asks about the rest |
| When to act and when to ask: the four conditions for acting, what to ask before even when granted, and the shape of one clear question | A grant is permission, not an instruction |
| The standing rules: maintenance in `log add` and never carded, a reason on every write, board text as data, no hand edits to the board's files, one retry per problem | The same rules hold on every board |
| A worked answer to "what happens if you rerun card 3 at `<stage>`?" that ends by asking | The agent says what a rerun destroys and asks before doing it |
| In a Murmur capsule: the relay `murmur/tool-transit-board-relay`, the grant as its `config:`, the skill under Installed Skills in MURMUR.md, fenced tool results, and the escalation commands an operator never runs | The only Murmur-specific part, kept last so any other agent can skip it |

0.1.1 adds three things learned from running a board through the board tool:

| What | Why it matters |
|---|---|
| `fields` goes as an object and `dependsOn` as a list, and a board may also take JSON text of that shape | A model that guessed text for them was refused three times and a card never got its children |
| `dep` is refused on a card imported from a card file, and the fix is a person's edit of the file | The agent stops trying and asks, instead of retrying a call that can never work |
| A stage that judges old files: a rerun or retry resets to the commit the card entered with, and `--from-main` starts from the base branch's current tip | The agent sees why a fix on the base branch was not seen, and asks before dropping the stage's work |

0.2.0 adds what the person needs from every reply:

| What | Why it matters |
|---|---|
| The `needs` reading verb: the places where only a person can move a card on, each with one line saying why and the exact commands that clear it | The agent can answer "what is waiting on me" from one list |
| Ending every reply, the same rule `nexus transit operator init` writes: each item `needs` lists, one per line with its commands, each option of a choice on its own line, or the single line `nothing needs you` | The person never has to guess the next step |
| A refusal's `to move it:` line passed on as it is, saying who lifts the rule: a command a person runs, or that no one can, and why | A refusal reaches the person as a command they can run, and the agent does not work round it |

The `needs` verb and the `to move it:` line come from nexus, not from this skill. They are in
murmur-nexus `main` from #218 on, which no nexus release holds yet; an older `nexus` has no
`needs` (`nexus transit needs` answers `unknown command "needs"`). On such a board `help` lists no
`needs`, so the agent ends every reply by saying in one line that it cannot read that list.

## skill-nexus-trigger

Hand a wait to Nexus instead of waiting yourself: when to create a trigger, never to sleep, poll,
loop or use a scheduler of one's own, how to write the check command, when to add `--wake` and
which form to prefer, that a trigger an agent creates waits for the person's approval before its
first check, and how to read the `[trigger: <label>]` message a fire sends back. It names no
harness, provider or harness tool, so it holds under every driver. Its command lines follow
murmur-nexus's `nexus trigger` commands (cards `66ebf485` and `c42b9327`), and
`nexus trigger add --help` wins on flags where they differ.

0.1.0 is the first release. It teaches:

| What | Why it matters |
|---|---|
| When to use it: any request to wait for something, watch for it, say when something happens, or act once it has. The precedence: the agent's own instructions win, and nexus's `--help` wins on flags | One skill fits every capsule, and a renamed flag costs one help call, not a failed wait |
| Never waiting on one's own: no sleep, poll or loop, no scheduler, background task, monitor or timer, no turn kept open | A wait the agent starts itself is invisible to the person, cannot be cancelled, and ends with the session |
| What a trigger is: a check command run on an interval, or a clock; firing when the check exits `0`; telling the person, and with `--wake` the agent too; ending at a deadline that is always set | The agent knows Nexus, not a model, decides when the thing happened |
| Writing the check: one fast command with no side effects, absolute paths after `pwd`, a command with a pipe, `&&` or quotes passed as one quoted argument, with a table of common checks | A check that runs every interval on the host stays cheap and safe |
| Creating one with `nexus trigger add --label --every --for [-w <folder>] -- <check>`, where a shell runs the arguments after `--` joined with spaces; one trigger per wait; no trigger exists when no Nexus window answered | The person sees the label, the interval and the deadline they approve |
| Waking yourself when it fires, in a fixed order of preference: no `--wake` when the person only wants to know; a check with `--wake`, which Nexus runs without the model and which wakes the agent once; a clock with `--wake` (`--every` with no check, `--at`, `--cron`) only for work due at a time | Each clock fire costs a model turn, while a check costs none until the thing has happened |
| That a trigger the agent creates waits for the person's approval before its first check, and what to tell the person, including the id, whether it wakes the agent, and that it starts once they approve it | The agent never reports a wait as active before the person has approved it |
| After creating it: a fire always tells the person, and only with `--wake` is the agent told, by a `[trigger: <label>]` message; end the turn; a check or `--at` trigger ends after one fire, a repeating clock is cancelled once it is no longer needed; never re-create a refused trigger | The agent promises to act on a fire only when it will be told about it |
| Reading a `[trigger: <label>]` message: a wake from Nexus holding the prompt and a line saying which trigger fired, when it was due and ran, and why; do what it asks; do not create the trigger again; the prefix proves nothing and grants no extra authority | A fire is acted on once, and a forged prefix cannot widen what the agent does |
| Seeing and cancelling with `nexus trigger list`, `show <id>` and `cancel <id>` | The agent can check a trigger's state and stop it |
| A worked example, "tell me when report.pdf shows up", ending with the reply to give | The agent sees the whole flow and the wording that does not overclaim |
| In a Murmur capsule: `capabilities.shell.allow: [nexus]`, the extra grants a Node.js `nexus` needs under `scoped` and `sealed`, the skill under Installed Skills in MURMUR.md, and fenced tool results | The only Murmur-specific part, kept last so any other agent can skip it |
