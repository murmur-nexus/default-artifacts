//! A minimal RFC 6455 WebSocket client over a [`Conn`], the per-call connection
//! `murmur:process` opens to Chrome's DevTools port.
//!
//! Frames are parsed out of a buffer, so a read that stops in the middle of a frame loses
//! nothing; every read waits at most the timeout its caller passes.

use base64::Engine as _;
use sha1::{Digest, Sha1};

use crate::host::{Conn, ProcError};

/// The largest message accepted, after reassembly. A full-page screenshot is the biggest thing
/// DevTools sends.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// The most bytes one read asks the host for.
const READ_CHUNK: u32 = 1024 * 1024;

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// One complete message.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

/// Why the connection ended.
#[derive(Debug)]
pub enum WsError {
    Closed,
    Io(String),
    Protocol(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "the WebSocket closed"),
            Self::Io(e) => write!(f, "WebSocket I/O error: {e}"),
            Self::Protocol(e) => write!(f, "WebSocket protocol error: {e}"),
        }
    }
}

/// Random bytes for the handshake key and the frame masks.
pub type Random<'a> = &'a dyn Fn(usize) -> Vec<u8>;

pub struct WebSocket<'r> {
    conn: Box<dyn Conn>,
    buf: Vec<u8>,
    /// The opcode and data of a fragmented message still arriving.
    partial: Option<(u8, Vec<u8>)>,
    random: Random<'r>,
    closed: bool,
}

impl<'r> WebSocket<'r> {
    /// Complete the upgrade on `conn` for `path` at `127.0.0.1:<port>`, waiting at most
    /// `timeout_ms` for each read of the response.
    pub fn handshake(
        mut conn: Box<dyn Conn>,
        port: u16,
        path: &str,
        random: Random<'r>,
        timeout_ms: u32,
    ) -> Result<Self, String> {
        let key = base64::engine::general_purpose::STANDARD.encode(random(16));
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        conn.write(request.as_bytes())
            .map_err(|e| format!("sending the WebSocket upgrade: {e}"))?;
        let mut buf = Vec::new();
        let header_end = loop {
            let chunk = match conn.read(64 * 1024, timeout_ms) {
                Ok(c) => c,
                Err(ProcError::TimedOut) => {
                    return Err("no WebSocket upgrade response in time".into())
                }
                Err(e) => return Err(format!("reading the WebSocket upgrade response: {e}")),
            };
            if chunk.is_empty() {
                return Err("the server closed the connection during the WebSocket upgrade".into());
            }
            buf.extend_from_slice(&chunk);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if buf.len() > 64 * 1024 {
                return Err("the WebSocket upgrade response headers exceed 64 KiB".into());
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(format!(
                "the server refused the WebSocket upgrade: {}",
                crate::util::one_line(status, 120)
            ));
        }
        let accept = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.trim().eq_ignore_ascii_case("sec-websocket-accept"))
            .map(|(_, v)| v.trim().to_string())
            .ok_or("the WebSocket upgrade response has no Sec-WebSocket-Accept")?;
        if accept != accept_key(&key) {
            return Err("the WebSocket upgrade response has a wrong Sec-WebSocket-Accept".into());
        }
        Ok(Self {
            conn,
            buf: buf[header_end + 4..].to_vec(),
            partial: None,
            random,
            closed: false,
        })
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        self.send_frame(0x1, text.as_bytes())
    }

    /// Send a close frame. Errors are ignored: the connection is going away either way.
    pub fn close(&mut self) {
        if !self.closed {
            let _ = self.send_frame(0x8, &1000u16.to_be_bytes());
            self.closed = true;
        }
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        let bytes = (self.random)(4);
        let mut mask = [0u8; 4];
        mask.copy_from_slice(&bytes[..4]);
        let frame = encode_frame(opcode, payload, mask);
        self.conn
            .write(&frame)
            .map_err(|e| WsError::Io(e.to_string()))
    }

    /// The next complete message, reading for at most `timeout_ms` when none is buffered;
    /// `None` when nothing complete arrived in time. Pings are answered here.
    pub fn poll(&mut self, timeout_ms: u32) -> Result<Option<Message>, WsError> {
        let mut waited = false;
        loop {
            if let Some(msg) = self.take_frame()? {
                return Ok(Some(msg));
            }
            // After the first read only take what is already there, so one poll never waits
            // much past its timeout.
            let wait = if waited { 1 } else { timeout_ms.max(1) };
            match self.conn.read(READ_CHUNK, wait) {
                Ok(chunk) if chunk.is_empty() => return Err(WsError::Closed),
                Ok(chunk) => self.buf.extend_from_slice(&chunk),
                Err(ProcError::TimedOut) if waited || self.buf.is_empty() => return Ok(None),
                Err(ProcError::TimedOut) => {}
                Err(e) => return Err(WsError::Io(e.to_string())),
            }
            waited = true;
        }
    }

    /// Parse one frame out of the buffer, if a whole one is there, and return the message it
    /// completes, if any.
    fn take_frame(&mut self) -> Result<Option<Message>, WsError> {
        loop {
            let Some(frame) = parse_frame(&self.buf)? else {
                return Ok(None);
            };
            let payload = self.buf[frame.payload_start..frame.payload_start + frame.len].to_vec();
            self.buf.drain(..frame.payload_start + frame.len);
            match frame.opcode {
                0x8 => {
                    self.close();
                    return Err(WsError::Closed);
                }
                0x9 => self.send_frame(0xA, &payload)?,
                0xA => {}
                0x0 => {
                    let (opcode, mut data) = self.partial.take().ok_or_else(|| {
                        WsError::Protocol("continuation frame with no message".into())
                    })?;
                    data.extend_from_slice(&payload);
                    if data.len() > MAX_MESSAGE_BYTES {
                        return Err(WsError::Protocol("message exceeds 64 MiB".into()));
                    }
                    if frame.fin {
                        return finish(opcode, data).map(Some);
                    }
                    self.partial = Some((opcode, data));
                }
                0x1 | 0x2 => {
                    if self.partial.is_some() {
                        return Err(WsError::Protocol(
                            "new message inside a fragmented one".into(),
                        ));
                    }
                    if frame.fin {
                        return finish(frame.opcode, payload).map(Some);
                    }
                    self.partial = Some((frame.opcode, payload));
                }
                other => return Err(WsError::Protocol(format!("unknown opcode {other:#x}"))),
            }
        }
    }
}

fn finish(opcode: u8, data: Vec<u8>) -> Result<Message, WsError> {
    if opcode == 0x1 {
        String::from_utf8(data)
            .map(Message::Text)
            .map_err(|_| WsError::Protocol("text message is not UTF-8".into()))
    } else {
        Ok(Message::Binary(data))
    }
}

struct FrameHead {
    fin: bool,
    opcode: u8,
    payload_start: usize,
    len: usize,
}

/// The head of the first frame in `buf`, once the whole frame is there.
fn parse_frame(buf: &[u8]) -> Result<Option<FrameHead>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0F;
    if buf[1] & 0x80 != 0 {
        return Err(WsError::Protocol("the server sent a masked frame".into()));
    }
    let (len, start) = match buf[1] & 0x7F {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as u64, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(b), 10)
        }
        n => (n as u64, 2),
    };
    if len > MAX_MESSAGE_BYTES as u64 {
        return Err(WsError::Protocol("frame exceeds 64 MiB".into()));
    }
    let len = len as usize;
    if opcode >= 0x8 && (len > 125 || !fin) {
        return Err(WsError::Protocol("malformed control frame".into()));
    }
    if buf.len() < start + len {
        return Ok(None);
    }
    Ok(Some(FrameHead {
        fin,
        opcode,
        payload_start: start,
        len,
    }))
}

/// One masked client frame with FIN set.
fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => frame.push(0x80 | n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    frame
}

fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(ACCEPT_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    /// A scripted peer: what the client wrote, and the chunks it reads in order (`None` is a
    /// timeout).
    #[derive(Clone, Default)]
    pub(crate) struct FakeConn {
        pub(crate) written: Arc<Mutex<Vec<u8>>>,
        reads: Arc<Mutex<VecDeque<Option<Vec<u8>>>>>,
    }

    impl Conn for FakeConn {
        fn read(&mut self, max: u32, _timeout_ms: u32) -> Result<Vec<u8>, ProcError> {
            let mut reads = self.reads.lock().unwrap();
            match reads.pop_front() {
                None | Some(None) => Err(ProcError::TimedOut),
                Some(Some(mut chunk)) => {
                    if chunk.len() > max as usize {
                        let rest = chunk.split_off(max as usize);
                        reads.push_front(Some(rest));
                    }
                    Ok(chunk)
                }
            }
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), ProcError> {
            self.written.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn zeros(n: usize) -> Vec<u8> {
        vec![0; n]
    }

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            n if n < 126 => f.push(n as u8),
            n if n <= u16::MAX as usize => {
                f.push(126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        f.extend_from_slice(payload);
        f
    }

    /// A client that has completed its upgrade on `conn`, with `extra` already received.
    pub(crate) fn upgraded(conn: &FakeConn, extra: &[u8]) -> WebSocket<'static> {
        // The key from sixteen zero bytes is AAAAAAAAAAAAAAAAAAAAAA==.
        let accept = accept_key("AAAAAAAAAAAAAAAAAAAAAA==");
        let mut response =
            format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
                .into_bytes();
        response.extend_from_slice(extra);
        conn.reads.lock().unwrap().push_back(Some(response));
        WebSocket::handshake(
            Box::new(conn.clone()),
            9222,
            "/devtools/browser/x",
            &zeros,
            1000,
        )
        .unwrap()
    }

    #[test]
    fn handshake_checks_the_accept_key_and_keeps_early_bytes() {
        let conn = FakeConn::default();
        let mut ws = upgraded(&conn, &server_frame(true, 1, b"early"));
        let written = String::from_utf8(conn.written.lock().unwrap().clone()).unwrap();
        assert!(written.starts_with("GET /devtools/browser/x HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n"));
        assert!(written.contains("Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n"));
        assert_eq!(ws.poll(10).unwrap(), Some(Message::Text("early".into())));
        assert_eq!(ws.poll(10).unwrap(), None);

        let bad = FakeConn::default();
        bad.reads.lock().unwrap().push_back(Some(
            b"HTTP/1.1 101 OK\r\nSec-WebSocket-Accept: nope\r\n\r\n".to_vec(),
        ));
        let e = WebSocket::handshake(Box::new(bad), 1, "/", &zeros, 10)
            .err()
            .unwrap();
        assert!(e.contains("wrong Sec-WebSocket-Accept"), "{e}");
        let refused = FakeConn::default();
        refused
            .reads
            .lock()
            .unwrap()
            .push_back(Some(b"HTTP/1.1 404 Not Found\r\n\r\n".to_vec()));
        let e = WebSocket::handshake(Box::new(refused), 1, "/", &zeros, 10)
            .err()
            .unwrap();
        assert!(
            e.contains("refused the WebSocket upgrade: HTTP/1.1 404"),
            "{e}"
        );
    }

    #[test]
    fn messages_split_across_reads_and_fragments_reassemble() {
        let conn = FakeConn::default();
        let mut ws = upgraded(&conn, &[]);
        let big = vec![b'x'; 70_000];
        let frame = server_frame(true, 1, &big);
        {
            let mut reads = conn.reads.lock().unwrap();
            reads.push_back(Some(frame[..10].to_vec()));
            reads.push_back(None);
            reads.push_back(Some(frame[10..].to_vec()));
            reads.push_back(Some(server_frame(false, 1, b"ab")));
            reads.push_back(Some(server_frame(true, 9, b"p")));
            reads.push_back(Some(server_frame(true, 0, b"cd")));
            reads.push_back(Some(server_frame(true, 8, &1000u16.to_be_bytes())));
        }
        // The timeout in the middle of a frame keeps the partial bytes.
        assert_eq!(ws.poll(10).unwrap(), None);
        assert_eq!(
            ws.poll(10).unwrap(),
            Some(Message::Text(String::from_utf8(big).unwrap()))
        );
        assert_eq!(ws.poll(10).unwrap(), Some(Message::Text("abcd".into())));
        assert!(matches!(ws.poll(10), Err(WsError::Closed)));
        // The client answered the ping and the close, masked.
        let written = conn.written.lock().unwrap().clone();
        let tail = &written[written.len() - 15..];
        assert_eq!(tail[0], 0x8A, "pong");
        assert_eq!(tail[1], 0x81);
        assert_eq!(tail[7], 0x88, "close");
    }

    #[test]
    fn client_frames_are_masked_text() {
        let conn = FakeConn::default();
        let mut ws = upgraded(&conn, &[]);
        let before = conn.written.lock().unwrap().len();
        ws.send_text("hi").unwrap();
        let written = conn.written.lock().unwrap()[before..].to_vec();
        assert_eq!(written, vec![0x81, 0x82, 0, 0, 0, 0, b'h', b'i']);
    }

    #[test]
    fn a_peer_close_and_a_masked_server_frame_end_the_connection() {
        let conn = FakeConn::default();
        let mut ws = upgraded(&conn, &[]);
        conn.reads.lock().unwrap().push_back(Some(Vec::new()));
        assert!(matches!(ws.poll(10), Err(WsError::Closed)));
        let conn = FakeConn::default();
        let mut ws = upgraded(&conn, &[0x81, 0x81, 1, 2, 3, 4, 5]);
        assert!(matches!(ws.poll(10), Err(WsError::Protocol(_))));
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn frames_round_trip_through_the_parser() {
        for len in [0usize, 5, 125, 126, 70_000] {
            let payload = vec![7u8; len];
            let mask = [1, 2, 3, 4];
            let frame = encode_frame(0x1, &payload, mask);
            // Unmask it the way a server would and re-encode as a server frame.
            let masked_start = frame.len() - len;
            let unmasked: Vec<u8> = frame[masked_start..]
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ mask[i % 4])
                .collect();
            assert_eq!(unmasked, payload);
            let mut server = vec![frame[0], frame[1] & 0x7F];
            server.extend_from_slice(&frame[2..masked_start - 4]);
            server.extend_from_slice(&payload);
            let head = parse_frame(&server).unwrap().unwrap();
            assert_eq!((head.fin, head.opcode, head.len), (true, 1, len));
            assert!(parse_frame(&server[..server.len() - 1]).unwrap().is_none() || len == 0);
        }
    }
}
