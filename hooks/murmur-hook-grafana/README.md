# murmur-hook-grafana

Emits OTel spans to a Grafana Tempo OTLP/HTTP endpoint for each capsule
lifecycle event.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: async · commit policy: none.

See [murmur.yaml](./murmur.yaml) for the full manifest, including the endpoint
configuration.
