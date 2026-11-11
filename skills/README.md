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
    version: "0.16.0"
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
