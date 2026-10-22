# tool-code-coverage

Spectrum-based fault localization (Ochiai / Tarantula) over a Rust repository
already indexed by `murmur/tool-code-graph`. Input is a directory of per-test
LCOV `.info` reports the agent produced via its own `cargo llvm-cov --lcov`
shell calls — this tool never runs tests or coverage itself. Suspicion scores
are written onto the code-graph database's `symbols` table.

Native binary tool (`runtime: tool`, `implementation: native`) — it links the
same bundled-SQLite C source as `murmur/tool-code-graph`.

See [murmur.yaml](./murmur.yaml) for the full manifest.

## What the capsule must grant

On Linux, with `mur` 0.7.0 or later, the tool runs in the capsule's own sandbox, with the capsule-wide grants ([Native tools](https://docs.murmur.nexus/reference/containment/#native-tools)). A `capabilities:` block on the tool's entry in the capsule's `artifacts:` does not narrow them and prints `W-SEC-008`.

| Need | Capsule grant |
|---|---|
| Read the LCOV files and read and write the code-graph db | `repo_path` and `coverage_dir` inside the session workdir, and the index `murmur/tool-code-graph` built there |

`localize` was run confined under both `scoped` and `sealed` with no grant beyond the workdir. See [What each native tool needs](https://docs.murmur.nexus/reference/default-artifacts/#native-tool-needs).
