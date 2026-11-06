//! The X client: the X11 protocol spoken over one `murmur:process` connection to the session's
//! own Xvfb.
//!
//! Requests are serialized with `x11rb-protocol`'s request structs and their replies parsed with
//! its `TryParse` impls; [`Connection`] tracks sequence numbers and [`PacketReader`] frames what
//! arrives. Every wait for the server is bounded by the call's deadline and by [`REPLY_WAIT_MS`].

use std::borrow::Cow;

use x11rb_protocol::connect::Connect;
use x11rb_protocol::connection::{Connection, ReplyFdKind};
use x11rb_protocol::errors::ConnectError;
use x11rb_protocol::packet_reader::PacketReader;
use x11rb_protocol::protocol::xproto::{
    ChangeKeyboardMappingRequest, GetImageReply, GetImageRequest, GetInputFocusRequest,
    GetKeyboardMappingReply, GetKeyboardMappingRequest, GetWindowAttributesReply,
    GetWindowAttributesRequest, ImageFormat, ImageOrder, MapState, QueryExtensionReply,
    QueryExtensionRequest, QueryPointerReply, QueryPointerRequest, QueryTreeReply,
    QueryTreeRequest, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb_protocol::protocol::xtest::FakeInputRequest;
use x11rb_protocol::x11_utils::TryParse;
use x11rb_protocol::SequenceNumber;

use crate::host::{Clock, Conn, ProcError};
use crate::xbackend::{zpixmap_to_rgb, Frame, KeyboardMapping, PixelLayout, XBackend, XResult};

/// The longest one wait for the server may take, within the call's remaining time.
pub const REPLY_WAIT_MS: u64 = 10_000;
/// The most bytes one `read` of the connection asks for.
const READ_CHUNK: u32 = 1024 * 1024;

const AUTH_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// The core protocol's error names, by code.
fn error_name(code: u8) -> &'static str {
    match code {
        1 => "BadRequest",
        2 => "BadValue",
        3 => "BadWindow",
        4 => "BadPixmap",
        5 => "BadAtom",
        6 => "BadCursor",
        7 => "BadFont",
        8 => "BadMatch",
        9 => "BadDrawable",
        10 => "BadAccess",
        11 => "BadAlloc",
        12 => "BadColormap",
        13 => "BadGContext",
        14 => "BadIDChoice",
        15 => "BadName",
        16 => "BadLength",
        17 => "BadImplementation",
        _ => "an extension's error",
    }
}

/// The line for an X error packet answering `request`.
pub fn x_error_line(packet: &[u8], request: &str) -> String {
    let code = packet.get(1).copied().unwrap_or(0);
    format!(
        "tool-computer: X error {code} ({}) on {request}",
        error_name(code)
    )
}

fn joined<const N: usize>(parts: [Cow<'_, [u8]>; N]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for part in parts {
        out.extend_from_slice(&part);
    }
    out
}

fn parse<T: TryParse>(packet: &[u8], request: &str) -> XResult<T> {
    T::try_parse(packet)
        .map(|(reply, _)| reply)
        .map_err(|e| format!("tool-computer: cannot read the display's {request} reply: {e:?}"))
}

/// One authenticated connection to the session's display, for one call.
pub struct XClient<'a> {
    conn: Box<dyn Conn>,
    clock: &'a dyn Clock,
    /// The call's deadline on [`Clock::now_ms`].
    deadline_ms: u64,
    proto: Connection,
    reader: PacketReader,
    root: Window,
    width: u16,
    height: u16,
    layout: PixelLayout,
    min_keycode: u8,
    max_keycode: u8,
    xtest_opcode: u8,
    /// Requests without a reply written since the last round trip, and their names.
    unchecked: Vec<(SequenceNumber, &'static str)>,
}

impl<'a> XClient<'a> {
    /// Completes the X setup over `conn` with `cookie` as MIT-MAGIC-COOKIE-1, then looks up the
    /// XTEST extension.
    pub fn setup(
        mut conn: Box<dyn Conn>,
        cookie: &[u8],
        clock: &'a dyn Clock,
        deadline_ms: u64,
    ) -> XResult<Self> {
        let (mut connect, request) =
            Connect::with_authorization(AUTH_NAME.to_vec(), cookie.to_vec());
        conn.write(&request)
            .map_err(|e| format!("tool-computer: cannot write to the display: {e}"))?;
        let until = wait_until(clock, deadline_ms);
        loop {
            let want = connect.buffer().len() as u32;
            let bytes = read_bounded(conn.as_mut(), clock, until, want, "the connection setup")?;
            let n = bytes.len();
            connect.buffer()[..n].copy_from_slice(&bytes);
            if connect.advance(n) {
                break;
            }
        }
        let setup = connect.into_setup().map_err(|e| {
            let reason = match &e {
                ConnectError::SetupFailed(failed) => {
                    String::from_utf8_lossy(&failed.reason).trim().to_string()
                }
                ConnectError::SetupAuthenticate(auth) => {
                    String::from_utf8_lossy(&auth.reason).trim().to_string()
                }
                other => other.to_string(),
            };
            format!("tool-computer: the display refused the connection: {reason}")
        })?;

        let screen = setup
            .roots
            .first()
            .ok_or("tool-computer: the display has no screen")?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual_id == screen.root_visual)
            .ok_or("tool-computer: the display's root visual is not listed")?;
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == screen.root_depth)
            .ok_or("tool-computer: the display lists no pixmap format for its depth")?;
        let layout = PixelLayout {
            bits_per_pixel: format.bits_per_pixel,
            scanline_pad: format.scanline_pad,
            lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
        };
        let size = (
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        );
        if size != (crate::DISPLAY_WIDTH, crate::DISPLAY_HEIGHT) {
            return Err(format!(
                "tool-computer: the display is {}x{}, not {}x{}",
                size.0,
                size.1,
                crate::DISPLAY_WIDTH,
                crate::DISPLAY_HEIGHT
            ));
        }
        let mut client = Self {
            conn,
            clock,
            deadline_ms,
            proto: Connection::new(),
            reader: PacketReader::new(),
            root: screen.root,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
            layout,
            min_keycode: setup.min_keycode,
            max_keycode: setup.max_keycode,
            xtest_opcode: 0,
            unchecked: Vec::new(),
        };
        let (bytes, _) = QueryExtensionRequest {
            name: Cow::Borrowed(b"XTEST"),
        }
        .serialize();
        let reply: QueryExtensionReply = client.request(joined(bytes), "QueryExtension")?;
        if !reply.present {
            return Err("tool-computer: the display has no XTEST extension".into());
        }
        client.xtest_opcode = reply.major_opcode;
        Ok(client)
    }

    /// The root window's size in pixels.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// How many children of the root are mapped and viewable.
    pub fn viewable_top_level_windows(&mut self) -> XResult<usize> {
        let (bytes, _) = QueryTreeRequest { window: self.root }.serialize();
        let tree: QueryTreeReply = self.request(joined(bytes), "QueryTree")?;
        let mut sequences = Vec::with_capacity(tree.children.len());
        for window in &tree.children {
            let (bytes, _) = GetWindowAttributesRequest { window: *window }.serialize();
            sequences.push(self.send(
                joined(bytes),
                ReplyFdKind::ReplyWithoutFDs,
                "GetWindowAttributes",
            )?);
        }
        let mut count = 0;
        for sequence in sequences {
            let packet = self.wait_packet(sequence, "GetWindowAttributes")?;
            // A window destroyed since the tree was read answers with an error; it is not
            // viewable.
            if packet[0] == 0 {
                continue;
            }
            let attrs: GetWindowAttributesReply = parse(&packet, "GetWindowAttributes")?;
            if attrs.map_state == MapState::VIEWABLE {
                count += 1;
            }
        }
        Ok(count)
    }

    /// One round trip, so every request without a reply written so far has been handled; the
    /// first of their errors, if any.
    pub fn sync(&mut self) -> XResult<()> {
        if self.unchecked.is_empty() {
            return Ok(());
        }
        let (bytes, _) = GetInputFocusRequest.serialize();
        let sequence = self.send(joined(bytes), ReplyFdKind::ReplyWithoutFDs, "GetInputFocus")?;
        self.wait_packet(sequence, "GetInputFocus")?;
        for (sequence, name) in std::mem::take(&mut self.unchecked) {
            if let Some((packet, _)) = self.proto.poll_for_reply_or_error(sequence) {
                if packet[0] == 0 {
                    return Err(x_error_line(&packet, name));
                }
            }
        }
        Ok(())
    }

    fn remaining_ms(&self) -> u64 {
        self.deadline_ms.saturating_sub(self.clock.now_ms())
    }

    /// Registers and writes one request.
    fn send(
        &mut self,
        bytes: Vec<u8>,
        kind: ReplyFdKind,
        name: &'static str,
    ) -> XResult<SequenceNumber> {
        let sequence = match self.proto.send_request(kind) {
            Some(sequence) => sequence,
            None => {
                // 65 534 requests without a reply in a row: a round trip keeps the 16-bit
                // sequence numbers on the wire unambiguous.
                let (bytes, _) = GetInputFocusRequest.serialize();
                let sync =
                    self.send(joined(bytes), ReplyFdKind::ReplyWithoutFDs, "GetInputFocus")?;
                self.wait_packet(sync, "GetInputFocus")?;
                self.proto
                    .send_request(kind)
                    .ok_or("tool-computer: the X connection lost its sequence numbers")?
            }
        };
        self.conn
            .write(&bytes)
            .map_err(|e| format!("tool-computer: cannot write {name} to the display: {e}"))?;
        if kind == ReplyFdKind::NoReply {
            self.unchecked.push((sequence, name));
        }
        Ok(sequence)
    }

    /// A request without a reply; its error surfaces at the next [`Self::sync`].
    fn send_void(&mut self, bytes: Vec<u8>, name: &'static str) -> XResult<()> {
        self.send(bytes, ReplyFdKind::NoReply, name).map(|_| ())
    }

    /// A request with a reply: earlier requests without one are checked first.
    fn request<T: TryParse>(&mut self, bytes: Vec<u8>, name: &'static str) -> XResult<T> {
        self.sync()?;
        let sequence = self.send(bytes, ReplyFdKind::ReplyWithoutFDs, name)?;
        let packet = self.wait_packet(sequence, name)?;
        if packet[0] == 0 {
            return Err(x_error_line(&packet, name));
        }
        parse(&packet, name)
    }

    /// The reply or error packet for `sequence`, reading the connection until it is there.
    fn wait_packet(&mut self, sequence: SequenceNumber, name: &'static str) -> XResult<Vec<u8>> {
        let until = wait_until(self.clock, self.deadline_ms);
        loop {
            if let Some((packet, _)) = self.proto.poll_for_reply_or_error(sequence) {
                return Ok(packet);
            }
            let bytes = read_bounded(self.conn.as_mut(), self.clock, until, READ_CHUNK, name)?;
            self.feed(&bytes);
        }
    }

    /// Frames `bytes` into packets and queues them; events are dropped, since none are selected.
    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let buffer = self.reader.buffer();
            let n = buffer.len().min(bytes.len());
            buffer[..n].copy_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if let Some(packet) = self.reader.advance(n) {
                self.proto.enqueue_packet(packet);
            }
        }
        while self.proto.poll_for_event_with_sequence().is_some() {}
    }

    fn fake(&mut self, kind: u8, detail: u8, x: i16, y: i16) -> XResult<()> {
        let (bytes, _) = FakeInputRequest {
            type_: kind,
            detail,
            time: 0,
            root: self.root,
            root_x: x,
            root_y: y,
            deviceid: 0,
        }
        .serialize(self.xtest_opcode);
        self.send_void(joined(bytes), "XTEST FakeInput")
    }
}

/// When a wait that starts now must end: within [`REPLY_WAIT_MS`] and the call's deadline.
fn wait_until(clock: &dyn Clock, deadline_ms: u64) -> u64 {
    (clock.now_ms() + REPLY_WAIT_MS).min(deadline_ms)
}

/// Up to `max` bytes, waiting no later than `until`. An empty read is a closed connection.
fn read_bounded(
    conn: &mut dyn Conn,
    clock: &dyn Clock,
    until: u64,
    max: u32,
    name: &str,
) -> XResult<Vec<u8>> {
    let started = clock.now_ms();
    let timed_out = || {
        let secs = until.saturating_sub(started).div_ceil(1000).max(1);
        format!("tool-computer: the display did not answer {name} within {secs} s")
    };
    loop {
        let left = until.saturating_sub(clock.now_ms());
        if left == 0 {
            return Err(timed_out());
        }
        match conn.read(max.max(1), left.min(u64::from(u32::MAX)) as u32) {
            Ok(bytes) if bytes.is_empty() => {
                return Err(format!(
                    "tool-computer: the display closed the connection during {name}"
                ))
            }
            Ok(bytes) => return Ok(bytes),
            Err(ProcError::TimedOut) => {
                if clock.now_ms() >= until {
                    return Err(timed_out());
                }
            }
            Err(e) => {
                return Err(format!(
                    "tool-computer: cannot read the display's answer to {name}: {e}"
                ))
            }
        }
    }
}

impl XBackend for XClient<'_> {
    fn motion(&mut self, x: u16, y: u16) -> XResult<()> {
        self.fake(MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)
    }

    fn button(&mut self, button: u8, press: bool) -> XResult<()> {
        let kind = if press {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(kind, button, 0, 0)
    }

    fn key(&mut self, keycode: u8, press: bool) -> XResult<()> {
        let kind = if press {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0)
    }

    fn keyboard_mapping(&mut self) -> XResult<KeyboardMapping> {
        let (min, max) = (self.min_keycode, self.max_keycode);
        let (bytes, _) = GetKeyboardMappingRequest {
            first_keycode: min,
            count: max - min + 1,
        }
        .serialize();
        let reply: GetKeyboardMappingReply = self.request(joined(bytes), "GetKeyboardMapping")?;
        Ok(KeyboardMapping {
            min_keycode: min,
            per_keycode: reply.keysyms_per_keycode,
            keysyms: reply.keysyms,
        })
    }

    fn change_mapping(&mut self, keycode: u8, keysyms: &[u32]) -> XResult<()> {
        let per = self.keyboard_mapping()?.per_keycode.max(1);
        let mut row = vec![0u32; usize::from(per)];
        for (slot, keysym) in row.iter_mut().zip(keysyms) {
            *slot = *keysym;
        }
        let (bytes, _) = ChangeKeyboardMappingRequest {
            keycode_count: 1,
            first_keycode: keycode,
            keysyms_per_keycode: per,
            keysyms: Cow::Owned(row),
        }
        .serialize();
        self.send_void(joined(bytes), "ChangeKeyboardMapping")?;
        self.sync()
    }

    fn query_pointer(&mut self) -> XResult<(i32, i32)> {
        let (bytes, _) = QueryPointerRequest { window: self.root }.serialize();
        let reply: QueryPointerReply = self.request(joined(bytes), "QueryPointer")?;
        Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    fn get_image(&mut self, x: u16, y: u16, width: u16, height: u16) -> XResult<Frame> {
        let (bytes, _) = GetImageRequest {
            format: ImageFormat::Z_PIXMAP,
            drawable: self.root,
            x: x as i16,
            y: y as i16,
            width,
            height,
            plane_mask: !0,
        }
        .serialize();
        let reply: GetImageReply = self.request(joined(bytes), "GetImage")?;
        let rgb = zpixmap_to_rgb(
            &reply.data,
            u32::from(width),
            u32::from(height),
            self.layout,
        )?;
        Ok(Frame {
            width: u32::from(width),
            height: u32::from(height),
            rgb,
        })
    }

    fn sleep(&mut self, duration: std::time::Duration) {
        let ms = (duration.as_millis() as u64).min(self.remaining_ms());
        self.clock.sleep_ms(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    struct FakeClock {
        now: Cell<u64>,
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }
        fn sleep_ms(&self, ms: u64) {
            self.now.set(self.now.get() + ms);
        }
        fn unix_secs(&self) -> u64 {
            0
        }
    }

    /// Answers each read with the next canned chunk; an empty queue times out.
    struct Canned {
        chunks: VecDeque<Vec<u8>>,
        clock: &'static FakeClock,
        written: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }

    impl Conn for Canned {
        fn read(&mut self, max: u32, timeout_ms: u32) -> Result<Vec<u8>, ProcError> {
            match self.chunks.pop_front() {
                Some(mut chunk) => {
                    if chunk.len() > max as usize {
                        let rest = chunk.split_off(max as usize);
                        self.chunks.push_front(rest);
                    }
                    Ok(chunk)
                }
                None => {
                    self.clock.sleep_ms(u64::from(timeout_ms));
                    Err(ProcError::TimedOut)
                }
            }
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), ProcError> {
            self.written.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn clock() -> &'static FakeClock {
        Box::leak(Box::new(FakeClock { now: Cell::new(0) }))
    }

    fn canned(clock: &'static FakeClock, chunks: Vec<Vec<u8>>) -> Box<dyn Conn> {
        Box::new(Canned {
            chunks: chunks.into(),
            clock,
            written: Default::default(),
        })
    }

    /// A setup-failed answer (status 0) carrying `reason`.
    fn setup_failed(reason: &[u8]) -> Vec<u8> {
        let padded = reason.len().div_ceil(4) * 4;
        let mut out = vec![0, reason.len() as u8, 11, 0, 0, 0];
        out.extend_from_slice(&((padded / 4) as u16).to_ne_bytes());
        out.extend_from_slice(reason);
        out.resize(8 + padded, 0);
        out
    }

    #[test]
    fn a_refused_setup_names_the_servers_reason() {
        let clock = clock();
        let answer =
            setup_failed(b"Authorization required, but no authorization protocol specified\n");
        // Split, so the framing is exercised across reads.
        let (a, b) = answer.split_at(5);
        let err = XClient::setup(
            canned(clock, vec![a.to_vec(), b.to_vec()]),
            b"k",
            clock,
            60_000,
        )
        .err()
        .unwrap();
        assert_eq!(
            err,
            "tool-computer: the display refused the connection: Authorization required, but no \
             authorization protocol specified"
        );
    }

    #[test]
    fn a_silent_display_times_out_within_ten_seconds() {
        let clock = clock();
        let err = XClient::setup(canned(clock, Vec::new()), b"k", clock, 60_000)
            .err()
            .unwrap();
        assert_eq!(
            err,
            "tool-computer: the display did not answer the connection setup within 10 s"
        );
        assert_eq!(clock.now_ms(), 10_000);
    }

    #[test]
    fn the_wait_is_cut_to_the_calls_deadline() {
        let clock = clock();
        let err = XClient::setup(canned(clock, Vec::new()), b"k", clock, 3_000)
            .err()
            .unwrap();
        assert!(err.ends_with("within 3 s"), "{err}");
    }

    #[test]
    fn a_closed_connection_is_named() {
        let clock = clock();
        let err = XClient::setup(canned(clock, vec![Vec::new()]), b"k", clock, 60_000)
            .err()
            .unwrap();
        assert_eq!(
            err,
            "tool-computer: the display closed the connection during the connection setup"
        );
    }

    #[test]
    fn x_errors_are_named_by_code() {
        let mut packet = vec![0u8; 32];
        packet[1] = 8;
        assert_eq!(
            x_error_line(&packet, "GetImage"),
            "tool-computer: X error 8 (BadMatch) on GetImage"
        );
        packet[1] = 140;
        assert_eq!(
            x_error_line(&packet, "XTEST FakeInput"),
            "tool-computer: X error 140 (an extension's error) on XTEST FakeInput"
        );
    }
}
