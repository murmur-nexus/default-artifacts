# murmur-tool-request-input

Human-in-the-loop pause gate — suspends the agent loop and requests external
input via A2A. The runtime transitions the task to `input-required` and blocks
until a follow-up `message/send` delivers an answer, which is returned as the
tool result.

WASM tool component (`runtime: tool`, `implementation: wasm`, world `tool`,
exports `murmur:tool/run`).

See [murmur.yaml](./murmur.yaml) for the full manifest.
