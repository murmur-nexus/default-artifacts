//! The transport contract, and the redirect rule every request chain follows.
//!
//! Two kinds of request leave this tool. A *gateway request* is addressed to the gateway base
//! the runtime put in `MURMUR_GATEWAY_ENDPOINT`; the runtime readdresses it to the operator's
//! `gateway.endpoint` (the Hub) and attaches the token. A *direct request* is addressed to an
//! absolute URL on any other host; the runtime checks it against `network.allow` and attaches
//! nothing. The tool sets no auth header on either kind.
//!
//! Redirects are followed here, hop by hop, because the gateway returns every `3xx` unfollowed:
//!
//! | `Location` | Followed as |
//! |---|---|
//! | relative (`/p`, `?q`, `p`), from a gateway request | a gateway request, same Hub origin |
//! | relative, from a direct request | a direct request to the same origin |
//! | absolute `https://` | a direct request, with no credential |
//! | absolute `http://` to `127.0.0.1`, `localhost` or `[::1]` | a direct request |
//! | absolute, naming the gateway's own authority | refused (`response_invalid`) |
//! | absolute `http://` to any other host, or any other scheme | refused |
//! | missing | refused |
//!
//! At most [`MAX_REDIRECTS`] redirects are followed per chain; a further one is refused.

use std::time::Duration;

use crate::config::is_loopback_authority;

/// Connect, first-byte and between-bytes timeout on every request.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest buffered response body. An API answer past this is `response_invalid`.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Redirects followed per request chain.
pub const MAX_REDIRECTS: usize = 3;
/// The address the runtime's credential gateway listens on inside the guest. A redirect to it
/// would be readdressed to the Hub with the token, so it is never followed.
pub const GATEWAY_AUTHORITY: &str = "127.0.0.1:9";
/// The `User-Agent` on every request.
pub const USER_AGENT: &str = "tool-huggingface/0.1.0";

/// The only two methods this tool sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
        }
    }
}

/// One response. Header names are lowercase. `body` is empty for a `HEAD`, for a followed
/// redirect, and for a `2xx` whose body went to a [`BodySink`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// The first value of header `name` (lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Why a request got no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpFailure {
    /// The runtime refused the destination: it is not on the allow-list this tool runs under.
    Denied { host: String },
    /// A buffered body went past [`MAX_RESPONSE_BYTES`].
    TooLarge,
    /// The sink refused a chunk; the message is the sink's.
    Aborted(String),
    /// DNS, TLS, connection, timeout or any other transport failure.
    Transport(String),
}

/// Where a streamed `2xx` body goes.
pub trait BodySink {
    /// Called once with the status and headers of a `2xx`, before any body byte. Returning
    /// `false` leaves the body unread.
    fn accept(&mut self, status: u16, headers: &[(String, String)]) -> bool;
    /// One chunk of the body. An `Err` stops the read as [`HttpFailure::Aborted`].
    fn write(&mut self, chunk: &[u8]) -> Result<(), String>;
}

/// Sends one request. The wasm adapter implements it over `wasi:http`; host tests implement it
/// with a recording fake. Neither follows redirects: [`Client`] does.
pub trait Transport {
    /// One buffered request. A `HEAD` and a redirect carry no body; any other body is read up
    /// to [`MAX_RESPONSE_BYTES`].
    fn request(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Reply, HttpFailure>;

    /// One `GET` whose `2xx` body, when `sink` accepts it, is written to `sink` chunk by chunk
    /// rather than held. A non-`2xx` body is buffered as [`Transport::request`] does.
    fn stream(
        &mut self,
        url: &str,
        headers: &[(String, String)],
        sink: &mut dyn BodySink,
    ) -> Result<Reply, HttpFailure>;
}

/// Whether a request went through the gateway or straight to its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Gateway,
    Direct,
}

/// One request of a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub route: Route,
    /// The authority the request was addressed to.
    pub host: String,
}

impl Hop {
    /// How the result text names the hop. A signed URL's query never appears.
    pub fn describe(&self) -> String {
        match self.route {
            Route::Gateway => {
                "Hub via gateway (credential attached by the runtime when one is bound)".to_string()
            }
            Route::Direct => format!("{} direct (no credential)", self.host),
        }
    }
}

/// A finished chain: the last response and every request it took.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub reply: Reply,
    pub hops: Vec<Hop>,
}

/// Why a chain ended without a final response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// A request failed in transport; `host` is the authority it was addressed to.
    Http { failure: HttpFailure, host: String },
    /// A redirect this tool refuses to follow; the message says why.
    Redirect(String),
}

/// Whether `status` is a redirect this tool follows.
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Sends request chains, following redirects by the rule in the module docs.
pub struct Client<'a> {
    transport: &'a mut dyn Transport,
    /// `MURMUR_GATEWAY_ENDPOINT`, without a trailing `/`.
    gateway: String,
}

impl<'a> Client<'a> {
    pub fn new(transport: &'a mut dyn Transport, gateway: &str) -> Self {
        Self {
            transport,
            gateway: gateway.trim_end_matches('/').to_string(),
        }
    }

    /// The gateway URL for `path_and_query`, which starts with `/`.
    pub fn gateway_url(&self, path_and_query: &str) -> String {
        format!("{}{path_and_query}", self.gateway)
    }

    /// A buffered chain. `stop` ends it early at a redirect the caller can use as is (a `HEAD`
    /// that already reports a size).
    pub fn fetch(
        &mut self,
        method: Method,
        url: &str,
        route: Route,
        headers: &[(String, String)],
        stop: &dyn Fn(&Reply) -> bool,
    ) -> Result<Fetched, ChainError> {
        self.chain(
            url,
            route,
            |transport, url| transport.request(method, url, headers),
            stop,
        )
    }

    /// A `GET` chain whose final `2xx` body goes to `sink`.
    pub fn stream(
        &mut self,
        url: &str,
        route: Route,
        headers: &[(String, String)],
        sink: &mut dyn BodySink,
    ) -> Result<Fetched, ChainError> {
        self.chain(
            url,
            route,
            |transport, url| transport.stream(url, headers, sink),
            &|_| false,
        )
    }

    fn chain(
        &mut self,
        url: &str,
        route: Route,
        mut send: impl FnMut(&mut dyn Transport, &str) -> Result<Reply, HttpFailure>,
        stop: &dyn Fn(&Reply) -> bool,
    ) -> Result<Fetched, ChainError> {
        let gateway_authority = authority(&self.gateway).unwrap_or_default().to_string();
        let mut url = url.to_string();
        let mut route = route;
        let mut hops = Vec::new();
        loop {
            let host = authority(&url).unwrap_or_default().to_string();
            hops.push(Hop {
                route,
                host: host.clone(),
            });
            let reply = send(&mut *self.transport, &url)
                .map_err(|failure| ChainError::Http { failure, host })?;
            if !is_redirect(reply.status) || stop(&reply) {
                return Ok(Fetched { reply, hops });
            }
            if hops.len() > MAX_REDIRECTS {
                return Err(ChainError::Redirect(format!(
                    "the Hub sent more than {MAX_REDIRECTS} redirects in a row; not following"
                )));
            }
            let Some(location) = reply.header("location") else {
                return Err(ChainError::Redirect(format!(
                    "HTTP {} redirect without a Location header",
                    reply.status
                )));
            };
            let (next, next_route) = next_hop(&url, route, location, &gateway_authority)?;
            url = next;
            route = next_route;
        }
    }
}

/// Where a redirect from `current` (sent on `route`) to `location` goes, by the module rule.
pub fn next_hop(
    current: &str,
    route: Route,
    location: &str,
    gateway_authority: &str,
) -> Result<(String, Route), ChainError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(ChainError::Redirect(
            "redirect with an empty Location header".to_string(),
        ));
    }
    let refused = |why: &str| {
        ChainError::Redirect(format!(
            "refusing to follow a redirect to {}: {why}",
            strip_query(location)
        ))
    };
    let absolute = if let Some(rest) = location.strip_prefix("//") {
        let scheme = current.split("://").next().unwrap_or("https");
        Some(format!("{scheme}://{rest}"))
    } else if has_scheme(location) {
        Some(location.to_string())
    } else {
        None
    };
    let Some(target) = absolute else {
        let resolved = resolve_relative(current, location)
            .ok_or_else(|| refused("it cannot be resolved against the request URL"))?;
        return Ok((resolved, route));
    };
    let lower = target.to_ascii_lowercase();
    let is_https = lower.starts_with("https://");
    if !is_https && !lower.starts_with("http://") {
        return Err(refused("only http(s) redirects are followed"));
    }
    let target_authority = authority(&target)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| refused("it names no host"))?;
    if target_authority.contains('@') {
        return Err(refused("it carries userinfo"));
    }
    if target_authority.eq_ignore_ascii_case(GATEWAY_AUTHORITY)
        || target_authority.eq_ignore_ascii_case(gateway_authority)
    {
        return Err(refused(
            "it names the credential gateway's own address, where the runtime would attach the token",
        ));
    }
    if !is_https && !is_loopback_authority(target_authority) {
        return Err(refused(
            "a plain-http redirect is followed only to a loopback host",
        ));
    }
    Ok((target, Route::Direct))
}

/// `location` resolved against `current`: `/p` keeps scheme and authority, `?q` replaces the
/// query, anything else replaces the last path segment. A fragment is dropped.
fn resolve_relative(current: &str, location: &str) -> Option<String> {
    let location = location.split('#').next().unwrap_or_default();
    let (scheme, after_scheme) = current.split_once("://")?;
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let path_and_more = &after_scheme[authority_end..];
    let path = &path_and_more[..path_and_more
        .find(['?', '#'])
        .unwrap_or(path_and_more.len())];
    let path = if path.is_empty() { "/" } else { path };
    Some(if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else if location.starts_with('?') {
        format!("{scheme}://{authority}{path}{location}")
    } else {
        let directory = &path[..=path.rfind('/').unwrap_or(0)];
        format!("{scheme}://{authority}{directory}{location}")
    })
}

/// Whether `value` starts with a URI scheme (`letter *(letter / digit / + - .) ":"`) ahead of
/// any `/`, `?` or `#`.
fn has_scheme(value: &str) -> bool {
    let Some(colon) = value.find(':') else {
        return false;
    };
    if value[..colon].contains(['/', '?', '#']) {
        return false;
    }
    let mut chars = value[..colon].chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The authority of an absolute `scheme://authority/...` URL.
pub fn authority(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `url` without its query or fragment, for messages: a signed URL's query is a credential of
/// its own.
pub fn strip_query(url: &str) -> &str {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    &url[..end]
}

/// Percent-encode everything but RFC 3986 unreserved characters.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Each `/`-separated segment of `path` percent-encoded, the separators kept.
pub fn encode_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// `?k=v&k=v`, each key and value percent-encoded; empty for no pairs.
pub fn query_string(pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect();
    format!("?{}", joined.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_rule() {
        let gw = "127.0.0.1:9";
        let hub = "http://127.0.0.1:9/a/resolve/main/f";
        assert_eq!(
            next_hop(hub, Route::Gateway, "/api/resolve-cache/x?y=1", gw).unwrap(),
            (
                "http://127.0.0.1:9/api/resolve-cache/x?y=1".to_string(),
                Route::Gateway
            )
        );
        assert_eq!(
            next_hop(hub, Route::Gateway, "https://cdn.example/b?sig=1", gw).unwrap(),
            ("https://cdn.example/b?sig=1".to_string(), Route::Direct)
        );
        assert_eq!(
            next_hop(hub, Route::Gateway, "http://127.0.0.1:7/b", gw).unwrap(),
            ("http://127.0.0.1:7/b".to_string(), Route::Direct)
        );
        for bad in [
            "http://cdn.example/b",
            "http://127.0.0.1:9/x",
            "https://127.0.0.1:9/x",
            "ftp://x/y",
            "",
            "//cdn.example/b",
        ] {
            assert!(next_hop(hub, Route::Gateway, bad, gw).is_err(), "{bad:?}");
        }
        assert_eq!(
            next_hop("https://cdn.example/a/b?s=1", Route::Direct, "c?d", gw).unwrap(),
            ("https://cdn.example/a/c?d".to_string(), Route::Direct)
        );
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("a b/c[]"), "a%20b%2Fc%5B%5D");
        assert_eq!(encode_path("dir/a b.json"), "dir/a%20b.json");
        assert_eq!(
            query_string(&[("q", "x y".into()), ("n", "1".into())]),
            "?q=x%20y&n=1"
        );
    }
}
