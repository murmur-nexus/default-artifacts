# skill-create-manifest

Complete `murmur.yaml` schema reference — enables an agent to generate valid
capsule manifests from plain-language task descriptions.

Skill artifact (`runtime: skill`): no binary or WASM component, just the
[skill.md](./skill.md) guidance file, which the runtime installs to
`tools/<name>/skill.md` in the session's runtime directory
(`$MURMUR_RUNTIME_DIR`, read-only to the capsule) before the agent loop starts.
