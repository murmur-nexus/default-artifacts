//! The DevTools protocol over one WebSocket: commands answered by id, and the page events the
//! tool keeps between calls.
//!
//! One I/O thread owns the socket. Commands reach it through a channel; responses and events
//! land in [`State`] behind a mutex, and callers wait on a condition variable. The thread also
//! dismisses a JavaScript dialog the moment one opens, because every other command on the page
//! blocks until it is gone.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::{
    buffers::{LogBuffer, CONSOLE_CAP, NETWORK_CAP},
    ws::{Message, WebSocket, WsUrl},
};

/// How long one DevTools command may take before the call gives up on it.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the I/O thread records.
#[derive(Debug)]
pub struct State {
    responses: HashMap<u64, Result<Value, String>>,
    /// Why the connection ended, once it has.
    pub closed: Option<String>,
    /// The flattened session of the page this tool drives.
    pub page_session: Option<String>,
    /// The page's main frame id, which is its target id.
    pub main_frame: Option<String>,
    pub load_events: u64,
    pub main_loading: bool,
    pub console: LogBuffer,
    pub network: LogBuffer,
    /// Console errors, exceptions and error log entries seen since the connection opened.
    pub errors: u64,
    /// Dialogs dismissed since the last change summary, already formatted.
    pub dialogs: Vec<String>,
    /// `requestId` → `(method, url)` for requests still in flight.
    requests: HashMap<String, (String, String)>,
}

impl State {
    fn new() -> Self {
        Self {
            responses: HashMap::new(),
            closed: None,
            page_session: None,
            main_frame: None,
            load_events: 0,
            main_loading: false,
            console: LogBuffer::new(CONSOLE_CAP),
            network: LogBuffer::new(NETWORK_CAP),
            errors: 0,
            dialogs: Vec::new(),
            requests: HashMap::new(),
        }
    }
}

enum Outgoing {
    Text(String),
    Close,
}

struct Shared {
    state: Mutex<State>,
    cond: Condvar,
}

pub struct Cdp {
    tx: mpsc::Sender<Outgoing>,
    shared: Arc<Shared>,
    next_id: Arc<AtomicU64>,
    /// The timeout of [`Cdp::browser`] and [`Cdp::page`], in milliseconds.
    timeout_ms: AtomicU64,
}

impl Cdp {
    pub fn connect(url: &str) -> Result<Self, String> {
        let url = WsUrl::parse(url)?;
        let ws = WebSocket::connect(&url, Duration::from_millis(15))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            cond: Condvar::new(),
        });
        let next_id = Arc::new(AtomicU64::new(1));
        let (tx, rx) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            let next_id = Arc::clone(&next_id);
            thread::Builder::new()
                .name("cdp-io".into())
                .spawn(move || io_loop(ws, rx, &shared, &next_id))
                .map_err(|e| format!("cannot start the DevTools reader: {e}"))?;
        }
        Ok(Self {
            tx,
            shared,
            next_id,
            timeout_ms: AtomicU64::new(COMMAND_TIMEOUT.as_millis() as u64),
        })
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wait until `ready` holds or `deadline` passes; true when it held.
    pub fn wait_until(&self, deadline: Instant, ready: impl Fn(&State) -> bool) -> bool {
        let mut state = self.state();
        loop {
            if ready(&state) {
                return true;
            }
            if state.closed.is_some() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .cond
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Set the timeout of later [`Cdp::browser`] and [`Cdp::page`] commands.
    pub fn set_timeout(&self, timeout: Duration) {
        self.timeout_ms
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.load(Ordering::SeqCst))
    }

    /// A browser-level command.
    pub fn browser(&self, method: &str, params: Value) -> Result<Value, String> {
        self.command(None, method, params, self.timeout())
    }

    /// A command on the page session.
    pub fn page(&self, method: &str, params: Value) -> Result<Value, String> {
        let session = self
            .state()
            .page_session
            .clone()
            .ok_or("no page is attached")?;
        self.command(Some(&session), method, params, self.timeout())
    }

    pub fn command(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        if let Some(reason) = &self.state().closed {
            return Err(format!("the browser connection is closed ({reason})"));
        }
        self.tx
            .send(Outgoing::Text(message.to_string()))
            .map_err(|_| "the browser connection is closed".to_string())?;
        let deadline = Instant::now() + timeout;
        let mut state = self.state();
        loop {
            if let Some(result) = state.responses.remove(&id) {
                return result.map_err(|e| format!("{method}: {e}"));
            }
            if let Some(reason) = &state.closed {
                return Err(format!(
                    "the browser connection closed during {method} ({reason})"
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "{method} got no answer from the browser within {} s",
                    timeout.as_secs_f64()
                ));
            }
            state = self
                .shared
                .cond
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Close the socket; later commands fail at once.
    pub fn disconnect(&self) {
        let _ = self.tx.send(Outgoing::Close);
    }
}

fn io_loop(mut ws: WebSocket, rx: mpsc::Receiver<Outgoing>, shared: &Shared, next_id: &AtomicU64) {
    let reason = loop {
        let mut stop = None;
        loop {
            match rx.try_recv() {
                Ok(Outgoing::Text(text)) => {
                    if let Err(e) = ws.send_text(&text) {
                        stop = Some(e.to_string());
                        break;
                    }
                }
                Ok(Outgoing::Close) => {
                    ws.close();
                    stop = Some("disconnected".to_string());
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    ws.close();
                    stop = Some("disconnected".to_string());
                    break;
                }
            }
        }
        if let Some(reason) = stop {
            break reason;
        }
        match ws.poll() {
            Ok(None) => {}
            Ok(Some(Message::Text(text))) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(dismiss) = handle_message(&mut state, value) {
                    let id = next_id.fetch_add(1, Ordering::SeqCst);
                    let mut msg = json!({"id": id, "method": "Page.handleJavaScriptDialog", "params": {"accept": false}});
                    msg["sessionId"] = json!(dismiss);
                    drop(state);
                    if let Err(e) = ws.send_text(&msg.to_string()) {
                        break e.to_string();
                    }
                }
                shared.cond.notify_all();
            }
            Ok(Some(Message::Binary(_))) => {}
            Err(e) => break e.to_string(),
        }
    };
    let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
    state.closed = Some(reason);
    drop(state);
    shared.cond.notify_all();
}

/// Record one message. Returns the session to dismiss a dialog on, when one just opened.
fn handle_message(state: &mut State, msg: Value) -> Option<String> {
    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
        let result = match msg.get("error") {
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
        };
        state.responses.insert(id, result);
        return None;
    }
    let method = msg.get("method").and_then(Value::as_str)?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let session = msg.get("sessionId").and_then(Value::as_str);

    if session.is_none() {
        // Browser-level events: only our page going away matters.
        if method == "Target.detachedFromTarget"
            && params.get("sessionId").and_then(Value::as_str) == state.page_session.as_deref()
            && state.page_session.is_some()
        {
            state.closed = Some("the page was closed".into());
        }
        return None;
    }
    if session != state.page_session.as_deref() {
        return None;
    }
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match method {
        "Page.loadEventFired" => state.load_events += 1,
        "Page.frameStartedLoading" | "Page.frameStoppedLoading"
            if params.get("frameId").and_then(Value::as_str) == state.main_frame.as_deref() =>
        {
            state.main_loading = method == "Page.frameStartedLoading";
        }
        "Page.javascriptDialogOpening" => {
            state.dialogs.push(format!(
                "dialog: {} \"{}\" (dismissed)",
                str_of(&params, "type"),
                crate::util::one_line(&str_of(&params, "message"), 300)
            ));
            return state.page_session.clone();
        }
        "Runtime.consoleAPICalled" => {
            let kind = str_of(&params, "type");
            let level = match kind.as_str() {
                "error" | "assert" => "error",
                "warning" => "warning",
                other => other,
            };
            if level == "error" {
                state.errors += 1;
            }
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
            state.console.push(&entry);
        }
        "Runtime.exceptionThrown" => {
            state.errors += 1;
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
            state.console.push(&entry);
        }
        "Log.entryAdded" => {
            let e = params.get("entry").cloned().unwrap_or(Value::Null);
            let level = match str_of(&e, "level").as_str() {
                "verbose" => "debug".to_string(),
                other => other.to_string(),
            };
            if level == "error" {
                state.errors += 1;
            }
            let entry = format!(
                "[{level}] {}{}",
                str_of(&e, "text"),
                location(&str_of(&e, "url"), e.get("lineNumber"))
            );
            state.console.push(&entry);
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
                if state.requests.len() < 10_000 {
                    state.requests.insert(
                        str_of(&params, "requestId"),
                        (method.to_string(), url.to_string()),
                    );
                }
            }
        }
        "Network.responseReceived" => {
            let status = params
                .pointer("/response/status")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as u64;
            if status >= 400 {
                let url = params
                    .pointer("/response/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let method = state
                    .requests
                    .get(&str_of(&params, "requestId"))
                    .map(|(m, _)| m.clone())
                    .unwrap_or_else(|| "GET".into());
                let entry = format!("[{status}] {method} {url}");
                state.network.push(&entry);
            }
        }
        "Network.loadingFailed" => {
            if let Some((method, url)) = state.requests.remove(&str_of(&params, "requestId")) {
                let canceled = params
                    .get("canceled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !canceled {
                    let mut reason = str_of(&params, "errorText");
                    let blocked = str_of(&params, "blockedReason");
                    if !blocked.is_empty() {
                        reason = format!("{reason} (blocked: {blocked})");
                    }
                    let entry = format!("[failed] {method} {url} — {reason}");
                    state.network.push(&entry);
                }
            }
        }
        "Network.loadingFinished" => {
            state.requests.remove(&str_of(&params, "requestId"));
        }
        "Inspector.detached" => state.closed = Some("the page was detached".into()),
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

    fn page_state() -> State {
        let mut s = State::new();
        s.page_session = Some("S".into());
        s.main_frame = Some("F".into());
        s
    }

    #[test]
    fn console_and_exceptions_are_formatted_and_counted() {
        let mut s = page_state();
        handle_message(
            &mut s,
            json!({"sessionId":"S","method":"Runtime.consoleAPICalled","params":{
            "type":"error","args":[{"type":"string","value":"boom-load"}],
            "stackTrace":{"callFrames":[{"url":"http://h/","lineNumber":4}]}}}),
        );
        handle_message(
            &mut s,
            json!({"sessionId":"S","method":"Runtime.exceptionThrown","params":{
            "exceptionDetails":{"text":"Uncaught","url":"http://h/a.js","lineNumber":0,
            "exception":{"description":"Error: bad\n    at x"}}}}),
        );
        handle_message(
            &mut s,
            json!({"sessionId":"OTHER","method":"Runtime.consoleAPICalled","params":{"type":"error","args":[]}}),
        );
        assert_eq!(s.errors, 2);
        assert_eq!(
            s.console.drain(),
            vec![
                "[error] boom-load (http://h/:5)",
                "[exception] Uncaught Error: bad (http://h/a.js:1)"
            ]
        );
    }

    #[test]
    fn network_failures_and_error_statuses() {
        let mut s = page_state();
        for (id, url) in [("1", "http://h/missing.js"), ("2", "http://h/down")] {
            handle_message(
                &mut s,
                json!({"sessionId":"S","method":"Network.requestWillBeSent","params":{
                "requestId":id,"request":{"url":url,"method":"GET"}}}),
            );
        }
        handle_message(
            &mut s,
            json!({"sessionId":"S","method":"Network.responseReceived","params":{
            "requestId":"1","response":{"url":"http://h/missing.js","status":404}}}),
        );
        handle_message(
            &mut s,
            json!({"sessionId":"S","method":"Network.loadingFailed","params":{
            "requestId":"2","errorText":"net::ERR_CONNECTION_REFUSED","canceled":false}}),
        );
        assert_eq!(
            s.network.drain(),
            vec![
                "[404] GET http://h/missing.js",
                "[failed] GET http://h/down — net::ERR_CONNECTION_REFUSED"
            ]
        );
    }

    #[test]
    fn dialogs_are_recorded_and_dismissed() {
        let mut s = page_state();
        let dismiss = handle_message(
            &mut s,
            json!({"sessionId":"S","method":"Page.javascriptDialogOpening","params":{"type":"alert","message":"hi"}}),
        );
        assert_eq!(dismiss.as_deref(), Some("S"));
        assert_eq!(s.dialogs, vec!["dialog: alert \"hi\" (dismissed)"]);
    }
}
