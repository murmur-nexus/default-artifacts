# murmur-driver-anthropic

Anthropic Messages API inference driver for Murmur agent capsules.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the Anthropic
Messages API, including SSE streaming, extended-thinking blocks, and
model-family handling (Claude 3.x vs Claude 4+ naming and parameter rules).

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. See [murmur.yaml](./murmur.yaml) for the full manifest.
