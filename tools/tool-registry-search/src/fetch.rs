//! GET an index over a transport, following redirects here rather than in the transport.
//!
//! The runtime does not follow redirects for a guest, and checks every request against the
//! network allow-list the tool runs under. So each hop is a fresh [`Http::get`], and the runtime
//! admits or denies it on its own; this module never decides that a host is allowed.

use std::time::Duration;

use serde_json::Value;

use crate::search::{err, err_result};

/// The most redirects one fetch follows, as the native 0.5.0 build's HTTP client did.
pub const MAX_REDIRECTS: usize = 5;

/// The connect, first-byte and between-bytes timeout of each request.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// One blocking GET, with no request body and no redirect handling.
pub trait Http {
    /// GETs `url`, an absolute `http://` or `https://` URL.
    ///
    /// A followable redirect ([`is_followed_redirect`]) that carries `Location` comes back with
    /// an empty body, unread. Any other body is read up to
    /// [`MAX_INDEX_BYTES`](crate::search::MAX_INDEX_BYTES); one past it is
    /// [`HttpFailure::TooLarge`].
    fn get(&mut self, url: &str) -> Result<HttpReply, HttpFailure>;
}

/// What one GET answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    /// The first `Location` header, as text.
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// Why one GET produced no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpFailure {
    /// The runtime refused the request: `host`, the URL's authority, is outside the network
    /// allow-list this tool runs under.
    Denied { host: String },
    /// The body ran past [`MAX_INDEX_BYTES`](crate::search::MAX_INDEX_BYTES).
    TooLarge,
    /// Anything else: DNS, connect, TLS, timeout, a broken stream.
    Transport(String),
}

/// Whether `status` is a redirect [`fetch`] follows.
pub fn is_followed_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// GETs `source`, following at most [`MAX_REDIRECTS`] redirects, and returns the final body.
///
/// A status of 400 or more fails; any other status that is not a followed redirect, 2xx or
/// otherwise, gives its body as the index. Every failure is a `fetch_error` naming `source`, the
/// URL the caller passed, never a later hop's URL except where it explains the failure.
pub fn fetch(source: &str, http: &mut dyn Http) -> Result<Vec<u8>, Value> {
    let fail = |detail: String| {
        err_result(
            err::FETCH_ERROR,
            format!("failed to fetch {source}: {detail}"),
        )
    };
    let mut url = source.to_string();
    let mut followed = 0;
    loop {
        let reply = match http.get(&url) {
            Ok(reply) => reply,
            Err(HttpFailure::Denied { host }) => {
                return Err(fail(format!(
                    "{host} is not in the network allow-list this tool runs under; list it in \
                     the capsule's capabilities.network.allow, and in capabilities.network.allow \
                     on this tool's entry if the entry narrows it"
                )))
            }
            Err(HttpFailure::TooLarge) => {
                return Err(err_result(
                    err::FETCH_ERROR,
                    format!(
                        "{source} answered with more than 16 MiB; an artifact index is never \
                         that large"
                    ),
                ))
            }
            Err(HttpFailure::Transport(e)) => return Err(fail(e)),
        };

        let status = reply.status;
        if is_followed_redirect(status) {
            if followed == MAX_REDIRECTS {
                return Err(fail(format!("more than {MAX_REDIRECTS} redirects")));
            }
            let Some(location) = reply.location else {
                return Err(fail(format!(
                    "HTTP {status} from {url} redirects with no Location header"
                )));
            };
            let Some(next) = resolve_location(&url, &location) else {
                return Err(fail(format!(
                    "HTTP {status} from {url} redirects to {location}, which is not an http or \
                     https URL"
                )));
            };
            followed += 1;
            url = next;
            continue;
        }
        if status >= 400 {
            return Err(err_result(
                err::FETCH_ERROR,
                format!("HTTP {status} fetching {source}"),
            ));
        }
        return Ok(reply.body);
    }
}

/// The absolute URL `location` names, read against `current`, or `None` when `location` names
/// a scheme other than `http`/`https`.
///
/// An absolute `http(s)://` URL is taken as is; `//host/p` takes `current`'s scheme; `/p` takes
/// its scheme and authority; `?q` replaces its query; anything else replaces the last segment of
/// its path. The target's own query is kept.
pub fn resolve_location(current: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if let Some(scheme_end) = scheme_len(location) {
        let scheme = location[..scheme_end].to_ascii_lowercase();
        let rest = &location[scheme_end + 1..];
        return match scheme.as_str() {
            "http" | "https" if rest.starts_with("//") => Some(format!("{scheme}:{rest}")),
            _ => None,
        };
    }

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

    Some(if location.starts_with("//") {
        format!("{scheme}:{location}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else if location.starts_with('?') {
        format!("{scheme}://{authority}{path}{location}")
    } else {
        let directory = &path[..=path.rfind('/').unwrap_or(0)];
        format!("{scheme}://{authority}{directory}{location}")
    })
}

/// The length of `value`'s URI scheme, the part before its first `:`, when it has one: a letter
/// followed by letters, digits, `+`, `-` or `.`, ahead of any `/`, `?` or `#`.
fn scheme_len(value: &str) -> Option<usize> {
    let colon = value.find(':')?;
    let scheme = &value[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(colon)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport answering from a script of `(expected url, answer)`, in order.
    struct Scripted {
        script: Vec<(String, Result<HttpReply, HttpFailure>)>,
        seen: Vec<String>,
    }

    impl Scripted {
        fn new(script: Vec<(&str, Result<HttpReply, HttpFailure>)>) -> Self {
            Scripted {
                script: script
                    .into_iter()
                    .rev()
                    .map(|(url, answer)| (url.to_string(), answer))
                    .collect(),
                seen: Vec::new(),
            }
        }
    }

    impl Http for Scripted {
        fn get(&mut self, url: &str) -> Result<HttpReply, HttpFailure> {
            self.seen.push(url.to_string());
            let (expected, answer) = self
                .script
                .pop()
                .unwrap_or_else(|| panic!("unscripted GET {url}"));
            assert_eq!(url, expected, "GET order");
            answer
        }
    }

    fn redirect(status: u16, location: &str) -> Result<HttpReply, HttpFailure> {
        Ok(HttpReply {
            status,
            location: Some(location.to_string()),
            body: Vec::new(),
        })
    }

    fn ok(body: &[u8]) -> Result<HttpReply, HttpFailure> {
        Ok(HttpReply {
            status: 200,
            location: None,
            body: body.to_vec(),
        })
    }

    fn message(out: &Value) -> &str {
        out["message"].as_str().unwrap()
    }

    fn fetch_error(out: Value) -> String {
        assert_eq!(out["error_kind"], err::FETCH_ERROR, "{out}");
        assert_eq!(out["status"], "error");
        assert_eq!(out["summary"], out["message"]);
        message(&out).to_string()
    }

    #[test]
    fn a_200_body_is_the_index() {
        let mut http = Scripted::new(vec![("https://a/i.json", ok(b"{}"))]);
        assert_eq!(fetch("https://a/i.json", &mut http).unwrap(), b"{}");
    }

    #[test]
    fn exactly_five_redirects_are_followed_with_every_followed_status() {
        let mut http = Scripted::new(vec![
            ("https://a/0", redirect(301, "https://a/1")),
            ("https://a/1", redirect(302, "https://a/2")),
            ("https://a/2", redirect(303, "https://a/3")),
            ("https://a/3", redirect(307, "https://a/4")),
            ("https://a/4", redirect(308, "https://b/5")),
            ("https://b/5", ok(b"index")),
        ]);
        assert_eq!(fetch("https://a/0", &mut http).unwrap(), b"index");
        assert_eq!(http.seen.len(), 6);
    }

    #[test]
    fn a_sixth_redirect_is_refused() {
        let mut script: Vec<(String, Result<HttpReply, HttpFailure>)> = (0..6)
            .map(|i| {
                (
                    format!("https://a/{i}"),
                    redirect(302, &format!("/{}", i + 1)),
                )
            })
            .collect();
        script.reverse();
        let mut http = Scripted {
            script,
            seen: Vec::new(),
        };
        let out = fetch("https://a/0", &mut http).unwrap_err();
        assert_eq!(
            fetch_error(out),
            "failed to fetch https://a/0: more than 5 redirects"
        );
        assert_eq!(
            http.seen.len(),
            6,
            "the sixth redirect's target is never requested"
        );
    }

    #[test]
    fn a_redirect_with_no_location_is_refused() {
        let mut http = Scripted::new(vec![(
            "https://a/i",
            Ok(HttpReply {
                status: 302,
                location: None,
                body: Vec::new(),
            }),
        )]);
        let out = fetch("https://a/i", &mut http).unwrap_err();
        assert_eq!(
            fetch_error(out),
            "failed to fetch https://a/i: HTTP 302 from https://a/i redirects with no Location \
             header"
        );
    }

    #[test]
    fn a_redirect_to_another_scheme_is_refused() {
        for location in ["ftp://a/i.json", "file:///etc/passwd", "FILE:/etc/passwd"] {
            let mut http = Scripted::new(vec![
                ("https://a/0", redirect(301, "https://a/1")),
                ("https://a/1", redirect(307, location)),
            ]);
            let out = fetch("https://a/0", &mut http).unwrap_err();
            assert_eq!(
                fetch_error(out),
                format!(
                    "failed to fetch https://a/0: HTTP 307 from https://a/1 redirects to \
                     {location}, which is not an http or https URL"
                )
            );
        }
    }

    #[test]
    fn relative_locations_resolve_against_the_current_url_keeping_their_query() {
        let mut http = Scripted::new(vec![
            (
                "https://a/x/y/latest?old=1",
                redirect(302, "//b.example/p/q?k=v"),
            ),
            ("https://b.example/p/q?k=v", redirect(302, "/root/r?n=2")),
            (
                "https://b.example/root/r?n=2",
                redirect(302, "sibling.json?s=3"),
            ),
            ("https://b.example/root/sibling.json?s=3", ok(b"done")),
        ]);
        assert_eq!(
            fetch("https://a/x/y/latest?old=1", &mut http).unwrap(),
            b"done"
        );
    }

    #[test]
    fn resolve_location_cases() {
        let cur = "http://h:8/a/b/c?q=1";
        for (location, want) in [
            ("https://o/p?x=1", "https://o/p?x=1"),
            ("HTTP://o/p", "http://o/p"),
            ("//o/p?x", "http://o/p?x"),
            ("/p?x", "http://h:8/p?x"),
            ("d.json?x", "http://h:8/a/b/d.json?x"),
            ("?z=2", "http://h:8/a/b/c?z=2"),
        ] {
            assert_eq!(
                resolve_location(cur, location).as_deref(),
                Some(want),
                "{location}"
            );
        }
        assert_eq!(
            resolve_location("http://h", "d.json").as_deref(),
            Some("http://h/d.json")
        );
        assert_eq!(
            resolve_location("http://h?q", "/p").as_deref(),
            Some("http://h/p")
        );
        for refused in [
            "ftp://o/p",
            "file:///etc/passwd",
            "data:text/plain,x",
            "https:o/p",
        ] {
            assert_eq!(resolve_location(cur, refused), None, "{refused}");
        }
    }

    #[test]
    fn a_404_names_the_source_even_after_a_redirect() {
        let mut http = Scripted::new(vec![
            ("https://a/latest", redirect(302, "https://b/v1/i.json")),
            (
                "https://b/v1/i.json",
                Ok(HttpReply {
                    status: 404,
                    location: None,
                    body: b"not here".to_vec(),
                }),
            ),
        ]);
        let out = fetch("https://a/latest", &mut http).unwrap_err();
        assert_eq!(fetch_error(out), "HTTP 404 fetching https://a/latest");
    }

    #[test]
    fn a_3xx_that_is_not_followed_gives_its_body() {
        let mut http = Scripted::new(vec![(
            "https://a/i",
            Ok(HttpReply {
                status: 304,
                location: Some("/elsewhere".into()),
                body: b"cached".to_vec(),
            }),
        )]);
        assert_eq!(fetch("https://a/i", &mut http).unwrap(), b"cached");
    }

    #[test]
    fn too_large_names_the_source() {
        let mut http = Scripted::new(vec![
            ("https://a/i", redirect(302, "https://b/i")),
            ("https://b/i", Err(HttpFailure::TooLarge)),
        ]);
        let out = fetch("https://a/i", &mut http).unwrap_err();
        assert_eq!(
            fetch_error(out),
            "https://a/i answered with more than 16 MiB; an artifact index is never that large"
        );
    }

    #[test]
    fn a_denied_hop_names_its_host_and_the_allow_list() {
        let mut http = Scripted::new(vec![
            (
                "https://a/latest",
                redirect(302, "https://assets.example/i"),
            ),
            (
                "https://assets.example/i",
                Err(HttpFailure::Denied {
                    host: "assets.example".into(),
                }),
            ),
        ]);
        let out = fetch("https://a/latest", &mut http).unwrap_err();
        assert_eq!(
            fetch_error(out),
            "failed to fetch https://a/latest: assets.example is not in the network allow-list \
             this tool runs under; list it in the capsule's capabilities.network.allow, and in \
             capabilities.network.allow on this tool's entry if the entry narrows it"
        );
    }

    #[test]
    fn a_transport_failure_is_passed_on() {
        let mut http = Scripted::new(vec![(
            "http://a/i",
            Err(HttpFailure::Transport(
                "ErrorCode::ConnectionRefused".into(),
            )),
        )]);
        let out = fetch("http://a/i", &mut http).unwrap_err();
        assert_eq!(
            fetch_error(out),
            "failed to fetch http://a/i: ErrorCode::ConnectionRefused"
        );
    }

    #[test]
    fn only_the_five_redirect_statuses_are_followed() {
        for status in [301, 302, 303, 307, 308] {
            assert!(is_followed_redirect(status));
        }
        for status in [200, 300, 304, 305, 306, 400, 404] {
            assert!(!is_followed_redirect(status));
        }
    }
}
