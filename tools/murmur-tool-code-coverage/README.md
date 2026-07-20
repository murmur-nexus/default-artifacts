# murmur-tool-code-coverage

Spectrum-based fault localization (Ochiai / Tarantula) over a Rust repository
already indexed by `murmur-tool-code-graph`. Input is a directory of per-test
LCOV `.info` reports the agent produced via its own `cargo llvm-cov --lcov`
shell calls — this tool never runs tests or coverage itself. Suspicion scores
are written onto the code-graph database's `symbols` table.

Native binary tool (`runtime: tool`, `implementation: native`) — it links the
same bundled-SQLite C source as `murmur-tool-code-graph`.

See [murmur.yaml](./murmur.yaml) for the full manifest.
