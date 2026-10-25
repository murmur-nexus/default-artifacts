//! The operator-written `config:` block: which repo this tool may touch, and every gate on
//! pushing and merging into it.
//!
//! As in the tavily precedent this follows, the block is walked key by key so every refusal
//! names the key it is about, and a credential-shaped key is refused outright — the GitHub
//! token belongs on this entry's `gateway:`, attached by the runtime, never read here.

use serde_json::Value;

/// The only `config_version` this build understands.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

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

/// A merge method GitHub accepts on `PUT /repos/{owner}/{repo}/pulls/{n}/merge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    const ALL: &[(&'static str, Self)] = &[
        ("merge", Self::Merge),
        ("squash", Self::Squash),
        ("rebase", Self::Rebase),
    ];

    pub fn as_str(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, variant)| *variant == self)
            .map(|(name, _)| *name)
            .expect("every variant is in ALL")
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, variant)| *variant)
    }
}

/// A loaded, validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub config_version: u64,
    /// The one repo this tool's entry may touch, as `owner/name`.
    pub repo: String,
    pub allow_merge: bool,
    pub merge_methods: Vec<MergeMethod>,
    pub allow_force_push: bool,
    pub allow_push_default_branch: bool,
    /// The line `pr_create` appends to a PR body; `None` (unset or `false`) sends the body
    /// untouched.
    pub pr_footer: Option<String>,
    /// The `Key: value` trailer `pr_merge` appends to a merge or squash commit message; `None`
    /// (unset or `false`) sends none.
    pub merge_trailer: Option<String>,
}

/// Parse and validate the operator config. The `Err` string is the operator-facing message;
/// the caller turns it into a `config_invalid` result.
///
/// Unlike tavily's tool, this tool has no usable default: `repo` is required, so there is no
/// `Config::default()` — an entry with no `config:` block at all is refused the same as an
/// entry with an empty one.
pub fn parse_config(text: &str) -> Result<Config, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("the `config:` block is not valid configuration JSON: {e}"))?;
    let Value::Object(raw) = value else {
        return Err("the `config:` block must be a mapping".to_string());
    };

    if let Some(key) = raw.keys().find(|key| is_credential_shaped(key)) {
        return Err(format!(
            "config: must not carry a credential ('{key}'); bind the GitHub token on this \
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

    let repo = match raw.get("repo") {
        Some(Value::String(repo)) => parse_repo(repo)?,
        Some(other) => return Err(format!("repo must be a string \"owner/name\", got {other}")),
        None => return Err("repo is required, as \"owner/name\"".to_string()),
    };

    let allow_merge = bool_field(&raw, "allow_merge")?.unwrap_or(false);
    let allow_force_push = bool_field(&raw, "allow_force_push")?.unwrap_or(false);
    let allow_push_default_branch = bool_field(&raw, "allow_push_default_branch")?.unwrap_or(false);
    let pr_footer = pr_footer_field(&raw)?;
    let merge_trailer = merge_trailer_field(&raw)?;

    let merge_methods = match raw.get("merge_methods") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item.as_str().and_then(MergeMethod::parse) {
                Some(method) => Ok(method),
                None => Err(format!(
                    "merge_methods entries must be one of merge, squash, rebase, got {item}"
                )),
            })
            .collect::<Result<Vec<_>, String>>()?,
        Some(other) => return Err(format!("merge_methods must be a list, got {other}")),
    };

    let known = [
        "config_version",
        "repo",
        "allow_merge",
        "merge_methods",
        "allow_force_push",
        "allow_push_default_branch",
        "pr_footer",
        "merge_trailer",
    ];
    if let Some(unknown) = raw.keys().find(|key| !known.contains(&key.as_str())) {
        return Err(format!("'{unknown}' is not a recognised config: key"));
    }

    Ok(Config {
        config_version: SUPPORTED_CONFIG_VERSION,
        repo,
        allow_merge,
        merge_methods,
        allow_force_push,
        allow_push_default_branch,
        pr_footer,
        merge_trailer,
    })
}

fn bool_field(raw: &serde_json::Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match raw.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(format!("{key} must be a boolean, got {other}")),
    }
}

/// `pr_footer`: absent or `false` is off; otherwise one non-blank line, stored verbatim.
fn pr_footer_field(raw: &serde_json::Map<String, Value>) -> Result<Option<String>, String> {
    match raw.get("pr_footer") {
        None | Some(Value::Bool(false)) => Ok(None),
        Some(Value::Bool(true)) => Err(
            "pr_footer: true does not say what to append; set pr_footer to the line to append, \
             e.g. pr_footer: \"Opened by the release bot\", or leave it unset to send the body \
             untouched"
                .to_string(),
        ),
        Some(Value::String(line)) if is_single_line(line) && !line.trim().is_empty() => {
            Ok(Some(line.clone()))
        }
        Some(other) => Err(format!(
            "pr_footer must be one non-empty line of text or false, got {other}"
        )),
    }
}

/// `merge_trailer`: absent or `false` is off; otherwise one `Key: value` trailer line.
fn merge_trailer_field(raw: &serde_json::Map<String, Value>) -> Result<Option<String>, String> {
    match raw.get("merge_trailer") {
        None | Some(Value::Bool(false)) => Ok(None),
        Some(Value::Bool(true)) => Err(
            "merge_trailer: true does not say what to append; set merge_trailer to the trailer \
             line to append, e.g. merge_trailer: \"Co-authored-by: Release Bot \
             <bot@example.com>\", or leave it unset to send none"
                .to_string(),
        ),
        Some(Value::String(line))
            if is_single_line(line) && crate::trailer::is_trailer_line(line) =>
        {
            Ok(Some(line.clone()))
        }
        Some(other) => Err(format!(
            "merge_trailer must be one trailer line \"Key: value\" or false, got {other}"
        )),
    }
}

fn is_single_line(text: &str) -> bool {
    !text.contains(['\n', '\r'])
}

fn parse_repo(repo: &str) -> Result<String, String> {
    let mut parts = repo.splitn(3, '/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("repo must be \"owner/name\", got {repo:?}"));
    };
    if owner.trim().is_empty() || name.trim().is_empty() {
        return Err(format!("repo must be \"owner/name\", got {repo:?}"));
    }
    Ok(repo.to_string())
}

fn is_credential_shaped(key: &str) -> bool {
    let lower = key.to_lowercase();
    CREDENTIAL_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Value {
        serde_json::json!({"config_version": 1, "repo": "acme/widgets"})
    }

    #[test]
    fn parses_a_minimal_config() {
        let config = parse_config(&minimal().to_string()).unwrap();
        assert_eq!(config.repo, "acme/widgets");
        assert!(!config.allow_merge);
        assert!(config.merge_methods.is_empty());
        assert_eq!(config.pr_footer, None);
        assert_eq!(config.merge_trailer, None);
    }

    #[test]
    fn requires_repo() {
        let err = parse_config(&serde_json::json!({"config_version": 1}).to_string()).unwrap_err();
        assert!(err.contains("repo"), "{err}");
    }

    #[test]
    fn rejects_a_credential_shaped_key() {
        let mut value = minimal();
        value["token"] = Value::from("ghp_xxx");
        let err = parse_config(&value.to_string()).unwrap_err();
        assert!(err.contains("credential"), "{err}");
    }

    #[test]
    fn rejects_a_bad_repo_shape() {
        let mut value = minimal();
        value["repo"] = Value::from("not-a-repo");
        let err = parse_config(&value.to_string()).unwrap_err();
        assert!(err.contains("owner/name"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys() {
        let mut value = minimal();
        value["mystery"] = Value::from(true);
        let err = parse_config(&value.to_string()).unwrap_err();
        assert!(err.contains("mystery"), "{err}");
    }

    #[test]
    fn parses_merge_methods() {
        let mut value = minimal();
        value["merge_methods"] = serde_json::json!(["squash", "rebase"]);
        let config = parse_config(&value.to_string()).unwrap();
        assert_eq!(
            config.merge_methods,
            vec![MergeMethod::Squash, MergeMethod::Rebase]
        );
    }

    fn refusal(key: &str, value: Value) -> String {
        let mut config = minimal();
        config[key] = value;
        parse_config(&config.to_string()).unwrap_err()
    }

    #[test]
    fn pr_footer_and_merge_trailer_false_mean_off() {
        let mut value = minimal();
        value["pr_footer"] = Value::from(false);
        value["merge_trailer"] = Value::from(false);
        let config = parse_config(&value.to_string()).unwrap();
        assert_eq!(config.pr_footer, None);
        assert_eq!(config.merge_trailer, None);
    }

    #[test]
    fn parses_a_pr_footer_line_verbatim() {
        let mut value = minimal();
        value["pr_footer"] = Value::from("  Opened by our release bot ");
        let config = parse_config(&value.to_string()).unwrap();
        assert_eq!(
            config.pr_footer.as_deref(),
            Some("  Opened by our release bot ")
        );
    }

    #[test]
    fn rejects_pr_footer_true_and_says_to_set_the_line() {
        let err = refusal("pr_footer", Value::from(true));
        assert!(err.contains("pr_footer"), "{err}");
        assert!(err.contains("the line to append"), "{err}");
    }

    #[test]
    fn rejects_a_pr_footer_that_is_not_one_non_empty_line() {
        for bad in [
            Value::from(""),
            Value::from("   "),
            Value::from("one\ntwo"),
            Value::from("one\rtwo"),
            Value::from(7),
            serde_json::json!(["a"]),
        ] {
            let err = refusal("pr_footer", bad.clone());
            assert!(err.contains("pr_footer"), "{bad}: {err}");
        }
    }

    #[test]
    fn parses_a_merge_trailer_line() {
        let mut value = minimal();
        value["merge_trailer"] = Value::from("Co-authored-by: Bot <bot@example.com>");
        let config = parse_config(&value.to_string()).unwrap();
        assert_eq!(
            config.merge_trailer.as_deref(),
            Some("Co-authored-by: Bot <bot@example.com>")
        );
    }

    #[test]
    fn rejects_merge_trailer_true_and_says_to_set_the_trailer() {
        let err = refusal("merge_trailer", Value::from(true));
        assert!(err.contains("merge_trailer"), "{err}");
        assert!(err.contains("trailer line to append"), "{err}");
    }

    #[test]
    fn rejects_a_merge_trailer_that_is_not_one_trailer_line() {
        for bad in [
            Value::from("not a trailer"),
            Value::from("yes"),
            Value::from(""),
            Value::from("Co-authored-by: A <a@example.com>\nSigned-off-by: B <b@example.com>"),
            Value::from(1),
        ] {
            let err = refusal("merge_trailer", bad.clone());
            assert!(err.contains("merge_trailer"), "{bad}: {err}");
        }
    }
}
