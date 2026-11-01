//! A minimal PNG writer: 8-bit RGB (colour type 2), one IDAT through zlib, no filtering.

use std::io::Write;

use flate2::{write::ZlibEncoder, Compression};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32fast::hash(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// `rgb` is `width * height * 3` bytes, rows top to bottom.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(rgb.len(), width as usize * height as usize * 3);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // Bit depth 8, colour type 2 (RGB), deflate, adaptive filtering, no interlace.
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let row = width as usize * 3;
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::fast());
    for line in rgb.chunks_exact(row.max(1)).take(height as usize) {
        // Filter type 0 (None) per scanline.
        zlib.write_all(&[0]).expect("writing to a Vec");
        zlib.write_all(line).expect("writing to a Vec");
    }
    let idat = zlib.finish().expect("writing to a Vec");

    let mut out = Vec::with_capacity(idat.len() + 64);
    out.extend_from_slice(&SIGNATURE);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn the_output_is_a_png_whose_pixels_round_trip() {
        let (w, h) = (3u32, 2u32);
        let rgb: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let png = encode_rgb(w, h, &rgb);
        assert_eq!(&png[..8], &SIGNATURE);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), w);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), h);
        assert_eq!(&png[24..29], &[8, 2, 0, 0, 0]);
        let ihdr_crc = crc32fast::hash(&png[12..29]);
        assert_eq!(
            u32::from_be_bytes(png[29..33].try_into().unwrap()),
            ihdr_crc
        );

        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(&png[41..41 + idat_len])
            .read_to_end(&mut raw)
            .unwrap();
        let mut pixels = Vec::new();
        for line in raw.chunks(1 + w as usize * 3) {
            assert_eq!(line[0], 0);
            pixels.extend_from_slice(&line[1..]);
        }
        assert_eq!(pixels, rgb);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}
