//! A valid PNG built byte by byte for tests: signature, IHDR, one IDAT holding stored
//! (uncompressed) deflate blocks, and IEND, every CRC and the zlib checksum computed here.
//! Shared by the unit tests and, through `#[path]`, the integration tests.

/// A `width`×`height` 8-bit RGB PNG, every pixel `#FF8000`.
pub fn png(width: u32, height: u32) -> Vec<u8> {
    let row: Vec<u8> = std::iter::once(0u8) // filter: none
        .chain((0..width).flat_map(|_| [0xFF, 0x80, 0x00]))
        .collect();
    let raw: Vec<u8> = (0..height).flat_map(|_| row.iter().copied()).collect();

    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(0xFFFF).peekable();
    if blocks.peek().is_none() {
        zlib.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        zlib.push(u8::from(blocks.peek().is_none()));
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // depth 8, RGB, deflate, no filter, no interlace

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    png.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(body);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    b << 16 | a
}
