//! The X operations an action needs, behind [`XBackend`], and the pure pixel and keymap helpers
//! the X client in [`crate::x11`] builds on.

use std::time::Duration;

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
