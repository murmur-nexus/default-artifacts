# skill-create-manifest

Complete `murmur.yaml` schema reference — enables an agent to generate valid
capsule manifests from plain-language task descriptions.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`tools/<name>/skill.md` in the session's runtime directory
(`$MURMUR_RUNTIME_DIR`, read-only to the capsule) before the agent loop starts.

`skill.md` gives the paths `mur` 0.7.0 or later uses:
`$MURMUR_RUNTIME_DIR/tools/<name>/skill.md` and `$MURMUR_RUNTIME_DIR/MURMUR.md`, or
`murmur-runtime/…` in a file tool. `mur` v0.6.0 and earlier put both in the capsule workdir,
which is the layout 0.1.0 of this skill describes.

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
| A catalog row for each of the 27 default artifacts, with what its entry needs | §10 |
| Pins that each equal the pinned artifact's current version | §3, §7, §8, §10 |
