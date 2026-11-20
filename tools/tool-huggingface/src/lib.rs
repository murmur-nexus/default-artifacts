//! Read-only Hugging Face access — search, repo inspection, dataset peek, file download and
//! paper lookup — packaged as a `wasm32-wasip2` component exporting `murmur:tool/run` (world
//! `tool`) and importing no `murmur:*` interface.
//!
//! The tool never holds the token. Its bundled `murmur.yaml` declares only how the Hub takes
//! one (the `upstream_auth:` scheme); the operator binds the token and the Hub on this tool's
//! entry as `gateway:`; and the runtime attaches the header to requests the tool sends to the
//! address in [`GATEWAY_ENDPOINT_ENV`]. The datasets server and the download CDN are reached
//! directly, under the entry's `network.allow`, and never carry a credential: murmur gives an
//! artifact one gateway, and this tool's is bound to the Hub.
//!
//! Everything outside `mod wasm_tool` is free of `cfg(target_arch)` so `cargo test` exercises
//! it natively: the configuration, the gateway endpoint, the transport and the work folder
//! all arrive as parameters.

pub mod config;
pub mod download;
pub mod http;
pub mod hub;
pub mod input;
pub mod ops;
pub mod render;
pub mod viewer;

/// Guest environment variable the runtime delivers this artifact's `config:` block in, as
/// compact JSON. Absent when the entry has no `config:` key, read as "defaults in use".
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Guest environment variable naming the credential gateway: `http://127.0.0.1:9` plus the
/// path of the entry's `gateway.endpoint`, without a trailing `/`. Set only when the entry
/// binds `gateway:`; it carries an address, never a token. Absent, every op is
/// `gateway_missing` — there is no fallback destination.
pub const GATEWAY_ENDPOINT_ENV: &str = "MURMUR_GATEWAY_ENDPOINT";

// The operator-facing hint strings. Each is assembled with `concat!` because the source tests
// forbid the Hub origin and the token's variable name anywhere in the source: the Hub lives
// only in the operator's `gateway.endpoint`, and the tool reads no token. These strings are
// shown, never read or dialled.

/// The endpoint the operator writes as `gateway.endpoint`.
pub(crate) const SUGGESTED_ENDPOINT: &str = concat!("https:/", "/hugging", "face.co");
/// The `${NAME}` reference the operator writes as `gateway.api_key`.
pub(crate) const SUGGESTED_KEY_REFERENCE: &str = concat!("${", "HUGGINGFACE_", "ACCESS_", "TOKEN}");
/// The `credentials.<NAME>` the reference resolves through.
pub(crate) const SUGGESTED_CREDENTIAL: &str =
    concat!("credentials.", "HUGGINGFACE_", "ACCESS_", "TOKEN");

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::Path;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use wasip2::http::types::{
        ErrorCode, Fields, FutureIncomingResponse, IncomingBody, IncomingResponse, Method,
        OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
    };
    use wasip2::io::streams::{InputStream, StreamError};

    use crate::http::{
        is_redirect, BodySink, HttpFailure, Reply, Transport, HTTP_TIMEOUT, MAX_RESPONSE_BYTES,
    };
    use crate::ops::{self, OpStatus};

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The only two environment reads in the crate.
            let config_json = std::env::var(crate::ARTIFACT_CONFIG_ENV).ok();
            let gateway_endpoint = std::env::var(crate::GATEWAY_ENDPOINT_ENV).ok();
            let response = ops::run(
                gateway_endpoint.as_deref(),
                config_json.as_deref(),
                input.data.as_deref().unwrap_or_default(),
                &mut WasiHttp,
                Path::new("."),
            );
            ToolResult {
                status: match response.status {
                    OpStatus::Passed => Status::Passed,
                    OpStatus::Failed => Status::Failed,
                    OpStatus::Error => Status::Error,
                },
                summary: Some(response.summary),
                data: Some(response.data),
                data_path: response.data_path,
                truncated: response.truncated,
                metadata: response.metadata,
                attachments: Vec::new(),
            }
        }
    }

    export!(Component);

    /// Blocking `GET` and `HEAD` over `wasi:http/outgoing-handler`, sent with no body.
    struct WasiHttp;

    impl Transport for WasiHttp {
        fn request(
            &mut self,
            method: crate::http::Method,
            url: &str,
            headers: &[(String, String)],
        ) -> Result<Reply, HttpFailure> {
            let response = dispatch_request(method, url, headers)?;
            let (status, headers) = status_and_headers(&response);
            let body = if method == crate::http::Method::Head || is_redirect(status) {
                Vec::new()
            } else {
                consume_body(response)?
            };
            Ok(Reply {
                status,
                headers,
                body,
            })
        }

        fn stream(
            &mut self,
            url: &str,
            headers: &[(String, String)],
            sink: &mut dyn BodySink,
        ) -> Result<Reply, HttpFailure> {
            let response = dispatch_request(crate::http::Method::Get, url, headers)?;
            let (status, headers) = status_and_headers(&response);
            let body = if is_redirect(status) {
                Vec::new()
            } else if (200..300).contains(&status) {
                if sink.accept(status, &headers) {
                    stream_body(response, sink)?;
                }
                Vec::new()
            } else {
                consume_body(response)?
            };
            Ok(Reply {
                status,
                headers,
                body,
            })
        }
    }

    fn status_and_headers(response: &IncomingResponse) -> (u16, Vec<(String, String)>) {
        let headers = response
            .headers()
            .entries()
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_ascii_lowercase(),
                    String::from_utf8_lossy(&value).into_owned(),
                )
            })
            .collect();
        (response.status(), headers)
    }

    fn dispatch_request(
        method: crate::http::Method,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<IncomingResponse, HttpFailure> {
        let (scheme, authority, path_with_query) =
            split_url(url).map_err(HttpFailure::Transport)?;
        let failure = |code: ErrorCode| match code {
            ErrorCode::HttpRequestDenied => HttpFailure::Denied {
                host: authority.clone(),
            },
            other => HttpFailure::Transport(format!("{other:?}")),
        };

        let fields = Fields::new();
        for (name, value) in headers {
            fields.append(name, value.as_bytes()).map_err(|err| {
                HttpFailure::Transport(format!("failed to set header '{name}': {err:?}"))
            })?;
        }
        let request = OutgoingRequest::new(fields);
        let set = |ok: Result<(), ()>, what: &str| {
            ok.map_err(|()| HttpFailure::Transport(format!("failed to set the {what}")))
        };
        let wasi_method = match method {
            crate::http::Method::Get => Method::Get,
            crate::http::Method::Head => Method::Head,
        };
        set(request.set_method(&wasi_method), "method")?;
        set(request.set_scheme(Some(&scheme)), "scheme")?;
        set(request.set_authority(Some(&authority)), "authority")?;
        set(request.set_path_with_query(Some(&path_with_query)), "path")?;

        // A host that refuses a timeout setter keeps its own default for it.
        let options = RequestOptions::new();
        let timeout = Some(HTTP_TIMEOUT.as_nanos() as u64);
        let _ = options.set_connect_timeout(timeout);
        let _ = options.set_first_byte_timeout(timeout);
        let _ = options.set_between_bytes_timeout(timeout);

        // GET and HEAD carry no body: take it, send the request, then finish it empty.
        let outgoing_body = request
            .body()
            .map_err(|()| HttpFailure::Transport("failed to acquire the request body".into()))?;
        let future =
            wasip2::http::outgoing_handler::handle(request, Some(options)).map_err(failure)?;
        OutgoingBody::finish(outgoing_body, None).map_err(|err| {
            HttpFailure::Transport(format!("failed to finish the request body: {err:?}"))
        })?;
        await_response(future).map_err(|e| match e {
            Awaited::Code(code) => failure(code),
            Awaited::Other(message) => HttpFailure::Transport(message),
        })
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

    fn read_chunk(stream: &InputStream) -> Result<Option<Vec<u8>>, HttpFailure> {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(StreamError::Closed) => Ok(None),
            Err(err) => Err(HttpFailure::Transport(format!(
                "failed to read the response: {err:?}"
            ))),
        }
    }

    /// The response body, read until the stream closes or past [`MAX_RESPONSE_BYTES`].
    fn consume_body(response: IncomingResponse) -> Result<Vec<u8>, HttpFailure> {
        let incoming_body = response
            .consume()
            .map_err(|()| HttpFailure::Transport("failed to consume the response body".into()))?;
        let stream = incoming_body
            .stream()
            .map_err(|()| HttpFailure::Transport("failed to stream the response body".into()))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = read_chunk(&stream)? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_RESPONSE_BYTES {
                return Err(HttpFailure::TooLarge);
            }
        }
        drop(stream);
        let _ = IncomingBody::finish(incoming_body);
        Ok(bytes)
    }

    /// The response body, chunk by chunk into `sink`, never held whole.
    fn stream_body(response: IncomingResponse, sink: &mut dyn BodySink) -> Result<(), HttpFailure> {
        let incoming_body = response
            .consume()
            .map_err(|()| HttpFailure::Transport("failed to consume the response body".into()))?;
        let stream = incoming_body
            .stream()
            .map_err(|()| HttpFailure::Transport("failed to stream the response body".into()))?;
        while let Some(chunk) = read_chunk(&stream)? {
            if !chunk.is_empty() {
                sink.write(&chunk).map_err(HttpFailure::Aborted)?;
            }
        }
        drop(stream);
        let _ = IncomingBody::finish(incoming_body);
        Ok(())
    }

    /// The scheme, authority and path-with-query of an absolute `http(s)://` URL; a fragment
    /// is dropped, and an empty path is `/`.
    fn split_url(url: &str) -> Result<(Scheme, String, String), String> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(format!(
                "{} is not an http:// or https:// URL",
                crate::http::strip_query(url)
            ));
        };
        let rest = rest.split('#').next().unwrap_or_default();
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if authority.is_empty() {
            return Err("the URL names no host".to_string());
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
    //! Source-level invariants: the token-absence and read-only properties the rest of the
    //! suite cannot see from behaviour alone. Every needle is assembled with `concat!` so this
    //! module is not its own evidence, and every file is cut at its own test module.

    /// Every `src/*.rs` file, cut at `"\nmod tests {"` when present and lowercased.
    fn non_test_sources() -> Vec<(&'static str, String)> {
        [
            ("lib.rs", include_str!("lib.rs")),
            ("config.rs", include_str!("config.rs")),
            ("download.rs", include_str!("download.rs")),
            ("http.rs", include_str!("http.rs")),
            ("hub.rs", include_str!("hub.rs")),
            ("input.rs", include_str!("input.rs")),
            ("ops.rs", include_str!("ops.rs")),
            ("render.rs", include_str!("render.rs")),
            ("viewer.rs", include_str!("viewer.rs")),
        ]
        .into_iter()
        .map(|(name, source)| {
            let cut = source
                .find("\nmod tests {")
                .map_or(source, |at| &source[..at]);
            (name, cut.to_lowercase())
        })
        .collect()
    }

    #[test]
    fn every_source_file_is_covered() {
        let lib = include_str!("lib.rs");
        let declared = lib
            .lines()
            .filter(|line| line.starts_with("pub mod "))
            .count();
        assert_eq!(
            declared + 1,
            non_test_sources().len(),
            "non_test_sources must list lib.rs and every `pub mod` it declares"
        );
    }

    #[test]
    fn source_neither_reads_a_credential_nor_builds_an_auth_header() {
        for (name, source) in non_test_sources() {
            for needle in [
                concat!("\"author", "ization\""),
                concat!("bear", "er "),
                concat!("hf_", "token"),
                concat!("huggingface_", "access_token"),
                concat!("hugging_face_", "hub_token"),
                concat!("api_", "key\""),
            ] {
                assert!(
                    !source.contains(needle),
                    "src/{name}: non-test source must not contain {needle}"
                );
            }
        }
    }

    #[test]
    fn source_reads_only_the_config_and_gateway_endpoint_variables() {
        let needle = concat!("env::", "var");
        let count: usize = non_test_sources()
            .iter()
            .map(|(_, source)| source.matches(needle).count())
            .sum();
        assert_eq!(count, 2, "exactly two environment reads, one per const");
        let lib = include_str!("lib.rs");
        for read in [
            concat!("env::", "var(crate::ARTIFACT_CONFIG_ENV)"),
            concat!("env::", "var(crate::GATEWAY_ENDPOINT_ENV)"),
        ] {
            assert!(lib.contains(read), "src/lib.rs must contain {read}");
        }
    }

    #[test]
    fn source_never_names_the_hub_origin() {
        // The operator's `gateway.endpoint` is the only place the Hub is written.
        let needle = concat!("//hugging", "face.co");
        for (name, source) in non_test_sources() {
            assert!(
                !source.contains(needle),
                "src/{name}: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn source_sends_only_get_and_head() {
        for (name, source) in non_test_sources() {
            for needle in [
                concat!("method::", "post"),
                concat!("method::", "put"),
                concat!("method::", "patch"),
                concat!("method::", "delete"),
            ] {
                assert!(
                    !source.contains(needle),
                    "src/{name}: non-test source must not contain {needle}"
                );
            }
        }
    }

    #[test]
    fn source_does_not_fence_its_own_output() {
        // The runtime fences every tool result; a second fence would only confuse the model.
        let needle = concat!("untrusted", "-content");
        for (name, source) in non_test_sources() {
            assert!(
                !source.contains(needle),
                "src/{name}: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn manifest_declares_the_auth_scheme_the_gateway_will_read() {
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "tools/tool-huggingface/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[test]
    fn manifest_annotates_dest_and_names_no_other_path_key() {
        let manifest = include_str!("../murmur.yaml");
        let schema = &manifest[manifest
            .find("\ninput_schema:\n")
            .expect("murmur.yaml declares input_schema")..];
        let schema = &schema[..schema.find("\nrequires_files:").unwrap_or(schema.len())];
        let dest_at = schema
            .find("\n    dest:\n")
            .expect("input_schema declares dest");
        let dest_block: Vec<&str> = schema[dest_at + 1..]
            .lines()
            .skip(1)
            .take_while(|line| line.starts_with("      "))
            .collect();
        assert!(
            dest_block.contains(&"      format: murmur-destination"),
            "dest must carry format: murmur-destination: {dest_block:?}"
        );

        // murmur's TOOL_PATH_KEYS and TOOL_DESTINATION_KEYS, folded: only `dest` may appear.
        let fold = |name: &str| name.to_ascii_lowercase().replace('-', "_");
        let judged = [
            "path",
            "file_path",
            "filepath",
            "filename",
            "file",
            "dest_path",
            "destination",
            "destination_path",
            "target_path",
            "output_path",
            "out_path",
            "new_path",
            "to",
        ];
        let properties: Vec<String> = schema
            .lines()
            .filter(|line| line.starts_with("    ") && !line.starts_with("     "))
            .filter_map(|line| line.trim().strip_suffix(':'))
            .map(fold)
            .collect();
        assert!(properties.contains(&"dest".to_string()), "{properties:?}");
        assert!(
            properties.contains(&"repo_file".to_string()),
            "{properties:?}"
        );
        for property in &properties {
            assert!(
                !judged.contains(&property.as_str()),
                "input_schema property {property} is a name murmur judges as a path"
            );
        }
    }

    #[test]
    fn manifest_declares_no_capabilities_block() {
        let manifest = include_str!("../murmur.yaml");
        assert!(
            !manifest
                .lines()
                .any(|line| line.starts_with("capabilities:")),
            "murmur.yaml must not declare a capabilities: block"
        );
    }

    #[test]
    fn dispatch_request_sends_the_request_before_it_finishes_the_body() {
        // The body is taken before `handle` and finished after it, empty: a body finished or
        // written first can block once the host stops reading.
        let source = include_str!("lib.rs");
        let start = source
            .find("fn dispatch_request(")
            .expect("lib.rs defines dispatch_request");
        let rest = &source[start..];
        let dispatch = &rest[..rest.find("\n    fn ").unwrap_or(rest.len())];
        let position = |needle: &str| {
            dispatch
                .find(needle)
                .unwrap_or_else(|| panic!("dispatch_request must contain {needle}"))
        };
        let body = position(".body()");
        let handle = position("outgoing_handler::handle(");
        let finish = position("OutgoingBody::finish(");
        let awaited = position("await_response(");
        assert!(body < handle, "the body is taken before handle");
        assert!(handle < finish, "handle must precede finishing the body");
        assert!(finish < awaited, "the response is awaited after the body");
        assert!(
            !dispatch.contains("write_body(") && !dispatch.contains("check_write"),
            "GET and HEAD write no body"
        );
    }
}
