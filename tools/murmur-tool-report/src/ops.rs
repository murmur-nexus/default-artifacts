//! The two operations, the output envelope, and the error-kind vocabulary.
//!
//! There are exactly two: `report`, which is terminal, and `progress`, which is not.
//! Deliberately absent is a read-back operation — the file is the interface, and a capsule
//! asking what it already concluded would be reading its own memory back through a tool.
//! Equally absent is anything that routes, retries or bounces on an outcome: this crate
//! validates `outcome` against a closed set and branches on none of its members, because
//! what an outcome means belongs to the consumer's configuration and not to the capsule
//! that emitted it.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::config::{parse_config, ReportConfig};
use crate::json_type_name;
use crate::report::{now_rfc3339_millis, Deliverable, NoteFor, ReportDoc, RunIdentity};
use crate::store::{Store, REPORT_FILE};

/// The complete error vocabulary. A caller sees one of these strings in `error_kind` and
/// nothing else.
pub mod kind {
    /// The call itself was malformed: unparseable, not an object, or missing a field the
    /// requested operation requires.
    pub const INVALID_INPUT: &str = "invalid_input";
    /// `operation` named something other than `report` or `progress`.
    pub const UNKNOWN_OPERATION: &str = "unknown_operation";
    /// `outcome` is outside the closed set.
    pub const UNKNOWN_OUTCOME: &str = "unknown_outcome";
    /// A deliverable carries a payload rather than a reference. Nothing was written.
    pub const INLINE_PAYLOAD_REFUSED: &str = "inline_payload_refused";
    /// A deliverable `kind` is outside the operator's declared set.
    pub const UNDECLARED_KIND: &str = "undeclared_kind";
    /// A note `stage` is outside the operator's declared set.
    pub const UNDECLARED_STAGE: &str = "undeclared_stage";
    /// The durable-state grant is missing, so there is nowhere durable to report to.
    pub const STATE_UNAVAILABLE: &str = "state_unavailable";
    /// The `config:` block is present but not usable.
    pub const CONFIG_INVALID: &str = "config_invalid";
    /// The filesystem refused a read, a write or the rename.
    pub const IO_ERROR: &str = "io_error";
}

/// Reserved metadata key: how the call affected the resource it addressed.
pub const META_STATE_EFFECT: &str = "state_effect";
/// Reserved metadata key: which resource the call addressed.
pub const META_RESOURCE_ID: &str = "resource_id";
/// `state_effect` for every call this tool serves: both operations write the report file,
/// and neither is a read a consumer could be credited for repeating.
pub const EFFECT_MUTATE: &str = "mutate";
/// Prefix of the opaque `resource_id` this tool declares. The host records the value
/// verbatim and never parses it; the one report file is the one resource.
pub const RESOURCE_PREFIX: &str = "report:";

/// The two operation names, in the order the manifest's enum lists them.
pub const OPERATIONS: [&str; 2] = ["report", "progress"];

/// The closed outcome vocabulary. Fixed so a caller that has never seen this capsule can
/// still route on it; validated here and branched on nowhere.
pub const OUTCOMES: [&str; 4] = ["success", "rejected", "blocked", "failed"];

/// How many characters a deliverable's `uri` may carry.
///
/// A reference to a file is a path or a URL; anything an order of magnitude longer than
/// the longest of those is a payload wearing a reference's name, and refusing it here is
/// what keeps `uri` a pointer rather than a smuggling channel.
pub const MAX_REFERENCE_CHARS: usize = 2048;

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
        | kind::UNKNOWN_OUTCOME
        | kind::INLINE_PAYLOAD_REFUSED
        | kind::UNDECLARED_KIND
        | kind::UNDECLARED_STAGE => OpStatus::Failed,
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

/// Dispatch one call against the report rooted at `state_dir`.
///
/// `state_dir`, `config_json` and `identity` are all supplied by the caller so nothing
/// below `lib.rs` knows the guest path or reads the process environment: the component
/// passes `state`, the `MURMUR_ARTIFACT_CONFIG` it was launched with and the session and
/// capsule the runtime stamped into its environment; host tests pass a temp directory and
/// literals. `config_json` of `None` means the artifact's manifest entry declared no
/// `config:` block at all, which is the permissive default and not an error.
pub fn run(
    state_dir: &Path,
    config_json: Option<&str>,
    identity: &RunIdentity,
    data: &str,
) -> Response {
    let resource_id = format!("{RESOURCE_PREFIX}{}", state_dir.join(REPORT_FILE).display());
    let (operation, args) = match parse_call(data) {
        Ok(parsed) => parsed,
        Err(e) => return failure(UNKNOWN_OPERATION_LABEL, &e, &resource_id),
    };
    match dispatch(state_dir, config_json, identity, &operation, &args) {
        Ok(response) => response,
        Err(e) => failure(&operation, &e, &resource_id),
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
    identity: &RunIdentity,
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
    let config = load_config(config_json)?;

    match operation {
        "report" => op_report(&store, &config, identity, args),
        "progress" => op_progress(&store, identity, args),
        _ => unreachable!("operation was checked against OPERATIONS above"),
    }
}

/// The operator configuration for this artifact, as the runtime delivered it.
///
/// An entry with no `config:` key gets no environment variable and therefore no block,
/// which is the permissive default rather than a fault: a capsule that has declared no
/// vocabulary can still report, which is what makes adoption incremental.
fn load_config(config_json: Option<&str>) -> Result<ReportConfig, OpError> {
    match config_json {
        None => Ok(ReportConfig::permissive()),
        Some(text) => {
            parse_config(text).map_err(|message| OpError::new(kind::CONFIG_INVALID, message))
        }
    }
}

// ── report ────────────────────────────────────────────────────────────────────

/// File the capsule's verdict. Terminal: after this the file says `concluded: true`.
///
/// Everything is validated before anything is written, so a refused report leaves the
/// previous report — or the absent file — exactly as it was.
fn op_report(
    store: &Store,
    config: &ReportConfig,
    identity: &RunIdentity,
    args: &Map<String, Value>,
) -> Result<Response, OpError> {
    let outcome = required_str(args, "outcome")?;
    if !OUTCOMES.contains(&outcome) {
        return Err(OpError::new(
            kind::UNKNOWN_OUTCOME,
            format!(
                "unknown outcome \"{outcome}\"; expected one of {}",
                OUTCOMES.join(", ")
            ),
        ));
    }
    let summary = required_str(args, "summary")?;
    let deliverables = parse_deliverables(args, config)?;
    let notes_for = parse_notes(args, config)?;

    let mut doc = store.load()?.unwrap_or_else(|| ReportDoc::new(identity));
    doc.stamp(identity);
    doc.conclude(
        outcome.to_string(),
        summary.to_string(),
        deliverables,
        notes_for,
        now_rfc3339_millis(),
    );
    store.save(&doc)?;

    let summary_line = match doc.revision {
        1 => format!("reported outcome \"{outcome}\" (revision 1)"),
        revision => format!(
            "reported outcome \"{outcome}\" (revision {revision}); the previous conclusion is \
             in superseded"
        ),
    };
    Ok(success("report", &doc, store, summary_line))
}

// ── progress ──────────────────────────────────────────────────────────────────

/// Record that work is advancing. Concludes nothing: `concluded`, `outcome`, `summary`,
/// `reported_at` and `revision` come back out of the file unchanged.
fn op_progress(
    store: &Store,
    identity: &RunIdentity,
    args: &Map<String, Value>,
) -> Result<Response, OpError> {
    let note = required_str(args, "note")?;

    let mut doc = store.load()?.unwrap_or_else(|| ReportDoc::new(identity));
    doc.stamp(identity);
    doc.add_progress(note.to_string(), now_rfc3339_millis());
    store.save(&doc)?;

    let summary_line = if doc.concluded {
        format!(
            "progress note recorded ({} total); the conclusion already filed is unchanged",
            doc.progress_count
        )
    } else {
        format!(
            "progress note recorded ({} total); no conclusion filed yet",
            doc.progress_count
        )
    };
    Ok(success("progress", &doc, store, summary_line))
}

// ── deliverables and notes ────────────────────────────────────────────────────

/// The `deliverables` list, validated against both the reference rule and the operator's
/// declared kinds. Absent means an empty list: a capsule may conclude with no artefacts.
fn parse_deliverables(
    args: &Map<String, Value>,
    config: &ReportConfig,
) -> Result<Vec<Deliverable>, OpError> {
    let values = optional_array(args, "deliverables")?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut deliverables = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let deliverable = parse_deliverable(value, index)?;
        if !seen.insert(deliverable.name.clone()) {
            return Err(OpError::new(
                kind::INVALID_INPUT,
                format!(
                    "two deliverables are named \"{}\"; a name identifies one deliverable \
                     within one report",
                    deliverable.name
                ),
            ));
        }
        if !config.accepts_kind(&deliverable.kind) {
            return Err(OpError::new(
                kind::UNDECLARED_KIND,
                format!(
                    "deliverable \"{}\" has kind \"{}\", which the operator did not declare \
                     (declared kinds: {}); add it under deliverables.kinds in the `config:` \
                     block on this artifact's entry in the capsule's murmur.yaml",
                    deliverable.name,
                    deliverable.kind,
                    config.declared_kinds()
                ),
            ));
        }
        deliverables.push(deliverable);
    }
    Ok(deliverables)
}

/// One deliverable: exactly `{name, kind, uri}`, all three present and non-empty, `uri` a
/// reference and not a payload.
///
/// The unknown-key rule is the strong form of "references only": rather than naming the
/// payload keys to refuse — `content`, `body`, `data` and whatever is invented next — the
/// object may carry only the three keys it is defined as. Everything else is refused
/// outright.
pub fn parse_deliverable(value: &Value, index: usize) -> Result<Deliverable, OpError> {
    let Value::Object(map) = value else {
        return Err(OpError::new(
            kind::INVALID_INPUT,
            format!(
                "deliverables[{index}] must be a JSON object of {{name, kind, uri}}, got {}",
                json_type_name(value)
            ),
        ));
    };

    // Resolved before the key check so a refusal names the deliverable the operator can
    // find in their own call, falling back to its position when even the name is unusable.
    let label = match map.get("name").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => format!("\"{name}\""),
        _ => format!("deliverables[{index}]"),
    };

    let unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !matches!(*key, "name" | "kind" | "uri"))
        .collect();
    if !unknown.is_empty() {
        return Err(OpError::new(
            kind::INLINE_PAYLOAD_REFUSED,
            format!(
                "deliverable {label} carries the key(s) {}; a deliverable is a reference — \
                 exactly {{name, kind, uri}} — and never an inline payload. Write the content \
                 to the workspace and name its path in \"uri\".",
                unknown
                    .iter()
                    .map(|key| format!("\"{key}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    let name = required_str(map, "name")?.to_string();
    let kind_tag = required_str(map, "kind")?.to_string();
    let uri = required_str(map, "uri")?.to_string();

    if uri.trim_start().to_ascii_lowercase().starts_with("data:") {
        return Err(OpError::new(
            kind::INLINE_PAYLOAD_REFUSED,
            format!(
                "deliverable {label} has a data: URI in \"uri\"; that is the payload inline \
                 under a reference's name. Write it to the workspace and name its path."
            ),
        ));
    }
    if uri.contains('\n') || uri.contains('\r') {
        return Err(OpError::new(
            kind::INLINE_PAYLOAD_REFUSED,
            format!(
                "deliverable {label} has a newline in \"uri\"; a reference is one line — a \
                 workspace path or a URI — not a document."
            ),
        ));
    }
    let length = uri.chars().count();
    if length > MAX_REFERENCE_CHARS {
        return Err(OpError::new(
            kind::INLINE_PAYLOAD_REFUSED,
            format!(
                "deliverable {label} has a \"uri\" of {length} characters, over the \
                 {MAX_REFERENCE_CHARS}-character reference cap; a value that long is a payload, \
                 not a pointer."
            ),
        ));
    }

    Ok(Deliverable { name, kind: kind_tag, uri })
}

/// The `notes_for` list, validated against the operator's declared stages. Absent means an
/// empty list.
fn parse_notes(
    args: &Map<String, Value>,
    config: &ReportConfig,
) -> Result<Vec<NoteFor>, OpError> {
    let values = optional_array(args, "notes_for")?;
    let mut notes = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let note = parse_note(value, index)?;
        if !config.accepts_stage(&note.stage) {
            return Err(OpError::new(
                kind::UNDECLARED_STAGE,
                format!(
                    "note for stage \"{}\" is outside the stages the operator declared \
                     (declared stages: {}); add it under notes_for.stages in the `config:` \
                     block on this artifact's entry in the capsule's murmur.yaml",
                    note.stage,
                    config.declared_stages()
                ),
            ));
        }
        notes.push(note);
    }
    Ok(notes)
}

/// One note: exactly `{stage, body}`, both present and non-empty. `stage` is opaque — this
/// crate never interprets it and assumes no board, no card and no stage name.
fn parse_note(value: &Value, index: usize) -> Result<NoteFor, OpError> {
    let Value::Object(map) = value else {
        return Err(OpError::new(
            kind::INVALID_INPUT,
            format!(
                "notes_for[{index}] must be a JSON object of {{stage, body}}, got {}",
                json_type_name(value)
            ),
        ));
    };
    let unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !matches!(*key, "stage" | "body"))
        .collect();
    if !unknown.is_empty() {
        return Err(OpError::new(
            kind::INVALID_INPUT,
            format!(
                "notes_for[{index}] carries the key(s) {}; a note is exactly {{stage, body}}",
                unknown
                    .iter()
                    .map(|key| format!("\"{key}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(NoteFor {
        stage: required_str(map, "stage")?.to_string(),
        body: required_str(map, "body")?.to_string(),
    })
}

// ── envelope helpers ──────────────────────────────────────────────────────────

/// The success envelope. Both operations report the same view of the file they just wrote,
/// so a caller reads `concluded` and `revision` the same way whichever one it called.
fn success(operation: &str, doc: &ReportDoc, store: &Store, summary: String) -> Response {
    let envelope = json!({
        "ok": true,
        "operation": operation,
        "concluded": doc.concluded,
        "outcome": doc.outcome,
        "revision": doc.revision,
        "superseded_count": doc.superseded_count,
        "progress_count": doc.progress_count,
        "report_path": store.report_path().display().to_string(),
    });
    Response {
        status: OpStatus::Passed,
        summary,
        data: envelope.to_string(),
        metadata: metadata(&format!("{RESOURCE_PREFIX}{}", store.report_path().display())),
    }
}

/// A failure envelope. It still declares the resource it addressed: a caller that repeats
/// a refused report is repeating a call against the same file, and the host's redundant-
/// call detection needs the addressing to see that.
fn failure(operation: &str, error: &OpError, resource_id: &str) -> Response {
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
        metadata: metadata(resource_id),
    }
}

/// The reserved metadata keys, on every response. Both operations write the one report
/// file, so the effect is `mutate` and the resource is that file.
fn metadata(resource_id: &str) -> Vec<(String, String)> {
    vec![
        (META_STATE_EFFECT.to_string(), EFFECT_MUTATE.to_string()),
        (META_RESOURCE_ID.to_string(), resource_id.to_string()),
    ]
}

fn required_str<'a>(args: &'a Map<String, Value>, field: &str) -> Result<&'a str, OpError> {
    match args.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s),
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

/// An optional list argument. Absent and `null` are both the empty list; a non-list is a
/// caller fault rather than a single value quietly wrapped into one.
fn optional_array<'a>(
    args: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a [Value], OpError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        Some(other) => Err(OpError::new(
            kind::INVALID_INPUT,
            format!("\"{field}\" must be a list, got {}", json_type_name(other)),
        )),
    }
}
