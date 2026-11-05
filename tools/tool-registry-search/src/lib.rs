//! Search a Murmur artifact index, packaged as a `wasm32-wasip2` component exporting
//! `murmur:tool/run` (world `tool`) and importing no `murmur:*` interface.
//!
//! An `http(s)://` index is fetched over `wasi:http`, so the runtime checks every request, each
//! redirect hop included, against the network allow-list the tool runs under. A file index opens
//! in the work folder, or, when absolute, inside a host folder the session opened.
//!
//! Everything outside `mod wasm_tool` is free of `cfg(target_arch)` so `cargo test` exercises
//! it natively: the transport and the host roots arrive as parameters, so nothing but the
//! adapter lists preopens or speaks `wasi:http`.

pub mod fetch;
pub mod paths;
pub mod search;

use serde_json::{json, Value};

use crate::fetch::Http;

/// How one call ended, as the runtime reads a tool's `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
    Passed,
    Failed,
    Error,
}

/// What one call hands the runtime: the fields of `ToolResult` this tool fills. The rest are
/// fixed — no `data_path`, never `truncated`, no `metadata`, no attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: OutcomeStatus,
    pub summary: Option<String>,
    /// Compact JSON of the result object's `data`, or `None` when it is null.
    pub data: Option<String>,
}

/// Runs one call on `ToolInput.data`.
///
/// `data` is parsed as JSON, falling back to the text itself as a string, and wrapped as
/// `{"data": …}`; no `data` at all is `{}`, which fails as `missing data field`. The result
/// object [`search::run`] returns is then mapped by [`outcome_of`].
pub fn call(data: Option<&str>, http: &mut dyn Http, host_roots: &[String]) -> Outcome {
    let raw = match data {
        Some(d) => {
            let value =
                serde_json::from_str::<Value>(d).unwrap_or_else(|_| Value::String(d.to_string()));
            json!({ "data": value }).to_string()
        }
        None => "{}".to_string(),
    };
    outcome_of(&search::run(&raw, http, host_roots))
}

/// The [`Outcome`] the runtime built from a native tool's output object, field for field:
/// `status` from `"status"` (`passed`, `failed`, anything else `error`), `summary` from
/// `"summary"`, and `data` the string `"data"` as is, else the compact JSON of a non-null
/// `"data"`, else `None`. `error_kind`, `message`, `ok` and the top-level mirror of `data` are
/// dropped here, as the runtime dropped them.
pub fn outcome_of(result: &Value) -> Outcome {
    let status = match result.get("status").and_then(Value::as_str) {
        Some("passed") => OutcomeStatus::Passed,
        Some("failed") => OutcomeStatus::Failed,
        _ => OutcomeStatus::Error,
    };
    let summary = result
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string);
    let data = match result.get("data") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    };
    Outcome {
        status,
        summary,
        data,
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use wasip2::filesystem::preopens;
    use wasip2::http::types::{
        ErrorCode, Fields, FutureIncomingResponse, IncomingResponse, Method, OutgoingRequest,
        RequestOptions, Scheme,
    };
    use wasip2::io::streams::StreamError;

    use crate::fetch::{is_followed_redirect, Http, HttpFailure, HttpReply, HTTP_TIMEOUT};
    use crate::paths::HOST_ROOT_NAME;
    use crate::search::MAX_INDEX_BYTES;
    use crate::{call, OutcomeStatus};

    struct Component;

    /// The names of the host roots murmur opened for this call, in preopen order. Each
    /// descriptor is dropped here, so none outlives the call.
    fn host_roots() -> Vec<String> {
        preopens::get_directories()
            .into_iter()
            .filter_map(|(descriptor, name)| {
                drop(descriptor);
                name.starts_with(HOST_ROOT_NAME).then_some(name)
            })
            .collect()
    }

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            let outcome = call(input.data.as_deref(), &mut WasiHttp, &host_roots());
            ToolResult {
                status: match outcome.status {
                    OutcomeStatus::Passed => Status::Passed,
                    OutcomeStatus::Failed => Status::Failed,
                    OutcomeStatus::Error => Status::Error,
                },
                summary: outcome.summary,
                data: outcome.data,
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: Vec::new(),
            }
        }
    }

    export!(Component);

    /// One blocking GET over `wasi:http/outgoing-handler`, sent with no body.
    struct WasiHttp;

    impl Http for WasiHttp {
        fn get(&mut self, url: &str) -> Result<HttpReply, HttpFailure> {
            let (scheme, authority, path_with_query) =
                split_url(url).map_err(HttpFailure::Transport)?;
            let failure = |code: ErrorCode| match code {
                ErrorCode::HttpRequestDenied => HttpFailure::Denied {
                    host: authority.clone(),
                },
                other => HttpFailure::Transport(format!("{other:?}")),
            };

            let request = OutgoingRequest::new(Fields::new());
            let set = |ok: Result<(), ()>, what: &str| {
                ok.map_err(|()| HttpFailure::Transport(format!("failed to set the {what}")))
            };
            set(request.set_method(&Method::Get), "method")?;
            set(request.set_scheme(Some(&scheme)), "scheme")?;
            set(request.set_authority(Some(&authority)), "authority")?;
            set(request.set_path_with_query(Some(&path_with_query)), "path")?;

            // A host that refuses a timeout setter keeps its own default for it.
            let options = RequestOptions::new();
            let timeout = Some(HTTP_TIMEOUT.as_nanos() as u64);
            let _ = options.set_connect_timeout(timeout);
            let _ = options.set_first_byte_timeout(timeout);
            let _ = options.set_between_bytes_timeout(timeout);

            let future =
                wasip2::http::outgoing_handler::handle(request, Some(options)).map_err(failure)?;
            let response = await_response(future).map_err(|e| match e {
                Awaited::Code(code) => failure(code),
                Awaited::Other(message) => HttpFailure::Transport(message),
            })?;

            let status = response.status();
            let location = response
                .headers()
                .get("location")
                .into_iter()
                .next()
                .map(|value| String::from_utf8_lossy(&value).into_owned());
            if is_followed_redirect(status) && location.is_some() {
                return Ok(HttpReply {
                    status,
                    location,
                    body: Vec::new(),
                });
            }
            let body = consume_body(response)?;
            Ok(HttpReply {
                status,
                location,
                body,
            })
        }
    }

    enum Awaited {
        Code(ErrorCode),
        Other(String),
    }

    fn await_response(future: FutureIncomingResponse) -> Result<IncomingResponse, Awaited> {
        loop {
            match future.get() {
                Some(Ok(Ok(response))) => return Ok(response),
                Some(Ok(Err(code))) => return Err(Awaited::Code(code)),
                Some(Err(())) => {
                    return Err(Awaited::Other(
                        "response future already consumed".to_string(),
                    ))
                }
                None => future.subscribe().block(),
            }
        }
    }

    /// The response body, read until the stream closes or past [`MAX_INDEX_BYTES`].
    fn consume_body(response: IncomingResponse) -> Result<Vec<u8>, HttpFailure> {
        let transport = |what: String| HttpFailure::Transport(what);
        let incoming_body = response
            .consume()
            .map_err(|()| transport("failed to consume the response body".to_string()))?;
        let stream = incoming_body
            .stream()
            .map_err(|()| transport("failed to stream the response body".to_string()))?;
        let mut bytes = Vec::new();
        loop {
            match stream.blocking_read(16 * 1024) {
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(StreamError::Closed) => break,
                Err(err) => return Err(transport(format!("failed to read the response: {err:?}"))),
            }
            if bytes.len() as u64 > MAX_INDEX_BYTES {
                return Err(HttpFailure::TooLarge);
            }
        }
        drop(stream);
        let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
        Ok(bytes)
    }

    /// The scheme, authority and path-with-query of an absolute `http(s)://` URL; a fragment is
    /// dropped, and an empty path is `/`.
    fn split_url(url: &str) -> Result<(Scheme, String, String), String> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(format!("{url} is not an http:// or https:// URL"));
        };
        let rest = rest.split('#').next().unwrap_or_default();
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if authority.is_empty() {
            return Err(format!("{url} names no host"));
        }
        let path_with_query = match &rest[authority_end..] {
            "" => "/".to_string(),
            query if query.starts_with('?') => format!("/{query}"),
            path => path.to_string(),
        };
        Ok((scheme, authority.to_string(), path_with_query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{HttpFailure, HttpReply};

    struct NoNetwork;

    impl Http for NoNetwork {
        fn get(&mut self, url: &str) -> Result<HttpReply, HttpFailure> {
            panic!("unexpected GET {url}")
        }
    }

    fn call_with(data: Option<&str>) -> Outcome {
        call(data, &mut NoNetwork, &[])
    }

    #[test]
    fn no_data_is_missing_data_field() {
        let outcome = call_with(None);
        assert_eq!(outcome.status, OutcomeStatus::Error);
        assert_eq!(outcome.summary.as_deref(), Some("missing data field"));
        assert_eq!(outcome.data, None);
    }

    #[test]
    fn object_data_runs_the_search_and_keeps_only_data() {
        let outcome = call_with(Some(
            r#"{"query":"tool-editor","index":"tests/fixtures/artifacts-index.json"}"#,
        ));
        assert_eq!(outcome.status, OutcomeStatus::Passed);
        assert_eq!(outcome.summary.as_deref(), Some("1 result(s)"));
        let data: Value = serde_json::from_str(outcome.data.as_deref().unwrap()).unwrap();
        assert_eq!(data["count"], 1);
        assert_eq!(data["results"][0]["name"], "murmur/tool-editor");
        assert!(data.get("ok").is_none() && data.get("message").is_none());
    }

    #[test]
    fn a_json_string_is_double_encoded_data() {
        let inner = r#"{"query":"tool-editor","index":"tests/fixtures/artifacts-index.json"}"#;
        let outcome = call_with(Some(&Value::String(inner.into()).to_string()));
        assert_eq!(outcome.status, OutcomeStatus::Passed, "{outcome:?}");
    }

    #[test]
    fn text_that_is_not_json_is_taken_as_a_string() {
        let outcome = call_with(Some("{nope"));
        assert_eq!(outcome.status, OutcomeStatus::Error);
        assert!(outcome
            .summary
            .unwrap()
            .starts_with("invalid data JSON string: "));
    }

    #[test]
    fn a_refusal_carries_its_message_as_summary_and_no_data() {
        let outcome = call_with(Some(r#"{"query":"x","index":"no/such/i.json"}"#));
        assert_eq!(outcome.status, OutcomeStatus::Error);
        assert!(outcome
            .summary
            .unwrap()
            .starts_with("failed to read no/such/i.json: "));
        assert_eq!(outcome.data, None);
    }

    #[test]
    fn outcome_of_maps_like_the_runtime_mapped_native_output() {
        let cases = [
            (
                json!({"status": "passed", "summary": "s", "data": {"a": 1}}),
                OutcomeStatus::Passed,
                Some("s"),
                Some(r#"{"a":1}"#),
            ),
            (
                json!({"status": "failed", "data": "text"}),
                OutcomeStatus::Failed,
                None,
                Some("text"),
            ),
            (
                json!({"status": "error", "summary": "m", "data": null, "error_kind": "io_error"}),
                OutcomeStatus::Error,
                Some("m"),
                None,
            ),
            (
                json!({"status": "weird", "summary": 3, "data": [1]}),
                OutcomeStatus::Error,
                None,
                Some("[1]"),
            ),
            (json!({}), OutcomeStatus::Error, None, None),
        ];
        for (result, status, summary, data) in cases {
            let outcome = outcome_of(&result);
            assert_eq!(outcome.status, status, "{result}");
            assert_eq!(outcome.summary.as_deref(), summary, "{result}");
            assert_eq!(outcome.data.as_deref(), data, "{result}");
        }
    }
}
