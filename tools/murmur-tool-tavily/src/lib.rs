//! Web search through Tavily's `POST /search`, packaged as a `wasm32-wasip2` component
//! exporting `murmur:tool/run` (world `tool`) and importing no `murmur:*` interface.
//!
//! The tool never holds the Tavily key. Its bundled `murmur.yaml` declares only how Tavily
//! takes a key (the `upstream_auth:` scheme); the operator binds the key and the upstream
//! on this tool's entry as `gateway:`; and the runtime, the only party holding both, attaches
//! the header to the one request the tool sends to the address in
//! [`GATEWAY_ENDPOINT_ENV`]. Every cost- and size-bearing lever is the operator's, in the
//! entry's `config:` block; the agent supplies the query and may only lower the result count.
//!
//! Everything below this file is free of `cfg(target_arch)` so `cargo test` exercises it
//! natively: the configuration, the gateway endpoint, the transport and the spill directory
//! all arrive as parameters, so nothing but the adapter here reads the environment or
//! speaks `wasi:http`.

pub mod config;
pub mod input;
pub mod ops;
pub mod render;
pub mod request;

/// Guest environment variable the runtime delivers this artifact's `config:` block in,
/// compact JSON, read off this artifact's own grant and no other's.
///
/// It is runtime-owned: the runtime injects it ahead of the manifest's
/// `capabilities.env.allow` allowlist and the allowlist builder skips the name, so no host
/// value can reach the guest under it and no capability declares it. An artifact entry with
/// no `config:` key gets no variable at all, which this tool reads as "defaults in use".
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Guest environment variable naming the credential gateway: `http://127.0.0.1:9` plus the
/// path of the entry's `gateway.endpoint`, without a trailing `/`.
///
/// The runtime sets it only when this tool's entry binds `gateway:`, and it carries an
/// address, never a key: the runtime attaches the key to requests sent to that address.
/// Absent, it is `gateway_missing` — there is no fallback destination.
pub const GATEWAY_ENDPOINT_ENV: &str = "MURMUR_GATEWAY_ENDPOINT";

// The operator-facing text of the `gateway_missing` and credential-in-config refusals, which
// show the operator what to paste. Each is assembled with `concat!` because the source tests
// in `mod tests` forbid the Tavily host and the key's variable name anywhere in the source:
// the only destination the tool addresses is `GATEWAY_ENDPOINT_ENV`, and it reads no key.
// These strings are shown, never read or dialled.

/// The endpoint the operator writes as `gateway.endpoint`.
pub(crate) const SUGGESTED_ENDPOINT: &str = concat!("https://api.", "tavily", ".com");
/// The `${NAME}` reference the operator writes as `gateway.api_key`.
pub(crate) const SUGGESTED_KEY_REFERENCE: &str = concat!("${", "TAVILY_", "API_", "KEY}");
/// The `credentials.<NAME>` the reference resolves through.
pub(crate) const SUGGESTED_CREDENTIAL: &str = concat!("credentials.", "TAVILY_", "API_", "KEY");

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

    /// Largest response body read into memory. Tavily's `/search` answer is a few tens of
    /// KiB with raw content off; anything past this is not a search result.
    const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

    struct Component;

    impl Guest for Component {
        fn run(input: ToolInput) -> ToolResult {
            // The only two environment reads in the crate. Everything below this module
            // takes both values as parameters, so the host tests supply them directly.
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
            let retry_after = response
                .headers()
                .get("retry-after")
                .into_iter()
                .next()
                .map(|value| String::from_utf8_lossy(&value).into_owned());
            let body = consume_body(response)?;
            Ok(HttpReply {
                status,
                retry_after,
                body,
            })
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
        {
            let stream = outgoing_body
                .write()
                .map_err(|()| "failed to open request body stream".to_string())?;
            let mut remaining: &[u8] = body;
            while !remaining.is_empty() {
                let budget = stream
                    .check_write()
                    .map_err(|e| format!("check-write failed: {e:?}"))?
                    as usize;
                if budget == 0 {
                    stream.subscribe().block();
                    continue;
                }
                let n = budget.min(remaining.len());
                stream
                    .write(&remaining[..n])
                    .map_err(|e| format!("write failed: {e:?}"))?;
                remaining = &remaining[n..];
            }
            stream.flush().map_err(|e| format!("flush failed: {e:?}"))?;
            stream.subscribe().block();
        }
        wasip2::http::types::OutgoingBody::finish(outgoing_body, None)
            .map_err(|err| format!("failed to finalize request body: {err:?}"))?;

        let future = wasip2::http::outgoing_handler::handle(request, None)
            .map_err(|err| format!("failed to dispatch HTTP request: {err:?}"))?;

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
    //! Source-level invariants: the key-absence properties the rest of the suite cannot see
    //! from behaviour alone. Every needle is assembled with `concat!` so that this module is
    //! not its own evidence, and every file is cut at its own test module, if it has one.

    /// Every `src/*.rs` file, cut at `"\nmod tests {"` when present and lowercased.
    fn non_test_sources() -> Vec<(&'static str, String)> {
        [
            ("lib.rs", include_str!("lib.rs")),
            ("config.rs", include_str!("config.rs")),
            ("input.rs", include_str!("input.rs")),
            ("ops.rs", include_str!("ops.rs")),
            ("render.rs", include_str!("render.rs")),
            ("request.rs", include_str!("request.rs")),
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
        // A new module the list above does not name would escape every needle below.
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
        // The runtime attaches the header declared under `upstream_auth:`, so a tool-side
        // credential read or header would put the key back in the guest.
        for (name, source) in non_test_sources() {
            for needle in [
                concat!("\"author", "ization\""),
                concat!("bear", "er "),
                concat!("\"x-api", "-key\""),
                concat!("tavily_", "api_key"),
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
    fn source_never_names_the_tavily_host() {
        // The operator's `gateway.endpoint` is the only place the destination is written.
        let needle = concat!("tavily", ".com");
        for (name, source) in non_test_sources() {
            assert!(
                !source.contains(needle),
                "src/{name}: non-test source must not contain {needle}"
            );
        }
    }

    #[test]
    fn manifest_declares_the_auth_scheme_the_gateway_will_read() {
        // The runtime builds the auth header from these two fields, so a dropped quote pair
        // (`value: Bearer {key}` is not the same scalar) or a renamed key leaves the gateway
        // with no scheme to apply and the launch refused.
        const BLOCK: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
        assert!(
            include_str!("../murmur.yaml").contains(BLOCK),
            "tools/murmur-tool-tavily/murmur.yaml must contain verbatim:\n{BLOCK}"
        );
    }

    #[test]
    fn manifest_schema_root_declares_no_destinations() {
        let manifest = include_str!("../murmur.yaml");
        assert!(
            manifest.contains("input_schema:\n  type: object\n  murmur-destinations: []\n"),
            "input_schema's root must be `type: object` and declare `murmur-destinations: []`"
        );
        assert!(
            manifest.contains("\n  required: [query]\n"),
            "input_schema must require query"
        );
    }

    #[test]
    fn manifest_declares_no_capabilities_block() {
        // The tool's only egress is the gateway, which no network grant governs; the operator
        // narrows direct egress on the entry.
        let manifest = include_str!("../murmur.yaml");
        assert!(
            !manifest
                .lines()
                .any(|line| line.starts_with("capabilities:")),
            "murmur.yaml must not declare a capabilities: block"
        );
    }
}
