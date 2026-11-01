//! Recognising a PNG, JPEG, GIF or WebP file, and its pixel size, from its header alone.
//!
//! `tool-read` attaches such a file for murmur to show the model, so it needs the media type
//! murmur checks the leading bytes against, plus the width and height for the result's summary
//! line. Both come from the first 32 bytes, plus for a JPEG the segment headers before its
//! start-of-frame, reached by seeking. The image body is never read or decoded, and nothing
//! is allocated in proportion to the file's size.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// The prefix every format but JPEG is recognised from: a WebP `VP8 ` frame header ends at 30.
const PREFIX_LEN: usize = 32;
/// JPEG segments walked before the search for a start-of-frame gives up.
const MAX_JPEG_SEGMENTS: usize = 256;
/// `FF` fill bytes skipped, across the whole JPEG walk, before it gives up. The JPEG standard
/// allows any number before a marker; the bound keeps a file of `FF`s from being read whole one
/// byte at a time.
const MAX_JPEG_FILL_BYTES: usize = 1024;

/// What an image file's header says: the media type murmur's content check expects, and the
/// pixel size. Width and height are never 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageHeader {
    pub(crate) media_type: &'static str,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// The header of the PNG, JPEG, GIF or WebP image `file` holds, read from its current position
/// at the start of the file. `Ok(None)` when the leading bytes are none of the four formats,
/// the header ends early, or it declares a width or height of 0; `Err` only for an I/O error
/// other than reaching the end of the file.
pub(crate) fn image_header<R: Read + Seek>(file: &mut R) -> io::Result<Option<ImageHeader>> {
    let mut prefix = [0u8; PREFIX_LEN];
    let len = read_prefix(file, &mut prefix)?;
    let prefix = &prefix[..len];

    let header = if prefix.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(prefix)
    } else if prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a") {
        gif(prefix)
    } else if prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP") {
        webp(prefix)
    } else if prefix.starts_with(&[0xFF, 0xD8, 0xFF]) {
        jpeg(file)?
    } else {
        None
    };
    Ok(header.filter(|h| h.width > 0 && h.height > 0))
}

/// Whether `path`'s final extension, compared without case, is one an image file carries:
/// `png`, `jpg`, `jpeg`, `gif` or `webp`.
pub(crate) fn has_image_name(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["png", "jpg", "jpeg", "gif", "webp"]
                .iter()
                .any(|image| ext.eq_ignore_ascii_case(image))
        })
}

/// Fill `buf` from `file`, stopping early at the end of the file; the count of bytes read.
fn read_prefix<R: Read>(file: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 3)?;
    Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
}

/// The IHDR chunk follows the 8-byte signature: length, `IHDR`, then width and height.
fn png(prefix: &[u8]) -> Option<ImageHeader> {
    if prefix.get(12..16) != Some(b"IHDR") {
        return None;
    }
    Some(ImageHeader {
        media_type: "image/png",
        width: be_u32(prefix, 16)?,
        height: be_u32(prefix, 20)?,
    })
}

/// The logical screen descriptor follows the 6-byte signature.
fn gif(prefix: &[u8]) -> Option<ImageHeader> {
    Some(ImageHeader {
        media_type: "image/gif",
        width: u32::from(le_u16(prefix, 6)?),
        height: u32::from(le_u16(prefix, 8)?),
    })
}

/// The first chunk after `RIFF <size> WEBP` holds the size: a lossy `VP8 ` key frame header, a
/// lossless `VP8L` header, or an extended `VP8X` canvas.
fn webp(prefix: &[u8]) -> Option<ImageHeader> {
    let (width, height) = match prefix.get(12..16)? {
        b"VP8 " => {
            if prefix.get(23..26) != Some(&[0x9D, 0x01, 0x2A]) {
                return None;
            }
            (
                u32::from(le_u16(prefix, 26)? & 0x3FFF),
                u32::from(le_u16(prefix, 28)? & 0x3FFF),
            )
        }
        b"VP8L" => {
            if prefix.get(20) != Some(&0x2F) {
                return None;
            }
            let bits = le_u32(prefix, 21)?;
            ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => (le_u24(prefix, 24)? + 1, le_u24(prefix, 27)? + 1),
        _ => return None,
    };
    Some(ImageHeader {
        media_type: "image/webp",
        width,
        height,
    })
}

/// Walk a JPEG's segments from offset 2 to its first start-of-frame, reading only each
/// marker, length and, at the SOF, the five bytes after the length; every other segment is
/// skipped by seeking. `None` at the end of the file, at an end-of-image or start-of-scan
/// before any SOF, after [`MAX_JPEG_SEGMENTS`], or at a byte where a marker must start that is
/// not `FF`.
fn jpeg<R: Read + Seek>(file: &mut R) -> io::Result<Option<ImageHeader>> {
    file.seek(SeekFrom::Start(2))?;
    let mut fill_bytes = 0;
    for _ in 0..MAX_JPEG_SEGMENTS {
        let Some([first]) = read_array::<_, 1>(file)? else {
            return Ok(None);
        };
        if first != 0xFF {
            return Ok(None);
        }
        let marker = loop {
            let Some([byte]) = read_array::<_, 1>(file)? else {
                return Ok(None);
            };
            if byte != 0xFF {
                break byte;
            }
            fill_bytes += 1;
            if fill_bytes > MAX_JPEG_FILL_BYTES {
                return Ok(None);
            }
        };
        match marker {
            // Restart markers and TEM stand alone, with no length.
            0xD0..=0xD7 | 0x01 => continue,
            // End of image, or start of scan: no SOF precedes the image data.
            0xD9 | 0xDA => return Ok(None),
            _ => {}
        }
        let Some(length) = read_array::<_, 2>(file)? else {
            return Ok(None);
        };
        let length = u16::from_be_bytes(length);
        if length < 2 {
            return Ok(None);
        }
        // Every SOFn but DHT (C4), JPG (C8) and DAC (CC), which share the C0–CF range.
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            // Precision, then height and width: +2, +3 and +5 from the length field's start.
            let Some(frame) = read_array::<_, 5>(file)? else {
                return Ok(None);
            };
            return Ok(Some(ImageHeader {
                media_type: "image/jpeg",
                width: u32::from(u16::from_be_bytes([frame[3], frame[4]])),
                height: u32::from(u16::from_be_bytes([frame[1], frame[2]])),
            }));
        }
        file.seek(SeekFrom::Current(i64::from(length) - 2))?;
    }
    Ok(None)
}

/// The next `N` bytes of `file`, or `None` when it ends first.
fn read_array<R: Read, const N: usize>(file: &mut R) -> io::Result<Option<[u8; N]>> {
    let mut buf = [0u8; N];
    match file.read_exact(&mut buf) {
        Ok(()) => Ok(Some(buf)),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::test_png::png;

    /// A reader over `bytes` that counts the bytes its `read` calls return.
    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: usize,
    }

    impl Counting {
        fn new(bytes: Vec<u8>) -> Self {
            Counting {
                inner: Cursor::new(bytes),
                read: 0,
            }
        }
    }

    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    impl Seek for Counting {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// The header of `bytes`, and how many bytes reading it took.
    fn sniff(bytes: Vec<u8>) -> (Option<ImageHeader>, usize) {
        let mut reader = Counting::new(bytes);
        let header = image_header(&mut reader).expect("a cursor read cannot fail");
        (header, reader.read)
    }

    fn header(media_type: &'static str, width: u32, height: u32) -> Option<ImageHeader> {
        Some(ImageHeader {
            media_type,
            width,
            height,
        })
    }

    /// `RIFF <size> WEBP` followed by `chunk`, padded with zeros to `len` bytes.
    fn webp_bytes(chunk: &[u8], len: usize) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&((len - 8) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(chunk);
        bytes.resize(len, 0);
        bytes
    }

    fn vp8(width: u16, height: u16) -> Vec<u8> {
        let mut chunk = b"VP8 ".to_vec();
        chunk.extend_from_slice(&1000u32.to_le_bytes());
        chunk.extend_from_slice(&[0x50, 0x2A, 0x00]); // frame tag
        chunk.extend_from_slice(&[0x9D, 0x01, 0x2A]); // start code
        chunk.extend_from_slice(&width.to_le_bytes());
        chunk.extend_from_slice(&height.to_le_bytes());
        webp_bytes(&chunk, 1046)
    }

    fn vp8l(width: u32, height: u32) -> Vec<u8> {
        let mut chunk = b"VP8L".to_vec();
        chunk.extend_from_slice(&500u32.to_le_bytes());
        chunk.push(0x2F);
        let bits = (width - 1) | (height - 1) << 14;
        chunk.extend_from_slice(&bits.to_le_bytes());
        webp_bytes(&chunk, 528)
    }

    fn vp8x(width: u32, height: u32) -> Vec<u8> {
        let mut chunk = b"VP8X".to_vec();
        chunk.extend_from_slice(&10u32.to_le_bytes());
        chunk.extend_from_slice(&[0x10, 0, 0, 0]); // flags, reserved
        chunk.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        chunk.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        webp_bytes(&chunk, 4096)
    }

    fn gif_bytes(version: &[u8; 6], width: u16, height: u16) -> Vec<u8> {
        let mut bytes = version.to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0xF7, 0, 0]);
        bytes.resize(2048, 0);
        bytes
    }

    /// A JPEG segment: `FF <marker> <length> <payload>`.
    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFF, marker];
        bytes.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    /// SOI, a 60,000-byte APP1, a DQT, then a start-of-frame `sof` of `width`×`height`, a scan
    /// and EOI.
    fn jpeg_bytes(sof: u8, width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        let mut app1 = b"Exif\0\0".to_vec();
        app1.resize(60_000 - 2, 0xAB);
        bytes.extend(segment(0xE1, &app1));
        bytes.extend(segment(0xDB, &[0u8; 65]));
        let mut frame = vec![8];
        frame.extend_from_slice(&height.to_be_bytes());
        frame.extend_from_slice(&width.to_be_bytes());
        frame.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        bytes.extend(segment(sof, &frame));
        bytes.extend(segment(0xDA, &[1, 1, 0, 0, 0x3F, 0]));
        bytes.extend_from_slice(&[0x12; 4096]);
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        bytes
    }

    #[test]
    fn image_header_reads_a_png_from_its_first_32_bytes() {
        let (got, read) = sniff(png(64, 48));
        assert_eq!(got, header("image/png", 64, 48));
        assert!(read <= 32, "read {read} bytes");
    }

    #[test]
    fn image_header_reads_a_png_signature_and_ihdr_followed_by_garbage() {
        let mut bytes = png(300, 200)[..33].to_vec();
        bytes.extend_from_slice(&[0x5A; 10_000]);
        let (got, read) = sniff(bytes);
        assert_eq!(got, header("image/png", 300, 200));
        assert!(read <= 32, "read {read} bytes");
    }

    #[test]
    fn image_header_reads_gif87a_and_gif89a() {
        for version in [b"GIF87a", b"GIF89a"] {
            let (got, read) = sniff(gif_bytes(version, 320, 7));
            assert_eq!(got, header("image/gif", 320, 7));
            assert!(read <= 32, "read {read} bytes");
        }
    }

    #[test]
    fn image_header_reads_webp_vp8_vp8l_and_vp8x() {
        for (bytes, width, height) in [
            (vp8(640, 480), 640, 480),
            (vp8(16383, 1), 16383, 1),
            (vp8l(1, 16384), 1, 16384),
            (vp8l(1000, 750), 1000, 750),
            (vp8x(20_000, 3), 20_000, 3),
            (vp8x(1 << 24, 1 << 24), 1 << 24, 1 << 24),
        ] {
            let (got, read) = sniff(bytes);
            assert_eq!(got, header("image/webp", width, height));
            assert!(read <= 32, "read {read} bytes");
        }
    }

    #[test]
    fn image_header_reads_a_vp8_size_without_its_scale_bits() {
        let mut bytes = vp8(640, 480);
        bytes[27] |= 0xC0;
        bytes[29] |= 0x40;
        assert_eq!(sniff(bytes).0, header("image/webp", 640, 480));
    }

    #[test]
    fn image_header_seeks_past_a_large_app1_to_a_baseline_jpeg_frame() {
        let bytes = jpeg_bytes(0xC0, 1920, 1080);
        assert!(bytes.len() > 60_000);
        let (got, read) = sniff(bytes);
        assert_eq!(got, header("image/jpeg", 1920, 1080));
        assert!(read < 1024, "read {read} bytes");
    }

    #[test]
    fn image_header_reads_a_progressive_jpeg_frame() {
        let (got, read) = sniff(jpeg_bytes(0xC2, 33, 4000));
        assert_eq!(got, header("image/jpeg", 33, 4000));
        assert!(read < 1024, "read {read} bytes");
    }

    #[test]
    fn image_header_skips_fill_bytes_and_standalone_jpeg_markers() {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xFF, 0xD0, 0xFF, 0x01];
        bytes.extend(segment(0xC4, &[0; 20])); // DHT, not a frame
        bytes.extend(segment(0xC1, &[8, 0, 9, 0, 12, 1, 1, 0x11, 0]));
        assert_eq!(sniff(bytes).0, header("image/jpeg", 12, 9));
    }

    #[test]
    fn image_header_is_none_for_a_header_cut_short() {
        let png_bytes = png(64, 48);
        let cuts: Vec<Vec<u8>> = vec![
            png_bytes[..8].to_vec(),
            png_bytes[..12].to_vec(),
            png_bytes[..23].to_vec(),
            gif_bytes(b"GIF89a", 10, 10)[..9].to_vec(),
            vp8(10, 10)[..29].to_vec(),
            vp8l(10, 10)[..24].to_vec(),
            vp8x(10, 10)[..29].to_vec(),
            b"RIFF\0\0\0\0WEBP".to_vec(),
            vec![0xFF, 0xD8, 0xFF],
            jpeg_bytes(0xC0, 10, 10)[..60_010].to_vec(),
        ];
        for bytes in cuts {
            let len = bytes.len();
            assert_eq!(sniff(bytes).0, None, "{len} bytes");
        }
        // The SOF's length is there but its height and width are not.
        let mut bytes = vec![0xFF, 0xD8];
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 8, 0]);
        assert_eq!(sniff(bytes).0, None);
    }

    #[test]
    fn image_header_is_none_for_a_zero_size() {
        for bytes in [
            png(0, 0),
            png(64, 0),
            gif_bytes(b"GIF89a", 0, 5),
            vp8(0, 0),
            jpeg_bytes(0xC0, 0, 0),
            jpeg_bytes(0xC2, 10, 0),
        ] {
            assert_eq!(sniff(bytes).0, None);
        }
    }

    #[test]
    fn image_header_is_none_for_a_jpeg_with_no_frame_in_256_segments() {
        let mut bytes = vec![0xFF, 0xD8];
        for _ in 0..256 {
            bytes.extend(segment(0xE2, &[0; 4]));
        }
        bytes.extend(segment(0xC0, &[8, 0, 9, 0, 12, 1, 1, 0x11, 0]));
        let (got, read) = sniff(bytes);
        assert_eq!(got, None);
        assert!(read < 32 + 256 * 4 + 1, "read {read} bytes");

        // 255 segments then the frame is still found.
        let mut bytes = vec![0xFF, 0xD8];
        for _ in 0..255 {
            bytes.extend(segment(0xE2, &[0; 4]));
        }
        bytes.extend(segment(0xC0, &[8, 0, 9, 0, 12, 1, 1, 0x11, 0]));
        assert_eq!(sniff(bytes).0, header("image/jpeg", 12, 9));
    }

    #[test]
    fn image_header_is_none_for_a_jpeg_scan_or_end_before_any_frame() {
        for marker in [0xDA, 0xD9] {
            let mut bytes = vec![0xFF, 0xD8];
            bytes.extend(segment(0xE0, &[0; 14]));
            bytes.extend_from_slice(&[0xFF, marker, 0, 4, 0, 0]);
            bytes.extend(segment(0xC0, &[8, 0, 9, 0, 12, 1, 1, 0x11, 0]));
            assert_eq!(sniff(bytes).0, None);
        }
    }

    #[test]
    fn image_header_is_none_for_a_jpeg_of_fill_bytes() {
        let mut bytes = vec![0xFF, 0xD8];
        bytes.resize(1 << 20, 0xFF);
        let (got, read) = sniff(bytes);
        assert_eq!(got, None);
        assert!(read < 2048, "read {read} bytes");
    }

    #[test]
    fn image_header_is_none_for_plain_text_and_other_files() {
        for bytes in [
            b"hello world\n".to_vec(),
            Vec::new(),
            b"%PDF-1.4\n".to_vec(),
            b"RIFF\x24\0\0\0WAVEfmt ".to_vec(),
            b"RIFF\x24\0\0\0WEBPABCD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0".to_vec(),
            b"\x89PNG\r\n\x1a\n\0\0\0\x0dIEND\0\0\0\x01\0\0\0\x01".to_vec(),
        ] {
            assert_eq!(sniff(bytes).0, None);
        }
    }

    #[test]
    fn has_image_name_matches_the_final_extension_without_case() {
        for name in [
            "a.png", "a.jpg", "a.jpeg", "a.gif", "a.webp", "X.PNG", "d/e.JpEg", "x.tar.gif",
        ] {
            assert!(has_image_name(name), "{name}");
        }
        for name in ["a.txt", "png", "a.png.txt", "a.pdf", "dir.png/a", "a.svg", ""] {
            assert!(!has_image_name(name), "{name}");
        }
    }
}
