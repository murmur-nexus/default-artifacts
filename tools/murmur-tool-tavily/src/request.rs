//! The one request a call sends: where, with which headers, carrying what.
//!
//! No function here sees a key. The URL is built from the gateway address the runtime
//! handed the tool, the headers describe only the body, and the body carries the query and
//! the operator's levers; the runtime attaches the credential header on the way out.

use serde_json::{json, Value};

use crate::config::{Config, SearchDepth};

/// The search URL: the gateway endpoint with any trailing `/` trimmed, plus `/search`.
pub fn url(gateway_endpoint: &str) -> String {
    format!("{}/search", gateway_endpoint.trim_end_matches('/'))
}

/// Exactly the request headers: the body's type, the accepted type and its length.
pub fn headers(body_len: usize) -> Vec<(String, String)> {
    vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("content-length".to_string(), body_len.to_string()),
    ]
}

/// The JSON body for one search.
///
/// The fixed `false` fields keep every declined lever off regardless of Tavily's defaults:
/// raw page content is unbounded, images are not text, and `auto_parameters` would override
/// the operator's `search_depth` and bill for it. `include_usage` makes the credit count
/// visible in the result. `max_results` is the already-clamped count.
pub fn body(config: &Config, query: &str, max_results: u32) -> Value {
    let mut body = json!({
        "query": query,
        "search_depth": config.search_depth.as_str(),
        "topic": config.topic.as_str(),
        "max_results": max_results,
        "include_answer": config.include_answer.to_json(),
        "include_raw_content": false,
        "include_images": false,
        "include_image_descriptions": false,
        "include_favicon": false,
        "auto_parameters": false,
        "include_usage": true,
    });
    let fields = body.as_object_mut().expect("json! built an object");
    if let Some(time_range) = config.time_range {
        fields.insert("time_range".to_string(), json!(time_range.as_str()));
    }
    if !config.include_domains.is_empty() {
        fields.insert("include_domains".to_string(), json!(config.include_domains));
    }
    if !config.exclude_domains.is_empty() {
        fields.insert("exclude_domains".to_string(), json!(config.exclude_domains));
    }
    if config.search_depth == SearchDepth::Advanced {
        fields.insert(
            "chunks_per_source".to_string(),
            json!(config.chunks_per_source),
        );
    }
    body
}
