//! A relay from a capsule to the board tool `nexus transit serve` answers (ADR 0019), packaged
//! as a `wasm32-wasip2` component exporting `murmur:tool/run` (world `tool`) and importing no
//! `murmur:*` interface.
//!
//! The relay knows the envelope and nothing else. Each call wraps the model's input, the
//! entry's `config:` block (the grant), the capsule's name and the session id into one envelope
//! and POSTs it to `/v1/call` at the gateway address; nexus checks the grant, runs the verb and
//! answers, and the relay passes the answer back byte for byte. It holds no verb list and no
//! board logic, so a new verb needs a nexus release and no relay release.
//!
//! The relay never holds the key. Its bundled `murmur.yaml` declares only how serve takes one
//! (the `upstream_auth:` scheme); the operator binds the key and serve's address on this tool's
//! entry as `gateway:`; and the runtime attaches the header to the one request the relay sends
//! to the address in [`GATEWAY_ENDPOINT_ENV`].
//!
//! Every module but the `wasm_tool` adapter here compiles for the host, so `cargo test`
//! exercises it natively: the environment and the transport arrive as parameters, and nothing
//! but the adapter reads the environment or speaks `wasi:http`.

pub mod answer;
pub mod envelope;
pub mod ops;

/// Guest environment variable the runtime delivers this entry's `config:` block in, compact
/// JSON, read off this entry's own grant and no other's. Absent when the entry has no
/// `config:`; the relay then sends `config: null` and nexus refuses the call as `bad-grant`.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Guest environment variable naming the credential gateway: `http://127.0.0.1:9` plus the path
/// of the entry's `gateway.endpoint`, without a trailing `/`. Set only when the entry binds
/// `gateway:`; it carries an address, never a key.
pub const GATEWAY_ENDPOINT_ENV: &str = "MURMUR_GATEWAY_ENDPOINT";

/// Guest environment variable holding the capsule manifest's `name`, sent as the envelope's
/// `capsule`: the actor nexus records a board change under.
pub const CAPSULE_NAME_ENV: &str = "MURMUR_CAPSULE_NAME";

/// Guest environment variable holding the session id (`ses_` and 32 lowercase hex digits), sent
/// as the envelope's `session`.
pub const SESSION_ID_ENV: &str = "MURMUR_SESSION_ID";

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use murmur_wasi_http::{write_body, BodyWrite};

    use crate::ops::{self, Env, HttpReply, OpStatus, Transport};

    /// Largest response body read into memory. A board tool answer is a card list at most; a
    /// body past this is reported as no answer.
    const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The only four environment reads in the crate. Everything below this module takes
            // them as parameters, so the host tests supply them directly.
            let env = Env {
                gateway_endpoint: std::env::var(crate::GATEWAY_ENDPOINT_ENV).ok(),
                artifact_config: std::env::var(crate::ARTIFACT_CONFIG_ENV).ok(),
                capsule_name: std::env::var(crate::CAPSULE_NAME_ENV).ok(),
                session_id: std::env::var(crate::SESSION_ID_ENV).ok(),
            };
            let response = ops::run(&env, input.data.as_deref(), &mut WasiHttp);

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

    /// One blocking POST over `wasi:http/outgoing-handler`, modelled on the drivers'.
    struct WasiHttp;

    impl Transport for WasiHttp {
        fn post(
            &mut self,
            url: &str,
            headers: &[(String, String)],
            body: &[u8],
        ) -> Result<HttpReply, String> {
            let response = dispatch_request(url, headers, body)?;
            let status = response.status();
            let body = consume_body(response)?;
            Ok(HttpReply { status, body })
        }
    }

    fn dispatch_request(
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<wasip2::http::types::IncomingResponse, String> {
        let (scheme, authority, path_with_query) = split_url(url)?;

        let fields = wasip2::http::types::Fields::new();
        for (name, value) in headers {
            fields
                .append(name, value.as_bytes())
                .map_err(|err| format!("failed to set header '{name}': {err:?}"))?;
        }

        let request = wasip2::http::types::OutgoingRequest::new(fields);
        request
            .set_method(&wasip2::http::types::Method::Post)
            .map_err(|()| "failed to set method".to_string())?;
        request
            .set_scheme(Some(&scheme))
            .map_err(|()| "failed to set scheme".to_string())?;
        request
            .set_authority(Some(&authority))
            .map_err(|()| "failed to set authority".to_string())?;
        request
            .set_path_with_query(Some(&path_with_query))
            .map_err(|()| "failed to set path".to_string())?;

        let outgoing_body = request
            .body()
            .map_err(|()| "failed to acquire request body".to_string())?;
        let future = wasip2::http::outgoing_handler::handle(request, None)
            .map_err(|err| format!("failed to dispatch HTTP request: {err:?}"))?;
        match write_body(&outgoing_body, body) {
            Ok(()) => wasip2::http::types::OutgoingBody::finish(outgoing_body, None)
                .map_err(|err| format!("failed to finalize request body: {err:?}"))?,
            // The host dropped the request without reading the rest of its body; the response
            // future reports why.
            Err(BodyWrite::Closed) => drop(outgoing_body),
            Err(BodyWrite::Failed(message)) => return Err(message),
        }

        await_response(future)
    }

    fn consume_body(response: wasip2::http::types::IncomingResponse) -> Result<Vec<u8>, String> {
        let incoming_body = response
            .consume()
            .map_err(|()| "failed to consume response body".to_string())?;
        let stream = incoming_body
            .stream()
            .map_err(|()| "failed to stream response body".to_string())?;
        let mut bytes = Vec::new();
        loop {
            let chunk = read_chunk(&stream)?;
            if chunk.is_empty() {
                break;
            }
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_RESPONSE_BYTES {
                return Err(format!(
                    "response body exceeded {MAX_RESPONSE_BYTES} bytes; not reading further"
                ));
            }
        }
        drop(stream);
        let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
        Ok(bytes)
    }

    fn read_chunk(stream: &wasip2::io::streams::InputStream) -> Result<Vec<u8>, String> {
        match stream.blocking_read(16 * 1024) {
            Ok(chunk) => Ok(chunk),
            Err(wasip2::io::streams::StreamError::Closed) => Ok(Vec::new()),
            Err(err) => Err(format!("failed to read response stream: {err:?}")),
        }
    }

    fn await_response(
        future: wasip2::http::types::FutureIncomingResponse,
    ) -> Result<wasip2::http::types::IncomingResponse, String> {
        loop {
            match future.get() {
                Some(Ok(Ok(response))) => return Ok(response),
                Some(Ok(Err(err))) => {
                    return Err(format!("transport error while awaiting response: {err:?}"));
                }
                Some(Err(())) => return Err("response future already consumed".to_string()),
                None => future.subscribe().block(),
            }
        }
    }

    fn split_url(url: &str) -> Result<(wasip2::http::types::Scheme, String, String), String> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (wasip2::http::types::Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (wasip2::http::types::Scheme::Http, rest)
        } else {
            return Err(format!("URL must start with http:// or https://: '{url}'"));
        };

        let mut parts = rest.splitn(2, '/');
        let authority = parts.next().unwrap_or_default().trim().to_string();
        if authority.is_empty() {
            return Err(format!("URL has no authority: '{url}'"));
        }
        let path = match parts.next() {
            Some("") | None => "/".to_string(),
            Some(path) => format!("/{path}"),
        };
        Ok((scheme, authority, path))
    }
}

#[cfg(test)]
mod tests {
    //! Source-level invariants: the key-absence and verb-absence properties the rest of the
    //! suite cannot see from behaviour alone. Every needle is assembled with `concat!` so that
    //! this module is not its own evidence, and every file is cut at its own test module, if it
    //! has one.

    /// Every `src/*.rs` file, cut at `"\nmod tests {"` when present.
    fn raw_non_test_sources() -> Vec<(&'static str, &'static str)> {
        [
            ("lib.rs", include_str!("lib.rs")),
            ("answer.rs", include_str!("answer.rs")),
            ("envelope.rs", include_str!("envelope.rs")),
            ("ops.rs", include_str!("ops.rs")),
        ]
        .into_iter()
        .map(|(name, source)| {
            let cut = source
                .find("\nmod tests {")
                .map_or(source, |at| &source[..at]);
            (name, cut)
        })
        .collect()
    }

    /// [`raw_non_test_sources`], lowercased.
    fn non_test_sources() -> Vec<(&'static str, String)> {
        raw_non_test_sources()
            .into_iter()
            .map(|(name, source)| (name, source.to_lowercase()))
            .collect()
    }

    #[test]
    fn every_source_file_is_covered() {
        // A new module the list above does not name would escape every needle below.
        let lib = include_str!("lib.rs");
        let declared = lib
            .lines()
            .filter(|line| line.starts_with("pub mod "))
            .count();
        assert_eq!(
            declared + 1,
            non_test_sources().len(),
            "raw_non_test_sources must list lib.rs and every `pub mod` it declares"
        );
    }

    #[test]
    fn source_holds_no_credential_or_auth_header() {
        // The runtime attaches the header declared under `upstream_auth:`, so a relay-side
        // credential read or header would put the key back in the guest.
        for (name, source) in non_test_sources() {
            for needle in [
                concat!("\"author", "ization\""),
                concat!("bear", "er "),
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
    fn source_reads_exactly_the_four_variables() {
        let needle = concat!("env::", "var");
        for (name, source) in non_test_sources() {
            let expected = if name == "lib.rs" { 4 } else { 0 };
            assert_eq!(
                source.matches(needle).count(),
                expected,
                "src/{name}: only lib.rs reads the environment, once per const"
            );
        }
        let lib = include_str!("lib.rs");
        for read in [
            concat!("env::", "var(crate::ARTIFACT_CONFIG_ENV)"),
            concat!("env::", "var(crate::GATEWAY_ENDPOINT_ENV)"),
            concat!("env::", "var(crate::CAPSULE_NAME_ENV)"),
            concat!("env::", "var(crate::SESSION_ID_ENV)"),
        ] {
            assert!(lib.contains(read), "src/lib.rs must contain {read}");
        }
    }

    #[test]
    fn source_names_no_verb() {
        // Verbs live in nexus alone: a verb list here would need a relay release for a new one.
        for (name, source) in raw_non_test_sources() {
            for needle in [
                concat!("\"l", "s\""),
                concat!("\"sh", "ow\""),
                concat!("\"ad", "d\""),
                concat!("\"de", "p\""),
                concat!("\"m", "v\""),
                concat!("\"ed", "it\""),
                concat!("\"eve", "nt\""),
            ] {
                assert!(
                    !source.contains(needle),
                    "src/{name}: non-test source must not contain {needle}"
                );
            }
        }
        // `help` appears once, inside the input line's text.
        let help = concat!("\"he", "lp\"");
        let total: usize = raw_non_test_sources()
            .iter()
            .map(|(_, source)| source.matches(help).count())
            .sum();
        assert_eq!(total, 1, "{help} appears exactly once in non-test source");
        assert!(crate::answer::INPUT_NOT_JSON.contains(help));
    }

    #[test]
    fn no_target_cfg_outside_the_adapter() {
        let needle = concat!("cfg(target", "_arch");
        for (name, source) in raw_non_test_sources() {
            let expected = usize::from(name == "lib.rs");
            assert_eq!(
                source.matches(needle).count(),
                expected,
                "src/{name}: only the wasm_tool adapter is target-gated"
            );
        }
        assert!(include_str!("lib.rs").contains(concat!(
            "#[cfg(target",
            "_arch = \"wasm32\")]\nmod wasm_tool {"
        )));
    }

    #[test]
    fn manifest_declares_the_auth_scheme_the_gateway_will_read() {
        // The runtime builds the auth header from these two fields, so a dropped quote pair
        // (`value: Bearer {key}` is not the same scalar) or a renamed key leaves the gateway
        // with no scheme to apply and the launch refused.
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "tools/tool-transit-board-relay/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[test]
    fn manifest_schema_root_is_opaque_and_writes_nothing() {
        // `mv` carries a `to` naming a stage, which the `read_only` key-name rules would judge
        // as a destination; the relay writes no file.
        let manifest = include_str!("../murmur.yaml");
        assert!(
            manifest.contains(
                "input_schema:\n  type: object\n  format: murmur-opaque\n  murmur-destinations: []\n"
            ),
            "input_schema's root must be `type: object`, `format: murmur-opaque` and declare \
             `murmur-destinations: []`"
        );
        assert!(
            manifest.contains("\n  required: [verb]\n"),
            "input_schema must require verb"
        );
    }

    #[test]
    fn manifest_declares_no_capabilities_block() {
        // The relay's only egress is the gateway, which no network grant governs.
        let manifest = include_str!("../murmur.yaml");
        assert!(
            !manifest
                .lines()
                .any(|line| line.starts_with("capabilities:")),
            "murmur.yaml must not declare a capabilities: block"
        );
    }

    #[test]
    fn dispatch_request_sends_the_request_before_it_writes_the_body() {
        // Nothing drains the request body until `handle` passes the request to the host, so a
        // body written first blocks forever once it outgrows the host's buffer.
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
        let handle = position("outgoing_handler::handle(");
        let write = position("write_body(");
        let finish = position("OutgoingBody::finish(");
        let awaited = position("await_response(");
        assert!(handle < write, "handle must precede the body write");
        assert!(
            !dispatch[..handle].contains("check_write"),
            "no body write may precede handle"
        );
        assert!(write < finish, "the body is finished after it is written");
        assert!(finish < awaited, "the response is awaited after the body");
    }
}
