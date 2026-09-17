//! Images: our decode, GPUI's draw (F-33, PV-D09).
//!
//! GPUI's asset loader does a blocking whole-file read and a full-resolution
//! decode — right for a bundled 24 px icon, catastrophic for a photographer's
//! directory. So this module owns the pipeline and GPUI only draws the
//! result:
//!
//! 1. Probe the header for dimensions without decoding. Past
//!    [`crate::config::IMAGE_SOURCE_PIXEL_CAP`] no allocation happens at all.
//! 2. Decode off-thread (the caller runs this off the window thread) with
//!    the decoder's own allocation limits set as the backstop.
//! 3. Downscale to the long-edge cap with a triangle filter.
//! 4. Convert to BGRA and hand out [`Bitmap`]s, the layout `RenderImage`
//!    wants.
//!
//! EXIF orientation is deliberately not applied yet: the orientation query
//! is private API on this `image` version, and hand-rolling EXIF parsing
//! for one tag is the wrong trade. Phone photos may arrive unrotated until
//! the decoder exposes it or the orientation is read structurally.
//!
//! Animated GIF and WebP decode to bounded frame lists; anything past the
//! frame or memory cap falls back to the first frame with a flag the footer
//! reports. HEIC/RAW go to the OS thumbnail path (PV-D07); SVG previews as
//! source until the bounded rasteriser lands.

use std::io::Cursor;

use super::config::{
    ANIMATION_FRAME_CAP, ANIMATION_MEMORY_CAP, IMAGE_SOURCE_PIXEL_CAP, IMAGE_TARGET_LONG_EDGE,
};

/// A decoded bitmap in the BGRA layout `RenderImage` wants: row-major,
/// no padding, `width * height * 4` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// Whether any pixel is non-opaque. Detected during the BGRA swizzle —
    /// no second pass — so the view knows when the checkerboard matters.
    pub has_transparency: bool,
}

impl Bitmap {
    /// Whether the buffer matches the dimensions it claims.
    ///
    /// Checked at the boundary rather than trusted: a wrong stride would be
    /// read as pixels.
    pub fn is_well_formed(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.bgra.len() == (self.width as usize) * (self.height as usize) * 4
    }
}

/// One animation frame: composited full-canvas bitmap plus delay.
#[derive(Clone, Debug)]
pub struct Frame {
    pub bitmap: Bitmap,
    pub delay_ms: u32,
}

/// A decoded image: natural size plus display-ready frames.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    /// Dimensions before downscaling, for the honest footer.
    pub natural: (u32, u32),
    /// Human format name for the header ("PNG image").
    pub format_name: &'static str,
    /// One entry for stills; bounded frames for animations.
    pub frames: Vec<Frame>,
    /// True when frames were cut by count or memory (first frame kept).
    pub frames_capped: bool,
}

/// Why an image did not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// Past the pixel cap: never an attempted decode.
    TooLarge { width: u32, height: u32 },
    /// A valid image in a format with no in-process decoder (HEIC, AVIF):
    /// the OS-thumbnail path's future customer, metadata until then — never
    /// the hex floor, which would present a good photo as a corrupt file.
    UnsupportedFormat,
    /// The header lies or the bytes are corrupt.
    Undecodable,
}

/// Decode budgeted bytes into display-ready frames.
///
/// Runs on the caller's thread, which must not be the window thread:
/// decoders block and allocate. Reads `bytes` only; the byte budget was
/// enforced by the caller through `Vfs::read`.
pub fn decode(bytes: &[u8]) -> Result<DecodedImage, ImageError> {
    // Recognised-but-undecodable first: `guess_format` below cannot name
    // these (no HEIC/AVIF decoder is compiled in), and a failed guess would
    // otherwise misreport a good photo as corrupt bytes.
    if has_unsupported_image_brand(bytes) {
        return Err(ImageError::UnsupportedFormat);
    }
    let format = ::image::guess_format(bytes).map_err(|_| ImageError::Undecodable)?;
    if !matches!(
        format,
        ::image::ImageFormat::Png
            | ::image::ImageFormat::Jpeg
            | ::image::ImageFormat::Gif
            | ::image::ImageFormat::WebP
            | ::image::ImageFormat::Bmp
            | ::image::ImageFormat::Tiff
            | ::image::ImageFormat::Ico
            | ::image::ImageFormat::Qoi
    ) {
        return Err(ImageError::UnsupportedFormat);
    }
    let format_name = format_name(format);
    let (width, height) = probe_dimensions(bytes, format).ok_or(ImageError::Undecodable)?;
    if width as u64 * height as u64 > IMAGE_SOURCE_PIXEL_CAP {
        return Err(ImageError::TooLarge { width, height });
    }

    if matches!(
        format,
        ::image::ImageFormat::Gif | ::image::ImageFormat::WebP
    ) {
        decode_animated(bytes, format, (width, height), format_name)
    } else {
        decode_static(bytes, format, (width, height), format_name)
    }
}

fn format_name(format: ::image::ImageFormat) -> &'static str {
    match format {
        ::image::ImageFormat::Png => "PNG",
        ::image::ImageFormat::Jpeg => "JPEG",
        ::image::ImageFormat::Gif => "GIF",
        ::image::ImageFormat::WebP => "WebP",
        ::image::ImageFormat::Bmp => "BMP",
        ::image::ImageFormat::Tiff => "TIFF",
        ::image::ImageFormat::Ico => "ICO",
        ::image::ImageFormat::Qoi => "QOI",
        _ => "Image",
    }
}

/// Header dimensions without decoding. `None` means "cannot tell" — never
/// a guess: the caller treats it as undecodable rather than allocating
/// blind.
pub fn probe_dimensions(bytes: &[u8], format: ::image::ImageFormat) -> Option<(u32, u32)> {
    match format {
        ::image::ImageFormat::Png => {
            // Signature, IHDR length+type, then width/height big-endian.
            if bytes.len() >= 24 && bytes[12..16] == [b'I', b'H', b'D', b'R'][..] {
                Some((
                    u32::from_be_bytes(bytes[16..20].try_into().ok()?),
                    u32::from_be_bytes(bytes[20..24].try_into().ok()?),
                ))
            } else {
                None
            }
        }
        ::image::ImageFormat::Gif => {
            if bytes.len() >= 10 {
                Some((
                    u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32,
                    u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32,
                ))
            } else {
                None
            }
        }
        ::image::ImageFormat::Bmp => {
            // BITMAPINFOHEADER and later (40+ byte headers): width i32 at
            // 18, height i32 at 22. Older core headers are refused rather
            // than misread.
            if bytes.len() >= 26 && bytes[14] >= 40 {
                let width = i32::from_le_bytes(bytes[18..22].try_into().ok()?).unsigned_abs();
                let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?).unsigned_abs();
                (width > 0 && height > 0).then_some((width, height))
            } else {
                None
            }
        }
        ::image::ImageFormat::Ico => {
            // ICONDIR entry: width/height bytes, where 0 means 256.
            if bytes.len() >= 14 {
                let width = if bytes[6] == 0 {
                    256
                } else {
                    u32::from(bytes[6])
                };
                let height = if bytes[7] == 0 {
                    256
                } else {
                    u32::from(bytes[7])
                };
                Some((width, height))
            } else {
                None
            }
        }
        ::image::ImageFormat::Qoi => {
            if bytes.len() >= 14 && &bytes[..4] == b"qoif" {
                Some((
                    u32::from_be_bytes(bytes[4..8].try_into().ok()?),
                    u32::from_be_bytes(bytes[8..12].try_into().ok()?),
                ))
            } else {
                None
            }
        }
        ::image::ImageFormat::WebP => probe_webp(bytes),
        ::image::ImageFormat::Jpeg => probe_jpeg(bytes),
        ::image::ImageFormat::Tiff => probe_tiff(bytes),
        _ => None,
    }
}

/// WebP dimensions from the container: VP8/VP8L headers or the VP8X canvas.
fn probe_webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 16 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8 " => {
            // Frame tag (3 bytes) then 9D 012A start code, then 14-bit
            // width-1 and height-1, little-endian.
            let header = bytes.get(16..26)?;
            if header[3] != 0x9D || header[4] != 0x01 || header[5] != 0x2A {
                return None;
            }
            let width = (u16::from_le_bytes([header[6], header[7]]) & 0x3FFF) as u32 + 1;
            let height = (u16::from_le_bytes([header[8], header[9]]) & 0x3FFF) as u32 + 1;
            Some((width, height))
        }
        b"VP8L" => {
            // Signature byte then 14-bit width-1 and height-1 packed LE.
            let bits = bytes.get(16..21)?;
            if bits[0] != 0x2F {
                return None;
            }
            let packed = u32::from_le_bytes([bits[1], bits[2], bits[3], bits[4]]) & 0x0FFF_FFFF;
            Some((((packed & 0x3FFF) + 1), (((packed >> 14) & 0x3FFF) + 1)))
        }
        b"VP8X" => {
            // Reserved + ICC/XMP/alpha/animation flags, then 24-bit
            // width-1 and height-1.
            let canvas = bytes.get(20..26)?;
            let width = u32::from_be_bytes([0, canvas[0], canvas[1], canvas[2]]) + 1;
            let height = u32::from_be_bytes([0, canvas[3], canvas[4], canvas[5]]) + 1;
            Some((width, height))
        }
        _ => None,
    }
}

/// JPEG dimensions by scanning markers for the first SOF, skipping
/// standalone markers. Stops at SOS: past compressed data lies only entropy.
fn probe_jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || &bytes[..2] != &[0xFF, 0xD8] {
        return None;
    }
    let mut index = 2;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xFF {
            return None;
        }
        let marker = bytes[index + 1];
        index += 2;
        // Standalone markers carry no length.
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        if index + 2 > bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;
        if length < 2 || index + length > bytes.len() {
            return None;
        }
        // SOF0–SOF3, SOF5–SOF7, SOF9–SOF11, SOF13–SOF15 (not DHT/DAC/SOS).
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            if length < 7 {
                return None;
            }
            let height = u16::from_be_bytes([bytes[index + 3], bytes[index + 4]]) as u32;
            let width = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
            return (width > 0 && height > 0).then_some((width, height));
        }
        if marker == 0xDA {
            return None;
        }
        index += length;
    }
    None
}

/// TIFF dimensions by walking the first IFD, either byte order.
///
/// Tags 256 (width) and 257 (height), SHORT or LONG. Only the first IFD is
/// read: multipage TIFFs preview their first page, like everything else
/// previews its head.
fn probe_tiff(bytes: &[u8]) -> Option<(u32, u32)> {
    let little = match bytes.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let read_u16 = |at: usize| -> Option<u16> {
        let pair: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        })
    };
    let read_u32 = |at: usize| -> Option<u32> {
        let quad: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(quad)
        } else {
            u32::from_be_bytes(quad)
        })
    };
    let first_ifd = read_u32(4)? as usize;
    let entries = read_u16(first_ifd.checked_add(0)?)? as usize;
    if entries > 256 {
        return None;
    }
    let mut width = None;
    let mut height = None;
    for slot in 0..entries {
        let at = first_ifd
            .checked_add(2)?
            .checked_add(slot.checked_mul(12)?)?;
        let tag = read_u16(at)?;
        let kind = read_u16(at + 2)?;
        let value = match kind {
            // SHORT: low two bytes in file order; LONG: the quad.
            3 => read_u16(at + 8).map(u32::from),
            4 => read_u32(at + 8),
            _ => None,
        };
        match tag {
            256 => width = value,
            257 => height = value,
            _ => {}
        }
    }
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
        _ => None,
    }
}

/// `ftyp` brands with no in-process decoder (HEIC, AVIF).
///
/// Mirrors the brand list in `classify::is_avif`, which is what routes these
/// files to `Kind::Image` in the first place: every brand recognised there
/// must be recognised here, or a valid photo falls through to `Undecodable`
/// and previews as hex. Found means "a real image we cannot rasterise", so
/// the caller reports metadata — never corruption.
fn has_unsupported_image_brand(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let brand = bytes.get(8..12).unwrap_or_default();
    matches!(
        brand,
        b"avif" | b"avis" | b"heic" | b"heix" | b"hevc" | b"hevx"
    )
}

fn decode_static(
    bytes: &[u8],
    format: ::image::ImageFormat,
    natural: (u32, u32),
    format_name: &'static str,
) -> Result<DecodedImage, ImageError> {
    // The probe's pixel cap is the real gate; these decoder limits are the
    // backstop for headers that lie about their dimensions.
    let mut decoder_limits = ::image::Limits::default();
    decoder_limits.max_image_width = Some(16_384);
    decoder_limits.max_image_height = Some(16_384);
    decoder_limits.max_alloc = Some(192 * 1024 * 1024);
    let mut reader = ::image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits);
    let image = reader.decode().map_err(|_| ImageError::Undecodable)?;
    let bitmap = downscale_to_bitmap(&image, natural);
    Ok(DecodedImage {
        natural,
        format_name,
        frames: vec![Frame {
            bitmap,
            delay_ms: 0,
        }],
        frames_capped: false,
    })
}

fn decode_animated(
    bytes: &[u8],
    format: ::image::ImageFormat,
    natural: (u32, u32),
    format_name: &'static str,
) -> Result<DecodedImage, ImageError> {
    let frames = collect_frames(bytes, format)?;
    if frames.len() <= 1 {
        let bitmap = frames
            .first()
            .map(|frame| downscale_frame(frame, natural))
            .unwrap_or_else(|| placeholder_bitmap());
        return Ok(DecodedImage {
            natural,
            format_name,
            frames: vec![Frame {
                bitmap,
                delay_ms: 0,
            }],
            frames_capped: false,
        });
    }
    let mut kept = Vec::new();
    let mut retained_bytes = 0usize;
    let mut capped = false;
    for frame in frames.into_iter().take(ANIMATION_FRAME_CAP + 1) {
        if kept.len() >= ANIMATION_FRAME_CAP {
            capped = true;
            break;
        }
        let bitmap = downscale_frame(&frame, natural);
        retained_bytes += bitmap.bgra.len();
        // Whichever limit comes first: count or memory.
        if retained_bytes > ANIMATION_MEMORY_CAP && !kept.is_empty() {
            capped = true;
            break;
        }
        kept.push(Frame {
            bitmap,
            delay_ms: frame_delay_ms(&frame),
        });
    }
    if kept.is_empty() {
        return Err(ImageError::Undecodable);
    }
    Ok(DecodedImage {
        natural,
        format_name,
        frames: kept,
        frames_capped: capped,
    })
}

fn collect_frames(
    bytes: &[u8],
    format: ::image::ImageFormat,
) -> Result<Vec<::image::Frame>, ImageError> {
    use ::image::AnimationDecoder as _;
    let cursor = Cursor::new(bytes);
    let frames: Result<Vec<::image::Frame>, _> = match format {
        ::image::ImageFormat::Gif => ::image::codecs::gif::GifDecoder::new(cursor)
            .map_err(|_| ImageError::Undecodable)?
            .into_frames()
            .collect(),
        ::image::ImageFormat::WebP => ::image::codecs::webp::WebPDecoder::new(cursor)
            .map_err(|_| ImageError::Undecodable)?
            .into_frames()
            .collect(),
        _ => return Err(ImageError::Undecodable),
    };
    frames.map_err(|_| ImageError::Undecodable)
}

/// GIF delays under 10 ms mean "as fast as possible" to encoders and
/// "spin the timer" to us: browsers clamp them to 100 ms, and so do we.
fn frame_delay_ms(frame: &::image::Frame) -> u32 {
    // `numer_denom_ms` already expresses the delay in milliseconds as a
    // ratio (50 ms arrives as 50000/1000): divide, do not rescale.
    let (numer, denom) = frame.delay().numer_denom_ms();
    let ms = if denom == 0 {
        100
    } else {
        u64::from(numer) / u64::from(denom)
    };
    if ms < 10 { 100 } else { ms.min(60_000) as u32 }
}

fn downscale_frame(frame: &::image::Frame, natural: (u32, u32)) -> Bitmap {
    let rgba = frame.buffer();
    let dynamic = ::image::DynamicImage::ImageRgba8(rgba.clone());
    downscale_to_bitmap(&dynamic, natural)
}

/// Downscale to the long-edge cap with a triangle filter, then BGRA.
///
/// Triangle at preview sizes: the low-cost baseline, visually compared
/// against Lanczos before anything heavier is considered. Already-small
/// images pass through untouched — no resample, no softening.
fn downscale_to_bitmap(image: &::image::DynamicImage, natural: (u32, u32)) -> Bitmap {
    let (width, height) = natural;
    let long_edge = width.max(height);
    let resized = if long_edge > IMAGE_TARGET_LONG_EDGE {
        let scale = IMAGE_TARGET_LONG_EDGE as f32 / long_edge as f32;
        let target_width = ((width as f32 * scale).round() as u32).max(1);
        let target_height = ((height as f32 * scale).round() as u32).max(1);
        image.resize(target_width, target_height, ::image::imageops::Triangle)
    } else {
        image.clone()
    };
    let rgba = resized.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let (bgra, has_transparency) = rgba_to_bgra(rgba.into_raw());
    Bitmap {
        width,
        height,
        bgra,
        has_transparency,
    }
}

fn rgba_to_bgra(mut bytes: Vec<u8>) -> (Vec<u8>, bool) {
    let mut transparent = false;
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        transparent |= pixel[3] != 255;
    }
    (bytes, transparent)
}

/// Whether any pixel of an already-BGRA buffer is non-opaque.
///
/// For bitmaps that arrive BGRA from elsewhere (the OS thumbnail service),
/// where no swizzle pass ran to notice. One linear scan, off the render
/// path, so the view knows when the checkerboard matters.
pub fn detect_transparency(bgra: &[u8]) -> bool {
    bgra.chunks_exact(4).any(|pixel| pixel[3] != 255)
}

/// Last-resort 1×1 so an animation with no usable frame still has a body.
/// Unreachable through `decode` (empty frame lists refuse first); kept so
/// the frame path below cannot produce a zero-frame image by construction.
fn placeholder_bitmap() -> Bitmap {
    Bitmap {
        width: 1,
        height: 1,
        bgra: vec![0, 0, 0, 0],
        has_transparency: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{ColorType, DynamicImage, GenericImageView, Rgba, RgbaImage};

    fn solid_rgba(width: u32, height: u32, pixel: Rgba<u8>) -> RgbaImage {
        RgbaImage::from_pixel(width, height, pixel)
    }

    fn encode_png(width: u32, height: u32, pixel: Rgba<u8>) -> Vec<u8> {
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(solid_rgba(width, height, pixel))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                ::image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn png_probes_decodes_and_orders_bgra() {
        let red = Rgba([255, 0, 0, 255]);
        let bytes = encode_png(4, 3, red);
        assert_eq!(
            probe_dimensions(&bytes, ::image::ImageFormat::Png),
            Some((4, 3))
        );
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.natural, (4, 3));
        assert_eq!(decoded.frames.len(), 1);
        let bitmap = &decoded.frames[0].bitmap;
        assert!(bitmap.is_well_formed());
        assert_eq!(&bitmap.bgra[..4], &[0, 0, 255, 255], "red arrives as BGRA");
    }

    #[test]
    fn jpeg_probes_and_decodes() {
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(::image::RgbImage::from_pixel(
            8,
            6,
            ::image::Rgb([10, 20, 30]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            ::image::ImageFormat::Jpeg,
        )
        .unwrap();
        assert_eq!(
            probe_dimensions(&bytes, ::image::ImageFormat::Jpeg),
            Some((8, 6))
        );
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.natural, (8, 6));
        assert!(decoded.frames[0].bitmap.is_well_formed());
    }

    #[test]
    fn gif_header_probes() {
        let header = b"GIF89a"
            .iter()
            .chain([2, 0, 3, 0].iter())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            probe_dimensions(&header, ::image::ImageFormat::Gif),
            Some((2, 3))
        );
    }

    #[test]
    fn webp_lossy_header_probes() {
        // RIFF....WEBPVP8_ + frame tag + 9D012A + 14-bit dims minus one.
        let mut bytes = b"RIFF\x1A\0\0\0WEBPVP8 ".to_vec();
        bytes.extend_from_slice(&[0x30, 0x01, 0x00, 0x9D, 0x01, 0x2A, 0x09, 0x00, 0x06, 0x00]);
        assert_eq!(
            probe_dimensions(&bytes, ::image::ImageFormat::WebP),
            Some((10, 7))
        );
    }

    #[test]
    fn webp_lossless_header_probes() {
        let mut bytes = b"RIFF\x16\0\0\0WEBPVP8L".to_vec();
        // Signature 0x2F then 28-bit packed (width-1 | (height-1) << 14).
        let packed: u32 = 9 | (6 << 14);
        bytes.push(0x2F);
        bytes.extend_from_slice(&packed.to_le_bytes());
        assert_eq!(
            probe_dimensions(&bytes, ::image::ImageFormat::WebP),
            Some((10, 7))
        );
    }

    #[test]
    fn animated_gif_decodes_frames_with_delays() {
        use ::image::codecs::gif::GifEncoder;
        use ::image::{Delay, Frame};

        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            let red = Frame::from_parts(
                solid_rgba(2, 2, Rgba([255, 0, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(50, 1),
            );
            // Zero delay means "as fast as possible" to encoders and must
            // not spin our timer: it clamps to 100 ms.
            let blue = Frame::from_parts(
                solid_rgba(2, 2, Rgba([0, 0, 255, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(0, 1),
            );
            encoder.encode_frame(red).unwrap();
            encoder.encode_frame(blue).unwrap();
        }
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.frames.len(), 2);
        assert_eq!(decoded.frames[0].delay_ms, 50);
        assert_eq!(decoded.frames[1].delay_ms, 100);
        assert!(!decoded.frames_capped);
    }

    #[test]
    fn a_lying_png_header_refuses_before_allocating() {
        // A 20000x20000 claim is 1.6 GB of RGBA. The probe must refuse it
        // from the header alone — this test completing at all is the proof
        // no allocation happened.
        let mut bytes = encode_png(2, 2, Rgba([0, 0, 0, 255]));
        bytes[16..20].copy_from_slice(&20000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&20000u32.to_be_bytes());
        assert_eq!(
            decode(&bytes).unwrap_err(),
            ImageError::TooLarge {
                width: 20000,
                height: 20000
            }
        );
    }

    #[test]
    fn truncated_bytes_are_undecodable_not_a_panic() {
        let bytes = encode_png(4, 4, Rgba([1, 2, 3, 255]));
        assert_eq!(decode(&bytes[..20]).unwrap_err(), ImageError::Undecodable);
    }

    #[test]
    fn long_edges_downscale_to_the_cap() {
        let bytes = encode_png(5000, 100, Rgba([9, 9, 9, 255]));
        let decoded = decode(&bytes).unwrap();
        // Natural size is reported honestly; the bitmap is bounded.
        assert_eq!(decoded.natural, (5000, 100));
        let bitmap = &decoded.frames[0].bitmap;
        assert_eq!(bitmap.width, super::super::config::IMAGE_TARGET_LONG_EDGE);
        assert!(bitmap.is_well_formed());
    }

    #[test]
    fn small_images_pass_through_unresampled() {
        let bytes = encode_png(3, 2, Rgba([7, 8, 9, 255]));
        let bitmap = &decode(&bytes).unwrap().frames[0].bitmap;
        assert_eq!((bitmap.width, bitmap.height), (3, 2));
    }

    #[test]
    fn tiff_little_endian_probes() {
        // II*\0, IFD at 8, one entry: tag 256 (SHORT) = 640, then 257 = 480.
        // Only the header and first IFD matter — no decode happens here.
        let mut bytes = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 2, 0];
        bytes.extend_from_slice(&[0, 1, 3, 0, 1, 0, 0, 0, 0x80, 0x02, 0, 0]);
        bytes.extend_from_slice(&[1, 1, 3, 0, 1, 0, 0, 0, 0xE0, 0x01, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            probe_dimensions(&bytes, ::image::ImageFormat::Tiff),
            Some((640, 480))
        );
    }

    #[test]
    fn transparency_is_detected_per_pixel() {
        assert!(!detect_transparency(&[0, 0, 0, 255, 255, 255, 255, 255]));
        assert!(detect_transparency(&[0, 0, 0, 255, 0, 0, 0, 0]));
        assert!(!detect_transparency(&[]));
    }

    #[test]
    fn foreign_formats_are_undecodable() {
        assert_eq!(
            decode(b"%PDF-1.7....").unwrap_err(),
            ImageError::Undecodable
        );
        assert_eq!(
            decode(b"not an image at all").unwrap_err(),
            ImageError::Undecodable
        );
    }

    #[test]
    fn heic_and_avif_brands_are_unsupported_not_corrupt() {
        // A real iPhone photo must report metadata, never a hex dump: the
        // brand check runs before `guess_format`, which cannot name these.
        for brand in [b"heic", b"heix", b"hevc", b"hevx", b"avif", b"avis"] {
            let mut bytes = b"....ftyp".to_vec();
            bytes.extend_from_slice(brand);
            bytes.extend_from_slice(&[0u8; 64]);
            assert_eq!(
                decode(&bytes).unwrap_err(),
                ImageError::UnsupportedFormat,
                "brand {brand:?} must be unsupported, not undecodable",
            );
        }
    }

    #[test]
    fn ico_decodes_with_its_feature_enabled() {
        // The probe, the decode arm and this test agree: without the `ico`
        // decoder feature this would fall to `Undecodable`.
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(solid_rgba(4, 4, Rgba([1, 2, 3, 255])))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                ::image::ImageFormat::Ico,
            )
            .unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.format_name, "ICO");
        assert!(decoded.frames[0].bitmap.is_well_formed());
    }

    #[test]
    fn dimensions_survive_a_viewport() {
        // The pane downsamples from the probed size, not the decoded one:
        // both must agree on ordinary files.
        let bytes = encode_png(16, 9, Rgba([1, 1, 1, 255]));
        let (w, h) = probe_dimensions(&bytes, ::image::ImageFormat::Png).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!((w, h), decoded.natural);
        let _ = (ColorType::Rgba8, DynamicImage::new_rgba8(1, 1).dimensions());
    }
}
