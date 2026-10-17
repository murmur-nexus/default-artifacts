# skill-investigation-checkpoint

The investigations convention for `checkpoints/decisions.json` — record and
reuse investigative verdicts (ruled-out / confirmed / uncertain) across a
session instead of re-deriving them.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`tools/<name>/skill.md` in the session's runtime directory
(`$MURMUR_RUNTIME_DIR`, read-only to the capsule) before the agent loop starts.
