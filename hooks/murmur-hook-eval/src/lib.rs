//! murmur-hook-eval: structured evaluation hook for capsule sessions.
//!
//! Reads `MURMUR_EVAL_CONFIG` (JSON-serialized `EvalConfig`) from the WASI environment,
//! injected by the capsule runtime when `observability.eval` is set in the manifest.
//!
//! The contract, stated once: **if `MURMUR_EVAL_CONFIG` is present and non-empty, the
//! session ends with an `eval.jsonl` whose last line is a `dataset_run` record**, and its
//! `overall` is exactly one of `pass`, `fail`, `no_scores`, `config_error`. Only an absent
//! or whitespace-only `MURMUR_EVAL_CONFIG` — the capsule declared no `observability.eval`
//! — leaves no file behind. A config error never fails the session.
//!
//! The hook invents no thresholds. `max` is required on `max_turns` and `max_tokens`, and
//! `expected` is required and non-empty on `tool_sequence`; a scorer that omits one is a
//! [`ConfigError`] naming that key rather than a score computed against a number nobody
//! chose. Murmur's own manifest parser supplies those defaults upstream, so under a normal
//! capsule the hook is handed a config that already carries them.
//!
//! Scorer types implemented:
//!   - `exit_ok`: passes if session `exit_status == "ok"`
//!   - `max_turns`: passes if `total_turns <= max`
//!   - `max_tokens`: passes if `total_input + total_output` tokens `<= max`
//!   - `tool_sequence`: passes if the expected tools appear as a subsequence of the observed calls
//!   - `llm_judge`: recognized, scores nothing — see below
//!
//! LLM-as-judge is deferred to a later slice. The scorer type is recognized and is not a
//! config error, but it produces no score record at runtime: outbound API calls from WASM
//! need an API key env var and add latency to `on_session_end`, and doing it correctly
//! requires retry logic, cost controls and prompt design that deserve their own slice. A
//! config whose only scorers are `llm_judge` therefore lands on `no_scores`.
//!
//! Everything under `wasm_hook` is gated on `target_arch = "wasm32"` and so is unreachable
//! from `cargo test`. Config parsing, scoring and the `eval.jsonl` line building all live
//! here at the crate root over plain mirrors of the WIT records; the adapter reads the
//! environment, opens the file and posts to the collector, and nothing else.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// Plain mirror of `murmur:hook/lifecycle`'s `session-end-event`.
#[derive(Debug, Clone)]
pub struct SessionEnd {
    pub total_turns: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tool_calls: u32,
    pub total_shell_calls: u32,
    pub duration_ms: u64,
    pub exit_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScorerKind {
    ExitOk,
    MaxTurns(u32),
    MaxTokens(u64),
    ToolSequence(Vec<String>),
    LlmJudge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scorer {
    pub name: String,
    pub kind: ScorerKind,
}

/// A `MURMUR_EVAL_CONFIG` the hook refuses to score against, and the config path that
/// made it so.
///
/// `key` is either the literal `MURMUR_EVAL_CONFIG` (the whole value is unusable), the
/// literal `scorers`, or `scorers[<i>].<field>` with `i` the zero-based index in the
/// `scorers` array. It is carried verbatim onto the `dataset_run` record and into
/// `logs/hook-murmur-hook-eval.log`, so an operator reading either one sees the same
/// offending key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub key: String,
    pub message: String,
}

/// Everything one session's scoring produced, before it is given a dataset and case id.
#[derive(Debug, Clone)]
pub struct EvalReport {
    /// One of `pass`, `fail`, `no_scores`, `config_error`.
    pub overall: &'static str,
    pub score_records: Vec<Value>,
    /// Aggregated per-scorer score. A `BTreeMap` rather than a `HashMap` so the `scores`
    /// object serializes in a stable key order.
    pub scores: BTreeMap<String, f64>,
    /// Present only when `overall` is `config_error`.
    pub config_error: Option<ConfigError>,
}

/// What kind of JSON value this is, for a message that has to say what was found instead.
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Read the scorer list out of a `MURMUR_EVAL_CONFIG` value.
///
/// `Ok(vec![])` means a valid config that asks for no scoring, and never that something
/// went wrong. The first offending element wins: parsing stops there and the scorers that
/// happened to precede it are discarded, because a partially applied config scores a
/// session against a rubric nobody wrote.
///
/// Does no I/O and prints nothing — the caller logs what it returns.
pub fn parse_scorers(config_json: &str) -> Result<Vec<Scorer>, ConfigError> {
    let v: Value = serde_json::from_str(config_json).map_err(|e| ConfigError {
        key: "MURMUR_EVAL_CONFIG".to_string(),
        message: format!("not valid JSON: {e}"),
    })?;

    if !v.is_object() {
        return Err(ConfigError {
            key: "MURMUR_EVAL_CONFIG".to_string(),
            message: format!("expected a JSON object, found {}", json_kind(&v)),
        });
    }

    let scorers_json = v
        .get("scorers")
        .and_then(|s| s.as_array())
        .ok_or_else(|| ConfigError {
            key: "scorers".to_string(),
            message: "missing or not an array".to_string(),
        })?;

    let mut scorers = Vec::new();
    for (i, s) in scorers_json.iter().enumerate() {
        let scorer_type = s
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ConfigError {
                key: format!("scorers[{i}].type"),
                message: "missing or not a string".to_string(),
            })?;

        let name = s
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or(scorer_type)
            .to_string();

        let kind = match scorer_type {
            "exit_ok" => ScorerKind::ExitOk,
            "max_turns" => {
                let max = required_max(s, i, scorer_type)?;
                let max = u32::try_from(max).map_err(|_| ConfigError {
                    key: format!("scorers[{i}].max"),
                    message: format!("{max} does not fit the u32 turn counter the session reports"),
                })?;
                ScorerKind::MaxTurns(max)
            }
            "max_tokens" => ScorerKind::MaxTokens(required_max(s, i, scorer_type)?),
            "tool_sequence" => ScorerKind::ToolSequence(required_expected(s, i)?),
            "llm_judge" => ScorerKind::LlmJudge,
            other => {
                return Err(ConfigError {
                    key: format!("scorers[{i}].type"),
                    message: format!("unknown scorer type '{other}'"),
                })
            }
        };

        scorers.push(Scorer { name, kind });
    }
    Ok(scorers)
}

/// The threshold `max_turns` and `max_tokens` score against. Required: the hook has no
/// business choosing a session's turn or token budget on the operator's behalf.
fn required_max(scorer: &Value, i: usize, scorer_type: &str) -> Result<u64, ConfigError> {
    scorer
        .get("max")
        .and_then(|m| m.as_u64())
        .ok_or_else(|| ConfigError {
            key: format!("scorers[{i}].max"),
            message: format!("scorer type '{scorer_type}' requires a numeric 'max'"),
        })
}

/// The tool names `tool_sequence` scores against. Required and non-empty: an absent or
/// empty `expected` matches every session vacuously, which reads as a passing scorer
/// while measuring nothing.
fn required_expected(scorer: &Value, i: usize) -> Result<Vec<String>, ConfigError> {
    let invalid = || ConfigError {
        key: format!("scorers[{i}].expected"),
        message: "scorer type 'tool_sequence' requires 'expected' to be a non-empty list \
                  of tool names"
            .to_string(),
    };

    let arr = scorer
        .get("expected")
        .and_then(|e| e.as_array())
        .ok_or_else(invalid)?;
    if arr.is_empty() {
        return Err(invalid());
    }
    arr.iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(invalid))
        .collect()
}

// ── scoring ───────────────────────────────────────────────────────────────────

/// Whether `expected` appears as a subsequence of `observed`, and the fraction of it that
/// was matched.
///
/// An empty `expected` never reaches this from a parsed config — [`parse_scorers`]
/// refuses it — but the vacuous `(true, 1.0)` is kept for a caller that constructs a
/// scorer directly.
pub fn score_tool_sequence(observed: &[String], expected: &[String]) -> (bool, f64) {
    if expected.is_empty() {
        return (true, 1.0);
    }
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

/// Score one finished session against its configured scorers.
///
/// `overall` is `fail` if any scorer failed, `no_scores` if the scorers produced no score
/// records at all — an empty `scorers` array, or one whose every entry is `llm_judge` —
/// and `pass` otherwise.
pub fn score_session(
    scorers: &[Scorer],
    observed_tools: &[String],
    event: &SessionEnd,
    ts: u64,
) -> EvalReport {
    let total_tokens = event.total_input_tokens + event.total_output_tokens;

    let mut score_records: Vec<Value> = Vec::new();
    let mut scores: BTreeMap<String, f64> = BTreeMap::new();
    let mut any_fail = false;

    for scorer in scorers {
        let (pass, score, reason) = match &scorer.kind {
            // Stubbed — no score emitted.
            ScorerKind::LlmJudge => continue,
            ScorerKind::ExitOk => {
                let pass = event.exit_status == "ok";
                (
                    pass,
                    if pass { 1.0 } else { 0.0 },
                    format!("exit_status={}", event.exit_status),
                )
            }
            ScorerKind::MaxTurns(max) => {
                let pass = event.total_turns <= *max;
                (
                    pass,
                    if pass { 1.0 } else { 0.0 },
                    format!("turns={} max={}", event.total_turns, max),
                )
            }
            ScorerKind::MaxTokens(max) => {
                let pass = total_tokens <= *max;
                (
                    pass,
                    if pass { 1.0 } else { 0.0 },
                    format!("tokens={total_tokens} max={max}"),
                )
            }
            ScorerKind::ToolSequence(expected) => {
                let (pass, score) = score_tool_sequence(observed_tools, expected);
                (
                    pass,
                    score,
                    format!("observed={observed_tools:?} expected={expected:?}"),
                )
            }
        };

        if !pass {
            any_fail = true;
        }
        scores.insert(scorer.name.clone(), score);
        score_records.push(json!({
            "record_type": "event_score",
            "ts": ts,
            "turn": event.total_turns,
            "event_type": "session_end",
            "scorer": scorer.name,
            "result": if pass { "pass" } else { "fail" },
            "score": score,
            "reason": reason,
        }));
    }

    let overall = if any_fail {
        "fail"
    } else if scores.is_empty() {
        "no_scores"
    } else {
        "pass"
    };

    EvalReport {
        overall,
        score_records,
        scores,
        config_error: None,
    }
}

/// The report for a session whose `MURMUR_EVAL_CONFIG` could not be read: no score
/// records, no scores, and the offending key carried onto the `dataset_run` line.
pub fn config_error_report(error: ConfigError) -> EvalReport {
    EvalReport {
        overall: "config_error",
        score_records: Vec::new(),
        scores: BTreeMap::new(),
        config_error: Some(error),
    }
}

/// The exact contents of `eval.jsonl`, one JSON line per element, `dataset_run` last.
pub fn eval_jsonl_lines(
    report: &EvalReport,
    dataset_id: Option<&str>,
    case_id: Option<&str>,
    ts: u64,
) -> Vec<String> {
    let mut lines: Vec<String> = report
        .score_records
        .iter()
        .map(|record| record.to_string())
        .collect();

    let mut dataset_run = json!({
        "record_type": "dataset_run",
        "ts": ts,
        "dataset_id": dataset_id,
        "case_id": case_id,
        "overall": report.overall,
        "scores": report.scores,
    });
    if let Some(error) = &report.config_error {
        dataset_run["config_error"] = json!({
            "key": error.key,
            "message": error.message,
        });
    }
    lines.push(dataset_run.to_string());

    lines
}

#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use std::cell::RefCell;

    use super::{
        config_error_report, eval_jsonl_lines, parse_scorers, score_session, ConfigError, Scorer,
        ScorerKind, SessionEnd,
    };
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

        pub fn send_http_post(
            host: &str,
            port: u16,
            path: &str,
            body: &[u8],
        ) -> Result<(), String> {
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

    // ── per-session state ─────────────────────────────────────────────────────

    struct HookState {
        session_id: String,
        case_id: Option<String>,
        dataset_id: Option<String>,
        scorers: Vec<Scorer>,
        /// `Some` when `MURMUR_EVAL_CONFIG` could not be read. State is installed either
        /// way: the session still owes an `eval.jsonl`, and a config error is what that
        /// file has to report.
        config_error: Option<ConfigError>,
        otel_endpoint: Option<String>,
        tool_calls_observed: Vec<String>,
    }

    thread_local! {
        static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
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

            let (scorers, config_error) = match parse_scorers(&config_json) {
                Ok(scorers) => {
                    if scorers.is_empty() {
                        write_hook_warning(
                            "[murmur-hook-eval] MURMUR_EVAL_CONFIG configures no scorers — eval.jsonl will record overall=no_scores"
                        );
                    }
                    for scorer in &scorers {
                        if scorer.kind == ScorerKind::LlmJudge {
                            write_hook_warning(&format!(
                                "[murmur-hook-eval] scorer '{}' type 'llm_judge' is not yet implemented — scoring nothing",
                                scorer.name
                            ));
                        }
                    }
                    (scorers, None)
                }
                Err(error) => {
                    // The same key and message the dataset_run record carries, so an
                    // operator reading the log and one reading eval.jsonl see the same
                    // offending key.
                    write_hook_warning(&format!(
                        "[murmur-hook-eval] MURMUR_EVAL_CONFIG is invalid at '{}': {} — eval.jsonl will record overall=config_error",
                        error.key, error.message
                    ));
                    (Vec::new(), Some(error))
                }
            };

            let case_id = std::env::var("MURMUR_CASE_ID")
                .ok()
                .filter(|s| !s.is_empty());
            let dataset_id = std::env::var("MURMUR_DATASET_ID")
                .ok()
                .filter(|s| !s.is_empty());
            let otel_endpoint = std::env::var("MURMUR_OTEL_ENDPOINT")
                .ok()
                .filter(|s| !s.trim().is_empty());

            STATE.with(|s| {
                *s.borrow_mut() = Some(HookState {
                    session_id: ctx.session_id,
                    case_id,
                    dataset_id,
                    scorers,
                    config_error,
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
            let Some(mut state) = state else {
                return Ok(HookOutput::None);
            };

            let ts = unix_now_ms();
            let report = match state.config_error.take() {
                Some(error) => config_error_report(error),
                None => {
                    let session_end = SessionEnd {
                        total_turns: event.total_turns,
                        total_input_tokens: event.total_input_tokens,
                        total_output_tokens: event.total_output_tokens,
                        total_tool_calls: event.total_tool_calls,
                        total_shell_calls: event.total_shell_calls,
                        duration_ms: event.duration_ms,
                        exit_status: event.exit_status,
                    };
                    score_session(&state.scorers, &state.tool_calls_observed, &session_end, ts)
                }
            };

            let lines = eval_jsonl_lines(
                &report,
                state.dataset_id.as_deref(),
                state.case_id.as_deref(),
                ts,
            );
            if let Err(e) = write_eval_jsonl(&lines) {
                return Err(format!(
                    "[murmur-hook-eval] failed to write eval.jsonl: {e}"
                ));
            }

            // Export OTel log records if endpoint is set.
            if let Some(ref endpoint) = state.otel_endpoint {
                if !report.scores.is_empty() {
                    let trace_id = session_id_to_trace_id(&state.session_id);
                    if let Err(e) =
                        export_eval_logs(endpoint, &trace_id, &state.case_id, &report.score_records)
                    {
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

    // ── eval.jsonl writer ─────────────────────────────────────────────────────

    fn write_eval_jsonl(lines: &[String]) -> Result<(), String> {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open("./eval.jsonl")
            .map_err(|e| e.to_string())?;

        for line in lines {
            file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }
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

// ── native unit tests of the config and scoring seam ────────────────────────
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    const TS: u64 = 1_700_000_000_000;

    fn session(exit_status: &str, turns: u32, input: u64, output: u64) -> SessionEnd {
        SessionEnd {
            total_turns: turns,
            total_input_tokens: input,
            total_output_tokens: output,
            total_tool_calls: 0,
            total_shell_calls: 0,
            duration_ms: 5_000,
            exit_status: exit_status.to_string(),
        }
    }

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The whole of `eval.jsonl` for one config, as the adapter would write it.
    fn run(config_json: &str, observed: &[&str], event: &SessionEnd) -> Vec<String> {
        let report = match parse_scorers(config_json) {
            Ok(scorers) => score_session(&scorers, &tools(observed), event, TS),
            Err(error) => config_error_report(error),
        };
        eval_jsonl_lines(&report, Some("ds-1"), Some("case-1"), TS)
    }

    /// The `dataset_run` record — always the last line — parsed back from the file text.
    fn dataset_run(lines: &[String]) -> Value {
        let last = lines.last().expect("eval.jsonl is never empty");
        let value: Value = serde_json::from_str(last).expect("the last line is valid JSON");
        assert_eq!(value["record_type"], json!("dataset_run"));
        value
    }

    // ── the four acceptance cases ───────────────────────────────────────────

    #[test]
    fn malformed_json_is_a_config_error_naming_the_env_var() {
        let lines = run("{not json", &[], &session("ok", 1, 1, 1));
        assert_eq!(
            lines.len(),
            1,
            "a config error emits no event_score records"
        );
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("config_error"));
        assert_eq!(run["config_error"]["key"], json!("MURMUR_EVAL_CONFIG"));
        assert!(run["config_error"]["message"]
            .as_str()
            .expect("a message")
            .starts_with("not valid JSON: "));
        assert_eq!(run["scores"], json!({}));
        assert_eq!(run["dataset_id"], json!("ds-1"));
        assert_eq!(run["case_id"], json!("case-1"));
    }

    #[test]
    fn a_missing_scorers_key_is_a_config_error_naming_scorers() {
        let lines = run(r#"{"dataset_id":"ds-1"}"#, &[], &session("ok", 1, 1, 1));
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("config_error"));
        assert_eq!(
            run["config_error"],
            json!({"key": "scorers", "message": "missing or not an array"})
        );
    }

    #[test]
    fn an_unknown_scorer_type_is_a_config_error_naming_its_index() {
        let config = r#"{"scorers":[{"type":"exit_ok"},{"type":"exit_okay"}]}"#;
        let lines = run(config, &[], &session("ok", 1, 1, 1));
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("config_error"));
        assert_eq!(
            run["config_error"],
            json!({
                "key": "scorers[1].type",
                "message": "unknown scorer type 'exit_okay'",
            })
        );
        // The valid scorer that preceded the typo is not kept.
        assert_eq!(run["scores"], json!({}));
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn a_valid_config_scores_and_carries_no_config_error_key() {
        let config = r#"{"scorers":[{"type":"exit_ok","name":"success"}]}"#;
        let lines = run(config, &[], &session("ok", 1, 1, 1));
        assert_eq!(lines.len(), 2);
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("pass"));
        assert_eq!(run["scores"], json!({"success": 1.0}));
        assert!(
            run.get("config_error").is_none(),
            "config_error is present only when overall is config_error"
        );
    }

    // ── no_scores, and what it does and does not mean ───────────────────────

    #[test]
    fn an_empty_scorers_array_is_no_scores_and_not_an_error() {
        let lines = run(r#"{"scorers":[]}"#, &[], &session("ok", 1, 1, 1));
        assert_eq!(lines.len(), 1);
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("no_scores"));
        assert_eq!(run["scores"], json!({}));
        assert!(run.get("config_error").is_none());
        assert_eq!(parse_scorers(r#"{"scorers":[]}"#), Ok(vec![]));
    }

    #[test]
    fn an_llm_judge_only_config_is_no_scores_and_not_an_error() {
        let config = r#"{"scorers":[{"type":"llm_judge","name":"rubric"}]}"#;
        let lines = run(config, &[], &session("ok", 1, 1, 1));
        assert_eq!(lines.len(), 1, "llm_judge emits no score record");
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("no_scores"));
        assert_eq!(run["scores"], json!({}));
        assert!(run.get("config_error").is_none());
    }

    // ── the required keys, one test per case ────────────────────────────────

    fn config_error_of(config_json: &str) -> ConfigError {
        parse_scorers(config_json).expect_err("this config must be refused")
    }

    #[test]
    fn a_non_object_config_names_what_was_found_instead() {
        assert_eq!(
            config_error_of("[]"),
            ConfigError {
                key: "MURMUR_EVAL_CONFIG".to_string(),
                message: "expected a JSON object, found an array".to_string(),
            }
        );
        assert_eq!(
            config_error_of("\"scorers\"").message,
            "expected a JSON object, found a string"
        );
        assert_eq!(
            config_error_of("null").message,
            "expected a JSON object, found null"
        );
    }

    #[test]
    fn a_scorers_key_that_is_not_an_array_is_refused() {
        assert_eq!(
            config_error_of(r#"{"scorers":{"type":"exit_ok"}}"#),
            ConfigError {
                key: "scorers".to_string(),
                message: "missing or not an array".to_string(),
            }
        );
    }

    #[test]
    fn a_scorer_without_a_string_type_is_refused() {
        assert_eq!(
            config_error_of(r#"{"scorers":[{"name":"nameless"}]}"#),
            ConfigError {
                key: "scorers[0].type".to_string(),
                message: "missing or not a string".to_string(),
            }
        );
        assert_eq!(
            config_error_of(r#"{"scorers":[{"type":7}]}"#).key,
            "scorers[0].type"
        );
        // A non-object element has no `type` either.
        assert_eq!(
            config_error_of(r#"{"scorers":["exit_ok"]}"#).key,
            "scorers[0].type"
        );
    }

    #[test]
    fn max_turns_without_max_is_refused_rather_than_defaulted_to_ten() {
        assert_eq!(
            config_error_of(r#"{"scorers":[{"type":"max_turns","name":"turns"}]}"#),
            ConfigError {
                key: "scorers[0].max".to_string(),
                message: "scorer type 'max_turns' requires a numeric 'max'".to_string(),
            }
        );
    }

    #[test]
    fn max_tokens_without_max_is_refused_rather_than_defaulted_to_a_hundred_thousand() {
        assert_eq!(
            config_error_of(r#"{"scorers":[{"type":"max_tokens"}]}"#),
            ConfigError {
                key: "scorers[0].max".to_string(),
                message: "scorer type 'max_tokens' requires a numeric 'max'".to_string(),
            }
        );
        // A non-numeric `max` is the same refusal.
        assert_eq!(
            config_error_of(r#"{"scorers":[{"type":"max_tokens","max":"lots"}]}"#).key,
            "scorers[0].max"
        );
    }

    #[test]
    fn a_max_turns_ceiling_too_wide_for_u32_is_refused_rather_than_truncated() {
        let too_wide = u64::from(u32::MAX) + 1;
        let config = format!(r#"{{"scorers":[{{"type":"max_turns","max":{too_wide}}}]}}"#);
        let error = config_error_of(&config);

        assert_eq!(error.key, "scorers[0].max");
        assert_eq!(
            error.message,
            format!("{too_wide} does not fit the u32 turn counter the session reports")
        );
        // The same value is a legitimate max_tokens ceiling.
        let tokens = format!(r#"{{"scorers":[{{"type":"max_tokens","max":{too_wide}}}]}}"#);
        assert!(parse_scorers(&tokens).is_ok());
    }

    #[test]
    fn tool_sequence_without_a_non_empty_expected_is_refused() {
        let expected_message = "scorer type 'tool_sequence' requires 'expected' to be a \
                                non-empty list of tool names";

        for config in [
            r#"{"scorers":[{"type":"tool_sequence"}]}"#,
            r#"{"scorers":[{"type":"tool_sequence","expected":"bash"}]}"#,
            r#"{"scorers":[{"type":"tool_sequence","expected":[]}]}"#,
            r#"{"scorers":[{"type":"tool_sequence","expected":[1,2]}]}"#,
        ] {
            assert_eq!(
                config_error_of(config),
                ConfigError {
                    key: "scorers[0].expected".to_string(),
                    message: expected_message.to_string(),
                },
                "config {config} must be refused"
            );
        }
    }

    // ── the reason strings, verbatim ────────────────────────────────────────

    #[test]
    fn a_passing_config_emits_one_record_per_scorer_with_its_reason() {
        let config = r#"{"scorers":[
            {"type":"exit_ok","name":"success"},
            {"type":"max_turns","name":"turns","max":5},
            {"type":"max_tokens","name":"tokens","max":1000},
            {"type":"tool_sequence","name":"order","expected":["bash","python"]}
        ]}"#;
        let lines = run(
            config,
            &["bash", "editor", "python"],
            &session("ok", 3, 400, 100),
        );
        assert_eq!(lines.len(), 5);

        let reasons: Vec<(String, String)> = lines[..4]
            .iter()
            .map(|line| {
                let v: Value = serde_json::from_str(line).expect("a score record");
                assert_eq!(v["record_type"], json!("event_score"));
                assert_eq!(v["event_type"], json!("session_end"));
                assert_eq!(v["result"], json!("pass"));
                assert_eq!(v["score"], json!(1.0));
                assert_eq!(v["ts"], json!(TS));
                assert_eq!(v["turn"], json!(3));
                (
                    v["scorer"].as_str().expect("a scorer name").to_string(),
                    v["reason"].as_str().expect("a reason").to_string(),
                )
            })
            .collect();

        assert_eq!(
            reasons,
            vec![
                ("success".to_string(), "exit_status=ok".to_string()),
                ("turns".to_string(), "turns=3 max=5".to_string()),
                ("tokens".to_string(), "tokens=500 max=1000".to_string()),
                (
                    "order".to_string(),
                    r#"observed=["bash", "editor", "python"] expected=["bash", "python"]"#
                        .to_string()
                ),
            ]
        );

        let run = dataset_run(&lines);
        assert_eq!(run["overall"], json!("pass"));
        assert_eq!(
            run["scores"],
            json!({"order": 1.0, "success": 1.0, "tokens": 1.0, "turns": 1.0})
        );
    }

    #[test]
    fn one_failing_scorer_fails_the_run_and_the_rest_still_report() {
        let config = r#"{"scorers":[
            {"type":"exit_ok","name":"success"},
            {"type":"max_turns","name":"turns","max":2},
            {"type":"max_tokens","name":"tokens","max":100}
        ]}"#;
        let lines = run(config, &[], &session("error", 9, 400, 100));
        assert_eq!(lines.len(), 4);

        let records: Vec<Value> = lines[..3]
            .iter()
            .map(|line| serde_json::from_str(line).expect("a score record"))
            .collect();
        assert_eq!(records[0]["reason"], json!("exit_status=error"));
        assert_eq!(records[1]["reason"], json!("turns=9 max=2"));
        assert_eq!(records[2]["reason"], json!("tokens=500 max=100"));
        for record in &records {
            assert_eq!(record["result"], json!("fail"));
            assert_eq!(record["score"], json!(0.0));
        }

        let run = dataset_run(&lines);
        assert_eq!(run["overall"], json!("fail"));
        assert_eq!(
            run["scores"],
            json!({"success": 0.0, "tokens": 0.0, "turns": 0.0})
        );
        assert!(run.get("config_error").is_none());
    }

    /// A partial tool_sequence match fails the run while the other scorers still pass.
    #[test]
    fn a_partial_tool_sequence_fails_with_a_fractional_score() {
        let config = r#"{"scorers":[
            {"type":"exit_ok","name":"success"},
            {"type":"tool_sequence","name":"order","expected":["bash","python","git"]}
        ]}"#;
        let lines = run(config, &["bash", "python"], &session("ok", 1, 1, 1));
        let run = dataset_run(&lines);

        assert_eq!(run["overall"], json!("fail"));
        assert_eq!(run["scores"]["order"], json!(2.0 / 3.0));
        assert_eq!(run["scores"]["success"], json!(1.0));
    }

    // ── score_tool_sequence ─────────────────────────────────────────────────

    #[test]
    fn score_tool_sequence_matches_exactly() {
        assert_eq!(
            score_tool_sequence(&tools(&["bash", "python"]), &tools(&["bash", "python"])),
            (true, 1.0)
        );
    }

    #[test]
    fn score_tool_sequence_tolerates_gaps_between_the_expected_calls() {
        assert_eq!(
            score_tool_sequence(
                &tools(&["git", "bash", "editor", "python", "git"]),
                &tools(&["bash", "python"])
            ),
            (true, 1.0)
        );
    }

    #[test]
    fn score_tool_sequence_scores_a_partial_match_as_a_fraction() {
        assert_eq!(
            score_tool_sequence(&tools(&["bash"]), &tools(&["bash", "python", "git"])),
            (false, 1.0 / 3.0)
        );
    }

    #[test]
    fn score_tool_sequence_refuses_an_out_of_order_observation() {
        // "python" then "bash" satisfies only the first expected call.
        assert_eq!(
            score_tool_sequence(&tools(&["python", "bash"]), &tools(&["bash", "python"])),
            (false, 0.5)
        );
    }

    #[test]
    fn score_tool_sequence_over_no_observed_calls_scores_zero() {
        assert_eq!(
            score_tool_sequence(&[], &tools(&["bash", "python"])),
            (false, 0.0)
        );
    }
}
