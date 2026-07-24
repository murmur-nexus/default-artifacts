# murmur-hook-grafana

Emits OTel spans to a Grafana Tempo OTLP/HTTP endpoint for each capsule
lifecycle event.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: all events · mode: async · commit policy: none.

## Configuration

Declare in your capsule `murmur.yaml`:

```yaml
artifacts:
  - name: murmur-hook-grafana
    version: 0.2.0
    runtime: hook
    capabilities:
      network:
        allow:
          - http://localhost:4318   # must match your observability.otel_endpoint host

observability:
  otel_endpoint: http://localhost:4318
```

`capabilities.network.allow` must list **the same host you set in
`observability.otel_endpoint`** — the example above is a placeholder, not a
default. The hook has no endpoint of its own: it reads `MURMUR_OTEL_ENDPOINT`
from its WASI environment, which the runtime injects from your
`observability.otel_endpoint`, and POSTs the OTLP/JSON trace there at
`on_session_end`. If you point `otel_endpoint` at a Grafana Cloud or
self-hosted collector, put that host in `allow` instead.

If `otel_endpoint` is unset the hook logs a warning to stderr at session start
and exports nothing, so the network grant is only worth declaring alongside an
endpoint.

No filesystem capability is required — the hook writes no files.

The export is a hand-rolled HTTP/1.1 POST over a raw WASI socket
(`std::net::TcpStream`), not a `wasi-http` request. If your runtime enforces
network grants only on the `wasi-http` path, the grant above is necessary but
may not be sufficient — verify the trace actually reaches your collector after
enabling it.

The `capabilities:` block belongs on **your** capsule manifest's `artifacts:`
entry, not in this hook's bundled `murmur.yaml`. The runtime only reads the
operator-side grant; a `capabilities:` key inside a hook artifact is never
consulted for enforcement, which is what stops a hook from widening its own
access.

See [murmur.yaml](./murmur.yaml) for the full manifest, including the endpoint
configuration.
