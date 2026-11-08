//! The DevTools protocol over one WebSocket, for the length of one call: commands answered by
//! id, and the page events folded into what the tool keeps between calls.
//!
//! There is one thread. A command is sent, then messages are read until its answer arrives;
//! every event read on the way is folded into [`Events`]. A JavaScript dialog is dismissed the
//! moment it opens, because every other command on the page blocks until it is gone.
//!
//! `Runtime.enable` and `Log.enable` make Chrome replay the current document's stored console
//! messages, exceptions and log entries to every new DevTools session, and every call opens a
//! new one. Each such event is folded into a fingerprint (see [`fingerprint`]); one already
//! recorded in `state.json` is skipped, so an entry is reported once.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::{
    host::Clock,
    state::PageLog,
    ws::{Message, WebSocket},
};

/// How long one DevTools command may take before the call gives up on it.
pub const COMMAND_TIMEOUT_MS: u64 = 30_000;
/// The longest text a fingerprint keeps of an entry.
pub const FINGERPRINT_TEXT_CHARS: usize = 200;

/// What this call has seen on the page, plus the [`PageLog`] carried between calls.
#[derive(Debug, Default)]
pub struct Events {
    /// The flattened session of the page this tool drives.
    pub page_session: Option<String>,
    /// The page's main frame id, which is its target id.
    pub main_frame: Option<String>,
    pub load_events: u64,
    pub main_loading: bool,
    /// Dialogs dismissed during this call, already formatted.
    pub dialogs: Vec<String>,
    /// Why the connection ended, once it has.
    pub closed: Option<String>,
    /// The DevTools method the tool had last sent when it first found the connection closed.
    pub closed_during: Option<String>,
    pub log: PageLog,
    /// `requestId` → `(method, url)` for requests seen starting in this call.
    requests: HashMap<String, (String, String)>,
}

impl Events {
    pub fn new(log: PageLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }
}

pub struct Cdp<'a> {
    ws: WebSocket<'a>,
    clock: &'a dyn Clock,
    next_id: u64,
    /// The end every command and wait is cut to, on `clock`: the call's end, or a start's.
    deadline_ms: u64,
    responses: HashMap<u64, Result<Value, String>>,
    /// The method of the last command sent.
    last_method: Option<String>,
    pub ev: Events,
}

pub const OUT_OF_TIME: &str = "tool-browser: the call ran out of its 90 s";

impl<'a> Cdp<'a> {
    pub fn new(ws: WebSocket<'a>, clock: &'a dyn Clock, deadline_ms: u64, log: PageLog) -> Self {
        Self {
            ws,
            clock,
            next_id: 1,
            deadline_ms,
            responses: HashMap::new(),
            last_method: None,
            ev: Events::new(log),
        }
    }

    /// Move the end every command and wait is cut to, on `clock`.
    pub fn set_deadline(&mut self, deadline_ms: u64) {
        self.deadline_ms = deadline_ms;
    }

    pub fn clock(&self) -> &'a dyn Clock {
        self.clock
    }

    /// The call's end, on the clock.
    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    /// Record the method in flight the first time the connection is seen closed.
    fn note_closed(&mut self) {
        if self.ev.closed.is_some() && self.ev.closed_during.is_none() {
            self.ev.closed_during = self.last_method.clone();
        }
    }

    /// Whether the call's time is up: every command then answers [`OUT_OF_TIME`] unsent.
    pub fn out_of_time(&self) -> bool {
        self.clock.now_ms() >= self.deadline_ms
    }

    /// A browser-level command.
    pub fn browser(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.command(None, method, params, COMMAND_TIMEOUT_MS)
    }

    /// A command on the page session.
    pub fn page(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let session = self.ev.page_session.clone().ok_or("no page is attached")?;
        self.command(Some(&session), method, params, COMMAND_TIMEOUT_MS)
    }

    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<u64, String> {
        if let Some(reason) = &self.ev.closed {
            return Err(format!("the browser connection is closed ({reason})"));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        if let Err(e) = self.ws.send_text(&message.to_string()) {
            self.ev.closed = Some(e.to_string());
            self.note_closed();
            return Err(format!("the browser connection is closed ({e})"));
        }
        Ok(id)
    }

    pub fn command(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        if self.out_of_time() {
            return Err(OUT_OF_TIME.into());
        }
        if self.ev.closed.is_none() {
            self.last_method = Some(method.to_string());
        }
        let id = self.send(session, method, params)?;
        let until = self.clock.now_ms() + timeout_ms;
        loop {
            if let Some(result) = self.responses.remove(&id) {
                return result.map_err(|e| format!("{method}: {e}"));
            }
            if let Some(reason) = &self.ev.closed {
                return Err(format!(
                    "the browser connection closed during {method} ({reason})"
                ));
            }
            let now = self.clock.now_ms();
            if now >= self.deadline_ms {
                return Err(OUT_OF_TIME.into());
            }
            if now >= until {
                return Err(format!(
                    "{method} got no answer from the browser within {} s",
                    timeout_ms as f64 / 1000.0
                ));
            }
            self.read_one(until.min(self.deadline_ms) - now);
        }
    }

    /// Read events until `ready` holds or `until_ms` passes; true when it held.
    pub fn wait_until(&mut self, until_ms: u64, ready: impl Fn(&Events) -> bool) -> bool {
        loop {
            if ready(&self.ev) {
                return true;
            }
            if self.ev.closed.is_some() {
                return false;
            }
            let now = self.clock.now_ms();
            let end = until_ms.min(self.deadline_ms);
            if now >= end {
                return false;
            }
            self.read_one((end - now).min(100));
        }
    }

    /// Sleep `ms` while folding whatever events arrive.
    pub fn pump(&mut self, ms: u64) {
        let until = self.clock.now_ms() + ms;
        self.wait_until(until, |_| false);
    }

    /// Read and handle at most one message, waiting up to `wait_ms`.
    fn read_one(&mut self, wait_ms: u64) {
        self.read_message(wait_ms);
        self.note_closed();
    }

    fn read_message(&mut self, wait_ms: u64) {
        match self.ws.poll(wait_ms.clamp(1, 1_000) as u32) {
            Ok(None) | Ok(Some(Message::Binary(_))) => {}
            Ok(Some(Message::Text(text))) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    return;
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    self.responses.insert(id, response_of(&value));
                    return;
                }
                if let Some(dismiss) = handle_event(&mut self.ev, &value) {
                    // The answer is never waited for; a stray response is dropped.
                    let _ = self.send(
                        Some(&dismiss),
                        "Page.handleJavaScriptDialog",
                        json!({"accept": false}),
                    );
                }
            }
            Err(e) => self.ev.closed = Some(e.to_string()),
        }
    }

    /// Close the socket.
    pub fn close(&mut self) {
        self.ws.close();
    }
}

fn response_of(msg: &Value) -> Result<Value, String> {
    match msg.get("error") {
        Some(err) => Err(err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string()
            + &err
                .get("data")
                .and_then(Value::as_str)
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()),
        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// One console event's identity across DevTools sessions: its kind, Chrome's timestamp, the
/// exception id where there is one, and the first [`FINGERPRINT_TEXT_CHARS`] characters of the
/// entry as formatted.
pub fn fingerprint(
    kind: &str,
    timestamp: Option<&Value>,
    exception_id: Option<&Value>,
    text: &str,
) -> String {
    let num = |v: Option<&Value>| v.map(Value::to_string).unwrap_or_default();
    let head: String = text.chars().take(FINGERPRINT_TEXT_CHARS).collect();
    format!("{kind}|{}|{}|{head}", num(timestamp), num(exception_id))
}

/// `Failed to load resource: the server responded with a status of 404 (Not Found)` or
/// `Failed to load resource: net::ERR_CONNECTION_REFUSED`, from a network log entry, as the
/// method-less network line.
pub fn network_line_from_log(text: &str, url: &str) -> Option<String> {
    if let Some(i) = text.find("status of ") {
        let digits: String = text[i + 10..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return Some(format!("[{digits}] {url}"));
        }
    }
    let err = text.find("net::ERR_").map(|i| {
        text[i..]
            .split(|c: char| c.is_whitespace() || c == ')')
            .next()
            .unwrap_or("")
    })?;
    Some(format!("[failed] {url} — {err}"))
}

/// Fold one event. Returns the session to dismiss a dialog on, when one just opened.
pub fn handle_event(ev: &mut Events, msg: &Value) -> Option<String> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let session = msg.get("sessionId").and_then(Value::as_str);

    if session.is_none() {
        // Browser-level events: only our page going away matters.
        if method == "Target.detachedFromTarget"
            && ev.page_session.is_some()
            && params.get("sessionId").and_then(Value::as_str) == ev.page_session.as_deref()
        {
            ev.closed = Some("the page was closed".into());
        }
        return None;
    }
    if session != ev.page_session.as_deref() {
        return None;
    }
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match method {
        "Page.loadEventFired" => ev.load_events += 1,
        "Page.frameStartedLoading" | "Page.frameStoppedLoading"
            if params.get("frameId").and_then(Value::as_str) == ev.main_frame.as_deref() =>
        {
            ev.main_loading = method == "Page.frameStartedLoading";
        }
        "Page.frameNavigated" => {
            let frame = params.get("frame").cloned().unwrap_or(Value::Null);
            let main = frame.get("parentId").is_none()
                && frame.get("id").and_then(Value::as_str) == ev.main_frame.as_deref();
            let loader = frame
                .get("loaderId")
                .and_then(Value::as_str)
                .map(str::to_string);
            if main && loader.is_some() && loader != ev.log.loader_id {
                ev.log.new_document(loader);
            }
        }
        "Page.javascriptDialogOpening" => {
            ev.dialogs.push(format!(
                "dialog: {} \"{}\" (dismissed)",
                str_of(&params, "type"),
                crate::util::one_line(&str_of(&params, "message"), 300)
            ));
            return ev.page_session.clone();
        }
        "Runtime.consoleAPICalled" => {
            let kind = str_of(&params, "type");
            let level = match kind.as_str() {
                "error" | "assert" => "error",
                "warning" => "warning",
                other => other,
            };
            let text = params
                .get("args")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .map(remote_object_text)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let frame = params
                .pointer("/stackTrace/callFrames/0")
                .cloned()
                .unwrap_or(Value::Null);
            let entry = format!(
                "[{level}] {text}{}",
                location(&str_of(&frame, "url"), frame.get("lineNumber"))
            );
            let fp = fingerprint("console", params.get("timestamp"), None, &entry);
            if ev.log.take_fingerprint(fp) {
                if level == "error" {
                    ev.log.new_errors += 1;
                }
                ev.log.console.push(&entry);
            }
        }
        "Runtime.exceptionThrown" => {
            let d = params
                .get("exceptionDetails")
                .cloned()
                .unwrap_or(Value::Null);
            let description = d
                .pointer("/exception/description")
                .and_then(Value::as_str)
                .and_then(|s| s.lines().next())
                .unwrap_or("");
            let text = str_of(&d, "text");
            let message = match (text.as_str(), description) {
                (t, "") => t.to_string(),
                ("", desc) => desc.to_string(),
                (t, desc) if desc.starts_with(t) => desc.to_string(),
                (t, desc) => format!("{t} {desc}"),
            };
            let entry = format!(
                "[exception] {message}{}",
                location(&str_of(&d, "url"), d.get("lineNumber"))
            );
            let fp = fingerprint(
                "exception",
                params.get("timestamp"),
                d.get("exceptionId"),
                &entry,
            );
            if ev.log.take_fingerprint(fp) {
                ev.log.new_errors += 1;
                ev.log.console.push(&entry);
            }
        }
        "Log.entryAdded" => {
            let e = params.get("entry").cloned().unwrap_or(Value::Null);
            let level = match str_of(&e, "level").as_str() {
                "verbose" => "debug".to_string(),
                other => other.to_string(),
            };
            let url = str_of(&e, "url");
            let text = str_of(&e, "text");
            let entry = format!("[{level}] {text}{}", location(&url, e.get("lineNumber")));
            let fp = fingerprint("log", e.get("timestamp"), None, &entry);
            if !ev.log.take_fingerprint(fp) {
                return None;
            }
            if level == "error" {
                ev.log.new_errors += 1;
            }
            ev.log.console.push(&entry);
            let request = str_of(&e, "networkRequestId");
            if str_of(&e, "source") == "network"
                && !request.is_empty()
                && !ev.log.request_seen(&request)
            {
                if let Some(line) = network_line_from_log(&text, &url) {
                    ev.log.take_request(&request);
                    ev.log.network.push(&line);
                }
            }
        }
        "Network.requestWillBeSent" => {
            let url = params
                .pointer("/request/url")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !url.starts_with("data:") {
                let method = params
                    .pointer("/request/method")
                    .and_then(Value::as_str)
                    .unwrap_or("GET");
                if ev.requests.len() < 10_000 {
                    ev.requests.insert(
                        str_of(&params, "requestId"),
                        (method.to_string(), url.to_string()),
                    );
                }
            }
        }
        "Network.responseReceived" => {
            let id = str_of(&params, "requestId");
            let status = params
                .pointer("/response/status")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as u64;
            if ev.log.take_request(&id) && status >= 400 {
                let url = params
                    .pointer("/response/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let method = ev
                    .requests
                    .get(&id)
                    .map(|(m, _)| m.clone())
                    .unwrap_or_else(|| "GET".into());
                ev.log.network.push(&format!("[{status}] {method} {url}"));
            }
        }
        "Network.loadingFailed" => {
            let id = str_of(&params, "requestId");
            if let Some((method, url)) = ev.requests.remove(&id) {
                let canceled = params
                    .get("canceled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if ev.log.take_request(&id) && !canceled {
                    let mut reason = str_of(&params, "errorText");
                    let blocked = str_of(&params, "blockedReason");
                    if !blocked.is_empty() {
                        reason = format!("{reason} (blocked: {blocked})");
                    }
                    ev.log
                        .network
                        .push(&format!("[failed] {method} {url} — {reason}"));
                }
            }
        }
        "Network.loadingFinished" => {
            let id = str_of(&params, "requestId");
            ev.requests.remove(&id);
            ev.log.take_request(&id);
        }
        "Inspector.detached" => ev.closed = Some("the page was detached".into()),
        _ => {}
    }
    None
}

/// ` (<url>:<line>)` from a 0-based line number, or nothing without a URL.
fn location(url: &str, line: Option<&Value>) -> String {
    if url.is_empty() {
        return String::new();
    }
    match line.and_then(Value::as_u64) {
        Some(l) => format!(" ({url}:{})", l + 1),
        None => format!(" ({url})"),
    }
}

/// The text of a console argument, as DevTools describes it.
fn remote_object_text(arg: &Value) -> String {
    match arg.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => arg
            .get("unserializableValue")
            .or_else(|| arg.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                arg.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("undefined")
                    .to_string()
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        state::tests::FakeClock,
        ws::tests::{server_frame, upgraded, FakeConn},
    };

    #[test]
    fn the_method_in_flight_is_kept_when_the_connection_closes_on_its_answer() {
        let conn = FakeConn::default();
        let clock = FakeClock::default();
        let mut cdp = Cdp::new(
            upgraded(&conn, &server_frame(true, 0x8, &[])),
            &clock,
            90_000,
            PageLog::default(),
        );
        cdp.ev.page_session = Some("S".into());
        let e = cdp
            .page("Page.navigate", json!({"url": "https://example.com/"}))
            .unwrap_err();
        assert!(
            e.starts_with("the browser connection closed during Page.navigate ("),
            "{e}"
        );
        let e = cdp.page("Runtime.evaluate", json!({})).unwrap_err();
        assert!(e.starts_with("the browser connection is closed ("), "{e}");
        assert_eq!(cdp.ev.closed_during.as_deref(), Some("Page.navigate"));
    }

    #[test]
    fn a_close_during_the_load_wait_is_charged_to_the_navigation() {
        let conn = FakeConn::default();
        let clock = FakeClock::default();
        let mut early = server_frame(true, 0x1, br#"{"id":1,"result":{"loaderId":"L"}}"#);
        early.extend(server_frame(true, 0x8, &[]));
        let mut cdp = Cdp::new(upgraded(&conn, &early), &clock, 90_000, PageLog::default());
        cdp.ev.page_session = Some("S".into());
        assert!(cdp.page("Page.navigate", json!({"url": "x"})).is_ok());
        assert_eq!(cdp.ev.closed_during, None);
        assert!(!cdp.wait_until(30_000, |s| s.load_events > 0));
        assert!(cdp.page("DOM.getDocument", json!({})).is_err());
        assert_eq!(cdp.ev.closed_during.as_deref(), Some("Page.navigate"));
    }

    #[test]
    fn a_moved_deadline_cuts_commands_to_the_new_end() {
        let clock = crate::state::tests::FakeClock::default();
        let conn = crate::ws::tests::FakeConn::default();
        let mut cdp = Cdp::new(
            crate::ws::tests::upgraded(&conn, b""),
            &clock,
            100,
            PageLog::default(),
        );
        clock.sleep_ms(100);
        assert!(cdp.out_of_time());
        assert_eq!(
            cdp.browser("Browser.getVersion", json!({}))
                .err()
                .as_deref(),
            Some(OUT_OF_TIME)
        );
        cdp.set_deadline(200);
        assert!(!cdp.out_of_time());
        clock.sleep_ms(100);
        assert!(cdp.out_of_time());
    }

    fn page_events() -> Events {
        Events {
            page_session: Some("S".into()),
            main_frame: Some("F".into()),
            ..Events::default()
        }
    }

    fn console_error(text: &str, ts: f64) -> Value {
        json!({"sessionId":"S","method":"Runtime.consoleAPICalled","params":{
            "type":"error","timestamp":ts,"args":[{"type":"string","value":text}],
            "stackTrace":{"callFrames":[{"url":"http://h/","lineNumber":4}]}}})
    }

    #[test]
    fn console_and_exceptions_are_formatted_and_counted() {
        let mut s = page_events();
        handle_event(&mut s, &console_error("boom-load", 1.5));
        handle_event(
            &mut s,
            &json!({"sessionId":"S","method":"Runtime.exceptionThrown","params":{"timestamp":2.0,
            "exceptionDetails":{"exceptionId":1,"text":"Uncaught","url":"file:///w/p.html","lineNumber":10,
            "exception":{"description":"TypeError: x is not a function\n    at y"}}}}),
        );
        handle_event(
            &mut s,
            &json!({"sessionId":"OTHER","method":"Runtime.consoleAPICalled","params":{"type":"error","args":[]}}),
        );
        assert_eq!(s.log.new_errors, 2);
        assert_eq!(
            s.log.console.drain(),
            vec![
                "[error] boom-load (http://h/:5)",
                "[exception] Uncaught TypeError: x is not a function (file:///w/p.html:11)"
            ]
        );
    }

    #[test]
    fn replayed_events_are_skipped_and_new_ones_kept() {
        let mut first = page_events();
        handle_event(&mut first, &console_error("boom-load", 1.5));
        let log = first.log.clone();
        // A later call: a new session with the saved log; Chrome replays the stored message
        // and one that arrived between the calls.
        let mut later = Events::new(log);
        later.page_session = Some("S".into());
        later.main_frame = Some("F".into());
        later.log.console.drain();
        later.log.new_errors = 0;
        handle_event(&mut later, &console_error("boom-load", 1.5));
        handle_event(&mut later, &console_error("late", 9.0));
        assert_eq!(
            later.log.console.drain(),
            vec!["[error] late (http://h/:5)"]
        );
        assert_eq!(later.log.new_errors, 1);
        // The same text at another time is another message.
        handle_event(&mut later, &console_error("late", 9.5));
        assert_eq!(later.log.console.drain().len(), 1);
    }

    #[test]
    fn a_new_document_starts_a_new_fingerprint_set() {
        let mut s = page_events();
        handle_event(&mut s, &console_error("a", 1.0));
        let nav = |loader: &str| json!({"sessionId":"S","method":"Page.frameNavigated","params":{"frame":{"id":"F","loaderId":loader,"url":"x"}}});
        handle_event(&mut s, &nav("L1"));
        assert!(s.log.fingerprints.is_empty());
        handle_event(&mut s, &console_error("a", 1.0));
        handle_event(&mut s, &nav("L1"));
        assert_eq!(
            s.log.fingerprints.len(),
            1,
            "the same loader is the same document"
        );
        handle_event(
            &mut s,
            &json!({"sessionId":"S","method":"Page.frameNavigated","params":{"frame":{"id":"C","parentId":"F","loaderId":"L9"}}}),
        );
        assert_eq!(
            s.log.loader_id.as_deref(),
            Some("L1"),
            "a child frame is not the document"
        );
    }

    #[test]
    fn fingerprint_keeps_kind_time_id_and_200_characters() {
        let long = "x".repeat(500);
        let fp = fingerprint("exception", Some(&json!(12.25)), Some(&json!(3)), &long);
        assert_eq!(fp, format!("exception|12.25|3|{}", "x".repeat(200)));
        assert_eq!(fingerprint("log", None, None, "t"), "log|||t");
    }

    #[test]
    fn network_failures_and_error_statuses() {
        let mut s = page_events();
        for (id, url) in [("1", "http://h/missing.js"), ("2", "http://h/down")] {
            handle_event(
                &mut s,
                &json!({"sessionId":"S","method":"Network.requestWillBeSent","params":{
                "requestId":id,"request":{"url":url,"method":"GET"}}}),
            );
        }
        handle_event(
            &mut s,
            &json!({"sessionId":"S","method":"Network.responseReceived","params":{
            "requestId":"1","response":{"url":"http://h/missing.js","status":404}}}),
        );
        handle_event(
            &mut s,
            &json!({"sessionId":"S","method":"Network.loadingFailed","params":{
            "requestId":"2","errorText":"net::ERR_CONNECTION_REFUSED","canceled":false}}),
        );
        // Chrome's log entry for the same 404 goes to the console, not again to network.
        handle_event(
            &mut s,
            &json!({"sessionId":"S","method":"Log.entryAdded","params":{"entry":{"source":"network","level":"error",
            "text":"Failed to load resource: the server responded with a status of 404 (Not Found)",
            "url":"http://h/missing.js","timestamp":5.0,"networkRequestId":"1"}}}),
        );
        assert_eq!(
            s.log.network.drain(),
            vec![
                "[404] GET http://h/missing.js",
                "[failed] GET http://h/down — net::ERR_CONNECTION_REFUSED"
            ]
        );
        assert_eq!(
            s.log.console.drain(),
            vec!["[error] Failed to load resource: the server responded with a status of 404 (Not Found) (http://h/missing.js)"]
        );
    }

    #[test]
    fn a_network_failure_between_calls_has_no_method() {
        let mut s = page_events();
        for (id, text, url) in [
            ("7", "Failed to load resource: net::ERR_CONNECTION_REFUSED", "http://127.0.0.1:9/x"),
            ("8", "Failed to load resource: the server responded with a status of 500 (Internal Server Error)", "http://h/api"),
        ] {
            let event = json!({"sessionId":"S","method":"Log.entryAdded","params":{"entry":{"source":"network","level":"error",
                "text":text,"url":url,"timestamp":1.0,"networkRequestId":id}}});
            handle_event(&mut s, &event);
            // The replay in the call after does not report it again.
            handle_event(&mut s, &event);
        }
        assert_eq!(
            s.log.network.drain(),
            vec![
                "[failed] http://127.0.0.1:9/x — net::ERR_CONNECTION_REFUSED",
                "[500] http://h/api"
            ]
        );
        assert_eq!(s.log.console.drain().len(), 2);
        assert_eq!(network_line_from_log("something else", "u"), None);
    }

    #[test]
    fn dialogs_are_recorded_and_dismissed() {
        let mut s = page_events();
        let dismiss = handle_event(
            &mut s,
            &json!({"sessionId":"S","method":"Page.javascriptDialogOpening","params":{"type":"alert","message":"hi"}}),
        );
        assert_eq!(dismiss.as_deref(), Some("S"));
        assert_eq!(s.dialogs, vec!["dialog: alert \"hi\" (dismissed)"]);
    }
}
