# skill-create-manifest

Complete `murmur.yaml` schema reference — enables an agent to generate valid
capsule manifests from plain-language task descriptions.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`tools/<name>/skill.md` in the session's runtime directory
(`$MURMUR_RUNTIME_DIR`, read-only to the capsule) before the agent loop starts.

`skill.md` gives the paths a `mur` released after v0.6.0 uses:
`$MURMUR_RUNTIME_DIR/tools/<name>/skill.md` and `$MURMUR_RUNTIME_DIR/MURMUR.md`, or
`murmur-runtime/…` in a file tool. `mur` v0.6.0 and earlier put both in the capsule workdir,
which is the layout 0.1.0 of this skill describes.

0.2.0 also teaches the records layout of a `mur` released after v0.6.0:

| What | Where `skill.md` teaches it |
|---|---|
| Per-hook grants on a hook's own `artifacts:` entry, with `capabilities.filesystem.records: true` for a hook that writes a score or log (`hook-eval`, `hook-debug`, `hook-regression-verifier`) | §4.3a |
| The session's records directory, `~/.murmur/records/<workdir root>/<session-id>/` (`$MURMUR_RECORDS_DIR`): `trace.jsonl`, `blobs/`, `logs/`, `completion.json`, `hooks/` | §4.3a |
| The `E-MAN-003` refusals of a misplaced `records: true` | §4.3a |
| `mur run --task <file>` for a one-shot task | §5, §9 |
