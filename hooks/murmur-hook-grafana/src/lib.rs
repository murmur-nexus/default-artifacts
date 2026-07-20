/// murmur-hook-grafana: emits OTel spans to a Grafana Tempo OTLP/HTTP endpoint.
///
/// Reads MURMUR_OTEL_ENDPOINT from the WASI environment (injected by the capsule runtime
/// at instantiation time when observability.otel_endpoint is set in the capsule manifest).
///
/// Behaviour when MURMUR_OTEL_ENDPOINT is absent or empty: logs a warning to stderr (which
/// routes to logs/hook-murmur-hook-grafana.log) and becomes a no-op for the session.
///
/// On each lifecycle callback the hook buffers a SpanData record. On on-session-end it
/// finalises the root span and POSTs the full trace as OTLP/JSON to the configured endpoint.
/// If the POST fails the error is logged and the session continues (non-fatal).
///
/// TCP outbound connections are granted by the capsule runtime (inherit_network is set in
/// the hook WASI context when any hook is instantiated).

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::cell::RefCell;

    use utils::{parse_endpoint, send_http_post, session_id_to_trace_id, unix_now_ns};

    mod utils {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::{SystemTime, UNIX_EPOCH};

        pub fn parse_endpoint(endpoint: &str) -> Result<(String, u16, String), String> {
            let without_scheme = endpoint
                .strip_prefix("https://")
                .or_else(|| endpoint.strip_prefix("http://"))
                .unwrap_or(endpoint);

            let (host_port, path_prefix) = without_scheme
                .split_once('/')
                .map(|(h, p)| (h, format!("/{p}")))
                .unwrap_or((without_scheme, String::new()));

            let path_prefix = path_prefix.trim_end_matches('/').to_string();

            let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
                let port = p.parse::<u16>().unwrap_or(4318);
                (h.to_string(), port)
            } else {
                (host_port.to_string(), 4318u16)
            };

            if host.is_empty() {
                return Err(format!("empty host in endpoint '{endpoint}'"));
            }

            Ok((host, port, path_prefix))
        }

        pub fn send_http_post(host: &str, port: u16, path: &str, body: &[u8]) -> Result<(), String> {
            let addr = format!("{host}:{port}");
            let mut stream =
                TcpStream::connect(&addr).map_err(|e| format!("connect to {addr} failed: {e}"))?;

            let header = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(header.as_bytes())
                .map_err(|e| format!("write header: {e}"))?;
            stream
                .write_all(body)
                .map_err(|e| format!("write body: {e}"))?;
            stream.flush().map_err(|e| format!("flush: {e}"))?;

            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap_or(0);
            let response = core::str::from_utf8(&buf[..n]).unwrap_or("");

            if !response.starts_with("HTTP/1.1 2") && !response.starts_with("HTTP/1.0 2") {
                let status_line = response.lines().next().unwrap_or("(no response)");
                return Err(format!("OTLP endpoint returned non-2xx: {status_line}"));
            }

            Ok(())
        }

        pub fn session_id_to_trace_id(session_id: &str) -> String {
            const OFFSET: u64 = 14_695_981_039_346_656_037;
            const PRIME: u64 = 1_099_511_628_211;

            let mut h1 = OFFSET;
            for b in session_id.as_bytes() {
                h1 ^= *b as u64;
                h1 = h1.wrapping_mul(PRIME);
            }

            let mut h2 = OFFSET;
            for b in h1.to_le_bytes() {
                h2 ^= b as u64;
                h2 = h2.wrapping_mul(PRIME);
            }

            format!("{h1:016x}{h2:016x}")
        }

        pub fn unix_now_ns() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        }
    }
    use serde_json::json;

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    // ── per-session state ─────────────────────────────────────────────────────

    struct HookState {
        endpoint: String,
        capsule_name: String,
        capsule_version: String,
        session_id: String,
        model: String,
        trace_id: String,
        root_span_id: String,
        session_start_ns: u64,
        spans: Vec<SpanData>,
        span_counter: u32,
    }

    struct SpanData {
        span_id: String,
        parent_span_id: String,
        name: String,
        start_ns: u64,
        end_ns: u64,
        attributes: Vec<(String, serde_json::Value)>,
        ok: bool,
    }

    thread_local! {
        static STATE: RefCell<Option<HookState>> = RefCell::new(None);
    }

    // ── hook implementation ───────────────────────────────────────────────────

    pub struct MurmurHookGrafana;

    use exports::murmur::hook::lifecycle::HookOutput;

    impl exports::murmur::hook::lifecycle::Guest for MurmurHookGrafana {
        fn on_stage(
            _event: exports::murmur::hook::lifecycle::StageEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(
            ctx: exports::murmur::hook::lifecycle::SessionContext,
        ) -> Result<HookOutput, String> {
            let endpoint = match std::env::var("MURMUR_OTEL_ENDPOINT") {
                Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
                _ => {
                    eprintln!(
                        "[murmur-hook-grafana] MURMUR_OTEL_ENDPOINT is not set — no OTel spans will be exported for this session"
                    );
                    return Ok(HookOutput::None);
                }
            };

            let trace_id = session_id_to_trace_id(&ctx.session_id);
            let root_span_id = format!("{}0000", &trace_id[..12]);
            let session_start_ns = unix_now_ns();

            STATE.with(|s| {
                *s.borrow_mut() = Some(HookState {
                    endpoint,
                    capsule_name: ctx.capsule_name,
                    capsule_version: ctx.capsule_version,
                    session_id: ctx.session_id,
                    model: ctx.model,
                    trace_id,
                    root_span_id,
                    session_start_ns,
                    spans: Vec::new(),
                    span_counter: 1,
                });
            });

            Ok(HookOutput::None)
        }

        fn on_inference(
            event: exports::murmur::hook::lifecycle::InferenceEvent,
        ) -> Result<HookOutput, String> {
            STATE.with(|s| {
                let mut guard = s.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return;
                };
                let end_ns = unix_now_ns();
                let start_ns = end_ns.saturating_sub(1_000_000);
                let mut attrs = vec![
                    ("turn".to_string(), json!(event.turn)),
                    ("gen_ai.usage.input_tokens".to_string(), json!(event.input_tokens.to_string())),
                    ("gen_ai.usage.output_tokens".to_string(), json!(event.output_tokens.to_string())),
                    ("decision".to_string(), json!(event.decision)),
                ];
                if let Some(name) = event.tool_name {
                    attrs.push(("tool_name".to_string(), json!(name)));
                }
                if let Some(tools) = event.tools {
                    attrs.push(("gen_ai.request.tools".to_string(), json!(tools)));
                }
                if let Some(prompt) = event.prompt {
                    attrs.push(("gen_ai.prompt".to_string(), json!(prompt)));
                }
                if let Some(output) = event.output {
                    attrs.push(("gen_ai.completion".to_string(), json!(output)));
                }
                push_span(state, "capsule.inference", start_ns, end_ns, attrs, true);
            });
            Ok(HookOutput::None)
        }

        fn on_tool_call(
            event: exports::murmur::hook::lifecycle::ToolEvent,
        ) -> Result<HookOutput, String> {
            STATE.with(|s| {
                let mut guard = s.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return;
                };
                let end_ns = unix_now_ns();
                let start_ns = end_ns.saturating_sub(event.duration_ms * 1_000_000);
                let ok = event.status == "ok";
                let attrs = vec![
                    ("turn".to_string(), json!(event.turn)),
                    ("tool_name".to_string(), json!(event.tool_name)),
                    ("input_bytes".to_string(), json!(event.input_bytes.to_string())),
                    ("output_bytes".to_string(), json!(event.output_bytes.to_string())),
                    ("duration_ms".to_string(), json!(event.duration_ms.to_string())),
                    ("status".to_string(), json!(event.status)),
                ];
                push_span(state, "capsule.tool_call", start_ns, end_ns, attrs, ok);
            });
            Ok(HookOutput::None)
        }

        fn on_shell(
            event: exports::murmur::hook::lifecycle::ShellEvent,
        ) -> Result<HookOutput, String> {
            STATE.with(|s| {
                let mut guard = s.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return;
                };
                let end_ns = unix_now_ns();
                let start_ns = end_ns.saturating_sub(event.duration_ms * 1_000_000);
                let ok = event.exit_code == 0;
                let cmd: String = event.command.chars().take(200).collect();
                let stdout: String = event.stdout.chars().take(4096).collect();
                let mut attrs = vec![
                    ("turn".to_string(), json!(event.turn)),
                    ("command".to_string(), json!(cmd)),
                    ("exit_code".to_string(), json!(event.exit_code)),
                    ("duration_ms".to_string(), json!(event.duration_ms.to_string())),
                    ("stdout".to_string(), json!(stdout)),
                ];
                if !event.stderr.is_empty() {
                    let stderr: String = event.stderr.chars().take(4096).collect();
                    attrs.push(("stderr".to_string(), json!(stderr)));
                }
                push_span(state, "capsule.shell", start_ns, end_ns, attrs, ok);
            });
            Ok(HookOutput::None)
        }

        fn on_compaction(
            event: exports::murmur::hook::lifecycle::CompactionEvent,
        ) -> Result<HookOutput, String> {
            STATE.with(|s| {
                let mut guard = s.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return;
                };
                let end_ns = unix_now_ns();
                let start_ns = end_ns.saturating_sub(1_000_000);
                let attrs = vec![
                    ("session_tokens".to_string(), json!(event.session_tokens.to_string())),
                    ("threshold".to_string(), json!(event.threshold)),
                    ("message_count".to_string(), json!(event.messages.len())),
                ];
                push_span(state, "capsule.compaction", start_ns, end_ns, attrs, true);
            });
            Ok(HookOutput::None)
        }

        fn on_session_end(
            event: exports::murmur::hook::lifecycle::SessionEndEvent,
        ) -> Result<HookOutput, String> {
            let state = STATE.with(|s| s.borrow_mut().take());
            let Some(state) = state else {
                return Ok(HookOutput::None);
            };

            let session_end_ns = unix_now_ns();
            let ok = event.exit_status == "ok";

            // Build root span
            let mut all_spans = vec![SpanData {
                span_id: state.root_span_id.clone(),
                parent_span_id: String::new(),
                name: "capsule.session".to_string(),
                start_ns: state.session_start_ns,
                end_ns: session_end_ns,
                attributes: vec![
                    ("service.name".to_string(), json!(state.capsule_name)),
                    ("service.version".to_string(), json!(state.capsule_version)),
                    ("model".to_string(), json!(state.model)),
                    ("exit_status".to_string(), json!(event.exit_status)),
                    ("murmur.session_id".to_string(), json!(state.session_id)),
                    ("total_turns".to_string(), json!(event.total_turns)),
                    ("total_input_tokens".to_string(), json!(event.total_input_tokens.to_string())),
                    ("total_output_tokens".to_string(), json!(event.total_output_tokens.to_string())),
                ],
                ok,
            }];
            all_spans.extend(state.spans);

            // Also inject formation_id if available
            if let Ok(formation_id) = std::env::var("MURMUR_FORMATION_ID") {
                if !formation_id.is_empty() {
                    if let Some(root) = all_spans.first_mut() {
                        root.attributes.push(("murmur.formation_id".to_string(), json!(formation_id)));
                    }
                }
            }

            if let Err(e) = export_trace(&state.endpoint, &state.trace_id, &all_spans) {
                return Err(format!("[murmur-hook-grafana] OTLP export failed: {e}"));
            }

            Ok(HookOutput::None)
        }

        fn on_task_start(
            _event: exports::murmur::hook::lifecycle::TaskStartEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_end(
            _event: exports::murmur::hook::lifecycle::TaskEndEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn push_span(
        state: &mut HookState,
        name: &str,
        start_ns: u64,
        end_ns: u64,
        attributes: Vec<(String, serde_json::Value)>,
        ok: bool,
    ) {
        let idx = state.span_counter;
        state.span_counter += 1;
        let span_id = format!("{}{:04x}", &state.trace_id[..12], idx);
        state.spans.push(SpanData {
            span_id,
            parent_span_id: state.root_span_id.clone(),
            name: name.to_string(),
            start_ns,
            end_ns,
            attributes,
            ok,
        });
    }

    fn export_trace(endpoint: &str, trace_id: &str, spans: &[SpanData]) -> Result<(), String> {
        let json_body = build_otlp_json(trace_id, spans).to_string();
        let body = json_body.as_bytes();

        let (host, port, path_prefix) = parse_endpoint(endpoint)?;
        let path = format!("{path_prefix}/v1/traces");

        send_http_post(&host, port, &path, body)
    }

    fn build_otlp_json(trace_id: &str, spans: &[SpanData]) -> serde_json::Value {
        let spans_json: Vec<serde_json::Value> = spans
            .iter()
            .map(|span| {
                let attrs: Vec<serde_json::Value> = span
                    .attributes
                    .iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(s) => json!({"stringValue": s}),
                            serde_json::Value::Number(n) => json!({"intValue": n.to_string()}),
                            other => json!({"stringValue": other.to_string()}),
                        };
                        json!({"key": k, "value": val})
                    })
                    .collect();

                let status_code: u8 = if span.ok { 1 } else { 2 };
                let mut obj = json!({
                    "traceId": trace_id,
                    "spanId": span.span_id,
                    "name": span.name,
                    "kind": 1,
                    "startTimeUnixNano": span.start_ns.to_string(),
                    "endTimeUnixNano": span.end_ns.to_string(),
                    "attributes": attrs,
                    "status": {"code": status_code},
                });
                if !span.parent_span_id.is_empty() {
                    obj["parentSpanId"] = json!(span.parent_span_id);
                }
                obj
            })
            .collect();

        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        {"key": "service.name", "value": {"stringValue":
                            spans.first().and_then(|s| {
                                s.attributes.iter().find(|(k, _)| k == "service.name").map(|(_, v)| v.as_str().unwrap_or("capsule"))
                            }).unwrap_or("capsule")
                        }},
                        {"key": "service.version", "value": {"stringValue":
                            spans.first().and_then(|s| {
                                s.attributes.iter().find(|(k, _)| k == "service.version").map(|(_, v)| v.as_str().unwrap_or("unknown"))
                            }).unwrap_or("unknown")
                        }},
                        {"key": "telemetry.sdk.name", "value": {"stringValue": "murmur-hook-grafana"}},
                    ]
                },
                "scopeSpans": [{
                    "scope": {"name": "murmur-hook-grafana", "version": "0.3.16"},
                    "spans": spans_json,
                }]
            }]
        })
    }

    export!(MurmurHookGrafana);
}
