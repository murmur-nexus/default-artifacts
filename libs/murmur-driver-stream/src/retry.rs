//! Telling murmur whether a failed inference call may be made again.
//!
//! murmur makes a `transport: http` driver call again when it failed transiently. Its credential
//! gateway sees the status and a connection that broke before the response head; only the driver
//! sees what went wrong after a `2xx` head — a stream cut before the provider said why the turn
//! stopped, or an overload the provider reported inside the stream. The driver says so through
//! one reserved `tool-result.metadata` tuple, which [`Failure::metadata`] renders.

use std::fmt;

/// The reserved `tool-result.metadata` key murmur reads on a failed call: a call whose `status`
/// is not `passed`, or whose payload has `stop_reason: "error"`. `"true"` marks the failure
/// transient, so murmur makes the call again; `"false"` vetoes a retry of a status murmur would
/// otherwise retry. murmur counts only the first tuple with this key, and treats any other value
/// as absent.
pub const TRANSIENT_METADATA_KEY: &str = "transient";

/// What a driver says about a failure murmur cannot judge from the status alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// The provider answered `2xx` and the call then failed in a way the same request may not
    /// fail again: the stream was cut, or the provider reported an overload, server fault, rate
    /// limit or lost connection. murmur retries it.
    Transient,
    /// The provider answered with a status murmur retries, but for a reason waiting will not
    /// clear, such as a `429` naming an exhausted quota. murmur fails the call at once.
    Permanent,
}

impl Retry {
    /// `("transient", "true")` for [`Retry::Transient`], `("transient", "false")` for
    /// [`Retry::Permanent`].
    pub fn metadata(self) -> (String, String) {
        let value = match self {
            Retry::Transient => "true",
            Retry::Permanent => "false",
        };
        (TRANSIENT_METADATA_KEY.to_string(), value.to_string())
    }
}

/// A failed driver call: the message its error payload carries, and what it tells murmur about
/// retrying. `retry: None` leaves the decision to murmur, which then judges by the status its
/// gateway recorded or the connection failure it saw, and retries nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub retry: Option<Retry>,
}

impl Failure {
    /// A failure that says nothing about retrying.
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: None,
        }
    }

    /// A failure murmur retries.
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: Some(Retry::Transient),
        }
    }

    /// A failure murmur must not retry, whatever its status.
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: Some(Retry::Permanent),
        }
    }

    /// The `tool-result.metadata` tuples this failure adds: none for a plain failure, one
    /// [`TRANSIENT_METADATA_KEY`] tuple otherwise.
    pub fn metadata(&self) -> Vec<(String, String)> {
        self.retry.map(Retry::metadata).into_iter().collect()
    }
}

/// The failure for a read of the response body that failed after the provider answered with
/// `status`: `driver: failed to read response stream: <detail>`, where `detail` is the stream
/// error as the driver reports it.
///
/// After a `2xx` head the provider accepted the request and the connection broke while it
/// answered, so the failure is transient. After any other status it is plain: murmur decides
/// from the status alone, and a broken read must not turn a `400` into a retried call.
pub fn read_failure(status: u16, detail: &str) -> Failure {
    let message = format!("driver: failed to read response stream: {detail}");
    if (200..300).contains(&status) {
        Failure::transient(message)
    } else {
        Failure::plain(message)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::plain(message)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    #[test]
    fn the_key_is_the_one_murmur_reads() {
        assert_eq!(TRANSIENT_METADATA_KEY, "transient");
    }

    #[test]
    fn retry_renders_true_or_false() {
        assert_eq!(Retry::Transient.metadata(), tuple("transient", "true"));
        assert_eq!(Retry::Permanent.metadata(), tuple("transient", "false"));
    }

    #[test]
    fn constructors_set_the_message_and_the_retry() {
        assert_eq!(
            Failure::plain("a"),
            Failure {
                message: "a".to_string(),
                retry: None
            }
        );
        assert_eq!(Failure::transient("b").retry, Some(Retry::Transient));
        assert_eq!(
            Failure::permanent("c".to_string()).retry,
            Some(Retry::Permanent)
        );
        assert_eq!(Failure::transient("b").message, "b");
    }

    #[test]
    fn a_string_converts_to_a_plain_failure() {
        let failure: Failure = "driver: boom".to_string().into();
        assert_eq!(failure, Failure::plain("driver: boom"));
    }

    #[test]
    fn a_plain_failure_adds_no_metadata() {
        assert!(Failure::plain("x").metadata().is_empty());
    }

    #[test]
    fn a_classified_failure_adds_exactly_one_tuple() {
        assert_eq!(
            Failure::transient("x").metadata(),
            vec![tuple("transient", "true")]
        );
        assert_eq!(
            Failure::permanent("x").metadata(),
            vec![tuple("transient", "false")]
        );
    }

    #[test]
    fn a_read_failure_after_a_2xx_head_is_transient() {
        for status in [200, 201, 299] {
            assert_eq!(
                read_failure(status, "LastOperationFailed(Error)"),
                Failure::transient(
                    "driver: failed to read response stream: LastOperationFailed(Error)"
                )
            );
        }
    }

    #[test]
    fn a_read_failure_after_any_other_status_is_plain() {
        for status in [100, 300, 400, 429, 500, 503] {
            assert_eq!(
                read_failure(status, "x"),
                Failure::plain("driver: failed to read response stream: x")
            );
        }
    }

    #[test]
    fn display_is_the_message() {
        assert_eq!(
            Failure::transient("driver: stream cut").to_string(),
            "driver: stream cut"
        );
    }
}
