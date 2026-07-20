//! Pure text-format parsers for the four supported test runners, plus format
//! auto-detection. Each parser consumes the runner's default (no-special-flags)
//! console output and returns a `Vec<Failure>` plus the runner-reported passed
//! count. `total` and `failed` are computed by the caller (`ops`).
//!
//! There is deliberately no trait abstraction here — the caller dispatches on a
//! `format` string via a 4-arm `match`, mirroring the sibling native tools.

use serde_json::{json, Value};

/// One structured failure. `file`/`line` are `None` when the runner output did
/// not carry a resolvable location (serialized as JSON `null`). `stable_id` is
/// filled later, and only for `cargo_test` with a resolvable code-graph db.
pub struct Failure {
    pub test_name: String,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub exception: String,
    pub message: String,
    pub stable_id: Option<String>,
}

impl Failure {
    pub fn to_json(&self) -> Value {
        json!({
            "test_name": self.test_name,
            "file": self.file,
            "line": self.line,
            "exception": self.exception,
            "message": self.message,
            "stable_id": self.stable_id,
        })
    }
}

/// Detect the runner format from unambiguous signature markers in the raw text.
/// Returns `None` when no format's markers are present, so the caller can fail
/// with an actionable message rather than silently misparsing.
pub fn detect_format(raw: &str) -> Option<String> {
    // cargo prints a "test result:" line for every test binary — highly specific.
    if raw.contains("test result:") && raw.contains("running ") {
        return Some("cargo_test".to_string());
    }
    // pytest banners / short summary section.
    if raw.contains("short test summary info")
        || raw.contains("=== test session starts ===")
        || has_banner(raw, "FAILURES")
    {
        return Some("pytest".to_string());
    }
    // go's per-test FAIL marker, or a panic crash dump with a goroutine stack.
    if raw.contains("--- FAIL:") || (raw.contains("panic:") && raw.contains("goroutine ")) {
        return Some("go_test".to_string());
    }
    // jest's suite banner plus its trailing "Tests:" tally.
    if raw.contains("Tests:") && (raw.contains("FAIL ") || raw.contains("\u{25cf} ")) {
        return Some("jest".to_string());
    }
    None
}

/// True when some line, once its `=` padding and surrounding whitespace are
/// stripped, is exactly `word` (a pytest section banner like `=== FAILURES ===`).
fn has_banner(raw: &str, word: &str) -> bool {
    raw.lines().any(|l| {
        let t = l.trim();
        t.contains('=') && t.trim_matches('=').trim() == word
    })
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Parse a `file:line[:col]` fragment into `(file, line)`, tolerating an
/// optional trailing column. Unix paths only (no drive-letter colon handling).
fn parse_file_line(loc: &str) -> Option<(String, i64)> {
    let (head, last) = loc.rsplit_once(':')?;
    if last.parse::<i64>().is_ok() {
        // `file:line:col` — the middle segment is the line.
        if let Some((h2, l2)) = head.rsplit_once(':') {
            if let Ok(line) = l2.parse::<i64>() {
                return Some((h2.to_string(), line));
            }
        }
        // `file:line` — the trailing segment is the line.
        if let Ok(line) = last.parse::<i64>() {
            return Some((head.to_string(), line));
        }
    }
    None
}

/// Extract the integer that immediately precedes `kw` (e.g. `3` from
/// `"3 passed"`). Returns 0 when the keyword is not found.
fn extract_count(s: &str, kw: &str) -> i64 {
    let words: Vec<&str> = s
        .split([' ', ';', ',', '.', '\t'])
        .filter(|w| !w.is_empty())
        .collect();
    for pair in words.windows(2) {
        if pair[1] == kw {
            if let Ok(n) = pair[0].parse::<i64>() {
                return n;
            }
        }
    }
    0
}

// ── cargo_test ────────────────────────────────────────────────────────────────

/// `cargo test` default console output.
///
/// Failures come from `---- <test_name> stdout ----` panic blocks. `file`/`line`
/// come from the `thread '...' panicked at <file>:<line>:<col>:` line;
/// `exception` is `"panic"`; `message` is every line after the panic line up to
/// the next block boundary (the `RUST_BACKTRACE` note excluded), preserving
/// multi-line assertion bodies. `passed` is summed across every `test result:`
/// line.
pub fn parse_cargo(raw: &str) -> (Vec<Failure>, i64) {
    let lines: Vec<&str> = raw.lines().collect();
    let mut passed = 0i64;
    for line in &lines {
        if let Some(rest) = line.trim_start().strip_prefix("test result:") {
            passed += extract_count(rest, "passed");
        }
    }

    let mut failures = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let name = trimmed
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"));
        let Some(name) = name else {
            i += 1;
            continue;
        };

        let mut file = None;
        let mut line_no = None;
        let mut msg_lines: Vec<String> = Vec::new();
        let mut seen_panic = false;

        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            let lt = l.trim();
            if (lt.starts_with("---- ") && lt.ends_with(" stdout ----"))
                || lt == "failures:"
                || lt.starts_with("test result:")
            {
                break;
            }
            if !seen_panic {
                if let Some(p) = l.find("panicked at ") {
                    let loc = l[p + "panicked at ".len()..].trim().trim_end_matches(':');
                    if let Some((f, n)) = parse_file_line(loc) {
                        file = Some(f);
                        line_no = Some(n);
                    }
                    seen_panic = true;
                    j += 1;
                    continue;
                }
            } else {
                let ls = l.trim_start();
                if ls.starts_with("note: run with") && l.contains("RUST_BACKTRACE") {
                    j += 1;
                    continue;
                }
                msg_lines.push(l.to_string());
            }
            j += 1;
        }
        trim_blank_edges(&mut msg_lines);

        failures.push(Failure {
            test_name: name.to_string(),
            file,
            line: line_no,
            exception: "panic".to_string(),
            message: msg_lines.join("\n"),
            stable_id: None,
        });
        i = j;
    }
    (failures, passed)
}

fn trim_blank_edges(v: &mut Vec<String>) {
    while v.first().is_some_and(|s| s.trim().is_empty()) {
        v.remove(0);
    }
    while v.last().is_some_and(|s| s.trim().is_empty()) {
        v.pop();
    }
}

// ── pytest ────────────────────────────────────────────────────────────────────

/// `pytest` default console output.
///
/// Failures come from the `FAILURES` section's per-test blocks. Each block is
/// delimited by an underscore-padded title line (`____ test_foo ____`) and ends
/// in a `<file>:<line>: <ExceptionType>` footer. `exception` is that
/// `ExceptionType`; `message` is built from the block's `E `-prefixed lines;
/// `test_name` is `<file>::<test_function>`.
pub fn parse_pytest(raw: &str) -> (Vec<Failure>, i64) {
    let lines: Vec<&str> = raw.lines().collect();

    // Passed count from the final summary banner (`=== 2 failed, 3 passed in.. ===`).
    let mut passed = 0i64;
    for l in &lines {
        if l.contains(" passed") && (l.contains(" in ") || l.contains("=")) {
            passed = extract_count(l, "passed");
        }
    }

    // Locate the FAILURES section.
    let Some(start) = lines
        .iter()
        .position(|l| l.trim().contains('=') && l.trim().trim_matches('=').trim() == "FAILURES")
    else {
        return (Vec::new(), passed);
    };

    let mut failures = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for l in &lines[start + 1..] {
        let t = l.trim();
        // Any other `=`-banner ends the FAILURES section.
        if !t.is_empty() && t.starts_with('=') && t.ends_with('=') {
            break;
        }
        if is_underscore_title(t) {
            if let Some((name, body)) = cur.take() {
                failures.push(pytest_block(&name, &body));
            }
            cur = Some((t.trim_matches('_').trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = cur.as_mut() {
            body.push(l);
        }
    }
    if let Some((name, body)) = cur.take() {
        failures.push(pytest_block(&name, &body));
    }
    (failures, passed)
}

fn is_underscore_title(t: &str) -> bool {
    t.starts_with('_') && t.ends_with('_') && !t.trim_matches('_').trim().is_empty()
}

fn pytest_block(name: &str, body: &[&str]) -> Failure {
    // message: the `E `-prefixed lines, with the marker and its padding removed.
    let mut msg_lines = Vec::new();
    for l in body {
        if l.starts_with('E') && (l.len() == 1 || l[1..].starts_with([' ', '\t'])) {
            msg_lines.push(l[1..].trim_start().to_string());
        }
    }

    // footer: the last `<file>:<line>: <exception>` line in the block.
    let mut file = None;
    let mut line_no = None;
    let mut exception = "Error".to_string();
    for l in body {
        if let Some((f, n, exc)) = pytest_footer(l.trim()) {
            file = Some(f);
            line_no = Some(n);
            exception = exc;
        }
    }

    let test_name = match &file {
        Some(f) => format!("{f}::{name}"),
        None => name.to_string(),
    };
    Failure {
        test_name,
        file,
        line: line_no,
        exception,
        message: msg_lines.join("\n"),
        stable_id: None,
    }
}

fn pytest_footer(l: &str) -> Option<(String, i64, String)> {
    let (left, right) = l.split_once(": ")?;
    let (file, num) = left.rsplit_once(':')?;
    let line = num.parse::<i64>().ok()?;
    if file.is_empty() || right.trim().is_empty() {
        return None;
    }
    Some((file.to_string(), line, right.trim().to_string()))
}

// ── go_test ───────────────────────────────────────────────────────────────────

/// `go test` default console output.
///
/// Two failure sources: `--- FAIL: <TestName> (<dur>)` blocks (indented
/// `file.go:N: message` line → `file`/`line`/`message`, `exception:
/// "test_failure"`), and a top-level `panic: <message>` crash dump (first
/// `\t<file>:<line>` goroutine frame → `file`/`line`, `exception: "panic"`,
/// `test_name` from the preceding stack function). `passed` counts `--- PASS:`.
pub fn parse_go(raw: &str) -> (Vec<Failure>, i64) {
    let lines: Vec<&str> = raw.lines().collect();
    let passed = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("--- PASS:"))
        .count() as i64;

    let mut failures = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let t = l.trim_start();

        if let Some(rest) = t.strip_prefix("--- FAIL: ") {
            let name = rest.split(" (").next().unwrap_or(rest).trim().to_string();
            let mut file = None;
            let mut line_no = None;
            let mut message = String::new();
            let mut j = i + 1;
            while j < lines.len() {
                let bt = lines[j].trim_start();
                if bt.starts_with("--- FAIL:")
                    || bt.starts_with("--- PASS:")
                    || bt.starts_with("=== RUN")
                    || bt.starts_with("=== PAUSE")
                    || bt.starts_with("=== CONT")
                    || lines[j].starts_with("panic:")
                    || bt == "PASS"
                    || bt == "FAIL"
                    || bt.starts_with("ok ")
                    || bt.starts_with("FAIL\t")
                {
                    break;
                }
                if file.is_none() {
                    if let Some((f, n, m)) = parse_go_loc(bt) {
                        file = Some(f);
                        line_no = Some(n);
                        message = m;
                    }
                }
                j += 1;
            }
            // A `--- FAIL:` block with no addressable detail line and no message
            // carries nothing actionable — it is typically a test aborted by the
            // `panic:` crash dump below (which surfaces it with a location) or a
            // bare `t.Fail()`. Skip it so panicking tests are not double-counted.
            if file.is_some() || !message.is_empty() {
                failures.push(Failure {
                    test_name: name,
                    file,
                    line: line_no,
                    exception: "test_failure".to_string(),
                    message,
                    stable_id: None,
                });
            }
            i = j;
            continue;
        }

        // A crash panic is printed at column 0 (indented `panic:` lines inside a
        // recovered-panic dump are ignored via the `starts_with` on the raw line).
        if let Some(rest) = l.strip_prefix("panic: ") {
            let message = rest.trim().to_string();
            let mut file = None;
            let mut line_no = None;
            let mut test_name = String::new();
            let mut k = i + 1;
            while k < lines.len() {
                if let Some((f, n)) = parse_go_frame(lines[k]) {
                    file = Some(f);
                    line_no = Some(n);
                    if k > 0 {
                        test_name = go_func_name(lines[k - 1]);
                    }
                    break;
                }
                k += 1;
            }
            failures.push(Failure {
                test_name,
                file,
                line: line_no,
                exception: "panic".to_string(),
                message,
                stable_id: None,
            });
            i = (k + 1).max(i + 1);
            continue;
        }

        i += 1;
    }
    (failures, passed)
}

fn parse_go_loc(bt: &str) -> Option<(String, i64, String)> {
    let (left, right) = bt.split_once(": ")?;
    if !left.contains(".go:") {
        return None;
    }
    let (file, num) = left.rsplit_once(':')?;
    let line = num.parse::<i64>().ok()?;
    Some((file.to_string(), line, right.trim().to_string()))
}

fn parse_go_frame(sl: &str) -> Option<(String, i64)> {
    if !sl.starts_with(char::is_whitespace) {
        return None;
    }
    let tok = sl.split_whitespace().next()?;
    if !tok.contains(".go:") {
        return None;
    }
    let (f, n) = tok.rsplit_once(':')?;
    let line = n.parse::<i64>().ok()?;
    Some((f.to_string(), line))
}

fn go_func_name(line: &str) -> String {
    let s = line.trim();
    let before_paren = s.split('(').next().unwrap_or(s);
    before_paren
        .rsplit('.')
        .next()
        .unwrap_or(before_paren)
        .trim()
        .to_string()
}

// ── jest ──────────────────────────────────────────────────────────────────────

/// `jest` default console output.
///
/// Failures come from `FAIL <file>` blocks containing `\u{25cf} <suite> › <test>`
/// entries. `file`/`line` come from an `at ... (<file>:<line>:<col>)` stack
/// frame (preferring one inside the current `FAIL` file); `exception` is
/// `"AssertionError"` when the block contains an `expect(...)` matcher, else an
/// explicit `<ErrorClass>: ` prefix if present, else `"Error"`; `test_name` is
/// the `<suite> › <test>` string. `passed` comes from the `Tests:` tally.
pub fn parse_jest(raw: &str) -> (Vec<Failure>, i64) {
    let lines: Vec<&str> = raw.lines().collect();
    let mut passed = 0i64;
    for l in &lines {
        if l.trim_start().starts_with("Tests:") {
            passed = extract_count(l, "passed");
        }
    }

    let mut failures = Vec::new();
    let mut cur_file: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();

        if let Some(rest) = t.strip_prefix("FAIL ") {
            cur_file = rest.split_whitespace().next().map(|s| s.to_string());
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("PASS ") {
            cur_file = rest.split_whitespace().next().map(|s| s.to_string());
            i += 1;
            continue;
        }

        if let Some(title) = t.strip_prefix("\u{25cf} ") {
            let test_name = title.trim().to_string();
            let mut j = i + 1;
            let mut body: Vec<&str> = Vec::new();
            while j < lines.len() {
                let bt = lines[j].trim_start();
                if bt.starts_with("\u{25cf} ")
                    || bt.starts_with("FAIL ")
                    || bt.starts_with("PASS ")
                    || bt.starts_with("Tests:")
                    || bt.starts_with("Test Suites:")
                {
                    break;
                }
                body.push(lines[j]);
                j += 1;
            }

            let joined = body.join("\n");
            let exception = if joined.contains("expect(") {
                "AssertionError".to_string()
            } else if let Some(e) = jest_error_class(&body) {
                e
            } else {
                "Error".to_string()
            };
            let message = jest_message(&body);
            let (file, line) = jest_location(&body, cur_file.as_deref());

            failures.push(Failure {
                test_name,
                file,
                line,
                exception,
                message,
                stable_id: None,
            });
            i = j;
            continue;
        }
        i += 1;
    }
    (failures, passed)
}

fn jest_error_class(body: &[&str]) -> Option<String> {
    for l in body {
        let t = l.trim_start();
        if let Some((head, _)) = t.split_once(": ") {
            if head.ends_with("Error") && !head.contains(char::is_whitespace) {
                return Some(head.to_string());
            }
        }
    }
    None
}

fn jest_message(body: &[&str]) -> String {
    let mut out = Vec::new();
    for l in body {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if is_code_frame(t) || t.starts_with("at ") {
            break;
        }
        out.push(t.to_string());
    }
    out.join("\n")
}

fn is_code_frame(t: &str) -> bool {
    let s = t.trim_start_matches('>').trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty() && s.contains('|')
}

fn jest_location(body: &[&str], cur_file: Option<&str>) -> (Option<String>, Option<i64>) {
    let mut first: Option<(String, i64)> = None;
    for l in body {
        let t = l.trim();
        if !t.starts_with("at ") {
            continue;
        }
        let parsed = if let Some(open) = t.rfind('(') {
            t[open + 1..]
                .split_once(')')
                .and_then(|(inner, _)| parse_file_line(inner))
        } else {
            t.strip_prefix("at ")
                .and_then(|r| r.split_whitespace().next())
                .and_then(parse_file_line)
        };
        if let Some((f, n)) = parsed {
            if first.is_none() {
                first = Some((f.clone(), n));
            }
            if let Some(cf) = cur_file {
                if f == cf || f.contains(cf) || cf.contains(&f) {
                    return (Some(f), Some(n));
                }
            }
        }
    }
    match first {
        Some((f, n)) => (Some(f), Some(n)),
        None => (None, None),
    }
}
