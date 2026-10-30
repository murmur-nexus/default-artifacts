//! Pushing a commit range, fetching a branch back, and driving pull requests through GitHub's
//! REST, Git Data and GraphQL APIs, packaged as a `wasm32-wasip2` component exporting
//! `murmur:tool/run` (world `tool`) and importing no `murmur:*` interface.
//!
//! The tool never holds the GitHub token. Its bundled `murmur.yaml` declares only how GitHub
//! takes a token (the `upstream_auth:` scheme); the operator binds the token and pins the one
//! repo this tool's entry may touch as `gateway:` plus `config:`; and the runtime, the only
//! party holding both, attaches the header to every request the tool sends to the address in
//! [`GATEWAY_ENDPOINT_ENV`].
//!
//! Everything below this file is free of `cfg(target_arch)` so `cargo test` exercises it
//! natively: the configuration, the gateway endpoint, the transport and the repository root
//! all arrive as parameters, so nothing but the adapter here reads the environment, the local
//! filesystem or speaks `wasi:http`.

pub mod config;
pub mod footer;
pub mod git;
pub mod github;
pub mod input;
pub mod ops;
pub mod trailer;

/// Guest environment variable the runtime delivers this artifact's `config:` block in,
/// compact JSON, read off this artifact's own grant and no other's.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Guest environment variable naming the credential gateway: `http://127.0.0.1:9` plus the
/// path of the entry's `gateway.endpoint`, without a trailing `/`.
///
/// The runtime sets it only when this tool's entry binds `gateway:`, and it carries an
/// address, never a token: the runtime attaches the token to requests sent to that address.
/// Absent, it is `gateway_missing` — there is no fallback destination.
pub const GATEWAY_ENDPOINT_ENV: &str = "MURMUR_GATEWAY_ENDPOINT";

/// The endpoint the operator writes as `gateway.endpoint`, assembled with `concat!` so the
/// source-invariant tests below can forbid the GitHub host appearing anywhere else in the
/// crate while still letting this operator-facing suggestion show it.
pub(crate) const SUGGESTED_ENDPOINT: &str = concat!("https://api.", "github", ".com");
/// The `${NAME}` reference the operator writes as `gateway.api_key`.
pub(crate) const SUGGESTED_KEY_REFERENCE: &str = concat!("${", "GITHUB_", "TOKEN}");
/// The `credentials.<NAME>` the reference resolves through.
pub(crate) const SUGGESTED_CREDENTIAL: &str = concat!("credentials.", "GITHUB_", "TOKEN");

#[cfg(target_arch = "wasm32")]
mod wasm_tool {
    wit_bindgen::generate!({
        path: "../../wit/guest",
        world: "tool",
        generate_all,
    });

    use std::path::Path;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

    use crate::ops::{self, HttpReply, OpStatus, Transport};

    /// Largest response body read into memory.
    const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
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
                data_path: None,
                truncated: false,
                metadata: response.metadata,
            }
        }
    }

    export!(Component);

    /// One blocking HTTP call over `wasi:http/outgoing-handler`, modelled on the drivers'.
    struct WasiHttp;

    impl Transport for WasiHttp {
        fn request(
            &mut self,
            method: &str,
            path: &str,
            headers: &[(String, String)],
            body: Option<&serde_json::Value>,
        ) -> Result<HttpReply, String> {
            let body_bytes = body
                .map(|value| value.to_string().into_bytes())
                .unwrap_or_default();
            // `path` is always a `/`-rooted, relative path (see `crate::github`); the gateway
            // endpoint the runtime hands the guest is the only authority ever dialled.
            let gateway = std::env::var(crate::GATEWAY_ENDPOINT_ENV)
                .map_err(|_| "gateway endpoint not set".to_string())?;
            let url = format!("{}{}", gateway.trim_end_matches('/'), path);
            let response = dispatch_request(method, &url, headers, &body_bytes)?;
            let status = response.status();
            let location = response
                .headers()
                .get("location")
                .into_iter()
                .next()
                .map(|value| String::from_utf8_lossy(&value).into_owned());
            let body = consume_body(response)?;
            Ok(HttpReply {
                status,
                location,
                body,
            })
        }
    }

    fn dispatch_request(
        method: &str,
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
            .set_method(&parse_method(method))
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

    /// Why a request body was not written whole.
    enum BodyWrite {
        /// The host closed the body stream.
        Closed,
        /// Any other failure, as its error message.
        Failed(String),
    }

    /// Stream `body` into `outgoing_body`, after `outgoing-handler.handle`: nothing drains the
    /// host's bounded body buffer before then. The output stream is a child of `outgoing_body`
    /// and drops when this returns, before the caller finishes or drops its parent.
    fn write_body(
        outgoing_body: &wasip2::http::types::OutgoingBody,
        body: &[u8],
    ) -> Result<(), BodyWrite> {
        let stream = outgoing_body
            .write()
            .map_err(|()| BodyWrite::Failed("failed to open request body stream".to_string()))?;
        let mut remaining: &[u8] = body;
        while !remaining.is_empty() {
            let budget = stream
                .check_write()
                .map_err(|e| body_write(e, |e| format!("check-write failed: {e:?}")))?
                as usize;
            if budget == 0 {
                stream.subscribe().block();
                continue;
            }
            let n = budget.min(remaining.len());
            stream
                .write(&remaining[..n])
                .map_err(|e| body_write(e, |e| format!("write failed: {e:?}")))?;
            remaining = &remaining[n..];
        }
        stream
            .flush()
            .map_err(|e| body_write(e, |e| format!("flush failed: {e:?}")))?;
        stream.subscribe().block();
        Ok(())
    }

    /// [`BodyWrite::Closed`] for a closed stream, otherwise `failed`'s message.
    fn body_write(
        err: wasip2::io::streams::StreamError,
        failed: fn(wasip2::io::streams::StreamError) -> String,
    ) -> BodyWrite {
        match err {
            wasip2::io::streams::StreamError::Closed => BodyWrite::Closed,
            err => BodyWrite::Failed(failed(err)),
        }
    }

    fn parse_method(method: &str) -> wasip2::http::types::Method {
        match method {
            "GET" => wasip2::http::types::Method::Get,
            "POST" => wasip2::http::types::Method::Post,
            "PATCH" => wasip2::http::types::Method::Patch,
            "PUT" => wasip2::http::types::Method::Put,
            "DELETE" => wasip2::http::types::Method::Delete,
            other => wasip2::http::types::Method::Other(other.to_string()),
        }
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
    //! Source-level invariants the rest of the suite cannot see from behaviour alone.

    fn non_test_sources() -> Vec<(&'static str, String)> {
        [
            ("lib.rs", include_str!("lib.rs")),
            ("config.rs", include_str!("config.rs")),
            ("footer.rs", include_str!("footer.rs")),
            ("input.rs", include_str!("input.rs")),
            ("ops.rs", include_str!("ops.rs")),
            ("trailer.rs", include_str!("trailer.rs")),
            ("git/mod.rs", include_str!("git/mod.rs")),
            ("git/date.rs", include_str!("git/date.rs")),
            ("git/object.rs", include_str!("git/object.rs")),
            ("git/pack.rs", include_str!("git/pack.rs")),
            ("git/rebuild.rs", include_str!("git/rebuild.rs")),
            ("git/store.rs", include_str!("git/store.rs")),
            ("git/walk.rs", include_str!("git/walk.rs")),
            ("git/write.rs", include_str!("git/write.rs")),
            ("github/mod.rs", include_str!("github/mod.rs")),
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
    fn source_reads_only_the_config_and_gateway_endpoint_variables() {
        let needle = concat!("env::", "var");
        let count: usize = non_test_sources()
            .iter()
            .map(|(_, source)| source.matches(needle).count())
            .sum();
        // lib.rs reads ARTIFACT_CONFIG_ENV and GATEWAY_ENDPOINT_ENV once each in `run`, plus
        // GATEWAY_ENDPOINT_ENV again in the wasm-only transport to build the request URL.
        assert_eq!(
            count, 3,
            "exactly three environment reads, none of them a credential"
        );
        let lib = include_str!("lib.rs");
        for read in [
            concat!("env::", "var(crate::ARTIFACT_CONFIG_ENV)"),
            concat!("env::", "var(crate::GATEWAY_ENDPOINT_ENV)"),
        ] {
            assert!(lib.contains(read), "src/lib.rs must contain {read}");
        }
    }

    #[test]
    fn source_never_reads_a_github_credential_or_builds_an_auth_header() {
        for (name, source) in non_test_sources() {
            for needle in [
                concat!("\"author", "ization\""),
                concat!("bear", "er "),
                concat!("github_", "token"),
                concat!("gh_", "token"),
            ] {
                assert!(
                    !source.contains(needle),
                    "src/{name}: non-test source must not contain {needle}"
                );
            }
        }
    }

    #[test]
    fn source_never_names_the_github_host_outside_the_suggested_endpoint_constant() {
        let needle = concat!("api.github", ".com");
        for (name, source) in non_test_sources() {
            if name == "lib.rs" {
                // SUGGESTED_ENDPOINT is assembled with concat! precisely so this needle does
                // not appear literally; nothing else in lib.rs may name the host either.
                continue;
            }
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
            "tools/tool-github/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[test]
    fn source_and_manifest_never_name_nexus() {
        // Assembled with concat! so this test's own text does not trip a grep for the name.
        let needle = concat!("nex", "us");
        for (name, source) in non_test_sources() {
            assert!(
                !source.contains(needle),
                "src/{name}: non-test source must not contain {needle}"
            );
        }
        assert!(
            !include_str!("../murmur.yaml")
                .to_lowercase()
                .contains(needle),
            "tools/tool-github/murmur.yaml must not contain {needle}"
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
