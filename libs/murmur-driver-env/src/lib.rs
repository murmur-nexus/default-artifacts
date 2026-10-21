//! The inference endpoint a driver talks to, resolved from the host's environment.
//!
//! Where inference goes is the host's fact, never the driver's. The capsule names the
//! provider as `gateway.endpoint` on the driver's `artifacts:` entry, and murmur validates it
//! when the manifest is parsed. The driver is handed the runtime's own loopback gateway plus
//! the path of that endpoint; the runtime readdresses each request to the real upstream and
//! attaches the key from `gateway.api_key`, so the driver never holds the key. A driver that
//! substitutes its own URL when the variable is absent bypasses that gateway: its requests go
//! where the driver guessed rather than where the capsule said, unmetered by the session's
//! token budget. So the resolution lives here once, refuses rather than defaults, and no
//! driver carries a provider URL of its own.

/// The environment variable murmur's runtime delivers the endpoint in: its loopback gateway
/// (`http://127.0.0.1:9`) followed by the path of the driver entry's `gateway.endpoint`.
pub const INFERENCE_ENDPOINT_VAR: &str = "MURMUR_INFERENCE_ENDPOINT";

/// The endpoint the host supplied, trimmed, or the reason it is unusable.
///
/// Takes the already-read value rather than reading the environment itself, so the contract
/// is testable on a host target without mutating the process environment. Absent and
/// set-but-empty are distinct refusals: an operator who set the variable from an unresolved
/// `${VAR}` needs to learn something different from one who never set it.
pub fn require_endpoint(value: Option<&str>) -> Result<String, String> {
    let Some(value) = value else {
        return Err(format!("driver: missing {INFERENCE_ENDPOINT_VAR}"));
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("driver: {INFERENCE_ENDPOINT_VAR} is set but empty"));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_endpoint_is_reported_as_missing() {
        assert_eq!(
            require_endpoint(None),
            Err("driver: missing MURMUR_INFERENCE_ENDPOINT".to_string())
        );
    }

    #[test]
    fn an_endpoint_set_to_nothing_is_reported_as_set_but_empty() {
        assert_eq!(
            require_endpoint(Some("")),
            Err("driver: MURMUR_INFERENCE_ENDPOINT is set but empty".to_string())
        );
    }

    #[test]
    fn an_endpoint_of_only_whitespace_is_reported_as_set_but_empty() {
        let expected = Err("driver: MURMUR_INFERENCE_ENDPOINT is set but empty".to_string());
        assert_eq!(require_endpoint(Some("   ")), expected);
        assert_eq!(require_endpoint(Some("\t\n ")), expected);
    }

    #[test]
    fn a_usable_endpoint_is_returned_unchanged() {
        assert_eq!(
            require_endpoint(Some("https://gateway.internal/v1")),
            Ok("https://gateway.internal/v1".to_string())
        );
    }

    #[test]
    fn a_usable_endpoint_is_returned_without_surrounding_whitespace() {
        // A host that supplied a trailing newline would otherwise put it inside the URL the
        // driver formats, and the request would go to a host name with a space in it.
        assert_eq!(
            require_endpoint(Some("  https://gateway.internal/v1\n")),
            Ok("https://gateway.internal/v1".to_string())
        );
    }

    #[test]
    fn every_refusal_names_the_variable_the_operator_must_set() {
        for refusal in [require_endpoint(None), require_endpoint(Some(""))] {
            let message = refusal.expect_err("the value is unusable");
            assert!(
                message.contains(INFERENCE_ENDPOINT_VAR),
                "a refusal that does not name the variable leaves the operator guessing: {message}"
            );
        }
    }
}
