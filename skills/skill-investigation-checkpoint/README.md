# skill-investigation-checkpoint

The investigations convention for `checkpoints/decisions.json` — record and
reuse investigative verdicts (ruled-out / confirmed / uncertain) across a
session instead of re-deriving them.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`tools/<name>/skill.md` in the session's runtime directory
(`$MURMUR_RUNTIME_DIR`, read-only to the capsule) before the agent loop starts.

`skill.md` points the agent at `$MURMUR_RUNTIME_DIR/MURMUR.md` (`murmur-runtime/MURMUR.md` in a
file tool), where `mur` 0.7.0 or later writes it. On `mur` v0.6.0 and earlier the file is in
the capsule workdir, which is the layout 0.2.0 of this skill describes.
