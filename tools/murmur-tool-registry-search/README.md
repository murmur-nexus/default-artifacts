# murmur-tool-registry-search

Searches the Murmur artifact registry for artifacts matching a keyword.
Returns ranked results with name, version, runtime type, description, and
published date. Supports the public default-artifacts index (default), the
local artifact store, or a custom index URL.

Native binary tool (`runtime: tool`, `implementation: native`) — it performs
HTTPS via a native TLS stack that is not available to a `wasm32-wasip2` guest.

See [murmur.yaml](./murmur.yaml) for the full manifest.
