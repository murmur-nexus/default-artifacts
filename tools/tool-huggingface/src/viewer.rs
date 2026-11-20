//! `dataset`: a peek at one dataset through the datasets server, without downloading it.
//!
//! Every datasets-server request is a direct request: murmur gives an artifact one gateway,
//! bound to the Hub, so this host is reached without a credential. A gated or private dataset
//! therefore cannot be peeked. When the server answers `401` or `404`, one gateway request to
//! the Hub's `/api/datasets/<repo>` says which case it is.
//!
//! `/is-valid` is not called: `/splits` already answers `501`/`5xx` with the server's own
//! reason for a dataset it cannot serve, and `/search` or `/filter` do the same for a split
//! without an index, so a first `/is-valid` would cost a request and classify nothing more.

use serde_json::{json, Map, Value};

use crate::http::{query_string, Route};
use crate::input::DatasetInput;
use crate::ops::{
    body_value, failure, hub_failure, kind, parse_json, read_success, spill, upstream_text, Ctx,
    Response,
};
use crate::render::{cut, finish, Page};

/// Every string value inside a row is cut to this many bytes.
pub const ROW_STRING_BYTES: usize = 1000;
/// A feature type that is not a plain value or a class label is shown as JSON cut to this.
const FEATURE_TYPE_BYTES: usize = 300;

/// Which datasets-server endpoint serves the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowsEndpoint {
    Rows,
    Search,
    Filter,
}

impl RowsEndpoint {
    pub fn path(self) -> &'static str {
        match self {
            Self::Rows => "rows",
            Self::Search => "search",
            Self::Filter => "filter",
        }
    }
}

/// The `/splits` path and query, exactly as sent.
pub fn splits_path(repo: &str) -> String {
    format!("/splits{}", query_string(&[("dataset", repo.to_string())]))
}

/// The rows path and query for the chosen config and split, exactly as sent.
pub fn rows_path(input: &DatasetInput, config: &str, split: &str) -> (RowsEndpoint, String) {
    let mut pairs: Vec<(&str, String)> = vec![
        ("dataset", input.repo.clone()),
        ("config", config.to_string()),
        ("split", split.to_string()),
    ];
    let endpoint = if let Some(query) = &input.query {
        pairs.push(("query", query.clone()));
        RowsEndpoint::Search
    } else if let Some(where_) = &input.where_ {
        pairs.push(("where", where_.clone()));
        if let Some(order_by) = &input.order_by {
            pairs.push(("orderby", order_by.clone()));
        }
        RowsEndpoint::Filter
    } else {
        RowsEndpoint::Rows
    };
    pairs.push(("offset", input.offset.to_string()));
    pairs.push(("length", input.limit.value.to_string()));
    (
        endpoint,
        format!("/{}{}", endpoint.path(), query_string(&pairs)),
    )
}

pub fn dataset(ctx: &mut Ctx, input: &DatasetInput) -> Response {
    let server = ctx.config.datasets_server_endpoint.clone();
    let subject = format!("dataset {}", input.repo);

    let splits_url = format!("{server}{}", splits_path(&input.repo));
    let splits_reply = match ctx.get_api(&splits_url, Route::Direct) {
        Ok(fetched) => fetched.reply,
        Err(response) => return response,
    };
    if !(200..300).contains(&splits_reply.status) {
        return viewer_failure(ctx, input, &splits_reply);
    }
    let splits_value =
        match parse_json(&splits_reply.body, "the datasets server's /splits response") {
            Ok(value) => value,
            Err(response) => return response,
        };
    let splits: Vec<(String, String)> = splits_value
        .get("splits")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|s| {
                    Some((
                        s.get("config")?.as_str()?.to_string(),
                        s.get("split")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if splits.is_empty() {
        let reason = ["failed", "pending"]
            .iter()
            .find_map(|key| {
                let list = splits_value.get(*key)?.as_array()?;
                let first = list.first()?;
                let text = first
                    .get("error")
                    .and_then(|e| e.get("error").or(Some(e)))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| first.to_string());
                Some(format!("{key}: {text}"))
            })
            .unwrap_or_else(|| "the server lists no splits".to_string());
        return failure(
            kind::VIEWER_UNAVAILABLE,
            format!(
                "the datasets server cannot serve {subject}: {}",
                cut(&reason, 300)
            ),
            Map::new(),
        );
    }

    let chosen = splits.iter().find(|(config, split)| {
        input.config.as_deref().is_none_or(|c| c == config)
            && input.split.as_deref().is_none_or(|s| s == split)
    });
    let Some((config, split)) = chosen.cloned() else {
        let listed: Vec<String> = splits.iter().map(|(c, s)| format!("{c}/{s}")).collect();
        return failure(
            kind::NOT_FOUND,
            format!(
                "{subject} has no config/split {}/{}; the datasets server lists {}",
                input.config.as_deref().unwrap_or("*"),
                input.split.as_deref().unwrap_or("*"),
                cut(&listed.join(", "), 1000)
            ),
            Map::new(),
        );
    };

    let (endpoint, path) = rows_path(input, &config, &split);
    let rows_url = format!("{server}{path}");
    let rows_reply = match ctx.get_api(&rows_url, Route::Direct) {
        Ok(fetched) => fetched.reply,
        Err(response) => return response,
    };
    if !(200..300).contains(&rows_reply.status) {
        return viewer_failure(ctx, input, &rows_reply);
    }
    let rows_value = match parse_json(
        &rows_reply.body,
        &format!("the datasets server's /{} response", endpoint.path()),
    ) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(rows) = rows_value.get("rows").and_then(Value::as_array) else {
        return failure(
            kind::RESPONSE_INVALID,
            format!(
                "the datasets server's /{} response has no rows list",
                endpoint.path()
            ),
            Map::new(),
        );
    };

    let total = rows_value.get("num_rows_total").and_then(Value::as_u64);
    let mut head = format!("Hugging Face dataset: {} — {config}/{split}", input.repo);
    let range = if rows.is_empty() {
        "no rows".to_string()
    } else {
        format!(
            "rows {}–{}",
            input.offset,
            input.offset + rows.len() as u64 - 1
        )
    };
    match endpoint {
        RowsEndpoint::Rows => head.push_str(&format!("; {range}")),
        RowsEndpoint::Search => head.push_str(&format!(
            "; search \"{}\"; {range} of the matches",
            input.query.as_deref().unwrap_or_default()
        )),
        RowsEndpoint::Filter => {
            head.push_str(&format!(
                "; where {}",
                input.where_.as_deref().unwrap_or_default()
            ));
            if let Some(order_by) = &input.order_by {
                head.push_str(&format!("; order by {order_by}"));
            }
            head.push_str(&format!("; {range} of the matches"));
        }
    }
    if let Some(total) = total {
        head.push_str(&format!(" (of {total})"));
    }
    if let Some(from) = input.limit.clamped_from {
        head.push_str(&format!(
            "\n[limit clamped from {from} to {}]",
            input.limit.value
        ));
    }
    head.push_str(ctx.defaults_line());

    let split_names: Vec<String> = splits.iter().map(|(c, s)| format!("{c}/{s}")).collect();
    let schema: Vec<String> = rows_value
        .get("features")
        .and_then(Value::as_array)
        .map(|features| features.iter().map(feature_line).collect())
        .unwrap_or_default();
    let row_lines: Vec<(String, bool)> = rows.iter().map(row_line).collect();

    let render = |limit: usize| {
        let mut page = Page::new(limit);
        page.push(&head, "header");
        // The splits list takes at most a quarter of what is left, cut at whole names.
        let split_room = page.room() / 4;
        let mut line = format!("\nsplits ({}):", split_names.len());
        let mut listed = 0;
        for (index, name) in split_names.iter().enumerate() {
            let piece = if index == 0 {
                format!(" {name}")
            } else {
                format!(", {name}")
            };
            if line.len() + piece.len() > split_room && listed > 0 {
                break;
            }
            line.push_str(&piece);
            listed += 1;
        }
        if listed < split_names.len() {
            line.push_str(&format!(" +{} more", split_names.len() - listed));
            page.note(format!("{listed} of {} splits listed", split_names.len()));
        }
        page.push(&line, "splits list");
        if !schema.is_empty() {
            page.push("\nschema:", "schema");
            for feature in &schema {
                page.push(&format!("\n{feature}"), "schema");
            }
        }
        page.push(&format!("\nrows ({} returned):", row_lines.len()), "rows");
        let mut shown = 0;
        for (line, cut_strings) in &row_lines {
            let piece = format!("\n{line}");
            if !page.try_push(&piece) {
                if shown == 0 {
                    page.push(&piece, "first row");
                    shown = 1;
                }
                break;
            }
            if *cut_strings {
                page.note(format!("row strings cut to {ROW_STRING_BYTES} bytes"));
            }
            shown += 1;
        }
        if shown < row_lines.len() {
            page.note(format!("{shown} of {} rows shown", row_lines.len()));
        }
        page
    };
    let out_dir = ctx.out_dir;
    let rendered = finish(ctx.config.max_output_bytes, render, || {
        let mut full = Map::new();
        full.insert("splits".to_string(), splits_value.clone());
        full.insert(endpoint.path().to_string(), rows_value.clone());
        spill(
            out_dir,
            "dataset",
            Value::Object(full).to_string().as_bytes(),
        )
    });
    let summary = match total {
        Some(total) => format!(
            "{} rows of {} {config}/{split} (of {total})",
            row_lines.len(),
            input.repo
        ),
        None => format!(
            "{} rows of {} {config}/{split}",
            row_lines.len(),
            input.repo
        ),
    };
    read_success("dataset", &input.repo, summary, rendered)
}

/// `name: dtype`, `name: ClassLabel [a, b]`, or `name: <_type> <json>`.
fn feature_line(feature: &Value) -> String {
    let name = feature.get("name").and_then(Value::as_str).unwrap_or("?");
    let ty = feature.get("type").cloned().unwrap_or(Value::Null);
    let kind = ty.get("_type").and_then(Value::as_str).unwrap_or_default();
    let shown = match kind {
        "Value" => ty
            .get("dtype")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        "ClassLabel" => {
            let names: Vec<&str> = ty
                .get("names")
                .and_then(Value::as_array)
                .map(|n| n.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!("ClassLabel [{}]", names.join(", "))
        }
        _ => cut(&ty.to_string(), FEATURE_TYPE_BYTES),
    };
    format!("{name}: {shown}")
}

/// `[row <row_idx>] {…}`, every string cut to [`ROW_STRING_BYTES`], and whether any was.
fn row_line(row: &Value) -> (String, bool) {
    let index = row
        .get("row_idx")
        .map_or_else(|| "?".to_string(), Value::to_string);
    let mut value = row.get("row").cloned().unwrap_or(Value::Null);
    let cut_any = cut_strings(&mut value);
    (format!("[row {index}] {value}"), cut_any)
}

fn cut_strings(value: &mut Value) -> bool {
    match value {
        Value::String(text) if text.len() > ROW_STRING_BYTES => {
            *text = cut(text, ROW_STRING_BYTES);
            true
        }
        Value::Array(items) => items.iter_mut().fold(false, |acc, v| cut_strings(v) | acc),
        Value::Object(map) => map.values_mut().fold(false, |acc, v| cut_strings(v) | acc),
        _ => false,
    }
}

/// A non-2xx datasets-server answer.
fn viewer_failure(ctx: &mut Ctx, input: &DatasetInput, reply: &crate::http::Reply) -> Response {
    let status = reply.status;
    let detail = upstream_text(None, &reply.body);
    let code = reply.header("x-error-code").unwrap_or_default();
    let coded = if code.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status} {code}")
    };
    let mut extra = Map::new();
    extra.insert("http_status".to_string(), json!(status));
    match status {
        401 | 404 => classify_via_hub(ctx, input, status, &coded, &detail),
        429 => {
            if let Some(retry_after) = reply.header("retry-after") {
                extra.insert("retry_after".to_string(), json!(retry_after));
            }
            failure(
                kind::RATE_LIMITED,
                format!("the datasets server rate-limited the request ({coded}): {detail}"),
                extra,
            )
        }
        500..=599 => failure(
            kind::VIEWER_UNAVAILABLE,
            format!(
                "the datasets server cannot serve dataset {} ({coded}): {detail}",
                input.repo
            ),
            extra,
        ),
        _ => failure(
            kind::UPSTREAM_ERROR,
            format!(
                "the datasets server refused the request for dataset {} ({coded}): {detail}",
                input.repo
            ),
            extra,
        ),
    }
}

/// Why the datasets server would not serve the dataset, asked of the Hub.
fn classify_via_hub(
    ctx: &mut Ctx,
    input: &DatasetInput,
    viewer_status: u16,
    coded: &str,
    detail: &str,
) -> Response {
    let subject = format!("dataset {}", input.repo);
    let why = "The datasets server is reached without a credential (murmur gives this tool \
               one gateway, bound to the Hub), so a gated or private dataset cannot be peeked; \
               op download can fetch the dataset's files through the gateway";
    let url = ctx.hub(&format!("/api/datasets/{}", input.repo));
    let fetched = match ctx.get_api(&url, Route::Gateway) {
        Ok(fetched) => fetched,
        Err(response) => return response,
    };
    let reply = fetched.reply;
    let mut extra = Map::new();
    if !(200..300).contains(&reply.status) {
        let hub = hub_failure(reply.status, &reply.headers, &reply.body, &subject, true);
        // Carry the viewer's context into the Hub's verdict.
        let data: Value = serde_json::from_str(&hub.data).unwrap_or(Value::Null);
        let error_kind = data["error_kind"].as_str().unwrap_or(kind::UPSTREAM_ERROR);
        let mut extra = Map::new();
        extra.insert("http_status".to_string(), json!(viewer_status));
        if let Some(retry) = data.get("retry_after") {
            extra.insert("retry_after".to_string(), retry.clone());
        }
        return failure(
            error_kind,
            format!(
                "{}. The datasets server answered {coded}: {detail}. {why}",
                hub.summary
            ),
            extra,
        );
    }
    let meta = body_value(&reply.body);
    let gated = meta.get("gated").cloned().unwrap_or(Value::Bool(false));
    let private = meta
        .get("private")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    extra.insert("http_status".to_string(), json!(viewer_status));
    if gated != Value::Bool(false) && !gated.is_null() {
        let mode = gated
            .as_str()
            .map_or_else(|| gated.to_string(), str::to_string);
        return failure(
            kind::GATED,
            format!(
                "{subject} is gated ({mode}); the datasets server answered {coded}: {detail}. {why}"
            ),
            extra,
        );
    }
    if private {
        return failure(
            kind::UPSTREAM_UNAUTHORIZED,
            format!(
                "{subject} is private; the datasets server answered {coded}: {detail}. {why}. \
                 Its files are readable through the gateway when the token bound as \
                 gateway.api_key ({}) can read the repo",
                crate::SUGGESTED_CREDENTIAL
            ),
            extra,
        );
    }
    failure(
        kind::VIEWER_UNAVAILABLE,
        format!(
            "the datasets server answered {coded} for {subject}, which the Hub lists as public \
             and not gated: {detail}"
        ),
        extra,
    )
}
