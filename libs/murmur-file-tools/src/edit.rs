//! `tool-edit`: apply `{old, new}` pairs to one file, in order, all or nothing.

use serde_json::Value;

use crate::{
    err, err_result, fail_msg, io_error_kind, ok_summary_only, reject_absolute,
    reject_unknown_fields, required_path, EDIT_PAIR_FIELDS,
};

/// Apply `edits` to one file, all or nothing. Every input is validated before the file is
/// read; the file is then read once, each `{old, new}` pair is applied in order to the
/// result of the one before, and the file is written once, only after every pair matched
/// exactly once. A failing pair is named `edit N of M`, counted from 1.
pub(crate) fn edit(op: &Value) -> Value {
    let path = match required_path(op, "path") {
        Ok(p) => p,
        Err(failure) => return failure,
    };
    if let Some(refusal) = reject_absolute("path", &path) {
        return refusal;
    }
    let pairs = match edit_pairs(op) {
        Ok(pairs) => pairs,
        Err(failure) => return failure,
    };
    let total = pairs.len();

    let mut text = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return err_result(io_error_kind(&e), format!("{path}: {e}")),
    };
    for (index, (old, new)) in pairs.iter().enumerate() {
        let position = index + 1;
        let count = occurrences(&text, old);
        if count != 1 {
            let kind = if count == 0 {
                err::STRING_NOT_FOUND
            } else {
                err::MULTIPLE_MATCHES
            };
            return err_result(
                kind,
                format!(
                    "edit {position} of {total}: old occurs {count} times in {path}; the file is unchanged"
                ),
            );
        }
        // The one occurrence is the first.
        if let Some(at) = text.find(old.as_str()) {
            text.replace_range(at..at + old.len(), new);
        }
    }

    match std::fs::write(&path, text) {
        Ok(()) => ok_summary_only(
            format!("applied {total} edit(s) to {path}"),
            format!("{total} edits"),
        ),
        Err(e) => err_result(io_error_kind(&e), format!("{path}: {e}")),
    }
}

/// The `(old, new)` pairs of `edits`, every one validated: a non-empty array of
/// objects holding only `old`, a non-empty string, and `new`, a string that may be empty.
fn edit_pairs(op: &Value) -> Result<Vec<(String, String)>, Value> {
    let edits = match op.get("edits") {
        None | Some(Value::Null) => return Err(fail_msg("missing required field: edits")),
        Some(Value::Array(edits)) => edits,
        Some(_) => return Err(fail_msg("edits must be an array of {old, new} objects")),
    };
    if edits.is_empty() {
        return Err(fail_msg(
            "edits is empty: it takes at least one {old, new} object",
        ));
    }
    let total = edits.len();
    let mut pairs = Vec::with_capacity(total);
    for (index, pair) in edits.iter().enumerate() {
        let place = format!("edit {} of {total}", index + 1);
        if !pair.is_object() {
            return Err(fail_msg(format!(
                "{place} must be an object with old and new"
            )));
        }
        if let Some(refusal) = reject_unknown_fields(pair, &place, "an edit", EDIT_PAIR_FIELDS) {
            return Err(refusal);
        }
        let old = match pair.get("old").and_then(Value::as_str) {
            Some("") => {
                return Err(fail_msg(format!(
                    "{place}: old is empty; it must name the text to replace"
                )))
            }
            Some(old) => old.to_string(),
            None => return Err(fail_msg(format!("{place}: missing required field: old"))),
        };
        let new = match pair.get("new").and_then(Value::as_str) {
            Some(new) => new.to_string(),
            None => return Err(fail_msg(format!("{place}: missing required field: new"))),
        };
        pairs.push((old, new));
    }
    Ok(pairs)
}

/// How many times `needle` occurs in `text`, overlapping occurrences included: every char
/// boundary at which `text` continues with `needle` counts, so `"aa"` occurs twice in
/// `"aaa"`. `needle` must be non-empty.
pub(crate) fn occurrences(text: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = text[from..].find(needle) {
        count += 1;
        let start = from + at;
        // Step one character past this occurrence's start, so the next search can find one
        // that overlaps it.
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    count
}
