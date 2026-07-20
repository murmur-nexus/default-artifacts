# murmur-driver-openai

OpenAI inference driver for Murmur agent capsules — Chat Completions, with the
Responses API for `gpt-5` and later models.

WASM component (`runtime: driver`, world `driver`, exports `murmur:tool/run`).
Translates between the Murmur canonical inference format and the OpenAI API,
including SSE streaming. Optional stateful continuation via
`previous_response_id` is gated behind an explicit `inference.driver.config`
store grant.

The API key is read from the `MURMUR_INFERENCE_API_KEY` environment variable at
runtime. See [murmur.yaml](./murmur.yaml) for the full manifest.
