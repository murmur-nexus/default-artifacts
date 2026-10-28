//! `tool-read`: a file's content, within the read limits.

use serde_json::{json, Value};

use crate::{
    err, err_result, format_result_size, io_error_kind, ok_with, reject_absolute, required_path,
};

// read limits. Every read, with or without a range, returns at most this many lines, this
// many bytes of `content`, and no line longer than `MAX_LINE_CHARS`; a read cut by one of
// them still succeeds, with a `truncated` note built by `read_cut_note`.
pub(crate) const MAX_READ_LINES: usize = 2000;
// About 25,000 tokens at about 4 bytes per token, and above 2000 lines of ordinary source
// (30-45 bytes per line), so on normal code the line limit binds first.
pub(crate) const MAX_READ_BYTES: usize = 100 * 1024;
// Characters (Unicode scalar values) a returned line keeps. A cut line is at most 8,000
// bytes, so the first line of a read always fits under `MAX_READ_BYTES`.
pub(crate) const MAX_LINE_CHARS: usize = 2000;

/// Read the file `path` names: the whole file when it fits every limit, else the lines from
/// `start_line` up to `end_line` or the first limit reached, with a `truncated` note naming it.
pub(crate) fn read(op: &Value) -> Value {
    let path = match required_path(op, "path") {
        Ok(p) => p,
        Err(failure) => return failure,
    };
    if let Some(refusal) = reject_absolute("path", &path) {
        return refusal;
    }

    // Optional 1-based, inclusive line bounds. Reading through `as_i64` (not `as_u64`) is
    // deliberate: a negative or zero `start_line` must surface as an INVALID_RANGE error,
    // not be silently dropped as "absent". A field present as anything non-integer (or
    // JSON null) is treated as absent.
    let start_arg = op.get("start_line").and_then(Value::as_i64);
    let end_arg = op.get("end_line").and_then(Value::as_i64);

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return err_result(io_error_kind(&e), format!("{path}: {e}")),
    };
    let total_lines = content.lines().count();

    // A read with no range of a file within every limit: the content verbatim, plus
    // `total_lines` so an agent can size a follow-up ranged read.
    if start_arg.is_none()
        && end_arg.is_none()
        && total_lines <= MAX_READ_LINES
        && content.len() <= MAX_READ_BYTES
        && content
            .lines()
            .all(|line| line.chars().nth(MAX_LINE_CHARS).is_none())
    {
        let byte_count = content.len();
        return ok_with(
            format!("read {path}"),
            json!({
                "content": content,
                "total_lines": total_lines,
            }),
            format!("{byte_count} bytes"),
        );
    }

    let start = start_arg.unwrap_or(1);
    if start < 1 {
        return err_result(
            err::INVALID_RANGE,
            format!("start_line must be >= 1, got {start}"),
        );
    }
    // start beyond EOF errors (states the real line count) rather than silently falling
    // back to the whole file. An empty file (0 lines) rejects any start_line here.
    if start as usize > total_lines {
        return err_result(
            err::INVALID_RANGE,
            format!("start_line {start} is beyond end of file ({total_lines} lines)"),
        );
    }
    // end defaults to EOF; an explicit end past EOF *clamps* (read-to-EOF is what an agent
    // means) — the one case that clamps instead of erroring.
    let requested_end = end_arg.map_or(total_lines as i64, |e| e.min(total_lines as i64));
    if start > requested_end {
        return err_result(
            err::INVALID_RANGE,
            format!("inverted range: start_line {start} is greater than end_line {requested_end}"),
        );
    }

    let start = start as usize;
    let requested_end = requested_end as usize;
    // Only an explicit span is refused; an omitted end_line reads up to the first limit.
    if end_arg.is_some() {
        let span = requested_end - start + 1;
        if span > MAX_READ_LINES {
            return err_result(
                err::RANGE_TOO_LARGE,
                format!(
                    "requested range of {span} lines exceeds the maximum of {MAX_READ_LINES}; narrow start_line/end_line"
                ),
            );
        }
    }
    let line_end = requested_end.min(start + MAX_READ_LINES - 1);

    // Lines `start..=line_end` in order, each cut to MAX_LINE_CHARS, until the next one
    // would take `content` past MAX_READ_BYTES. `lines()` strips line terminators, so the
    // "\n" join reproduces the original text of an uncut span.
    let mut sliced = String::new();
    let mut end = start - 1;
    let mut cut_lines = 0;
    let mut first_cut_line = None;
    for (number, line) in (start..=line_end).zip(content.lines().skip(start - 1)) {
        let (line, cut) = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((at, _)) => (&line[..at], true),
            None => (line, false),
        };
        let separator = usize::from(number > start);
        if sliced.len() + separator + line.len() > MAX_READ_BYTES {
            break;
        }
        if separator == 1 {
            sliced.push('\n');
        }
        sliced.push_str(line);
        end = number;
        if cut {
            cut_lines += 1;
            first_cut_line.get_or_insert(number);
        }
    }

    let stopped_at = if end < line_end {
        Some(ReadLimit::Bytes)
    } else if line_end < requested_end {
        Some(ReadLimit::Lines)
    } else {
        None
    };
    let byte_count = sliced.len();
    let mut data = json!({
        "content": sliced,
        "total_lines": total_lines,
        "start_line": start,
        "end_line": end,
    });
    let mut summary = format!("{byte_count} bytes (lines {start}-{end} of {total_lines}");
    let lines_cut = first_cut_line.map(|first| (cut_lines, first));
    if let Some(note) = read_cut_note(start, end, total_lines, stopped_at, lines_cut) {
        data["truncated"] = Value::String(note);
        summary.push_str(", truncated");
    }
    summary.push(')');

    ok_with(format!("read {path} lines {start}-{end}"), data, summary)
}

/// The read limit that stopped a read before the lines it asked for ran out.
#[derive(Clone, Copy)]
enum ReadLimit {
    Lines,
    Bytes,
}

/// The `truncated` note of a cut read: the limit that stopped it and the `start_line` that
/// continues it, then how many returned lines were cut to [`MAX_LINE_CHARS`] and the first
/// of them. `lines_cut` is `(count, first line number)`. `None` when the read was not cut.
fn read_cut_note(
    start: usize,
    end: usize,
    total_lines: usize,
    stopped_at: Option<ReadLimit>,
    lines_cut: Option<(usize, usize)>,
) -> Option<String> {
    let range = stopped_at.map(|reason| {
        let limit = match reason {
            ReadLimit::Lines => format!("the limit of {MAX_READ_LINES} lines"),
            ReadLimit::Bytes => {
                format!("the limit of {}", format_result_size(MAX_READ_BYTES))
            }
        };
        format!(
            "Read stopped at {limit}. Lines {start}-{end} of {total_lines} are returned; start_line {} continues the read.",
            end + 1
        )
    });
    let cut = lines_cut.map(|(count, first)| {
        format!(
            "Lines longer than {MAX_LINE_CHARS} characters are cut to their first {MAX_LINE_CHARS} characters: {count} in this read, the first at line {first}."
        )
    });
    match (range, cut) {
        (Some(range), Some(cut)) => Some(format!("{range} {cut}")),
        (range, cut) => range.or(cut),
    }
}
