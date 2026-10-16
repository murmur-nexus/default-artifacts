//! What the agent may ask for: a query, and optionally fewer results.
//!
//! The model's `input` arrives verbatim and unvalidated as `tool-input.data`, so every rule
//! in `input_schema` is enforced again here. Every other search lever is the operator's, and
//! naming one is refused with a message saying where it lives, rather than ignored.

use serde_json::{Map, Value};

/// The longest query accepted, in characters, after trimming.
pub const MAX_QUERY_CHARS: usize = 400;

/// A validated call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchInput {
    /// The query, trimmed.
    pub query: String,
    /// The agent's `max_results`, before the operator's clamp. `None` when omitted.
    pub max_results: Option<u64>,
}

/// Parse the tool input. The `Err` string is the agent-facing `invalid_input` message.
///
/// Three shapes are accepted: `{"query": …, "max_results"?: …}`; the same object encoded
/// twice, which some hosts send; and a bare JSON string, taken as the query. A string that
/// re-parses to an object is the double-encoded form; any other string is the query itself,
/// so a query such as `2024` is not mistaken for a number.
pub fn parse_input(data: &str) -> Result<SearchInput, String> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err("no input; expected {\"query\": \"...\"}".to_string());
    }
    let parsed: Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("input is not valid JSON: {e}; expected {{\"query\": \"...\"}}"))?;
    let args = match parsed {
        Value::Object(map) => map,
        Value::String(inner) => match serde_json::from_str::<Value>(&inner) {
            Ok(Value::Object(map)) => map,
            _ => return search_input(Value::String(inner), None),
        },
        other => {
            return Err(format!(
                "input must be a JSON object with a \"query\" string, got {}",
                json_type_name(&other)
            ))
        }
    };
    from_object(args)
}

fn from_object(mut args: Map<String, Value>) -> Result<SearchInput, String> {
    if let Some(key) = args
        .keys()
        .find(|key| !matches!(key.as_str(), "query" | "max_results"))
    {
        return Err(format!(
            "'{key}' is set by the operator in this tool's config: block, not per call; \
             accepted inputs are query and max_results"
        ));
    }
    let query = args.remove("query").ok_or("\"query\" is required")?;
    search_input(query, args.remove("max_results"))
}

fn search_input(query: Value, max_results: Option<Value>) -> Result<SearchInput, String> {
    let Value::String(query) = query else {
        return Err(format!(
            "\"query\" must be a string, got {}",
            json_type_name(&query)
        ));
    };
    let query = query.trim();
    if query.is_empty() {
        return Err("\"query\" must not be blank".to_string());
    }
    let chars = query.chars().count();
    if chars > MAX_QUERY_CHARS {
        return Err(format!(
            "\"query\" is {chars} characters; at most {MAX_QUERY_CHARS} are accepted"
        ));
    }
    let max_results = match max_results {
        None => None,
        Some(value) => match value.as_u64() {
            Some(n) if n >= 1 => Some(n),
            _ => {
                return Err(format!(
                    "\"max_results\" must be an integer of at least 1, got {value}"
                ))
            }
        },
    };
    Ok(SearchInput {
        query: query.to_string(),
        max_results,
    })
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
