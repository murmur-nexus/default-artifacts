/// murmur-hook-eval: structured evaluation hook for capsule sessions.
///
/// Reads MURMUR_EVAL_CONFIG (JSON-serialized EvalConfig) from the WASI environment
/// (injected by the capsule runtime when observability.eval is set in the manifest).
///
/// Behaviour when MURMUR_EVAL_CONFIG is absent or empty: logs a warning to stderr
/// (routes to logs/hook-murmur-hook-eval.log) and becomes a no-op for the session.
///
/// On each lifecycle callback the hook tracks events relevant to configured scorers.
/// On on-session-end it:
///   1. Scores events per configured scorers
///   2. Writes scored event records + dataset_run summary to workdir/eval.jsonl
///   3. If MURMUR_OTEL_ENDPOINT is set, exports eval scores as OTLP log records
///
/// Scorer types implemented:
///   - exit_ok: passes if session exit_status == "ok"
///   - max_turns: passes if total_turns <= max
///   - max_tokens: passes if total_input + total_output tokens <= max
///   - tool_sequence: passes if observed tool call sequence matches expected (subsequence match)
///   - llm_judge: stubbed — logs a "not yet implemented" warning and scores nothing
///
/// LLM-as-judge decision: deferred to a later slice. The scorer type is recognized
/// and validated in the manifest but produces no scores at runtime. Reason: outbound
/// API calls from WASM require an API key env var and add latency to on_session_end.
/// Implementing it correctly requires retry logic, cost controls, and prompt design
/// that deserve their own slice.

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::{cell::RefCell, fs};

    use utils::{parse_endpoint, send_http_post, session_id_to_trace_id, unix_now_ms, unix_now_ns};

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

        pub fn unix_now_ms() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        }
    }
    use serde_json::{json, Value};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    // ── eval config (mirrors murmur-artifact EvalConfig JSON shape) ──────────

    #[derive(Debug, Clone)]
    enum ScorerKind {
        ExitOk,
        MaxTurns(u32),
        MaxTokens(u64),
        ToolSequence(Vec<String>),
        LlmJudge,
    }

    #[derive(Debug, Clone)]
    struct Scorer {
        name: String,
        kind: ScorerKind,
    }

    fn parse_scorers(config_json: &str) -> Vec<Scorer> {
        let v: Value = match serde_json::from_str(config_json) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[murmur-hook-eval] failed to parse MURMUR_EVAL_CONFIG: {e}");
                return Vec::new();
            }
        };

        let scorers_json = match v.get("scorers").and_then(|s| s.as_array()) {
            Some(arr) => arr.clone(),
            None => {
                eprintln!("[murmur-hook-eval] MURMUR_EVAL_CONFIG.scorers is missing or not an array");
                return Vec::new();
            }
        };

        let mut scorers = Vec::new();
        for s in scorers_json {
            let scorer_type = s.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
            let name = s
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(scorer_type)
                .to_string();

            let kind = match scorer_type {
                "exit_ok" => ScorerKind::ExitOk,
                "max_turns" => {
                    let max = s.get("max").and_then(|m| m.as_u64()).unwrap_or(10) as u32;
                    ScorerKind::MaxTurns(max)
                }
                "max_tokens" => {
                    let max = s.get("max").and_then(|m| m.as_u64()).unwrap_or(100_000);
                    ScorerKind::MaxTokens(max)
                }
                "tool_sequence" => {
                    let expected: Vec<String> = s
                        .get("expected")
                        .and_then(|e| e.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    ScorerKind::ToolSequence(expected)
                }
                "llm_judge" => {
                    eprintln!(
                        "[murmur-hook-eval] scorer '{name}' type 'llm_judge' is not yet implemented — scoring nothing"
                    );
                    ScorerKind::LlmJudge
                }
                other => {
                    eprintln!("[murmur-hook-eval] unknown scorer type '{other}' for scorer '{name}' — skipping");
                    continue;
                }
            };

            scorers.push(Scorer { name, kind });
        }
        scorers
    }

    // ── per-session state ─────────────────────────────────────────────────────

    struct HookState {
        session_id: String,
        case_id: Option<String>,
        dataset_id: Option<String>,
        scorers: Vec<Scorer>,
        otel_endpoint: Option<String>,
        tool_calls_observed: Vec<String>,
    }

    thread_local! {
        static STATE: RefCell<Option<HookState>> = RefCell::new(None);
    }

    // ── log helpers ───────────────────────────────────────────────────────────

    fn write_hook_warning(msg: &str) {
        use std::io::Write;
        let log_dir = std::path::Path::new("./logs");
        let _ = std::fs::create_dir_all(log_dir);
        let log_path = log_dir.join("hook-murmur-hook-eval.log");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .and_then(|mut f| f.write_all(format!("{msg}\n").as_bytes()));
    }

    // ── hook implementation ───────────────────────────────────────────────────

    pub struct MurmurHookEval;

    use exports::murmur::hook::lifecycle::HookOutput;

    impl exports::murmur::hook::lifecycle::Guest for MurmurHookEval {
        fn on_stage(
            _event: exports::murmur::hook::lifecycle::StageEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(
            ctx: exports::murmur::hook::lifecycle::SessionContext,
        ) -> Result<HookOutput, String> {
            let config_json = match std::env::var("MURMUR_EVAL_CONFIG") {
                Ok(v) if !v.trim().is_empty() => v,
                _ => {
                    write_hook_warning(
                        "[murmur-hook-eval] MURMUR_EVAL_CONFIG is not set — no eval scores will be written for this session"
                    );
                    return Ok(HookOutput::None);
                }
            };

            let scorers = parse_scorers(&config_json);
            if scorers.is_empty() {
                write_hook_warning(
                    "[murmur-hook-eval] no valid scorers configured — eval.jsonl will not be written"
                );
                return Ok(HookOutput::None);
            }

            let case_id = std::env::var("MURMUR_CASE_ID").ok().filter(|s| !s.is_empty());
            let dataset_id = std::env::var("MURMUR_DATASET_ID").ok().filter(|s| !s.is_empty());
            let otel_endpoint = std::env::var("MURMUR_OTEL_ENDPOINT").ok().filter(|s| !s.trim().is_empty());

            STATE.with(|s| {
                *s.borrow_mut() = Some(HookState {
                    session_id: ctx.session_id,
                    case_id,
                    dataset_id,
                    scorers,
                    otel_endpoint,
                    tool_calls_observed: Vec::new(),
                });
            });

            Ok(HookOutput::None)
        }

        fn on_inference(
            _event: exports::murmur::hook::lifecycle::InferenceEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_tool_call(
            event: exports::murmur::hook::lifecycle::ToolEvent,
        ) -> Result<HookOutput, String> {
            STATE.with(|s| {
                let mut guard = s.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    state.tool_calls_observed.push(event.tool_name);
                }
            });
            Ok(HookOutput::None)
        }

        fn on_shell(
            _event: exports::murmur::hook::lifecycle::ShellEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_compaction(
            _event: exports::murmur::hook::lifecycle::CompactionEvent,
        ) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_end(
            event: exports::murmur::hook::lifecycle::SessionEndEvent,
        ) -> Result<HookOutput, String> {
            let state = STATE.with(|s| s.borrow_mut().take());
            let Some(state) = state else {
                return Ok(HookOutput::None);
            };

            let ts = unix_now_ms();
            let total_tokens = event.total_input_tokens + event.total_output_tokens;

            let mut score_records: Vec<Value> = Vec::new();
            let mut aggregated_scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
            let mut any_fail = false;

            for scorer in &state.scorers {
                match &scorer.kind {
                    ScorerKind::LlmJudge => {
                        // Stubbed — no score emitted
                        continue;
                    }
                    ScorerKind::ExitOk => {
                        let pass = event.exit_status == "ok";
                        let score = if pass { 1.0_f64 } else { 0.0_f64 };
                        let result = if pass { "pass" } else { "fail" };
                        if !pass { any_fail = true; }
                        aggregated_scores.insert(scorer.name.clone(), score);
                        score_records.push(json!({
                            "record_type": "event_score",
                            "ts": ts,
                            "turn": event.total_turns,
                            "event_type": "session_end",
                            "scorer": scorer.name,
                            "result": result,
                            "score": score,
                            "reason": format!("exit_status={}", event.exit_status),
                        }));
                    }
                    ScorerKind::MaxTurns(max) => {
                        let pass = event.total_turns <= *max;
                        let score = if pass { 1.0_f64 } else { 0.0_f64 };
                        let result = if pass { "pass" } else { "fail" };
                        if !pass { any_fail = true; }
                        aggregated_scores.insert(scorer.name.clone(), score);
                        score_records.push(json!({
                            "record_type": "event_score",
                            "ts": ts,
                            "turn": event.total_turns,
                            "event_type": "session_end",
                            "scorer": scorer.name,
                            "result": result,
                            "score": score,
                            "reason": format!("turns={} max={}", event.total_turns, max),
                        }));
                    }
                    ScorerKind::MaxTokens(max) => {
                        let pass = total_tokens <= *max;
                        let score = if pass { 1.0_f64 } else { 0.0_f64 };
                        let result = if pass { "pass" } else { "fail" };
                        if !pass { any_fail = true; }
                        aggregated_scores.insert(scorer.name.clone(), score);
                        score_records.push(json!({
                            "record_type": "event_score",
                            "ts": ts,
                            "turn": event.total_turns,
                            "event_type": "session_end",
                            "scorer": scorer.name,
                            "result": result,
                            "score": score,
                            "reason": format!("tokens={} max={}", total_tokens, max),
                        }));
                    }
                    ScorerKind::ToolSequence(expected) => {
                        let (pass, score) = score_tool_sequence(&state.tool_calls_observed, expected);
                        let result = if pass { "pass" } else { "fail" };
                        if !pass { any_fail = true; }
                        aggregated_scores.insert(scorer.name.clone(), score);
                        score_records.push(json!({
                            "record_type": "event_score",
                            "ts": ts,
                            "turn": event.total_turns,
                            "event_type": "session_end",
                            "scorer": scorer.name,
                            "result": result,
                            "score": score,
                            "reason": format!("observed={:?} expected={:?}", state.tool_calls_observed, expected),
                        }));
                    }
                }
            }

            let overall = if any_fail { "fail" } else if aggregated_scores.is_empty() { "no_scores" } else { "pass" };

            // Build dataset_run record
            let dataset_run = json!({
                "record_type": "dataset_run",
                "ts": ts,
                "dataset_id": state.dataset_id,
                "case_id": state.case_id,
                "overall": overall,
                "scores": aggregated_scores,
            });

            // Write eval.jsonl (relative to preopened workdir = ".")
            if let Err(e) = write_eval_jsonl(&score_records, &dataset_run) {
                return Err(format!("[murmur-hook-eval] failed to write eval.jsonl: {e}"));
            }

            // Export OTel log records if endpoint is set
            if let Some(ref endpoint) = state.otel_endpoint {
                if !aggregated_scores.is_empty() {
                    let trace_id = session_id_to_trace_id(&state.session_id);
                    if let Err(e) = export_eval_logs(endpoint, &trace_id, &state.case_id, &score_records) {
                        eprintln!("[murmur-hook-eval] OTLP log export failed: {e}");
                        // Non-fatal: eval.jsonl was already written
                    }
                }
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

    // ── scoring helpers ───────────────────────────────────────────────────────

    fn score_tool_sequence(observed: &[String], expected: &[String]) -> (bool, f64) {
        if expected.is_empty() {
            return (true, 1.0);
        }
        // Check if expected is a subsequence of observed
        let mut ei = 0;
        for tool in observed {
            if ei < expected.len() && tool == &expected[ei] {
                ei += 1;
            }
        }
        let matched = ei;
        let score = matched as f64 / expected.len() as f64;
        (matched == expected.len(), score)
    }

    // ── eval.jsonl writer ─────────────────────────────────────────────────────

    fn write_eval_jsonl(score_records: &[Value], dataset_run: &Value) -> Result<(), String> {
        use std::io::Write;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open("./eval.jsonl")
            .map_err(|e| e.to_string())?;

        for record in score_records {
            let line = serde_json::to_string(record).map_err(|e| e.to_string())?;
            file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }

        let run_line = serde_json::to_string(dataset_run).map_err(|e| e.to_string())?;
        file.write_all(run_line.as_bytes()).map_err(|e| e.to_string())?;
        file.write_all(b"\n").map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;

        Ok(())
    }

    // ── OTLP log export ───────────────────────────────────────────────────────

    fn export_eval_logs(
        endpoint: &str,
        trace_id: &str,
        case_id: &Option<String>,
        score_records: &[Value],
    ) -> Result<(), String> {
        let ts_ns = unix_now_ns().to_string();

        let log_records: Vec<Value> = score_records
            .iter()
            .map(|rec| {
                let scorer = rec.get("scorer").and_then(|v| v.as_str()).unwrap_or("");
                let result = rec.get("result").and_then(|v| v.as_str()).unwrap_or("");
                let score = rec.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);

                let mut attrs = vec![
                    json!({"key": "eval.scorer", "value": {"stringValue": scorer}}),
                    json!({"key": "eval.result", "value": {"stringValue": result}}),
                    json!({"key": "eval.score", "value": {"doubleValue": score}}),
                ];
                if let Some(id) = case_id {
                    attrs.push(json!({"key": "eval.case_id", "value": {"stringValue": id}}));
                }

                json!({
                    "timeUnixNano": ts_ns,
                    "traceId": trace_id,
                    "body": {"stringValue": format!("eval.scorer={scorer} result={result} score={score:.4}")},
                    "attributes": attrs,
                })
            })
            .collect();

        let payload = json!({
            "resourceLogs": [{
                "resource": {"attributes": [
                    {"key": "service.name", "value": {"stringValue": "murmur-hook-eval"}},
                ]},
                "scopeLogs": [{
                    "scope": {"name": "murmur-hook-eval", "version": "0.3.16"},
                    "logRecords": log_records,
                }]
            }]
        });

        let body = payload.to_string();
        let (host, port, path_prefix) = parse_endpoint(endpoint)?;
        let path = format!("{path_prefix}/v1/logs");
        send_http_post(&host, port, &path, body.as_bytes())
    }

    export!(MurmurHookEval);
}
