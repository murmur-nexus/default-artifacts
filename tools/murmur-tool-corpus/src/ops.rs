//! The five operations, the output envelope, and the error-kind vocabulary.
//!
//! There are exactly five: `append`, `get`, `read_recent`, `search` and `verify`.
//! Deliberately absent is any operation that returns the whole corpus — `read_recent` is
//! capped by operator config and `search` returns at most `k` excerpt hits, also capped,
//! so an unbounded read is not forbidden so much as inexpressible. Equally deliberately
//! absent is a `repair` verb: `verify` names the lines a scan could not use, and fixing
//! them is a human action on `corpus.jsonl`, because a rewriting code path would be the
//! only one in this crate that opens the corpus for something other than append.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::config::{parse_config, Config};
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
    /// `operation` named something other than the five.
    pub const UNKNOWN_OPERATION: &str = "unknown_operation";
    /// The durable-state grant is missing, so the corpus is unreachable.
    pub const STATE_UNAVAILABLE: &str = "state_unavailable";
    /// This artifact's entry in the capsule manifest declares no `config:` block.
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

/// The five operation names, in the order the manifest's enum lists them.
pub const OPERATIONS: [&str; 5] = ["append", "get", "read_recent", "search", "verify"];

/// How many characters of source text a search excerpt or a `verify` preview carries.
///
/// Enough to recognise the record without pulling its body into context — the whole point
/// of returning an excerpt rather than a record is that an agent scans hits cheaply and
/// calls `get` on the two or three it actually wants.
pub const EXCERPT_CHARS: usize = 120;

/// How many line numbers a single response will list, in `skipped_lines` or `bad_lines`.
///
/// The counts beside those lists are always the true totals, so the cap costs an operator
/// nothing but the tail of a list they would not read anyway. A corrupt corpus must not
/// flood an agent's context with line numbers.
pub const MAX_REPORTED_LINES: usize = 100;

/// `operation` reported in the envelope when the call did not name a usable one.
const UNKNOWN_OPERATION_LABEL: &str = "unknown";

/// A failure, carrying the error kind that decides the tool's status.
///
/// `skipped_lines` is empty unless the failure was raised after a scan that could not read
/// every line. Carrying it here is what lets a caller tell "no such record" apart from
/// "hidden behind a line I could not read" — the two look identical in a bare `not_found`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub kind: &'static str,
    pub message: String,
    pub skipped_lines: Vec<u64>,
}

impl OpError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), skipped_lines: Vec::new() }
    }

    /// Attach the skip list of the scan this failure was raised after.
    pub fn with_skipped(mut self, skipped: &[u64]) -> Self {
        self.skipped_lines = skipped.to_vec();
        self
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
/// `state_dir` and `config_json` are both supplied by the caller so nothing below
/// `lib.rs` knows the guest path or reads the process environment: the component passes
/// `state` and the `MURMUR_ARTIFACT_CONFIG` it was launched with, host tests pass a temp
/// directory and a literal. `None` means the artifact's manifest entry declared no
/// `config:` block at all.
pub fn run(state_dir: &Path, config_json: Option<&str>, data: &str) -> Response {
    let (operation, args) = match parse_call(data) {
        Ok(parsed) => parsed,
        Err(e) => return failure(UNKNOWN_OPERATION_LABEL, &e),
    };
    match dispatch(state_dir, config_json, &operation, &args) {
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
    config_json: Option<&str>,
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

    // `verify` runs on the state grant alone. It is the diagnostic an operator reaches for
    // when the corpus stops behaving, and the configuration may be precisely what is
    // missing — gating it behind the thing being diagnosed would make it useless exactly
    // when it is needed.
    if operation == "verify" {
        return op_verify(&store);
    }

    let config = load_config(config_json)?;
    match operation {
        "append" => op_append(&store, &config, args),
        "get" => op_get(&store, args),
        "read_recent" => op_read_recent(&store, &config, args),
        "search" => op_search(&store, &config, args),
        _ => unreachable!("operation was checked against OPERATIONS above"),
    }
}

/// The operator configuration for this artifact, as the runtime delivered it.
///
/// The runtime validates shape and not meaning — it guarantees well-formed JSON of an
/// object within its size cap, and nothing about which keys this tool needs — so every
/// semantic check stays in [`parse_config`].
fn load_config(config_json: Option<&str>) -> Result<Config, OpError> {
    let text = config_json.ok_or_else(|| {
        OpError::new(
            kind::CONFIG_MISSING,
            "this artifact's entry in the capsule's murmur.yaml declares no `config:` block; \
             add one under the murmur-tool-corpus entry declaring config_version and the \
             record types this corpus accepts — the store refuses every operation but \
             `verify` until an operator has declared them",
        )
    })?;
    parse_config(text).map_err(|message| OpError::new(kind::CONFIG_INVALID, message))
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

    let scan = store.read_all()?;
    let records = &scan.records;

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
                &scan.skipped_lines,
            ));
        }
    }

    if let Some(target) = withdraws {
        if !records.iter().any(|r| r.id == target) {
            // Worst case a bad line hides the target and a legitimate withdrawal is
            // refused; the skip list is what tells the caller to look before believing it.
            return Err(OpError::new(
                kind::WITHDRAW_TARGET_NOT_FOUND,
                format!("cannot withdraw \"{target}\": no record carries that id"),
            )
            .with_skipped(&scan.skipped_lines));
        }
        if let Some(existing) = withdrawal_index(records).get(target) {
            return Err(OpError::new(
                kind::ALREADY_WITHDRAWN,
                format!(
                    "cannot withdraw \"{target}\": it was already withdrawn by {} at {}",
                    existing.by, existing.at
                ),
            )
            .with_skipped(&scan.skipped_lines));
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
        &scan.skipped_lines,
    ))
}

// ── get ───────────────────────────────────────────────────────────────────────

fn op_get(store: &Store, args: &Map<String, Value>) -> Result<Response, OpError> {
    let id = required_str(args, "id")?;
    let scan = store.read_all()?;
    let record = scan
        .records
        .iter()
        .find(|r| r.id == id)
        .ok_or_else(|| {
            // A skip list here is the difference between "no such record" and "hidden
            // behind a line I could not read".
            OpError::new(kind::NOT_FOUND, format!("no record carries the id \"{id}\""))
                .with_skipped(&scan.skipped_lines)
        })?;

    let withdrawn = withdrawal_index(&scan.records).get(id).cloned();
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
        &scan.skipped_lines,
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

    let scan = store.read_all()?;
    let withdrawn = withdrawal_index(&scan.records);
    let mut matching: Vec<&Record> = scan
        .records
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
        &scan.skipped_lines,
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

    let scan = store.read_all()?;
    let withdrawn = withdrawal_index(&scan.records);

    let mut scored: Vec<(f64, &Record)> = Vec::new();
    for record in &scan.records {
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

    // Score descending, then recency (newest first). `created_at` is only
    // millisecond-resolution, so two records minted in the same millisecond tie on it;
    // the id breaks that tie in the same direction, because it embeds the same timestamp
    // followed by a per-call mint sequence and so sorts lexicographically in mint order.
    // Descending on both is what makes this newest-first *and* byte-identical across
    // repeated runs — an ascending id tie-break would silently order a same-millisecond
    // pair oldest-first, and disagree with `read_recent` on the same two records.
    scored.sort_by(|(sa, ra), (sb, rb)| {
        sb.partial_cmp(sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (&rb.created_at, &rb.id).cmp(&(&ra.created_at, &ra.id)))
    });

    // A hit is not a record: it carries an excerpt of the matching text, not the body.
    // Twenty-five full bodies per search is a context budget spent before the agent has
    // decided which two it wants — `get` retrieves those.
    let hits: Vec<Value> = scored
        .iter()
        .take(k)
        .map(|(score, record)| {
            json!({
                "id": record.id,
                "type": record.type_tag,
                "created_at": record.created_at,
                "score": score,
                "excerpt": excerpt_for(&query_terms, record),
            })
        })
        .collect();

    Ok(success(
        json!({ "ok": true, "operation": "search", "hits": hits }),
        format!("search \"{query}\": {} hit(s) (k={k})", hits.len()),
        EFFECT_READ,
        format!("corpus:search:{}", normalise_query(query)),
        &scan.skipped_lines,
    ))
}

/// The line of the record a hit shows: the **first** segment of [`searchable_text`] that
/// contains a query term, collapsed to one line and bounded at [`EXCERPT_CHARS`].
///
/// Consuming `searchable_text` rather than reaching into the body is what keeps excerpting
/// and scoring over exactly the same text, and keeps the seam a future embedding-based
/// retrieval will index over intact.
///
/// A record only reaches here with a positive score, so a matching segment exists; the
/// fallbacks keep a hit with no text at all out of the response should that ever change.
fn excerpt_for(query_terms: &BTreeSet<String>, record: &Record) -> String {
    let segments = searchable_text(record);
    let chosen = segments
        .iter()
        .find(|segment| terms(segment).iter().any(|term| query_terms.contains(term)))
        .or_else(|| segments.first());
    chosen.map(|segment| single_line_excerpt(segment)).unwrap_or_default()
}

// ── verify ────────────────────────────────────────────────────────────────────

/// Report every line the corpus scan cannot use.
///
/// Runs on the state grant alone, and writes nothing: this is a read, and there is no
/// repairing counterpart. Repair means editing `corpus.jsonl` by hand, which is a human
/// action taken with this report in front of you.
fn op_verify(store: &Store) -> Result<Response, OpError> {
    let report = store.verify()?;
    let bad_line_count = report.bad_lines.len() as u64;
    let bad_lines: Vec<Value> = report
        .bad_lines
        .iter()
        .take(MAX_REPORTED_LINES)
        .map(|bad| json!({ "line": bad.line, "error": bad.error, "preview": bad.preview }))
        .collect();

    let summary = format!(
        "verify: {} line(s), {} record(s), {bad_line_count} unreadable line(s)",
        report.lines, report.records
    );
    Ok(success(
        json!({
            "ok": true,
            "operation": "verify",
            "lines": report.lines,
            "records": report.records,
            "bad_line_count": bad_line_count,
            "bad_lines": bad_lines,
        }),
        summary,
        EFFECT_READ,
        "corpus:file".to_string(),
        // `verify` *is* the skip report; repeating it in the skip fields would say the
        // same thing twice and less precisely.
        &[],
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

/// One line of at most [`EXCERPT_CHARS`] characters, with `…` appended when characters
/// were dropped.
///
/// Truncation counts `chars()`, not bytes, so a multi-byte value is cut on a character
/// boundary rather than splitting a code point. `split_whitespace` both trims and collapses
/// every run, so no `\n`, `\r` or `\t` survives into an envelope.
pub(crate) fn single_line_excerpt(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(EXCERPT_CHARS).collect();
    if collapsed.chars().nth(EXCERPT_CHARS).is_some() {
        out.push('…');
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
    skipped: &[u64],
) -> Response {
    let mut envelope = envelope;
    let mut summary = summary;
    add_skip_fields(&mut envelope, &mut summary, skipped);
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
    let mut summary = format!("{}: {}", error.kind, error.message);
    let mut envelope = json!({
        "ok": false,
        "operation": operation,
        "error_kind": error.kind,
        "message": error.message,
    });
    add_skip_fields(&mut envelope, &mut summary, &error.skipped_lines);
    Response {
        status: status_for(error.kind),
        summary,
        data: envelope.to_string(),
        metadata: Vec::new(),
    }
}

/// Record on a response that its scan could not read every line.
///
/// The two fields are present together or not at all: `skipped_line_count` without the
/// numbers is unactionable, and neither field means the scan read the whole file. The list
/// is capped at [`MAX_REPORTED_LINES`] while the count stays the true total, and the count
/// is repeated in the summary, because that is the part a trace reader sees.
fn add_skip_fields(envelope: &mut Value, summary: &mut String, skipped: &[u64]) {
    if skipped.is_empty() {
        return;
    }
    if let Value::Object(map) = envelope {
        let reported: Vec<Value> =
            skipped.iter().take(MAX_REPORTED_LINES).map(|line| json!(line)).collect();
        map.insert("skipped_lines".to_string(), Value::Array(reported));
        map.insert("skipped_line_count".to_string(), json!(skipped.len() as u64));
    }
    summary.push_str(&format!("; skipped {} unparseable line(s)", skipped.len()));
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
