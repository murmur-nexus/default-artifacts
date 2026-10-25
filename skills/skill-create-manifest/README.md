# skill-create-manifest

Complete `murmur.yaml` schema reference — enables an agent to generate valid
capsule manifests from plain-language task descriptions.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`tools/<name>/skill.md` in the session's runtime directory, read-only to the
capsule, before the agent loop starts.

MURMUR.md is in the same directory. Its `- Runtime directory:` entry gives the
directory's path and the name each of the session's tools opens it by.

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
