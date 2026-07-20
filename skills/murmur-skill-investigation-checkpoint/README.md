# murmur-skill-investigation-checkpoint

The investigations convention for `checkpoints/decisions.json` — record and
reuse investigative verdicts (ruled-out / confirmed / uncertain) across a
session instead of re-deriving them.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`workdir/tools/<name>/skill.md` before the agent loop starts.
