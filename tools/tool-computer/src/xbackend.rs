//! The X operations an action needs, behind [`XBackend`], and the x11rb implementation that talks
//! to the keeper's Xvfb over loopback TCP.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    ConnectionExt as _, ImageFormat, ImageOrder, MapState, Window, BUTTON_PRESS_EVENT,
    BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

pub type XResult<T> = Result<T, String>;

/// A captured region as 8-bit RGB, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// The server's keycode-to-keysym table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardMapping {
    pub min_keycode: u8,
    pub per_keycode: u8,
    /// `per_keycode` keysyms for each keycode from `min_keycode` up.
    pub keysyms: Vec<u32>,
}

impl KeyboardMapping {
    fn entries(&self) -> impl Iterator<Item = (u8, &[u32])> {
        let per = usize::from(self.per_keycode.max(1));
        self.keysyms
            .chunks(per)
            .enumerate()
            .map(move |(i, syms)| (self.min_keycode.wrapping_add(i as u8), syms))
    }

    /// The keycode that types `keysym` and whether it needs Shift (level 1). Level 0 anywhere is
    /// preferred over level 1.
    pub fn find(&self, keysym: u32) -> Option<(u8, bool)> {
        for level in 0..2 {
            if let Some((code, _)) = self
                .entries()
                .find(|(_, syms)| syms.get(level) == Some(&keysym))
            {
                return Some((code, level == 1));
            }
        }
        None
    }

    /// Keycodes that map to no keysym at all, highest first, free to remap for a moment.
    pub fn spare_keycodes(&self) -> Vec<u8> {
        let mut spare: Vec<u8> = self
            .entries()
            .filter(|(code, syms)| *code >= 8 && syms.iter().all(|s| *s == 0))
            .map(|(code, _)| code)
            .collect();
        spare.reverse();
        spare
    }
}

/// Everything an action does to the display.
pub trait XBackend {
    /// Moves the pointer to an absolute root position.
    fn motion(&mut self, x: u16, y: u16) -> XResult<()>;
    fn button(&mut self, button: u8, press: bool) -> XResult<()>;
    fn key(&mut self, keycode: u8, press: bool) -> XResult<()>;
    fn keyboard_mapping(&mut self) -> XResult<KeyboardMapping>;
    /// Maps `keycode` to `keysyms` (padded with NoSymbol); an empty slice clears it.
    fn change_mapping(&mut self, keycode: u8, keysyms: &[u32]) -> XResult<()>;
    fn query_pointer(&mut self) -> XResult<(i32, i32)>;
    fn get_image(&mut self, x: u16, y: u16, width: u16, height: u16) -> XResult<Frame>;
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The pixel layout of the root window's images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelLayout {
    pub bits_per_pixel: u8,
    pub scanline_pad: u8,
    pub lsb_first: bool,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

fn channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = mask.count_ones();
    let value = (pixel & mask) >> shift;
    if bits >= 8 {
        (value >> (bits - 8)) as u8
    } else {
        ((value * 255) / ((1 << bits) - 1)) as u8
    }
}

/// Converts ZPixmap `data` of `width`x`height` to 8-bit RGB through the visual's masks.
pub fn zpixmap_to_rgb(
    data: &[u8],
    width: u32,
    height: u32,
    layout: PixelLayout,
) -> XResult<Vec<u8>> {
    let bpp = u32::from(layout.bits_per_pixel);
    if !matches!(bpp, 8 | 16 | 24 | 32) {
        return Err(format!(
            "the display uses {bpp} bits per pixel, which this tool cannot read"
        ));
    }
    let pad = u32::from(layout.scanline_pad.max(8));
    let stride = (width * bpp).div_ceil(pad) * pad / 8;
    let bytes = (bpp / 8) as usize;
    if data.len() < (stride * height) as usize {
        return Err(format!(
            "the X server returned {} image bytes for a {width}x{height} region",
            data.len()
        ));
    }
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for row in 0..height as usize {
        let line = &data[row * stride as usize..];
        for col in 0..width as usize {
            let px = &line[col * bytes..col * bytes + bytes];
            let mut pixel = 0u32;
            if layout.lsb_first {
                for (i, b) in px.iter().enumerate() {
                    pixel |= u32::from(*b) << (8 * i);
                }
            } else {
                for b in px {
                    pixel = (pixel << 8) | u32::from(*b);
                }
            }
            rgb.push(channel(pixel, layout.red_mask));
            rgb.push(channel(pixel, layout.green_mask));
            rgb.push(channel(pixel, layout.blue_mask));
        }
    }
    Ok(rgb)
}

/// A connection to one Xvfb, authenticated with its cookie.
pub struct X11 {
    conn: RustConnection,
    root: Window,
    layout: PixelLayout,
}

fn xerr(e: impl std::fmt::Display) -> String {
    format!("tool-computer: the X server refused a request: {e}")
}

impl X11 {
    /// Connects to `127.0.0.1:6000+display` with `cookie` as MIT-MAGIC-COOKIE-1. Never reads
    /// `DISPLAY` or `XAUTHORITY`.
    pub fn connect(display: u16, cookie: &[u8]) -> XResult<Self> {
        let addr = SocketAddr::from(([127, 0, 0, 1], 6000 + display));
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
            .map_err(|e| format!("cannot reach the display at {addr}: {e}"))?;
        let _ = tcp.set_nodelay(true);
        let (stream, _) = DefaultStream::from_tcp_stream(tcp)
            .map_err(|e| format!("cannot use the connection to {addr}: {e}"))?;
        let conn = RustConnection::connect_to_stream_with_auth_info(
            stream,
            0,
            b"MIT-MAGIC-COOKIE-1".to_vec(),
            cookie.to_vec(),
        )
        .map_err(|e| format!("the display at {addr} refused the connection: {e}"))?;

        let setup = conn.setup();
        let screen = setup
            .roots
            .first()
            .ok_or("the display has no screen")?
            .clone();
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual_id == screen.root_visual)
            .ok_or("the display's root visual is not listed")?;
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == screen.root_depth)
            .ok_or("the display lists no pixmap format for its depth")?;
        let layout = PixelLayout {
            bits_per_pixel: format.bits_per_pixel,
            scanline_pad: format.scanline_pad,
            lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
        };
        if conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .map_err(xerr)?
            .is_none()
        {
            return Err("the display has no XTEST extension".into());
        }
        let size = (
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        );
        if size != (crate::DISPLAY_WIDTH, crate::DISPLAY_HEIGHT) {
            return Err(format!(
                "the display at {addr} is {}x{}, not {}x{}",
                size.0,
                size.1,
                crate::DISPLAY_WIDTH,
                crate::DISPLAY_HEIGHT
            ));
        }
        Ok(Self {
            conn,
            root: screen.root,
            layout,
        })
    }

    /// How many children of the root are mapped and viewable.
    pub fn viewable_top_level_windows(&self) -> XResult<usize> {
        let tree = self
            .conn
            .query_tree(self.root)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
        let cookies: Vec<_> = tree
            .children
            .iter()
            .map(|w| self.conn.get_window_attributes(*w))
            .collect::<Result<_, _>>()
            .map_err(xerr)?;
        let mut count = 0;
        for cookie in cookies {
            // A window destroyed since the tree was read answers with an error; it is not viewable.
            if let Ok(attrs) = cookie.reply() {
                if attrs.map_state == MapState::VIEWABLE {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> XResult<()> {
        self.conn
            .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)
            .map_err(xerr)?;
        self.conn.flush().map_err(xerr)
    }
}

impl XBackend for X11 {
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
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
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
        self.conn
            .change_keyboard_mapping(1, keycode, per, &row)
            .map_err(xerr)?
            .check()
            .map_err(xerr)
    }

    fn query_pointer(&mut self) -> XResult<(i32, i32)> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
        Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    fn get_image(&mut self, x: u16, y: u16, width: u16, height: u16) -> XResult<Frame> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                x as i16,
                y as i16,
                width,
                height,
                !0,
            )
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_32_bit_lsb_pixmap_converts_through_the_masks() {
        let layout = PixelLayout {
            bits_per_pixel: 32,
            scanline_pad: 32,
            lsb_first: true,
            red_mask: 0xff0000,
            green_mask: 0x00ff00,
            blue_mask: 0x0000ff,
        };
        // Two pixels: pure red, then (0x12, 0x34, 0x56).
        let data = [0x00, 0x00, 0xff, 0x00, 0x56, 0x34, 0x12, 0x00];
        assert_eq!(
            zpixmap_to_rgb(&data, 2, 1, layout).unwrap(),
            vec![0xff, 0, 0, 0x12, 0x34, 0x56]
        );
    }

    #[test]
    fn a_16_bit_pixmap_scales_to_8_bits() {
        let layout = PixelLayout {
            bits_per_pixel: 16,
            scanline_pad: 32,
            lsb_first: true,
            red_mask: 0xf800,
            green_mask: 0x07e0,
            blue_mask: 0x001f,
        };
        // One white pixel padded to 4 bytes per row.
        let data = [0xff, 0xff, 0, 0];
        assert_eq!(
            zpixmap_to_rgb(&data, 1, 1, layout).unwrap(),
            vec![255, 255, 255]
        );
    }

    #[test]
    fn the_mapping_prefers_level_zero_and_finds_spare_codes() {
        let mapping = KeyboardMapping {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![0, 0, 0x61, 0x41, 0x31, 0x21, 0, 0],
        };
        assert_eq!(mapping.find(0x61), Some((9, false)));
        assert_eq!(mapping.find(0x41), Some((9, true)));
        assert_eq!(mapping.find(0x21), Some((10, true)));
        assert_eq!(mapping.find(0xe9), None);
        assert_eq!(mapping.spare_keycodes(), vec![11, 8]);
    }
}
