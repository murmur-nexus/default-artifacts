//! A Tavily `/search` response, parsed and rendered as the plain text the model reads.
//!
//! The runtime puts no size cap on a WASM tool's result, so the budget here is the only one:
//! the rendered text, trailer included, is at most the operator's `max_output_bytes`.
//! Everything Tavily returned passes through byte for byte apart from that cut. Nothing is
//! sanitised, escaped or stripped — the runtime's untrusted-content fence is what protects
//! the model, and a second, partial filter here would only suggest otherwise.

use serde_json::Value;

/// The ellipsis a cut ends with.
const ELLIPSIS: &str = "…";

/// One search hit, as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub content: String,
}

/// The parts of a response this tool renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResponse {
    pub answer: Option<String>,
    pub results: Vec<SearchHit>,
    /// `usage.credits`, as Tavily wrote the number.
    pub credits: Option<String>,
    pub request_id: Option<String>,
}

/// Parse a 2xx body. The `Err` string is the `response_invalid` message.
///
/// Only `results` is required. A result missing `title`, `url` or `content` renders that
/// field empty rather than failing the whole search.
pub fn parse_response(body: &[u8]) -> Result<SearchResponse, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| format!("Tavily's response is not JSON: {e}"))?;
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return Err("Tavily's response has no results array".to_string());
    };
    let text = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(SearchResponse {
        answer: value
            .get("answer")
            .and_then(Value::as_str)
            .map(str::to_string),
        results: results
            .iter()
            .map(|item| SearchHit {
                title: text(item, "title"),
                url: text(item, "url"),
                content: text(item, "content"),
            })
            .collect(),
        credits: value
            .get("usage")
            .and_then(|usage| usage.get("credits"))
            .filter(|credits| credits.is_number())
            .map(Value::to_string),
        request_id: value
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// What the header line reports besides the results themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header<'a> {
    pub query: &'a str,
    /// The count the agent asked for, or the operator's default when it asked for none.
    pub requested: u64,
    /// The operator's ceiling.
    pub capped_at: u32,
    pub depth: &'a str,
    /// Whether the entry has no `config:` block at all.
    pub defaults_in_use: bool,
}

/// The rendered text and how much of the response it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Results rendered, whole or cut.
    pub shown: usize,
    /// Whether anything was omitted or cut.
    pub truncated: bool,
    /// Where the full response was written, when it was truncated and the write succeeded.
    pub data_path: Option<String>,
}

/// Render `response` within `budget` bytes.
///
/// When everything fits, and the answer is within half the budget, the text is the whole
/// response and `spill` is never called. Otherwise `spill` is called once to write the full
/// response, returning where it went (`None` when the write failed), and a trailer line
/// saying so ends the text:
///
/// 1. the answer is cut to half the budget;
/// 2. whole results are appended in order while they fit;
/// 3. if not even the first fits, it is cut, so at least one result appears.
pub fn render(
    header: &Header,
    response: &SearchResponse,
    budget: usize,
    spill: impl FnOnce() -> Option<String>,
) -> Rendered {
    let head = head(header, response);
    let answer_cap = budget / 2;
    let answer = response
        .answer
        .as_deref()
        .filter(|answer| !answer.is_empty());

    let mut full = head.clone();
    if let Some(answer) = answer {
        full.push_str(&answer_block(answer));
    }
    for (index, hit) in response.results.iter().enumerate() {
        full.push_str(&result_block(index, hit));
    }
    let answer_fits = answer.is_none_or(|answer| answer.len() <= answer_cap);
    if full.len() <= budget && answer_fits {
        return Rendered {
            text: full,
            shown: response.results.len(),
            truncated: false,
            data_path: None,
        };
    }

    let data_path = spill();
    let total = response.results.len();
    // `shown` never exceeds `total`, so the trailer for `total` is the longest it can be.
    let reserve = 2 + trailer(total, total, budget, data_path.as_deref()).len();
    let available = budget.saturating_sub(reserve);

    let mut text = head;
    if let Some(answer) = answer {
        text.push_str(&answer_block(&cut(answer, answer_cap)));
    }
    let mut shown = 0;
    for (index, hit) in response.results.iter().enumerate() {
        let block = result_block(index, hit);
        if text.len() + block.len() > available {
            break;
        }
        text.push_str(&block);
        shown += 1;
    }
    if shown == 0 && total > 0 {
        let room = available.saturating_sub(text.len());
        text.push_str(&cut(&result_block(0, &response.results[0]), room));
        shown = 1;
    }
    // Only a pathological header or answer can still be over; cut it rather than the bound.
    if text.len() > available {
        text = cut(&text, available);
    }
    text.push_str("\n\n");
    text.push_str(&trailer(shown, total, budget, data_path.as_deref()));
    Rendered {
        text,
        shown,
        truncated: true,
        data_path,
    }
}

fn trailer(shown: usize, total: usize, budget: usize, data_path: Option<&str>) -> String {
    let saved = match data_path {
        Some(path) => format!("full results in {path}"),
        None => "the full results could not be saved".to_string(),
    };
    format!("[truncated: {shown} of {total} results shown within {budget} bytes; {saved}]")
}

fn head(header: &Header, response: &SearchResponse) -> String {
    let mut head = format!(
        "Tavily search: \"{}\" — {} results (requested {}, capped at {}); depth {}",
        header.query,
        response.results.len(),
        header.requested,
        header.capped_at,
        header.depth
    );
    if let Some(credits) = &response.credits {
        head.push_str(&format!("; credits {credits}"));
    }
    if header.defaults_in_use {
        head.push_str("\n[defaults in use — no config: block on this entry]");
    }
    head
}

fn answer_block(answer: &str) -> String {
    format!("\n\nAnswer: {answer}")
}

fn result_block(index: usize, hit: &SearchHit) -> String {
    format!(
        "\n\n[{}] {}\n{}\n{}",
        index + 1,
        hit.title,
        hit.url,
        hit.content
    )
}

/// `text` cut to at most `max` bytes at a UTF-8 character boundary, ending in `…` when cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let Some(keep) = max.checked_sub(ELLIPSIS.len()) else {
        return String::new();
    };
    let mut end = keep;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &text[..end])
}
