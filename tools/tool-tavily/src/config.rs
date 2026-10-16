//! The operator-written configuration that decides what a search may cost and how much of
//! it reaches the model.
//!
//! It lives in the `config:` block on this artifact's entry in the operator's capsule
//! manifest, where the agent cannot reach it. The runtime delivers the block as compact
//! JSON; what arrives here is that JSON. An entry with no block gets [`Config::default`];
//! an entry with an empty block is refused, because `config_version` is then missing.
//!
//! Loading is fail-closed and every refusal names the key it is about, so the block is
//! walked key by key rather than deserialised in one step. Unknown top-level keys are
//! tolerated so the operator can annotate the block — except credential-shaped ones, since
//! the key belongs on the entry's `gateway:` and a key written here would sit in plaintext
//! in the manifest while doing nothing.

use serde_json::Value;

/// The only `config_version` this build understands.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

/// `search_depth` when the key is absent.
pub const DEFAULT_SEARCH_DEPTH: SearchDepth = SearchDepth::Basic;
/// `max_results.default` when the key is absent.
pub const DEFAULT_MAX_RESULTS_DEFAULT: u32 = 5;
/// `max_results.max` when the key is absent.
pub const DEFAULT_MAX_RESULTS_MAX: u32 = 10;
/// `include_answer` when the key is absent.
pub const DEFAULT_INCLUDE_ANSWER: IncludeAnswer = IncludeAnswer::Off;
/// `topic` when the key is absent.
pub const DEFAULT_TOPIC: Topic = Topic::General;
/// `chunks_per_source` when the key is absent. Sent only with `search_depth: advanced`.
pub const DEFAULT_CHUNKS_PER_SOURCE: u32 = 3;
/// `max_output_bytes` when the key is absent.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16384;

/// Tavily's own ceiling on `max_results`.
pub const MAX_RESULTS_LIMIT: u32 = 20;
/// Tavily's own ceiling on `chunks_per_source`.
pub const CHUNKS_PER_SOURCE_LIMIT: u32 = 3;
/// The smallest render budget: room for the header, a short answer and one cut result.
pub const MIN_OUTPUT_BYTES: usize = 2048;
/// The largest render budget.
pub const MAX_OUTPUT_BYTES_LIMIT: usize = 65536;
/// Tavily's own ceiling on `include_domains`.
pub const MAX_INCLUDE_DOMAINS: usize = 300;
/// Tavily's own ceiling on `exclude_domains`.
pub const MAX_EXCLUDE_DOMAINS: usize = 150;

/// Fragments that make a top-level key credential-shaped, matched against its lowercase
/// form. `authoriz` is a stem, so `authorize` is caught too.
const CREDENTIAL_FRAGMENTS: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "authoriz",
    "bearer",
    "credential",
];

/// How much work, and so how many credits, Tavily spends per search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDepth {
    Basic,
    Advanced,
    Fast,
    UltraFast,
}

impl SearchDepth {
    const ALL: &[(&'static str, Self)] = &[
        ("basic", Self::Basic),
        ("advanced", Self::Advanced),
        ("fast", Self::Fast),
        ("ultra-fast", Self::UltraFast),
    ];

    pub fn as_str(self) -> &'static str {
        lookup_name(Self::ALL, self)
    }
}

/// Whether Tavily adds an LLM-written answer ahead of the results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeAnswer {
    Off,
    Basic,
    Advanced,
}

impl IncludeAnswer {
    /// The wire value: `false`, `"basic"` or `"advanced"`.
    pub fn to_json(self) -> Value {
        match self {
            Self::Off => Value::Bool(false),
            Self::Basic => Value::from("basic"),
            Self::Advanced => Value::from("advanced"),
        }
    }
}

/// Which of Tavily's search agents handles the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    General,
    News,
    Finance,
}

impl Topic {
    const ALL: &[(&'static str, Self)] = &[
        ("general", Self::General),
        ("news", Self::News),
        ("finance", Self::Finance),
    ];

    pub fn as_str(self) -> &'static str {
        lookup_name(Self::ALL, self)
    }
}

/// How far back results may be dated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRange {
    Day,
    Week,
    Month,
    Year,
}

impl TimeRange {
    const ALL: &[(&'static str, Self)] = &[
        ("day", Self::Day),
        ("week", Self::Week),
        ("month", Self::Month),
        ("year", Self::Year),
    ];

    pub fn as_str(self) -> &'static str {
        lookup_name(Self::ALL, self)
    }
}

fn lookup_name<T: PartialEq + Copy>(table: &[(&'static str, T)], value: T) -> &'static str {
    table
        .iter()
        .find(|(_, candidate)| *candidate == value)
        .map(|(name, _)| *name)
        .expect("every variant is in its table")
}

/// Bounds on the result count. `default` applies when the agent omits `max_results`; `max`
/// is the ceiling the agent's value is clamped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxResultsCaps {
    pub default: u32,
    pub max: u32,
}

impl Default for MaxResultsCaps {
    fn default() -> Self {
        Self {
            default: DEFAULT_MAX_RESULTS_DEFAULT,
            max: DEFAULT_MAX_RESULTS_MAX,
        }
    }
}

/// A loaded, validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub config_version: u64,
    pub search_depth: SearchDepth,
    pub max_results: MaxResultsCaps,
    pub include_answer: IncludeAnswer,
    pub topic: Topic,
    pub time_range: Option<TimeRange>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub chunks_per_source: u32,
    pub max_output_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: SUPPORTED_CONFIG_VERSION,
            search_depth: DEFAULT_SEARCH_DEPTH,
            max_results: MaxResultsCaps::default(),
            include_answer: DEFAULT_INCLUDE_ANSWER,
            topic: DEFAULT_TOPIC,
            time_range: None,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            chunks_per_source: DEFAULT_CHUNKS_PER_SOURCE,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Parse and validate the operator config. The `Err` string is the operator-facing
/// message; the caller turns it into a `config_invalid` result.
///
/// The runtime validates the delivered block's shape and not its meaning, so every check
/// below is this artifact's own. The JSON-parse and not-a-mapping arms are unreachable
/// through the runtime, which only ever delivers a JSON object, and are kept because they
/// are reachable from a host caller.
pub fn parse_config(text: &str) -> Result<Config, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("the `config:` block is not valid configuration JSON: {e}"))?;
    let Value::Object(raw) = value else {
        return Err("the `config:` block must be a mapping".to_string());
    };

    // First, so a key in the block is refused whatever else is wrong with it.
    if let Some(key) = raw.keys().find(|key| is_credential_shaped(key)) {
        return Err(format!(
            "config: must not carry a credential ('{key}'); bind the Tavily key on this entry \
             as gateway.api_key: {} — the runtime attaches it and this tool never reads it",
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
    if let Some(value) = raw.get("search_depth") {
        config.search_depth = enum_value("search_depth", value, SearchDepth::ALL)?;
    }
    if let Some(value) = raw.get("max_results") {
        config.max_results = max_results(value)?;
    }
    if let Some(value) = raw.get("include_answer") {
        config.include_answer = include_answer(value)?;
    }
    if let Some(value) = raw.get("topic") {
        config.topic = enum_value("topic", value, Topic::ALL)?;
    }
    if let Some(value) = raw.get("time_range") {
        config.time_range = Some(enum_value("time_range", value, TimeRange::ALL)?);
    }
    if let Some(value) = raw.get("include_domains") {
        config.include_domains = domains("include_domains", value, MAX_INCLUDE_DOMAINS)?;
    }
    if let Some(value) = raw.get("exclude_domains") {
        config.exclude_domains = domains("exclude_domains", value, MAX_EXCLUDE_DOMAINS)?;
    }
    if let Some(value) = raw.get("chunks_per_source") {
        config.chunks_per_source = integer_in(
            "chunks_per_source",
            value,
            1,
            CHUNKS_PER_SOURCE_LIMIT.into(),
        )? as u32;
    }
    if let Some(value) = raw.get("max_output_bytes") {
        config.max_output_bytes = integer_in(
            "max_output_bytes",
            value,
            MIN_OUTPUT_BYTES as u64,
            MAX_OUTPUT_BYTES_LIMIT as u64,
        )? as usize;
    }
    Ok(config)
}

fn is_credential_shaped(key: &str) -> bool {
    let lower = key.to_lowercase();
    CREDENTIAL_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn allowed_names<T>(table: &[(&'static str, T)]) -> String {
    table
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn enum_value<T: Copy>(key: &str, value: &Value, table: &[(&'static str, T)]) -> Result<T, String> {
    value
        .as_str()
        .and_then(|name| table.iter().find(|(candidate, _)| *candidate == name))
        .map(|(_, variant)| *variant)
        .ok_or_else(|| format!("{key} must be one of {}, got {value}", allowed_names(table)))
}

fn include_answer(value: &Value) -> Result<IncludeAnswer, String> {
    match value {
        Value::Bool(false) => Ok(IncludeAnswer::Off),
        Value::Bool(true) => Err(
            "include_answer: true is not accepted; write basic for a quick answer or advanced \
             for a detailed one (or false for none)"
                .to_string(),
        ),
        Value::String(name) if name == "basic" => Ok(IncludeAnswer::Basic),
        Value::String(name) if name == "advanced" => Ok(IncludeAnswer::Advanced),
        other => Err(format!(
            "include_answer must be one of false, basic, advanced, got {other}"
        )),
    }
}

fn integer_in(key: &str, value: &Value, min: u64, max: u64) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| format!("{key} must be an integer from {min} to {max}, got {value}"))
}

fn max_results(value: &Value) -> Result<MaxResultsCaps, String> {
    let Value::Object(map) = value else {
        return Err(format!(
            "max_results must be a mapping with default and max, got {value}"
        ));
    };
    if let Some(unknown) = map
        .keys()
        .find(|key| !matches!(key.as_str(), "default" | "max"))
    {
        return Err(format!(
            "max_results.{unknown} is not a recognised key; max_results takes default and max"
        ));
    }
    let limit = u64::from(MAX_RESULTS_LIMIT);
    let mut caps = MaxResultsCaps::default();
    if let Some(value) = map.get("default") {
        caps.default = integer_in("max_results.default", value, 1, limit)? as u32;
    }
    if let Some(value) = map.get("max") {
        caps.max = integer_in("max_results.max", value, 1, limit)? as u32;
    }
    if caps.default > caps.max {
        return Err(format!(
            "max_results.default ({}) must not exceed max_results.max ({})",
            caps.default, caps.max
        ));
    }
    Ok(caps)
}

fn domains(key: &str, value: &Value, limit: usize) -> Result<Vec<String>, String> {
    let Value::Array(items) = value else {
        return Err(format!("{key} must be a list of domain names, got {value}"));
    };
    if items.len() > limit {
        return Err(format!(
            "{key} lists {} domains; at most {limit} are allowed",
            items.len()
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item.as_str() {
            Some(domain) if !domain.trim().is_empty() => Ok(domain.to_string()),
            _ => Err(format!(
                "{key}[{index}] must be a non-empty domain name, got {item}"
            )),
        })
        .collect()
}
