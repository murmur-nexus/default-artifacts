//! Image sizing for the inference drivers that send images.
//!
//! murmur inlines each image a tool attaches as a base64 block in the request it hands a driver,
//! and does no decoding or resizing: what a model can see is driver knowledge. A driver runs every
//! image block through [`prepare_image`] with its model's [`Rule`] before translating the request,
//! so the provider receives an image it will not rescale again, and the driver reports, through
//! [`ImageReport`], exactly what it sent.
//!
//! The crate holds no provider rule. Each driver supplies its own size predicate, the media types
//! it always converts, and any per-image byte limit its provider states.
//!
//! Decoding is bounded before any pixel is read: [`MAX_DECODE_EDGE`] and [`MAX_DECODE_PIXELS`]
//! are checked against the header, and [`MAX_DECODE_ALLOC`] caps the decoder's own allocation.
//! Resizing is a separable Catmull-Rom resample over 8-bit samples, freeing each buffer as soon as
//! the next pass has consumed it, so one image at the decode ceiling stays well inside murmur's
//! 512 MiB per-store memory cap. The same input always produces the same output bytes.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, Limits};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::io::Cursor;

/// The longest side, in pixels, the driver decodes. Anthropic's stated per-image maximum.
pub const MAX_DECODE_EDGE: u32 = 8000;

/// The most pixels the driver decodes in one image: 160 MB as 8-bit RGBA.
pub const MAX_DECODE_PIXELS: u64 = 40_000_000;

/// The decoder's allocation cap, handed to `image::Limits::max_alloc`. Above the 160 MB an image
/// at [`MAX_DECODE_PIXELS`] needs as 8-bit RGBA, so it refuses only deeper sample formats.
pub const MAX_DECODE_ALLOC: u64 = 192 * 1024 * 1024;

/// The quality a resized JPEG is re-encoded at.
pub const JPEG_QUALITY: u8 = 90;

/// The resampling filter every resize uses.
pub const RESIZE_FILTER: &str = "Catmull-Rom";

/// The deflate level a resized PNG is written at, with adaptive row filtering. Level 1 writes a
/// 2352×1568 screenshot about a third larger than the encoder's default level in a fifth of the
/// time; the fastest setting writes it twice as large, which can push a high-resolution
/// screenshot past Anthropic's 10 MB per-image limit.
pub const PNG_COMPRESSION: CompressionType = CompressionType::Level(1);

/// Decode standard, padded base64.
pub fn decode_base64(data: &str) -> Result<Vec<u8>, String> {
    STANDARD.decode(data).map_err(|err| err.to_string())
}

/// Encode as standard, padded base64.
pub fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Lowercase hex sha256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn format_of(media_type: &str) -> Result<ImageFormat, String> {
    match media_type {
        "image/png" => Ok(ImageFormat::Png),
        "image/jpeg" => Ok(ImageFormat::Jpeg),
        "image/gif" => Ok(ImageFormat::Gif),
        "image/webp" => Ok(ImageFormat::WebP),
        other => Err(format!("unsupported media type '{other}'")),
    }
}

/// The width and height a header declares, read without decoding a pixel.
pub fn dimensions(media_type: &str, bytes: &[u8]) -> Result<(u32, u32), String> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.set_format(format_of(media_type)?);
    reader.into_dimensions().map_err(|err| err.to_string())
}

/// Whether an image of this size is inside [`MAX_DECODE_EDGE`] and [`MAX_DECODE_PIXELS`].
pub fn within_decode_ceiling(width: u32, height: u32) -> bool {
    width <= MAX_DECODE_EDGE
        && height <= MAX_DECODE_EDGE
        && u64::from(width) * u64::from(height) <= MAX_DECODE_PIXELS
}

/// The largest aspect-preserving size at or below `(width, height)` that `fits` accepts.
///
/// Anthropic's reference search: an image that already fits is returned as it is; otherwise the
/// long edge is binary-searched between 1 and its current length, with the short edge
/// `round_half_to_even(long / aspect)` and at least 1. A taller-than-wide image is searched with
/// its axes swapped. `fits` must accept `(1, 1)` for the result to fit; the result never enlarges.
pub fn largest_fit(width: u32, height: u32, fits: impl Fn(u32, u32) -> bool) -> (u32, u32) {
    if width == 0 || height == 0 || fits(width, height) {
        return (width, height);
    }
    if height > width {
        let (long, short) = search_long_edge(height, width, &|long, short| fits(short, long));
        (short, long)
    } else {
        search_long_edge(width, height, &fits)
    }
}

// `fits` takes `(long, short)`.
fn search_long_edge(long: u32, short: u32, fits: &dyn Fn(u32, u32) -> bool) -> (u32, u32) {
    let aspect = f64::from(long) / f64::from(short);
    let short_for = |edge: u32| ((f64::from(edge) / aspect).round_ties_even() as u32).max(1);
    let (mut lo, mut hi) = (1u32, long);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(mid, short_for(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, short_for(lo))
}

/// The media type a re-encoded image is sent as: PNG and JPEG keep theirs, and GIF and WebP
/// become a PNG of their first frame.
pub fn reencoded_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "image/jpeg",
        _ => "image/png",
    }
}

/// Decode `bytes`, resize to exactly `(width, height)` and re-encode as
/// [`reencoded_media_type`]. A JPEG is written at [`JPEG_QUALITY`]; a PNG at [`PNG_COMPRESSION`]
/// with adaptive filtering. The caller checks [`within_decode_ceiling`] first.
pub fn resize_and_encode(
    media_type: &str,
    bytes: &[u8],
    (width, height): (u32, u32),
) -> Result<Vec<u8>, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.set_format(format_of(media_type)?);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|err| format!("could not be decoded: {err}"))?;

    let (src_width, src_height) = (decoded.width(), decoded.height());
    let (samples, color) = into_8_bit(decoded);
    let resized = resample(samples, color, src_width, src_height, width, height);

    let mut out = Vec::new();
    let written = match reencoded_media_type(media_type) {
        "image/jpeg" => JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .write_image(&resized, width, height, color),
        _ => PngEncoder::new_with_quality(&mut out, PNG_COMPRESSION, PngFilter::Adaptive)
            .write_image(&resized, width, height, color),
    };
    written.map_err(|err| format!("could not be re-encoded after resizing: {err}"))?;
    Ok(out)
}

// The image as interleaved 8-bit samples. Deeper and floating-point formats are narrowed to
// 8-bit RGB or RGBA, which is what the encoders and every provider take.
fn into_8_bit(image: DynamicImage) -> (Vec<u8>, ExtendedColorType) {
    match image {
        DynamicImage::ImageLuma8(buffer) => (buffer.into_raw(), ExtendedColorType::L8),
        DynamicImage::ImageLumaA8(buffer) => (buffer.into_raw(), ExtendedColorType::La8),
        DynamicImage::ImageRgb8(buffer) => (buffer.into_raw(), ExtendedColorType::Rgb8),
        DynamicImage::ImageRgba8(buffer) => (buffer.into_raw(), ExtendedColorType::Rgba8),
        other if other.color().has_alpha() => {
            (other.into_rgba8().into_raw(), ExtendedColorType::Rgba8)
        }
        other => (other.into_rgb8().into_raw(), ExtendedColorType::Rgb8),
    }
}

fn catmull_rom(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        (1.5 * x - 2.5) * x * x + 1.0
    } else if x < 2.0 {
        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
    } else {
        0.0
    }
}

// For each output index along one axis: the first input index it reads, and the normalised
// weight of each input index from there on.
fn axis_taps(in_len: u32, out_len: u32) -> Vec<(usize, Vec<f32>)> {
    let ratio = f64::from(in_len) / f64::from(out_len);
    let scale = ratio.max(1.0);
    let support = 2.0 * scale;
    (0..out_len)
        .map(|out| {
            let center = (f64::from(out) + 0.5) * ratio;
            let left = ((center - support).floor().max(0.0)) as usize;
            let right = ((center + support).ceil() as usize).min(in_len as usize);
            let weights: Vec<f64> = (left..right)
                .map(|i| catmull_rom((i as f64 + 0.5 - center) / scale))
                .collect();
            let sum: f64 = weights.iter().sum();
            (left, weights.iter().map(|w| (w / sum) as f32).collect())
        })
        .collect()
}

fn to_sample(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

// Resize interleaved 8-bit samples: a horizontal pass into an 8-bit intermediate, which frees the
// source, then a vertical pass into the output, which frees the intermediate. A pass along an
// axis whose length does not change is skipped: Catmull-Rom at scale 1 is the identity.
fn resample(
    src: Vec<u8>,
    color: ExtendedColorType,
    width: u32,
    height: u32,
    new_width: u32,
    new_height: u32,
) -> Vec<u8> {
    let horizontal = if new_width == width {
        src
    } else {
        match color.channel_count() {
            1 => horizontal_pass::<1>(&src, width, height, new_width),
            2 => horizontal_pass::<2>(&src, width, height, new_width),
            3 => horizontal_pass::<3>(&src, width, height, new_width),
            _ => horizontal_pass::<4>(&src, width, height, new_width),
        }
    };
    if new_height == height {
        return horizontal;
    }
    let row = new_width as usize * usize::from(color.channel_count());
    vertical_pass(&horizontal, row, height, new_height)
}

fn horizontal_pass<const C: usize>(src: &[u8], width: u32, height: u32, new_width: u32) -> Vec<u8> {
    let taps = axis_taps(width, new_width);
    let (row_in, row_out) = (width as usize * C, new_width as usize * C);
    let mut out = vec![0u8; row_out * height as usize];
    for (src_row, out_row) in src.chunks_exact(row_in).zip(out.chunks_exact_mut(row_out)) {
        for ((start, weights), dst) in taps.iter().zip(out_row.chunks_exact_mut(C)) {
            let mut acc = [0f32; C];
            let pixels = src_row[start * C..].chunks_exact(C);
            for (weight, pixel) in weights.iter().zip(pixels) {
                for c in 0..C {
                    acc[c] += weight * f32::from(pixel[c]);
                }
            }
            for c in 0..C {
                dst[c] = to_sample(acc[c]);
            }
        }
    }
    out
}

fn vertical_pass(src: &[u8], row: usize, height: u32, new_height: u32) -> Vec<u8> {
    let taps = axis_taps(height, new_height);
    let mut out = vec![0u8; row * new_height as usize];
    let mut acc = vec![0f32; row];
    for (out_row, (start, weights)) in out.chunks_exact_mut(row).zip(&taps) {
        acc.iter_mut().for_each(|a| *a = 0.0);
        for (weight, src_row) in weights.iter().zip(src[start * row..].chunks_exact(row)) {
            for (a, sample) in acc.iter_mut().zip(src_row) {
                *a += weight * f32::from(*sample);
            }
        }
        for (dst, a) in out_row.iter_mut().zip(&acc) {
            *dst = to_sample(*a);
        }
    }
    out
}

/// The text block that stands in for an image the driver does not send.
pub fn not_shown_line(media_type: &str, bytes: u64, sha256: &str, reason: &str) -> String {
    format!("[image not shown: {media_type}, {bytes} bytes, sha256 {sha256}; {reason}]")
}

/// What became of one image block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Sent with its original bytes.
    Sent,
    /// Sent after being resized or re-encoded.
    Resized,
    /// Replaced by a [`not_shown_line`] text block.
    Replaced,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Sent => "sent",
            Outcome::Resized => "resized",
            Outcome::Replaced => "replaced",
        }
    }
}

/// The image as it went to the provider, for an outcome of `sent` or `resized`.
#[derive(Debug, Clone, PartialEq)]
pub struct SentImage {
    pub media_type: String,
    /// `None` only when the original size is unknown: a block whose source is not base64.
    pub size: Option<(u32, u32)>,
    /// Where in the provider request the image sits, in the driver's own vocabulary.
    pub placement: &'static str,
}

/// One entry of the `images` list a driver adds to its response.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageReport {
    /// Index of the message in the murmur request's `messages`.
    pub message: usize,
    /// Index of the block in that message's `content`.
    pub block: usize,
    /// The media type murmur sent.
    pub media_type: String,
    /// Length of the decoded image; of the source data as given when it is not valid base64 or
    /// not a base64 source.
    pub bytes: u64,
    /// sha256 of the same bytes `bytes` counts.
    pub sha256: String,
    pub outcome: Outcome,
    /// The header's width and height, when they were read.
    pub size: Option<(u32, u32)>,
    /// Set for `sent` and `resized`.
    pub sent: Option<SentImage>,
    /// Set for `replaced`: why the image was not sent.
    pub reason: Option<String>,
}

impl ImageReport {
    /// The entry as JSON: `message`, `block`, `media_type`, `bytes`, `sha256` and `outcome`
    /// always; `width` and `height` when known; `sent_media_type` and `placement` for `sent` and
    /// `resized`, with `sent_width`, `sent_height` and `scale` (`sent_width / width`) when the size
    /// is known; `reason` for `replaced`.
    pub fn to_json(&self) -> Value {
        let mut entry = Map::new();
        entry.insert("message".into(), self.message.into());
        entry.insert("block".into(), self.block.into());
        entry.insert("media_type".into(), self.media_type.clone().into());
        entry.insert("bytes".into(), self.bytes.into());
        entry.insert("sha256".into(), self.sha256.clone().into());
        entry.insert("outcome".into(), self.outcome.as_str().into());
        if let Some((width, height)) = self.size {
            entry.insert("width".into(), width.into());
            entry.insert("height".into(), height.into());
        }
        if let Some(sent) = &self.sent {
            entry.insert("sent_media_type".into(), sent.media_type.clone().into());
            if let (Some((sent_width, sent_height)), Some((width, _))) = (sent.size, self.size) {
                entry.insert("sent_width".into(), sent_width.into());
                entry.insert("sent_height".into(), sent_height.into());
                entry.insert(
                    "scale".into(),
                    (f64::from(sent_width) / f64::from(width)).into(),
                );
            }
            entry.insert("placement".into(), sent.placement.into());
        }
        if let Some(reason) = &self.reason {
            entry.insert("reason".into(), reason.clone().into());
        }
        Value::Object(entry)
    }
}

/// The `images` list for a response.
pub fn reports_json(reports: &[ImageReport]) -> Value {
    Value::Array(reports.iter().map(ImageReport::to_json).collect())
}

/// Add `reports` to a driver's successful response as its top-level `images` list. A response to
/// a request that held no image gains no key, so its bytes are unchanged.
pub fn attach_image_report(response: &mut Value, reports: &[ImageReport]) {
    if reports.is_empty() {
        return;
    }
    if let Some(response) = response.as_object_mut() {
        response.insert("images".to_string(), reports_json(reports));
    }
}

/// How one model takes images.
pub enum Rule<'a> {
    /// The model reads no image; every image becomes a [`not_shown_line`] with this reason.
    NoVision {
        reason: String,
    },
    Sized(SizeRule<'a>),
}

/// The provider rule for a model that reads images.
pub struct SizeRule<'a> {
    /// Whether the provider takes an image of this width and height without rescaling it.
    pub fits: &'a dyn Fn(u32, u32) -> bool,
    /// Media types re-encoded even when their size already fits.
    pub always_reencode: &'a [&'a str],
    /// The provider's per-image ceiling on the base64 it takes, and the phrase naming it in a
    /// reason, e.g. `Anthropic's per-image limit`.
    pub max_base64_len: Option<(usize, &'a str)>,
}

/// What the driver does with one image block.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send the block as it is.
    Keep,
    /// Send this media type and base64 data instead.
    Replace { media_type: String, data: String },
    /// Send this text block instead of the image.
    Line(String),
}

/// One image block, measured and sized under `rule`.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub action: Action,
    pub report: ImageReport,
}

/// Measure and size the base64 image at `messages[message].content[block]`.
///
/// Never fails: an image the driver cannot use becomes a [`not_shown_line`] with the reason, and
/// the request is still sent. `placement` is recorded for an image that is sent.
pub fn prepare_image(
    message: usize,
    block: usize,
    media_type: &str,
    data: &str,
    placement: &'static str,
    rule: &Rule<'_>,
) -> Prepared {
    let decoded = decode_base64(data);
    let (bytes, sha256) = match &decoded {
        Ok(image) => (image.len() as u64, sha256_hex(image)),
        Err(_) => (data.len() as u64, sha256_hex(data.as_bytes())),
    };
    let mut report = ImageReport {
        message,
        block,
        media_type: media_type.to_string(),
        bytes,
        sha256,
        outcome: Outcome::Replaced,
        size: None,
        sent: None,
        reason: None,
    };
    let replace = |mut report: ImageReport, reason: String| {
        let line = not_shown_line(&report.media_type, report.bytes, &report.sha256, &reason);
        report.outcome = Outcome::Replaced;
        report.reason = Some(reason);
        Prepared {
            action: Action::Line(line),
            report,
        }
    };

    let image = match decoded {
        Ok(image) => image,
        Err(err) => return replace(report, format!("the image data is not valid base64: {err}")),
    };
    let rule = match rule {
        Rule::NoVision { reason } => return replace(report, reason.clone()),
        Rule::Sized(rule) => rule,
    };
    let (width, height) = match dimensions(media_type, &image) {
        Ok(size) => size,
        Err(err) => {
            return replace(
                report,
                format!("the image's dimensions could not be read: {err}"),
            )
        }
    };
    report.size = Some((width, height));
    if !within_decode_ceiling(width, height) {
        return replace(
            report,
            format!(
                "the image is {width}x{height} px, over the driver's decode ceiling of \
                 {MAX_DECODE_EDGE} px a side and {MAX_DECODE_PIXELS} pixels"
            ),
        );
    }

    let target = largest_fit(width, height, rule.fits);
    let (outcome, action, sent_media_type, base64_len) =
        if target == (width, height) && !rule.always_reencode.contains(&media_type) {
            (
                Outcome::Sent,
                Action::Keep,
                media_type.to_string(),
                data.len(),
            )
        } else {
            let encoded = match resize_and_encode(media_type, &image, target) {
                Ok(encoded) => encoded,
                Err(err) => return replace(report, format!("the image {err}")),
            };
            drop(image);
            let sent_media_type = reencoded_media_type(media_type).to_string();
            let data = encode_base64(&encoded);
            let len = data.len();
            let action = Action::Replace {
                media_type: sent_media_type.clone(),
                data,
            };
            (Outcome::Resized, action, sent_media_type, len)
        };
    if let Some((limit, named)) = rule.max_base64_len {
        if base64_len > limit {
            return replace(
                report,
                format!("the image is {base64_len} bytes as base64, over {named} of {limit} bytes"),
            );
        }
    }
    report.outcome = outcome;
    report.sent = Some(SentImage {
        media_type: sent_media_type,
        size: Some(target),
        placement,
    });
    Prepared { action, report }
}

/// Measure and size the image at `messages[message].content[block]` whatever its source:
/// [`prepare_image`] for a base64 source, which is what `source_type` absent means, and
/// [`prepare_unmeasured`] for any other.
pub fn prepare_source(
    message: usize,
    block: usize,
    source_type: Option<&str>,
    media_type: &str,
    data: &str,
    placement: &'static str,
    rule: &Rule<'_>,
) -> Prepared {
    if source_type.is_none_or(|kind| kind == "base64") {
        prepare_image(message, block, media_type, data, placement, rule)
    } else {
        prepare_unmeasured(message, block, media_type, data, placement, rule)
    }
}

/// An image block whose source is not base64 (a URL, a file id): sent as it is and unmeasured,
/// unless the model reads no image. `bytes` and `sha256` describe the source data as given.
pub fn prepare_unmeasured(
    message: usize,
    block: usize,
    media_type: &str,
    data: &str,
    placement: &'static str,
    rule: &Rule<'_>,
) -> Prepared {
    let mut report = ImageReport {
        message,
        block,
        media_type: media_type.to_string(),
        bytes: data.len() as u64,
        sha256: sha256_hex(data.as_bytes()),
        outcome: Outcome::Sent,
        size: None,
        sent: None,
        reason: None,
    };
    if let Rule::NoVision { reason } = rule {
        let line = not_shown_line(media_type, report.bytes, &report.sha256, reason);
        report.outcome = Outcome::Replaced;
        report.reason = Some(reason.clone());
        return Prepared {
            action: Action::Line(line),
            report,
        };
    }
    report.sent = Some(SentImage {
        media_type: media_type.to_string(),
        size: None,
        placement,
    });
    Prepared {
        action: Action::Keep,
        report,
    }
}

/// Test images: a deterministic screenshot-like pattern, encoded in each supported format. Built
/// only for this crate's tests and under the `fixtures` feature, which only dev-dependencies turn
/// on, so no component links it.
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures {
    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    /// A `width`×`height` RGB image of flat panels, thin rules and dense glyph-like strokes: the
    /// mix a screenshot has, which exercises both the filter and the PNG compressor.
    pub fn screenshot_like(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            let panel = ((x / 240) + (y / 180)) % 3;
            let base = [240u8, 228, 210][panel as usize];
            let rule = x % 120 == 0 || y % 90 == 0;
            let glyph = (y % 18) < 11 && ((x * 7 + y * 3) % 13) < 4 && (x / 60) % 4 != 3;
            if rule {
                Rgb([90, 110, 160])
            } else if glyph {
                Rgb([20, 20, 30])
            } else {
                Rgb([base, base, base.saturating_add(8)])
            }
        })
    }

    pub fn png(image: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilter::Adaptive)
            .write_image(
                image,
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        out
    }

    pub fn jpeg(image: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 85)
            .write_image(
                image,
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        out
    }

    pub fn gif(image: &RgbImage) -> Vec<u8> {
        let rgba = RgbaImage::from_fn(image.width(), image.height(), |x, y| {
            let Rgb([r, g, b]) = *image.get_pixel(x, y);
            Rgba([r, g, b, 255])
        });
        let mut out = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut out)
            .encode(&rgba, rgba.width(), rgba.height(), ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    /// A PNG holding only a signature, an IHDR declaring `width`×`height`, an empty IDAT and
    /// IEND: enough for a header read, never decodable.
    pub fn png_header_only(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
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
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend(kind);
            out.extend(data);
            let crc = crc32(&out[start..]);
            out.extend(crc.to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &[]);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    pub fn webp(image: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .write_image(
                image,
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        out
    }
}

#[cfg(test)]
mod tests;
