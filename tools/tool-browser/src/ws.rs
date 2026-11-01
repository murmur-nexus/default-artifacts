//! A minimal RFC 6455 WebSocket client over `std::net::TcpStream`, with `wss://` through rustls.
//!
//! One thread owns a [`WebSocket`] and calls [`WebSocket::poll`] in a loop: the socket has a short
//! read timeout, and frames are parsed out of a buffer, so a timeout in the middle of a frame
//! loses nothing and the same thread can write between reads. That is what lets a TLS stream,
//! which cannot be split across threads, carry the DevTools connection too.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use sha1::{Digest, Sha1};

/// The largest message accepted, after reassembly. A full-page screenshot is the biggest thing
/// DevTools sends.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// A parsed `ws://` or `wss://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl WsUrl {
    pub fn parse(url: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(r) = strip_prefix_ci(url, "ws://") {
            (false, r)
        } else if let Some(r) = strip_prefix_ci(url, "wss://") {
            (true, r)
        } else {
            return Err(format!("{url} is not a ws:// or wss:// URL"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            let end = stripped
                .find(']')
                .ok_or_else(|| format!("{url}: unclosed [ in host"))?;
            let host = &stripped[..end];
            let port = stripped[end + 1..].strip_prefix(':');
            (host.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (authority.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(format!("{url} has no host"));
        }
        let port = match port {
            Some(p) => p
                .parse()
                .map_err(|_| format!("{url} has an invalid port"))?,
            None if tls => 443,
            None => 80,
        };
        Ok(Self {
            tls,
            host,
            port,
            path: path.to_string(),
        })
    }

    /// `host:port`, bracketing an IPv6 host.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    (text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &text[prefix.len()..])
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

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

pub struct WebSocket {
    stream: Box<dyn Stream>,
    buf: Vec<u8>,
    /// The opcode and data of a fragmented message still arriving.
    partial: Option<(u8, Vec<u8>)>,
    urandom: std::fs::File,
    closed: bool,
}

/// How long the client waits for the TCP connection and the upgrade response.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

impl WebSocket {
    /// Connect and complete the upgrade. Later reads time out after `poll_interval`.
    pub fn connect(url: &WsUrl, poll_interval: Duration) -> Result<Self, String> {
        let addrs: Vec<_> = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {}: {e}", url.host))?
            .collect();
        let mut last_err = format!("{} resolved to no address", url.host);
        let mut tcp = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let tcp = tcp.ok_or(last_err)?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let mut stream: Box<dyn Stream> = if url.tls {
            Box::new(tls_stream(
                &url.host,
                tcp.try_clone().map_err(|e| e.to_string())?,
            )?)
        } else {
            Box::new(tcp.try_clone().map_err(|e| e.to_string())?)
        };
        let urandom = std::fs::File::open("/dev/urandom")
            .map_err(|e| format!("cannot open /dev/urandom: {e}"))?;
        let mut key = [0u8; 16];
        (&urandom)
            .read_exact(&mut key)
            .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
        let key = base64::engine::general_purpose::STANDARD.encode(key);
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            url.path,
            url.authority()
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|_| stream.flush())
            .map_err(|e| format!("sending the WebSocket upgrade: {e}"))?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream
                .read(&mut chunk)
                .map_err(|e| format!("reading the WebSocket upgrade response: {e}"))?;
            if n == 0 {
                return Err("the server closed the connection during the WebSocket upgrade".into());
            }
            buf.extend_from_slice(&chunk[..n]);
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
        tcp.set_read_timeout(Some(poll_interval))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stream,
            buf: buf[header_end + 4..].to_vec(),
            partial: None,
            urandom,
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
        let mut mask = [0u8; 4];
        self.urandom
            .read_exact(&mut mask)
            .map_err(|e| WsError::Io(format!("reading /dev/urandom: {e}")))?;
        let frame = encode_frame(opcode, payload, mask);
        self.stream
            .write_all(&frame)
            .and_then(|_| self.stream.flush())
            .map_err(|e| WsError::Io(e.to_string()))
    }

    /// Read what is available, up to the socket's poll interval, and return the next complete
    /// message if one has arrived. Pings are answered here.
    pub fn poll(&mut self) -> Result<Option<Message>, WsError> {
        loop {
            if let Some(msg) = self.take_frame()? {
                return Ok(Some(msg));
            }
            let mut chunk = [0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(WsError::Closed),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(WsError::Io(e.to_string())),
            }
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

fn tls_stream(
    host: &str,
    tcp: TcpStream,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS setup: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("{host} is not a valid TLS server name: {e}"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("TLS setup: {e}"))?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse() {
        let u = WsUrl::parse("ws://127.0.0.1:9222/devtools/browser/abc").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.path.as_str()),
            (false, "127.0.0.1", 9222, "/devtools/browser/abc")
        );
        let u = WsUrl::parse("wss://example.com").unwrap();
        assert_eq!((u.tls, u.port, u.path.as_str()), (true, 443, "/"));
        let u = WsUrl::parse("ws://[::1]:9/x").unwrap();
        assert_eq!(u.authority(), "[::1]:9");
        assert!(WsUrl::parse("http://x").is_err());
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
