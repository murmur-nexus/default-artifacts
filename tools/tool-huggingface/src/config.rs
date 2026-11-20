//! The operator-written configuration: how much text reaches the model, how large a download
//! may be, and where the datasets server is.
//!
//! It lives in the `config:` block on this artifact's entry in the capsule manifest, where the
//! agent cannot reach it. The runtime delivers the block as compact JSON. An entry with no
//! block gets [`Config::default`]; an entry with an empty block is refused, because
//! `config_version` is then missing. Unknown keys are tolerated so the operator can annotate
//! the block, except credential-shaped ones: the token belongs on the entry's `gateway:`.

use serde_json::Value;

/// The only `config_version` this build understands.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

/// `max_output_bytes` when the key is absent.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16384;
/// The smallest render budget.
pub const MIN_OUTPUT_BYTES: usize = 2048;
/// The largest render budget.
pub const MAX_OUTPUT_BYTES_LIMIT: usize = 65536;

/// `max_download_bytes` when the key is absent: 100 MiB.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 104_857_600;
/// The largest download cap: 2 GiB.
pub const MAX_DOWNLOAD_BYTES_LIMIT: u64 = 2_147_483_648;

/// `datasets_server_endpoint` when the key is absent.
pub const DEFAULT_DATASETS_SERVER: &str = concat!("https://datasets-server.", "hugging", "face.co");

/// Fragments that make a top-level key credential-shaped, matched against its lowercase form.
const CREDENTIAL_FRAGMENTS: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "authoriz",
    "bearer",
    "credential",
];

/// A loaded, validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub config_version: u64,
    /// The budget for every op's text, trailer included.
    pub max_output_bytes: usize,
    /// The download cap, checked against the size a `HEAD` reports before any byte is fetched.
    pub max_download_bytes: u64,
    /// `scheme://authority` of the datasets server, no trailing `/`.
    pub datasets_server_endpoint: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: SUPPORTED_CONFIG_VERSION,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            datasets_server_endpoint: DEFAULT_DATASETS_SERVER.to_string(),
        }
    }
}

/// Parse and validate the operator config. The `Err` string is the operator-facing
/// `config_invalid` message, naming the key it is about.
pub fn parse_config(text: &str) -> Result<Config, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("the `config:` block is not valid configuration JSON: {e}"))?;
    let Value::Object(raw) = value else {
        return Err("the `config:` block must be a mapping".to_string());
    };

    if let Some(key) = raw.keys().find(|key| is_credential_shaped(key)) {
        return Err(format!(
            "config: must not carry a credential ('{key}'); bind the Hugging Face token on this \
             entry as gateway.api_key: {} — the runtime attaches it and this tool never reads it",
            crate::SUGGESTED_KEY_REFERENCE
        ));
    }

    match raw.get("config_version").map(Value::as_u64) {
        Some(Some(SUPPORTED_CONFIG_VERSION)) => {}
        Some(_) => {
            return Err(format!(
                "config_version must be {SUPPORTED_CONFIG_VERSION}, got {}",
                raw["config_version"]
            ))
        }
        None => {
            return Err(format!(
                "config_version is required and must be {SUPPORTED_CONFIG_VERSION}"
            ))
        }
    }

    let mut config = Config::default();
    if let Some(value) = raw.get("max_output_bytes") {
        config.max_output_bytes = integer_in(
            "max_output_bytes",
            value,
            MIN_OUTPUT_BYTES as u64,
            MAX_OUTPUT_BYTES_LIMIT as u64,
        )? as usize;
    }
    if let Some(value) = raw.get("max_download_bytes") {
        config.max_download_bytes =
            integer_in("max_download_bytes", value, 1, MAX_DOWNLOAD_BYTES_LIMIT)?;
    }
    if let Some(value) = raw.get("datasets_server_endpoint") {
        config.datasets_server_endpoint = datasets_server_endpoint(value)?;
    }
    Ok(config)
}

fn is_credential_shaped(key: &str) -> bool {
    let lower = key.to_lowercase();
    CREDENTIAL_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn integer_in(key: &str, value: &Value, min: u64, max: u64) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| format!("{key} must be an integer from {min} to {max}, got {value}"))
}

/// `https://<host>[:port]`, or `http://` with host `127.0.0.1`, `localhost` or `[::1]`; no
/// path, query, fragment or userinfo. A single trailing `/` is dropped.
fn datasets_server_endpoint(value: &Value) -> Result<String, String> {
    const KEY: &str = "datasets_server_endpoint";
    let Some(text) = value.as_str() else {
        return Err(format!("{KEY} must be a URL string, got {value}"));
    };
    let text = text.trim();
    let (scheme, rest) = if let Some(rest) = text.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = text.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(format!(
            "{KEY} must start with https:// (or http:// for a loopback host), got {text}"
        ));
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("{KEY} names no host: {text}"));
    }
    if authority.contains(['/', '?', '#']) {
        return Err(format!(
            "{KEY} must carry no path, query or fragment, got {text}"
        ));
    }
    if authority.contains('@') {
        return Err(format!("{KEY} must carry no userinfo, got {text}"));
    }
    if authority
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{KEY} must not contain spaces, got {text}"));
    }
    if scheme == "http" && !is_loopback_authority(authority) {
        return Err(format!(
            "{KEY} may use http:// only for 127.0.0.1, localhost or [::1], got {text}"
        ));
    }
    Ok(format!("{scheme}://{authority}"))
}

/// Whether `authority` (`host[:port]`) names a loopback host: `127.0.0.1`, `localhost` or
/// `[::1]`, case-insensitively.
pub fn is_loopback_authority(authority: &str) -> bool {
    let host = if authority.starts_with('[') {
        match authority.find(']') {
            Some(end) => &authority[..=end],
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_bounds() {
        assert_eq!(
            parse_config(r#"{"config_version":1}"#).unwrap(),
            Config::default()
        );
        assert!(parse_config("{}").is_err());
        assert!(parse_config(r#"{"config_version":2}"#).is_err());
        assert!(parse_config(r#"{"config_version":1,"max_output_bytes":2047}"#).is_err());
        assert!(parse_config(r#"{"config_version":1,"max_output_bytes":65537}"#).is_err());
        assert!(parse_config(r#"{"config_version":1,"max_download_bytes":0}"#).is_err());
        assert!(parse_config(r#"{"config_version":1,"max_download_bytes":2147483649}"#).is_err());
        assert!(parse_config(r#"{"config_version":1,"note":"ok"}"#).is_ok());
        let err = parse_config(r#"{"config_version":1,"hf_token":"x"}"#).unwrap_err();
        assert!(err.contains("hf_token"), "{err}");
    }

    #[test]
    fn datasets_server_endpoint_rules() {
        let parse = |url: &str| {
            parse_config(&format!(
                r#"{{"config_version":1,"datasets_server_endpoint":"{url}"}}"#
            ))
            .map(|c| c.datasets_server_endpoint)
        };
        assert_eq!(parse("https://ds.example").unwrap(), "https://ds.example");
        assert_eq!(parse("https://ds.example/").unwrap(), "https://ds.example");
        assert_eq!(
            parse("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(parse("http://[::1]:80").unwrap(), "http://[::1]:80");
        assert_eq!(parse("http://localhost").unwrap(), "http://localhost");
        for bad in [
            "http://ds.example",
            "https://ds.example/path",
            "https://ds.example?x=1",
            "https://u@ds.example",
            "ftp://ds.example",
            "https://",
            "http://127.0.0.2",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
