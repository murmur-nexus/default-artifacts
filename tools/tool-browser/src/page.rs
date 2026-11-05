//! The actions, run against the one page the tool drives, over this call's DevTools
//! connection.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::{
    cdp::Cdp,
    request::{self, Action, OpenTarget, Request, WAIT_DEFAULT_MS},
    snapshot::{self, AxTree, Refs, DIFF_MAX_LINES},
    state::{DocumentState, SavedSnapshot, StateDir},
};

/// How long `open` and `back` wait for the page's load event.
const LOAD_WAIT_MS: u64 = 30_000;
/// How long a change summary waits for a navigation an action started.
const SETTLE_WAIT_MS: u64 = 10_000;
/// The viewport, which a plain screenshot captures.
pub const VIEWPORT: (u32, u32) = (1280, 800);

/// What one action produced.
#[derive(Debug)]
pub struct Outcome {
    pub data: String,
    /// The PNG a screenshot wrote, relative to the work folder.
    pub attachment: Option<String>,
}

impl Outcome {
    fn text(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            attachment: None,
        }
    }
}

/// The last full snapshot, which a change summary is compared with.
struct Snap {
    lines: Vec<String>,
    url: String,
    title: String,
    document: Option<i64>,
}

pub struct Page<'a> {
    pub cdp: Cdp<'a>,
    pub target_id: String,
    refs: Refs,
    last: Option<Snap>,
    /// The work folder as the tool opens it.
    work: PathBuf,
    /// The work folder's host path, as Chrome sees it.
    workdir_abs: PathBuf,
    dir: StateDir,
    /// Screenshots written so far.
    pub screenshots: u32,
    /// Dialogs dismissed before this call and not yet reported.
    pending_dialogs: Vec<String>,
}

impl<'a> Page<'a> {
    /// The page `cdp` is attached to, with what the earlier calls saved of its document.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cdp: Cdp<'a>,
        target_id: String,
        doc: &DocumentState,
        work: &Path,
        workdir_abs: PathBuf,
        dir: StateDir,
        screenshots: u32,
        pending_dialogs: Vec<String>,
    ) -> Self {
        Self {
            cdp,
            target_id,
            refs: Refs::restore(&doc.refs, doc.next_ref, doc.root),
            last: doc.last.as_ref().map(|l| Snap {
                lines: l.lines.clone(),
                url: l.url.clone(),
                title: l.title.clone(),
                document: l.document,
            }),
            work: work.to_path_buf(),
            workdir_abs,
            dir,
            screenshots,
            pending_dialogs,
        }
    }

    /// The document state to save, and the dialogs not yet reported.
    pub fn saved(&mut self) -> (DocumentState, Vec<String>) {
        let (refs, next_ref, root) = self.refs.save();
        let mut dialogs = std::mem::take(&mut self.pending_dialogs);
        dialogs.append(&mut self.cdp.ev.dialogs);
        (
            DocumentState {
                root,
                refs,
                next_ref,
                last: self.last.as_ref().map(|l| SavedSnapshot {
                    lines: l.lines.clone(),
                    url: l.url.clone(),
                    title: l.title.clone(),
                    document: l.document,
                }),
            },
            dialogs,
        )
    }

    pub fn run(&mut self, req: &Request) -> Result<Outcome, String> {
        match req.action {
            Action::Open => self.open(req.url.as_deref().unwrap_or_default()),
            Action::Snapshot => self.snapshot(req.r#ref),
            Action::Click => {
                self.click(req.r#ref.unwrap_or_default())?;
                self.change_summary()
            }
            Action::Type => {
                let backend = self.resolve(req.r#ref.unwrap_or_default())?;
                self.cdp
                    .page("DOM.focus", json!({"backendNodeId": backend}))
                    .map_err(|e| node_error(req.r#ref.unwrap_or_default(), e))?;
                self.cdp.page(
                    "Input.insertText",
                    json!({"text": req.text.clone().unwrap_or_default()}),
                )?;
                if req.submit {
                    self.press(&request::parse_key("Enter")?)?;
                }
                self.change_summary()
            }
            Action::Press => {
                self.press(req.key.as_ref().ok_or("press needs key")?)?;
                self.change_summary()
            }
            Action::Select => {
                self.select(
                    req.r#ref.unwrap_or_default(),
                    req.value.as_deref().unwrap_or_default(),
                )?;
                self.change_summary()
            }
            Action::Wait => {
                self.wait(req)?;
                self.change_summary()
            }
            Action::Back => {
                self.back()?;
                self.change_summary()
            }
            Action::Console => {
                let lines = self.cdp.ev.log.console.drain();
                Ok(Outcome::text(if lines.is_empty() {
                    "no new console messages".to_string()
                } else {
                    lines.join("\n")
                }))
            }
            Action::Network => {
                let lines = self.cdp.ev.log.network.drain();
                Ok(Outcome::text(if lines.is_empty() {
                    "no failed requests".to_string()
                } else {
                    lines.join("\n")
                }))
            }
            Action::Screenshot => self.screenshot(req.r#ref, req.full_page),
            Action::Close => Err("close is handled before the page is attached".into()),
        }
    }

    // ── reading the page ────────────────────────────────────────────────────

    fn title_and_url(&mut self) -> (String, String) {
        let evaluated = self
            .cdp
            .page(
                "Runtime.evaluate",
                json!({"expression": "[document.title, location.href]", "returnByValue": true}),
            )
            .ok()
            .and_then(|r| r.pointer("/result/value").cloned());
        if let Some(Value::Array(v)) = evaluated {
            let s = |i: usize| v.get(i).and_then(Value::as_str).unwrap_or("").to_string();
            return (s(0), s(1));
        }
        let target_id = self.target_id.clone();
        let info = self
            .cdp
            .browser("Target.getTargetInfo", json!({"targetId": target_id}))
            .unwrap_or(Value::Null);
        let s = |k: &str| {
            info.pointer(&format!("/targetInfo/{k}"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        (s("title"), s("url"))
    }

    /// The whole page, rendered, with refs given out; becomes the comparison base.
    fn full_snapshot(&mut self) -> Result<(String, Snap), String> {
        let tree = AxTree::parse(&self.cdp.page("Accessibility.getFullAXTree", json!({}))?);
        self.refs.enter_document(tree.document());
        let lines = tree.render(&mut self.refs, None).unwrap_or_default();
        let (title, url) = self.title_and_url();
        let text = snapshot::assemble(&snapshot::page_line(&title, &url), &lines);
        Ok((
            text,
            Snap {
                lines,
                url,
                title,
                document: tree.document(),
            },
        ))
    }

    fn snapshot(&mut self, r: Option<u32>) -> Result<Outcome, String> {
        let Some(r) = r else {
            let (text, snap) = self.full_snapshot()?;
            self.last = Some(snap);
            return Ok(Outcome::text(text));
        };
        let backend = self.resolve(r)?;
        let tree = AxTree::parse(&self.cdp.page("Accessibility.getFullAXTree", json!({}))?);
        if tree.document() != self.refs.document() {
            return Err(unknown_ref(r));
        }
        let lines = tree
            .render(&mut self.refs, Some(backend))
            .ok_or_else(|| unknown_ref(r))?;
        let (title, url) = self.title_and_url();
        Ok(Outcome::text(snapshot::assemble(
            &snapshot::page_line(&title, &url),
            &lines,
        )))
    }

    fn resolve(&self, r: u32) -> Result<i64, String> {
        self.refs.backend(r).ok_or_else(|| unknown_ref(r))
    }

    /// The centre of `eN`'s content box, scrolled into view, in viewport coordinates.
    fn box_of(&mut self, r: u32, backend: i64) -> Result<[f64; 8], String> {
        let _ = self.cdp.page(
            "DOM.scrollIntoViewIfNeeded",
            json!({"backendNodeId": backend}),
        );
        let model = self
            .cdp
            .page("DOM.getBoxModel", json!({"backendNodeId": backend}))
            .map_err(|e| {
                let e = node_error(r, e);
                if e.starts_with("unknown ref") {
                    e
                } else {
                    format!("e{r} has no box on the page (it is hidden or not rendered)")
                }
            })?;
        let quad = model
            .pointer("/model/border")
            .and_then(Value::as_array)
            .filter(|q| q.len() == 8)
            .ok_or_else(|| format!("e{r} has no box on the page"))?;
        let mut out = [0.0; 8];
        for (o, v) in out.iter_mut().zip(quad) {
            *o = v.as_f64().unwrap_or(0.0);
        }
        Ok(out)
    }

    // ── actions ─────────────────────────────────────────────────────────────

    fn open(&mut self, url: &str) -> Result<Outcome, String> {
        let target = request::open_target(url, &self.work, Some(&self.workdir_abs))?;
        let url = match target {
            OpenTarget::Url(u) => u,
            OpenTarget::WorkFile(rel) => request::file_url(&self.workdir_abs.join(rel)),
        };
        let before = self.cdp.ev.load_events;
        let result = self.cdp.page("Page.navigate", json!({"url": url}))?;
        if let Some(err) = result
            .get("errorText")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
        {
            let mut line = format!("open {url} failed: {err}");
            if let Some(host) = host_of(&url).filter(|h| !is_loopback(h)) {
                line.push_str(&format!(
                    "; if {host} is outside the capsule, add it to capabilities.network.allow"
                ));
            }
            return Err(line);
        }
        let mut still_loading = false;
        if result.get("loaderId").is_some() {
            let until = self.cdp.clock().now_ms() + LOAD_WAIT_MS;
            still_loading = !self.cdp.wait_until(until, |s| s.load_events > before);
        }
        self.cdp.pump(250);
        let (mut text, snap) = self.full_snapshot()?;
        self.last = Some(snap);
        self.cdp.ev.log.new_errors = 0;
        self.cdp.ev.dialogs.clear();
        self.pending_dialogs.clear();
        if still_loading {
            text.push_str("\npage still loading after 30 s");
        }
        Ok(Outcome::text(text))
    }

    fn click(&mut self, r: u32) -> Result<(), String> {
        let backend = self.resolve(r)?;
        let q = self.box_of(r, backend)?;
        let x = (q[0] + q[2] + q[4] + q[6]) / 4.0;
        let y = (q[1] + q[3] + q[5] + q[7]) / 4.0;
        self.cdp.page(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        )?;
        self.cdp.page(
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1}),
        )?;
        self.cdp.page(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1}),
        )?;
        Ok(())
    }

    fn press(&mut self, key: &request::KeySpec) -> Result<(), String> {
        let mut down = json!({
            "type": if key.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "modifiers": key.modifiers,
            "key": key.key,
            "code": key.code,
            "windowsVirtualKeyCode": key.key_code,
            "nativeVirtualKeyCode": key.key_code,
        });
        if let Some(text) = &key.text {
            down["text"] = json!(text);
            down["unmodifiedText"] = json!(text);
        }
        self.cdp.page("Input.dispatchKeyEvent", down)?;
        self.cdp.page(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "modifiers": key.modifiers,
                "key": key.key,
                "code": key.code,
                "windowsVirtualKeyCode": key.key_code,
                "nativeVirtualKeyCode": key.key_code,
            }),
        )?;
        Ok(())
    }

    fn select(&mut self, r: u32, value: &str) -> Result<(), String> {
        let backend = self.resolve(r)?;
        let object = self
            .cdp
            .page("DOM.resolveNode", json!({"backendNodeId": backend}))
            .map_err(|e| node_error(r, e))?;
        let object_id = object
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| unknown_ref(r))?;
        const PICK: &str = "function(v) {
            if (this.tagName !== 'SELECT') return 'not-select';
            const opts = Array.from(this.options);
            const o = opts.find(o => o.value === v)
                || opts.find(o => o.label.trim() === v.trim() || o.text.trim() === v.trim());
            if (!o) return 'none:' + opts.map(o => o.label.trim()).join(', ');
            this.value = o.value;
            o.selected = true;
            this.dispatchEvent(new Event('input', {bubbles: true}));
            this.dispatchEvent(new Event('change', {bubbles: true}));
            return 'ok';
        }";
        let result = self.cdp.page(
            "Runtime.callFunctionOn",
            json!({"objectId": object_id, "functionDeclaration": PICK,
                   "arguments": [{"value": value}], "returnByValue": true}),
        )?;
        match result
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "ok" => Ok(()),
            "not-select" => Err(format!("select: e{r} is not a select element")),
            other => Err(format!(
                "select: e{r} has no option with value or label \"{}\"; its options are {}",
                crate::util::one_line(value, 80),
                crate::util::one_line(other.strip_prefix("none:").unwrap_or(other), 400)
            )),
        }
    }

    fn wait(&mut self, req: &Request) -> Result<(), String> {
        let timeout = req.ms.unwrap_or(WAIT_DEFAULT_MS);
        let deadline = self.cdp.clock().now_ms() + timeout;
        if let Some(text) = &req.text {
            let expression = format!(
                "(document.body || document.documentElement).innerText.includes({})",
                serde_json::to_string(text).unwrap_or_default()
            );
            loop {
                let found = self
                    .cdp
                    .page(
                        "Runtime.evaluate",
                        json!({"expression": expression, "returnByValue": true}),
                    )
                    .ok()
                    .and_then(|r| r.pointer("/result/value").and_then(Value::as_bool))
                    .unwrap_or(false);
                if found {
                    return Ok(());
                }
                if self.cdp.clock().now_ms() >= deadline {
                    return Err(format!(
                        "wait: \"{}\" did not appear within {timeout} ms",
                        crate::util::one_line(text, 80)
                    ));
                }
                self.cdp.pump(100);
            }
        }
        if let Some(r) = req.r#ref {
            let backend = self.resolve(r)?;
            loop {
                if self
                    .cdp
                    .page("DOM.getBoxModel", json!({"backendNodeId": backend}))
                    .is_ok()
                {
                    return Ok(());
                }
                if self.cdp.clock().now_ms() >= deadline {
                    return Err(format!("wait: e{r} had no box within {timeout} ms"));
                }
                self.cdp.pump(100);
            }
        }
        self.cdp.pump(timeout);
        Ok(())
    }

    fn back(&mut self) -> Result<(), String> {
        let history = self.cdp.page("Page.getNavigationHistory", json!({}))?;
        let index = history["currentIndex"].as_u64().unwrap_or(0) as usize;
        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        if index == 0 || entries.len() < index {
            return Err("nothing to go back to".into());
        }
        let entry_id = entries[index - 1]["id"].clone();
        self.cdp
            .page("Page.navigateToHistoryEntry", json!({"entryId": entry_id}))?;
        let deadline = self.cdp.clock().now_ms() + LOAD_WAIT_MS;
        while self.cdp.clock().now_ms() < deadline {
            let moved = self
                .cdp
                .page("Page.getNavigationHistory", json!({}))
                .ok()
                .and_then(|h| h["currentIndex"].as_u64())
                .is_some_and(|i| (i as usize) < index);
            let ready = self
                .cdp
                .page(
                    "Runtime.evaluate",
                    json!({"expression": "document.readyState", "returnByValue": true}),
                )
                .ok()
                .and_then(|r| {
                    r.pointer("/result/value")
                        .and_then(Value::as_str)
                        .map(|s| s == "complete")
                })
                .unwrap_or(false);
            if moved && ready {
                break;
            }
            self.cdp.pump(100);
        }
        Ok(())
    }

    fn screenshot(&mut self, r: Option<u32>, full_page: bool) -> Result<Outcome, String> {
        let mut params = json!({"format": "png"});
        if let Some(r) = r {
            let backend = self.resolve(r)?;
            let q = self.box_of(r, backend)?;
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let (x0, x1) = (
                xs.iter().cloned().fold(f64::MAX, f64::min),
                xs.iter().cloned().fold(f64::MIN, f64::max),
            );
            let (y0, y1) = (
                ys.iter().cloned().fold(f64::MAX, f64::min),
                ys.iter().cloned().fold(f64::MIN, f64::max),
            );
            let metrics = self.cdp.page("Page.getLayoutMetrics", json!({}))?;
            let px = metrics
                .pointer("/cssLayoutViewport/pageX")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let py = metrics
                .pointer("/cssLayoutViewport/pageY")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            params["clip"] = json!({"x": x0 + px, "y": y0 + py,
                "width": (x1 - x0).max(1.0), "height": (y1 - y0).max(1.0), "scale": 1});
            params["captureBeyondViewport"] = json!(true);
        } else if full_page {
            let metrics = self.cdp.page("Page.getLayoutMetrics", json!({}))?;
            let w = metrics
                .pointer("/cssContentSize/width")
                .and_then(Value::as_f64)
                .unwrap_or(VIEWPORT.0 as f64);
            let h = metrics
                .pointer("/cssContentSize/height")
                .and_then(Value::as_f64)
                .unwrap_or(VIEWPORT.1 as f64);
            params["clip"] = json!({"x": 0, "y": 0, "width": w.ceil().max(1.0), "height": h.ceil().max(1.0), "scale": 1});
            params["captureBeyondViewport"] = json!(true);
        }
        let shot = self.cdp.page("Page.captureScreenshot", params)?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str().unwrap_or(""))
            .map_err(|e| format!("screenshot: the browser sent invalid base64: {e}"))?;
        let (w, h) =
            png_size(&png).ok_or("screenshot: the browser sent something that is not a PNG")?;
        let n = self.screenshots + 1;
        let name = format!("screenshot-{n}.png");
        std::fs::write(self.dir.file(&name), &png)
            .map_err(|e| format!("screenshot: cannot write {}/{name}: {e}", self.dir.rel))?;
        self.screenshots = n;
        let rel = format!("{}/{name}", self.dir.rel);
        Ok(Outcome {
            data: format!("saved {rel} ({w}x{h}, {} KB)", png.len().div_ceil(1024)),
            attachment: Some(rel),
        })
    }

    // ── change summaries ────────────────────────────────────────────────────

    /// Wait for a navigation the action started, then compare a new snapshot with the last.
    fn change_summary(&mut self) -> Result<Outcome, String> {
        self.cdp.pump(200);
        let until = self.cdp.clock().now_ms() + SETTLE_WAIT_MS;
        let settled = self.cdp.wait_until(until, |s| !s.main_loading);
        let (text, snap) = self.full_snapshot()?;
        let mut dialogs = std::mem::take(&mut self.pending_dialogs);
        dialogs.append(&mut self.cdp.ev.dialogs);
        let new_errors = std::mem::take(&mut self.cdp.ev.log.new_errors);

        let mut out: Vec<String> = Vec::new();
        match &self.last {
            Some(last) if last.document == snap.document => {
                if last.url != snap.url {
                    out.push(format!("url: {}", snap.url));
                }
                if last.title != snap.title {
                    out.push(format!(
                        "title: {}",
                        crate::util::one_line(&snap.title, 200)
                    ));
                }
                let (added, removed, lines) = snapshot::diff(&last.lines, &snap.lines);
                if added + removed > 0 {
                    out.push(format!("changed: +{added} -{removed} lines"));
                    let shown = lines.len().min(DIFF_MAX_LINES);
                    out.extend(lines.into_iter().take(shown));
                    if added + removed > shown {
                        out.push(format!(
                            "… {} more changed lines; take a snapshot to see them",
                            added + removed - shown
                        ));
                    }
                } else if out.is_empty() {
                    out.push("no visible change".into());
                }
            }
            _ => out.push(text),
        }
        if !settled {
            out.push("page still loading after 10 s".into());
        }
        out.extend(dialogs);
        if new_errors > 0 {
            out.push(format!("console: {new_errors} new error(s); call console"));
        }
        self.last = Some(snap);
        Ok(Outcome::text(out.join("\n")))
    }
}

/// A DevTools error about a node, as the line a model can act on.
fn node_error(r: u32, e: String) -> String {
    if e.contains("No node") || e.contains("Could not find node") || e.contains("not found") {
        unknown_ref(r)
    } else {
        e
    }
}

fn unknown_ref(r: u32) -> String {
    format!("unknown ref e{r}; take a snapshot first")
}

/// The host of an `http(s)://` URL, without port or user info.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next()?.to_string()
    } else {
        authority.split(':').next()?.to_string()
    };
    (!host.is_empty()).then_some(host)
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
}

/// Width and height from a PNG's IHDR chunk.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_loopback() {
        assert_eq!(
            host_of("https://user@example.com:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:3000/").as_deref(), Some("::1"));
        assert!(
            is_loopback("127.0.0.1") && is_loopback("localhost") && !is_loopback("example.com")
        );
    }

    #[test]
    fn png_size_reads_ihdr() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1280u32.to_be_bytes());
        png.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((1280, 800)));
        assert_eq!(png_size(b"nope"), None);
    }
}
