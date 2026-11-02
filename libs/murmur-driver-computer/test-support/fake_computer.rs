//! `fake-computer`: a native Murmur tool standing in for `tool-computer` in the drivers'
//! `mur_run_computer_tool` tests.
//!
//! Test support, compiled with plain `rustc --edition 2021` by those tests; it is never an
//! artifact of this repository. It depends on nothing, so it carries its own small JSON reader.
//!
//! Input (stdin): the native envelope `{"data": <call>, "log_path": …}`, where `<call>` is an
//! object or that object JSON-encoded as a string.
//!
//! It appends `<call>` as one JSON line to `computer-calls.jsonl` in its current directory, which
//! is the work folder. Then, for `{"action":"fail"}`, it prints
//! `{"status":"error","summary":"fail","data":"tool-computer: fail"}`; for anything else it prints
//! a passed result shaped like `tool-computer`'s: `<action>[ at X,Y]`, `cursor: X,Y` and
//! `screenshot: shot.png (1440x900)`, attaching `shot.png` from the work folder.

use std::io::{Read, Write};

#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_number(&self) -> Option<&str> {
        match self {
            Json::Number(n) => Some(n),
            _ => None,
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(n),
            Json::Str(s) => write_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip_ws(&mut self) {
        while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn eat(&mut self, literal: &str) -> Option<()> {
        if self.bytes[self.at..].starts_with(literal.as_bytes()) {
            self.at += literal.len();
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_ws();
        match *self.bytes.get(self.at)? {
            b'n' => self.eat("null").map(|()| Json::Null),
            b't' => self.eat("true").map(|()| Json::Bool(true)),
            b'f' => self.eat("false").map(|()| Json::Bool(false)),
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.eat("]").is_some() {
                    return Some(Json::Array(items));
                }
                loop {
                    items.push(self.value()?);
                    self.skip_ws();
                    if self.eat(",").is_some() {
                        continue;
                    }
                    self.eat("]")?;
                    return Some(Json::Array(items));
                }
            }
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                self.skip_ws();
                if self.eat("}").is_some() {
                    return Some(Json::Object(members));
                }
                loop {
                    self.skip_ws();
                    let key = self.string()?;
                    self.skip_ws();
                    self.eat(":")?;
                    members.push((key, self.value()?));
                    self.skip_ws();
                    if self.eat(",").is_some() {
                        continue;
                    }
                    self.eat("}")?;
                    return Some(Json::Object(members));
                }
            }
            _ => {
                let start = self.at;
                while self.at < self.bytes.len()
                    && matches!(self.bytes[self.at], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    self.at += 1;
                }
                (self.at > start).then(|| {
                    Json::Number(String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
                })
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        self.eat("\"")?;
        let mut out = String::new();
        loop {
            let rest = std::str::from_utf8(&self.bytes[self.at..]).ok()?;
            let c = rest.chars().next()?;
            self.at += c.len_utf8();
            match c {
                '"' => return Some(out),
                '\\' => {
                    let e = *self.bytes.get(self.at)?;
                    self.at += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
                            self.at += 4;
                            let code = u32::from_str_radix(hex, 16).ok()?;
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                c => out.push(c),
            }
        }
    }
}

fn parse(text: &str) -> Option<Json> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = reader.value()?;
    reader.skip_ws();
    (reader.at == reader.bytes.len()).then_some(value)
}

fn print_error(summary: &str) {
    let mut out = String::from("{\"status\":\"error\",\"summary\":");
    write_string(summary, &mut out);
    out.push_str(",\"data\":");
    write_string(&format!("tool-computer: {summary}"), &mut out);
    out.push('}');
    println!("{out}");
}

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        print_error("could not read stdin");
        return;
    }
    let Some(envelope) = parse(&input) else {
        print_error("stdin is not JSON");
        return;
    };
    let call = match envelope.get("data") {
        Some(Json::Str(s)) => parse(s),
        Some(other) => Some(other.clone()),
        None => None,
    };
    let Some(call) = call else {
        print_error("the input has no data");
        return;
    };

    let mut line = String::new();
    call.write(&mut line);
    line.push('\n');
    let logged = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("computer-calls.jsonl")
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(err) = logged {
        print_error(&format!("cannot write computer-calls.jsonl: {err}"));
        return;
    }

    let action = call.get("action").and_then(Json::as_str).unwrap_or("none");
    if action == "fail" {
        println!(r#"{{"status":"error","summary":"fail","data":"tool-computer: fail"}}"#);
        return;
    }
    let x = call.get("x").and_then(Json::as_number);
    let y = call.get("y").and_then(Json::as_number);
    let (action_line, cursor) = match (x, y) {
        (Some(x), Some(y)) => (format!("{action} at {x},{y}"), format!("{x},{y}")),
        _ => (action.to_string(), "720,450".to_string()),
    };
    let data = format!("{action_line}\ncursor: {cursor}\nscreenshot: shot.png (1440x900)");
    let mut out = String::from("{\"status\":\"passed\",\"summary\":");
    write_string(&action_line, &mut out);
    out.push_str(",\"data\":");
    write_string(&data, &mut out);
    out.push_str(",\"attachments\":[{\"media_type\":\"image/png\",\"path\":\"shot.png\"}]}");
    println!("{out}");
}
