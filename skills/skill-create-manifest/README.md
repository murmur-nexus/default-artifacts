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
