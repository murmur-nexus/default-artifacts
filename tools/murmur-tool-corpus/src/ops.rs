//! The four operations, the output envelope, and the error-kind vocabulary.
//!
//! There are exactly four: `append`, `get`, `read_recent` and `search`. Deliberately
//! absent is any operation that returns the whole corpus — `read_recent` is capped by
//! operator config and `search` returns at most `k` hits, also capped, so an unbounded
//! read is not forbidden so much as inexpressible.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::id::mint_id;
use crate::record::{now_rfc3339_millis, searchable_text, Record};
use crate::schema::validate_body;
use crate::store::{withdrawal_index, Store};

/// The complete error vocabulary. A caller sees one of these strings in `error_kind` and
/// nothing else.
pub mod kind {
    /// The call itself was malformed: unparseable, not an object, or missing a field the
    /// requested operation requires.
    pub const INVALID_INPUT: &str = "invalid_input";
    /// `operation` named something other than the four.
    pub const UNKNOWN_OPERATION: &str = "unknown_operation";
    /// The durable-state grant is missing, so the corpus is unreachable.
    pub const STATE_UNAVAILABLE: &str = "state_unavailable";
    /// State is reachable but no operator has configured this corpus.
    pub const CONFIG_MISSING: &str = "config_missing";
    /// The operator configuration is present but not usable.
    pub const CONFIG_INVALID: &str = "config_invalid";
    /// The append names a type no operator declared.
    pub const UNKNOWN_TYPE: &str = "unknown_type";
    /// The body failed the operator's schema for its type. Nothing was written.
    pub const SCHEMA_VIOLATION: &str = "schema_violation";
    /// No record carries the requested id.
    pub const NOT_FOUND: &str = "not_found";
    /// The record a withdrawal names does not exist.
    pub const WITHDRAW_TARGET_NOT_FOUND: &str = "withdraw_target_not_found";
    /// The record a withdrawal names has already been withdrawn.
    pub const ALREADY_WITHDRAWN: &str = "already_withdrawn";
    /// A line in the corpus does not parse as a record.
    pub const CORPUS_CORRUPT: &str = "corpus_corrupt";
    /// The filesystem refused a read or an append.
    pub const IO_ERROR: &str = "io_error";
}

/// Reserved metadata key: how the call affected the resource it addressed.
pub const META_STATE_EFFECT: &str = "state_effect";
/// Reserved metadata key: which resource the call addressed.
pub const META_RESOURCE_ID: &str = "resource_id";
/// `state_effect` for a call that only observed state.
pub const EFFECT_READ: &str = "read";
/// `state_effect` for a call that appended a line.
pub const EFFECT_MUTATE: &str = "mutate";

/// The four operation names, in the order the manifest's enum lists them.
pub const OPERATIONS: [&str; 4] = ["append", "get", "read_recent", "search"];

/// `operation` reported in the envelope when the call did not name a usable one.
const UNKNOWN_OPERATION_LABEL: &str = "unknown";

/// A failure, carrying the error kind that decides the tool's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub kind: &'static str,
    pub message: String,
}

impl OpError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// The status the host sees, mapped from the error kind: a caller fault fails, an
/// environment or operator fault errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpStatus {
    Passed,
    Failed,
    Error,
}

/// Which status an error kind maps to.
pub fn status_for(error_kind: &str) -> OpStatus {
    match error_kind {
        kind::INVALID_INPUT
        | kind::UNKNOWN_OPERATION
        | kind::UNKNOWN_TYPE
        | kind::SCHEMA_VIOLATION
        | kind::NOT_FOUND
        | kind::WITHDRAW_TARGET_NOT_FOUND
        | kind::ALREADY_WITHDRAWN => OpStatus::Failed,
        _ => OpStatus::Error,
    }
}

/// Everything the WIT adapter needs to build a `ToolResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: OpStatus,
    pub summary: String,
    /// The JSON envelope, already serialised.
    pub data: String,
    pub metadata: Vec<(String, String)>,
}

/// Dispatch one call against the corpus rooted at `state_dir`.
///
/// `state_dir` is supplied by the caller so nothing below `lib.rs` knows the guest path:
/// the component passes `state`, host tests pass a temp directory.
pub fn run(state_dir: &Path, data: &str) -> Response {
    let (operation, args) = match parse_call(data) {
        Ok(parsed) => parsed,
        Err(e) => return failure(UNKNOWN_OPERATION_LABEL, &e),
    };
    match dispatch(state_dir, &operation, &args) {
        Ok(response) => response,
        Err(e) => failure(&operation, &e),
    }
}

/// Split the call into its operation name and its arguments.
///
/// A `data` payload that parses to a JSON *string* is re-parsed once: some hosts
/// double-encode the tool arguments.
fn parse_call(data: &str) -> Result<(String, Map<String, Value>), OpError> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err(OpError::new(
            kind::INVALID_INPUT,
            "no input; expected a JSON object with an \"operation\" field",
        ));
    }
    let mut parsed: Value = serde_json::from_str(trimmed)
        .map_err(|e| OpError::new(kind::INVALID_INPUT, format!("input is not valid JSON: {e}")))?;
    if let Value::String(inner) = &parsed {
        parsed = serde_json::from_str(inner).map_err(|e| {
            OpError::new(kind::INVALID_INPUT, format!("input is not valid JSON: {e}"))
        })?;
    }
    let args = match parsed {
        Value::Object(map) => map,
        other => {
            return Err(OpError::new(
                kind::INVALID_INPUT,
                format!("input must be a JSON object, got {}", json_type_name(&other)),
            ))
        }
    };
    let operation = args
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OpError::new(
                kind::INVALID_INPUT,
                format!(
                    "\"operation\" is required and must be one of {}",
                    OPERATIONS.join(", ")
                ),
            )
        })?
        .to_string();
    Ok((operation, args))
}

fn dispatch(
    state_dir: &Path,
    operation: &str,
    args: &Map<String, Value>,
) -> Result<Response, OpError> {
    if !OPERATIONS.contains(&operation) {
        return Err(OpError::new(
            kind::UNKNOWN_OPERATION,
            format!(
                "unknown operation \"{operation}\"; expected one of {}",
                OPERATIONS.join(", ")
            ),
        ));
    }

    let store = Store::open(state_dir)?;
    let config = store.load_config()?;

    match operation {
        "append" => op_append(&store, &config, args),
        "get" => op_get(&store, args),
        "read_recent" => op_read_recent(&store, &config, args),
        "search" => op_search(&store, &config, args),
        _ => unreachable!("operation was checked against OPERATIONS above"),
    }
}

// ── append ────────────────────────────────────────────────────────────────────

fn op_append(
    store: &Store,
    config: &Config,
    args: &Map<String, Value>,
) -> Result<Response, OpError> {
    let type_tag = required_str(args, "type")?;
    let body = args.get("body").cloned().ok_or_else(|| {
        OpError::new(
            kind::INVALID_INPUT,
            "\"body\" is required for append; a withdrawal is itself a first-class record \
             and carries its own body",
        )
    })?;
    let external_id = optional_str(args, "external_id")?;
    let withdraws = optional_str(args, "withdraws")?;

    let type_config = config.type_config(type_tag).ok_or_else(|| {
        OpError::new(
            kind::UNKNOWN_TYPE,
            format!(
                "type \"{type_tag}\" is not declared in the operator configuration \
                 (declared types: {})",
                declared_types(config)
            ),
        )
    })?;

    validate_body(&type_config.schema, &body).map_err(|e| {
        OpError::new(
            kind::SCHEMA_VIOLATION,
            format!("body does not satisfy the schema for type \"{type_tag}\": {e}"),
        )
    })?;

    let records = store.read_all()?;

    // The dedupe check comes before the withdrawal checks so a retried withdrawal is
    // idempotent: on the second call its target is already withdrawn by the first call's
    // own record, which would otherwise surface as `already_withdrawn`.
    if let Some(external_id) = external_id {
        if let Some(existing) = records
            .iter()
            .find(|r| r.type_tag == type_tag && r.external_id.as_deref() == Some(external_id))
        {
            return Ok(success(
                json!({
                    "ok": true,
                    "operation": "append",
                    "id": existing.id,
                    "deduped": true,
                }),
                format!(
                    "append deduped: type \"{type_tag}\" external_id \"{external_id}\" already \
                     recorded as {}",
                    existing.id
                ),
                EFFECT_READ,
                format!("corpus:{}", existing.id),
            ));
        }
    }

    if let Some(target) = withdraws {
        if !records.iter().any(|r| r.id == target) {
            return Err(OpError::new(
                kind::WITHDRAW_TARGET_NOT_FOUND,
                format!("cannot withdraw \"{target}\": no record carries that id"),
            ));
        }
        if let Some(existing) = withdrawal_index(&records).get(target) {
            return Err(OpError::new(
                kind::ALREADY_WITHDRAWN,
                format!(
                    "cannot withdraw \"{target}\": it was already withdrawn by {} at {}",
                    existing.by, existing.at
                ),
            ));
        }
    }

    let record = Record {
        id: mint_id(&type_config.prefix),
        type_tag: type_tag.to_string(),
        schema_version: type_config.schema_version,
        created_at: now_rfc3339_millis(),
        external_id: external_id.map(str::to_string),
        withdraws: withdraws.map(str::to_string),
        body,
    };
    store.append_record(&record)?;

    let summary = match withdraws {
        Some(target) => format!(
            "appended {} (type \"{type_tag}\") withdrawing {target}",
            record.id
        ),
        None => format!("appended {} (type \"{type_tag}\")", record.id),
    };
    Ok(success(
        json!({
            "ok": true,
            "operation": "append",
            "id": record.id,
            "deduped": false,
        }),
        summary,
        EFFECT_MUTATE,
        format!("corpus:{}", record.id),
    ))
}

// ── get ───────────────────────────────────────────────────────────────────────

fn op_get(store: &Store, args: &Map<String, Value>) -> Result<Response, OpError> {
    let id = required_str(args, "id")?;
    let records = store.read_all()?;
    let record = records
        .iter()
        .find(|r| r.id == id)
        .ok_or_else(|| OpError::new(kind::NOT_FOUND, format!("no record carries the id \"{id}\"")))?;

    let withdrawn = withdrawal_index(&records).get(id).cloned();
    let (view, summary) = match withdrawn {
        Some(withdrawal) => {
            let mut view = record_value(record)?;
            // The record is still on disk untouched; what a withdrawal removes is the
            // caller's access to its body, not the line.
            view.insert("body".to_string(), Value::Null);
            view.insert("withdrawn_by".to_string(), Value::String(withdrawal.by.clone()));
            view.insert("withdrawn_at".to_string(), Value::String(withdrawal.at.clone()));
            (
                view,
                format!("{id} is withdrawn by {} at {}", withdrawal.by, withdrawal.at),
            )
        }
        None => (
            record_value(record)?,
            format!("{id} (type \"{}\") resolved", record.type_tag),
        ),
    };

    Ok(success(
        json!({ "ok": true, "operation": "get", "record": Value::Object(view) }),
        summary,
        EFFECT_READ,
        format!("corpus:{id}"),
    ))
}

// ── read_recent ───────────────────────────────────────────────────────────────

fn op_read_recent(
    store: &Store,
    config: &Config,
    args: &Map<String, Value>,
) -> Result<Response, OpError> {
    let type_tag = required_str(args, "type")?;
    // `requested` is what the caller asked for after defaulting, reported alongside
    // `returned` so a clamp is visible rather than silent. Clamping in `u64` keeps a
    // count larger than the component's 32-bit `usize` a capped read, not a wrapped one.
    let requested = optional_count(args, "n")?.unwrap_or(config.read_recent.default as u64);
    let limit = requested.min(config.read_recent.max as u64) as usize;

    let records = store.read_all()?;
    let withdrawn = withdrawal_index(&records);
    let mut matching: Vec<&Record> = records
        .iter()
        .filter(|r| r.type_tag == type_tag && !withdrawn.contains_key(&r.id))
        .collect();
    // Newest first. `(created_at, id)` is exactly mint order: the id embeds the same
    // millisecond timestamp plus a per-call sequence, so it breaks a same-millisecond tie
    // in mint order.
    matching.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));

    let values: Vec<Value> = matching
        .iter()
        .take(limit)
        .map(|r| record_value(r).map(Value::Object))
        .collect::<Result<_, _>>()?;
    let returned = values.len();

    Ok(success(
        json!({
            "ok": true,
            "operation": "read_recent",
            "records": values,
            "returned": returned,
            "requested": requested,
        }),
        format!("read_recent type \"{type_tag}\": returned {returned} of {requested} requested"),
        EFFECT_READ,
        format!("corpus:type:{type_tag}"),
    ))
}

// ── search ────────────────────────────────────────────────────────────────────

fn op_search(
    store: &Store,
    config: &Config,
    args: &Map<String, Value>,
) -> Result<Response, OpError> {
    let query = required_str(args, "query")?;
    let k = optional_count(args, "k")?
        .unwrap_or(config.search.default_k as u64)
        .min(config.search.max_k as u64) as usize;
    let type_filter = optional_str(args, "type")?;

    let query_terms: BTreeSet<String> = terms(query).into_iter().collect();

    let records = store.read_all()?;
    let withdrawn = withdrawal_index(&records);

    let mut scored: Vec<(f64, &Record)> = Vec::new();
    for record in &records {
        if withdrawn.contains_key(&record.id) {
            continue;
        }
        if let Some(wanted) = type_filter {
            if record.type_tag != wanted {
                continue;
            }
        }
        let score = score_record(&query_terms, record);
        if score > 0.0 {
            scored.push((score, record));
        }
    }

    // Score descending, then recency (newest first), then id ascending as the final
    // determinism tie-break, so repeated runs against an unchanged corpus are
    // byte-identical.
    scored.sort_by(|(sa, ra), (sb, rb)| {
        sb.partial_cmp(sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| rb.created_at.cmp(&ra.created_at))
            .then_with(|| ra.id.cmp(&rb.id))
    });

    let hits: Vec<Value> = scored
        .iter()
        .take(k)
        .map(|(score, record)| {
            json!({
                "id": record.id,
                "type": record.type_tag,
                "created_at": record.created_at,
                "score": score,
                "body": record.body,
            })
        })
        .collect();

    Ok(success(
        json!({ "ok": true, "operation": "search", "hits": hits }),
        format!("search \"{query}\": {} hit(s) (k={k})", hits.len()),
        EFFECT_READ,
        format!("corpus:search:{}", normalise_query(query)),
    ))
}

/// The fraction of distinct query terms that appear in any of the record's searchable
/// segments. Zero query terms scores zero, so a query that tokenises to nothing matches
/// nothing rather than everything.
fn score_record(query_terms: &BTreeSet<String>, record: &Record) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let record_terms: BTreeSet<String> = searchable_text(record)
        .iter()
        .flat_map(|segment| terms(segment))
        .collect();
    let matched = query_terms.iter().filter(|t| record_terms.contains(*t)).count();
    matched as f64 / query_terms.len() as f64
}

/// Lowercase `[a-z0-9]+` runs. Everything else — punctuation, whitespace, non-ASCII — is
/// a separator, so the query and the indexed text are tokenised by the identical rule.
pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            current.push(c);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The query as it appears in `resource_id`: lowercased, whitespace-normalised.
fn normalise_query(query: &str) -> String {
    query.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── envelope helpers ──────────────────────────────────────────────────────────

fn success(
    envelope: Value,
    summary: String,
    state_effect: &str,
    resource_id: String,
) -> Response {
    Response {
        status: OpStatus::Passed,
        summary,
        data: envelope.to_string(),
        metadata: vec![
            (META_STATE_EFFECT.to_string(), state_effect.to_string()),
            (META_RESOURCE_ID.to_string(), resource_id),
        ],
    }
}

/// A failure envelope. It carries no `state_effect` or `resource_id`: a failed call
/// addressed nothing the host should record, and an unqualified `read` on a rejected
/// append would misreport it.
fn failure(operation: &str, error: &OpError) -> Response {
    Response {
        status: status_for(error.kind),
        summary: format!("{}: {}", error.kind, error.message),
        data: json!({
            "ok": false,
            "operation": operation,
            "error_kind": error.kind,
            "message": error.message,
        })
        .to_string(),
        metadata: Vec::new(),
    }
}

fn record_value(record: &Record) -> Result<Map<String, Value>, OpError> {
    match serde_json::to_value(record) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) | Err(_) => Err(OpError::new(
            kind::IO_ERROR,
            format!("record {} could not be rendered as a JSON object", record.id),
        )),
    }
}

fn declared_types(config: &Config) -> String {
    config.types.keys().cloned().collect::<Vec<_>>().join(", ")
}

fn required_str<'a>(args: &'a Map<String, Value>, field: &str) -> Result<&'a str, OpError> {
    match args.get(field) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        Some(Value::String(_)) => Err(OpError::new(
            kind::INVALID_INPUT,
            format!("\"{field}\" must be a non-empty string"),
        )),
        Some(other) => Err(OpError::new(
            kind::INVALID_INPUT,
            format!("\"{field}\" must be a string, got {}", json_type_name(other)),
        )),
        None => Err(OpError::new(
            kind::INVALID_INPUT,
            format!("\"{field}\" is required for this operation"),
        )),
    }
}

fn optional_str<'a>(args: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>, OpError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(Value::String(_)) => Err(OpError::new(
            kind::INVALID_INPUT,
            format!("\"{field}\" must be a non-empty string when present"),
        )),
        Some(other) => Err(OpError::new(
            kind::INVALID_INPUT,
            format!("\"{field}\" must be a string, got {}", json_type_name(other)),
        )),
    }
}

/// A positive count (`n` or `k`). Absent means "use the operator default"; present and
/// non-positive is a caller fault, because silently reading it as "unbounded" is the one
/// interpretation this store must never offer.
///
/// The count stays a `u64` rather than becoming a `usize` here. `usize` is 32 bits in the
/// component, so a caller asking for more than `u32::MAX` would wrap — and a request for
/// far too much would come back as an empty result set rather than a capped one, which is
/// the silent short read this store must never produce. Clamping against the operator cap
/// happens in `u64` and only the clamped value, always at most the cap, becomes a `usize`.
fn optional_count(args: &Map<String, Value>, field: &str) -> Result<Option<u64>, OpError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(0) | None => Err(OpError::new(
                kind::INVALID_INPUT,
                format!("\"{field}\" must be a positive integer when present"),
            )),
            Some(n) => Ok(Some(n)),
        },
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
