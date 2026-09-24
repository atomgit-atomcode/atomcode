//! Downscale + re-encode oversized user images before they enter the conversation.
//!
//! A large pasted/read screenshot is kept in the message history and re-sent on EVERY
//! turn (vision models don't get the VL-caption-and-strip treatment). On a strict or
//! size-limited gateway that manifested as a repeating "网络连接失败" / connection reset
//! that no re-login / restart / compaction could clear — the huge image poisoned the
//! session. Every comparable agent (opencode/codex/oh-my-pi) caps the longest edge and
//! targets a modest byte budget before sending; atomcode was the outlier that shipped
//! the raw bytes.
//!
//! Cap the longest edge at 1568px — Anthropic downscales images to 1568px internally
//! before vision processing, so this loses no detail the model would have used — and
//! target a ~1.5 MB raw budget (≈2 MB on the wire after base64). Prefer the original
//! quality (PNG, lossless — best for text/screenshots) and only step down to JPEG when
//! the budget demands it. NEVER panics or drops the image: any decode/encode failure
//! returns the input untouched (fail-open — a slightly-large image beats losing it).
//!
//! Note: an oversized *animated* GIF/WebP is flattened to its first frame on re-encode.
//! That's acceptable — a vision model only sees one frame anyway.

use base64::Engine;
use image::ImageEncoder;

/// Longest-edge cap in pixels. Matches Anthropic's internal vision downscale target;
/// other agents use 1568–2048. Resizing to this is lossless for the model.
const MAX_EDGE: u32 = 1568;

/// Re-encode target for the RAW (pre-base64) bytes. base64 inflates ~33%, so the
/// on-wire payload stays ~2 MB. Images already under this are left untouched.
const TARGET_BYTES: usize = 1_500_000;

/// Absolute pixel-count guard against decompression bombs: refuse to process an image
/// whose declared dimensions exceed this (checked from the header BEFORE decoding, so
/// we never allocate the monster in the first place).
const MAX_PIXELS: u64 = 50_000_000; // 50 MP

/// Hard per-edge ceiling handed to the decoder's `Limits` (rejects at the header before
/// pixel allocation) — a second guard alongside `MAX_PIXELS`.
const MAX_EDGE_DECODE: u32 = 20_000;

/// Decode allocation ceiling handed to the decoder's `Limits`. 50 MP × 4 bytes (RGBA) is
/// ~200 MB; leave headroom. A stream that would allocate past this fails as `Err`.
const MAX_DECODE_ALLOC: u64 = 300 * 1024 * 1024;

/// Normalize one base64 image. Returns `(media_type, base64)` — resized/re-encoded when
/// oversized, otherwise the input verbatim.
pub fn normalize_image_base64(media_type: &str, data_b64: &str) -> (String, String) {
    let engine = base64::engine::general_purpose::STANDARD;
    let Ok(raw) = engine.decode(data_b64.as_bytes()) else {
        return (media_type.to_string(), data_b64.to_string());
    };
    match normalize_image_raw(&raw) {
        Some((mt, out)) => (mt, engine.encode(out)),
        None => (media_type.to_string(), data_b64.to_string()),
    }
}

/// Normalize raw image bytes. `Some((media_type, bytes))` when the image was resized /
/// re-encoded (the returned media_type reflects the new encoding — png↔jpeg); `None`
/// when it is already within budget or could not be processed, in which case the caller
/// keeps the original bytes and media_type.
pub fn normalize_image_raw(raw: &[u8]) -> Option<(String, Vec<u8>)> {
    // Cheap header-only dimension probe (no pixel decode). Skips the common
    // small-AND-modest-sized image without decoding, while still catching the
    // compressible-but-huge-dimension case (a small-byte file with a 15000px edge) that
    // a byte-size gate alone would miss — providers reject an edge past ~8000px.
    let format = image::ImageReader::new(std::io::Cursor::new(raw))
        .with_guessed_format()
        .ok()?
        .format();
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(raw))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if raw.len() <= TARGET_BYTES && w.max(h) <= MAX_EDGE {
        return None; // already within both the byte budget and the edge cap
    }
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None; // absurd dimensions — don't spend memory decoding/resizing it
    }
    // Decode with an explicit allocation/dimension ceiling. We deliberately do NOT rely
    // on `catch_unwind`: the release profile is `panic = "abort"`, so it would be a
    // no-op. The header probe + `Limits` bound the work before any large allocation, and
    // a malformed stream returns `Err` → `None` (fail open, keep the original).
    let mut reader = image::ImageReader::new(std::io::Cursor::new(raw));
    if let Some(f) = format {
        reader.set_format(f);
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE_DECODE);
    limits.max_image_height = Some(MAX_EDGE_DECODE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let img = if decoded.width().max(decoded.height()) > MAX_EDGE {
        decoded.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3)
    } else {
        decoded
    };

    // Prefer PNG (lossless — keeps small text/lines crisp). If PNG fits, done. Otherwise
    // walk a short JPEG quality ladder and take the first that fits, else the smallest
    // encoding we produced (still far below the raw original).
    let mut best: Option<(String, Vec<u8>)> = None;
    if let Some(png) = encode_png(&img) {
        if png.len() <= TARGET_BYTES {
            return Some(("image/png".to_string(), png));
        }
        best = Some(("image/png".to_string(), png));
    }
    for q in [85u8, 70, 55, 40] {
        if let Some(jpg) = encode_jpeg(&img, q) {
            if jpg.len() <= TARGET_BYTES {
                return Some(("image/jpeg".to_string(), jpg));
            }
            let smaller = best
                .as_ref()
                .map(|(_, b)| jpg.len() < b.len())
                .unwrap_or(true);
            if smaller {
                best = Some(("image/jpeg".to_string(), jpg));
            }
        }
    }
    // Only report a change if we actually shrank it below the original.
    match best {
        Some((mt, bytes)) if bytes.len() < raw.len() => Some((mt, bytes)),
        _ => None,
    }
}

fn encode_png(img: &image::DynamicImage) -> Option<Vec<u8>> {
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Option<Vec<u8>> {
    // JPEG has no alpha — flatten to RGB.
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(out)
}

/// Decode a packed Windows `CF_DIB` clipboard payload into `(width, height,
/// RGBA8)`.
///
/// A `CF_DIB` is a `BITMAPINFOHEADER`-family header, optional bitfield masks
/// and palette, then the pixels — a BMP file without its 14-byte file header.
/// It is wrapped in a synthesized one here and decoded through the BMP *file*
/// path, so the explicit pixel offset (`bfOffBits`) is computed from the header
/// rather than guessed. Guessing is the bug this exists for: `arboard`'s
/// header-less decode places the pixels wrongly for V4/V5 headers with
/// `BI_BITFIELDS` compression and rejects the image — and that is exactly what
/// the Windows Snipping Tool (`Win+Shift+S`) and Qt-based screenshot tools put
/// on the clipboard, so a screenshot read as "no image".
///
/// Target-independent on purpose: the decoder is judged on every host, and only
/// the reading of the clipboard is Windows-only (it lives with the screen).
pub fn dib_to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    const FILE_HEADER_SIZE: u64 = 14;
    const INFO_HEADER_SIZE: u64 = 40;
    const BI_BITFIELDS: u32 = 3;

    if dib.len() < INFO_HEADER_SIZE as usize {
        return None;
    }
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?))
    };
    let header_size = u64::from(u32_at(0)?);
    if header_size < INFO_HEADER_SIZE || header_size > dib.len() as u64 {
        return None;
    }
    let bit_count = u16::from_le_bytes([*dib.get(14)?, *dib.get(15)?]);
    let compression = u32_at(16)?;
    let colors_used = u64::from(u32_at(32)?);

    // A plain BITMAPINFOHEADER with BI_BITFIELDS is followed by three DWORD
    // masks; the larger (V2..V5) headers carry the masks inside themselves.
    let mask_bytes: u64 = if header_size == INFO_HEADER_SIZE && compression == BI_BITFIELDS {
        12
    } else {
        0
    };
    let palette_entries: u64 = if colors_used != 0 {
        colors_used
    } else if bit_count <= 8 {
        1u64 << bit_count
    } else {
        0
    };
    let pixel_offset =
        u32::try_from(FILE_HEADER_SIZE + header_size + mask_bytes + palette_entries * 4).ok()?;
    let file_size = u32::try_from(FILE_HEADER_SIZE + dib.len() as u64).ok()?;

    let mut bmp = Vec::with_capacity(FILE_HEADER_SIZE as usize + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&pixel_offset.to_le_bytes());
    bmp.extend_from_slice(dib);

    let decoded = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    let rgba = decoded.into_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}

#[cfg(test)]
mod dib_tests {
    //! The exact DIB shapes screenshot tools put on the Windows clipboard — the
    //! ones `arboard`'s header-less decode rejects.

    use super::dib_to_rgba;

    fn push32(v: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn push16(v: u16, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// 2x2 bottom-up BGRA: memory rows are [red, green] (bottom) then
    /// [blue, white] (top), all opaque.
    const PIXELS_2X2: [u8; 16] = [
        0x00, 0x00, 0xff, 0xff, // red
        0x00, 0xff, 0x00, 0xff, // green
        0xff, 0x00, 0x00, 0xff, // blue
        0xff, 0xff, 0xff, 0xff, // white
    ];

    /// What Qt writes for 32-bit content: a plain `BITMAPINFOHEADER` with
    /// `BI_BITFIELDS` and three trailing DWORD masks.
    fn qt_cf_dib(width: u32, height: u32, pixels_bgra: &[u8], compression: u32) -> Vec<u8> {
        let mut d = Vec::with_capacity(52 + pixels_bgra.len());
        push32(40, &mut d);
        push32(width, &mut d);
        push32(height, &mut d);
        push16(1, &mut d);
        push16(32, &mut d);
        push32(compression, &mut d);
        push32(pixels_bgra.len() as u32, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        if compression == 3 {
            push32(0x00ff_0000, &mut d);
            push32(0x0000_ff00, &mut d);
            push32(0x0000_00ff, &mut d);
        }
        d.extend_from_slice(pixels_bgra);
        d
    }

    /// A `CF_DIBV5` as the Snipping Tool and Qt tools leave it: a 124-byte
    /// `BITMAPV5HEADER`, `BI_BITFIELDS`, masks inside the header.
    fn dibv5(width: u32, height: u32, pixels_bgra: &[u8]) -> Vec<u8> {
        let mut d = Vec::with_capacity(124 + pixels_bgra.len());
        push32(124, &mut d);
        push32(width, &mut d);
        push32(height, &mut d);
        push16(1, &mut d);
        push16(32, &mut d);
        push32(3, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0x00ff_0000, &mut d);
        push32(0x0000_ff00, &mut d);
        push32(0x0000_00ff, &mut d);
        push32(0xff00_0000, &mut d);
        push32(0x7352_4742, &mut d); // LCS_sRGB
        d.extend_from_slice(&[0u8; 36]);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(4, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        push32(0, &mut d);
        assert_eq!(d.len(), 124);
        d.extend_from_slice(pixels_bgra);
        d
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    fn pixels(dib: &[u8]) -> (u32, u32, Vec<[u8; 4]>) {
        let (w, h, rgba) = dib_to_rgba(dib).expect("the DIB decodes");
        let px = rgba
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
        (w, h, px)
    }

    #[test]
    fn a_dibv5_from_a_screenshot_tool_decodes() {
        let (w, h, px) = pixels(&dibv5(2, 2, &PIXELS_2X2));
        assert_eq!((w, h), (2, 2));
        // Top row first (the array is bottom-up), BGRA → RGBA.
        assert_eq!(px, vec![BLUE, WHITE, RED, GREEN]);
    }

    #[test]
    fn a_qt_dib_with_trailing_masks_decodes() {
        let (w, h, px) = pixels(&qt_cf_dib(2, 2, &PIXELS_2X2, 3));
        assert_eq!((w, h), (2, 2));
        assert_eq!(px, vec![BLUE, WHITE, RED, GREEN]);
    }

    #[test]
    fn a_plain_bi_rgb_dib_decodes_opaque() {
        // The fourth byte is unused in BI_RGB; zero it to show it is not read
        // as transparency.
        let mut pixels_bgra = PIXELS_2X2;
        for alpha in pixels_bgra.iter_mut().skip(3).step_by(4) {
            *alpha = 0;
        }
        let (w, h, px) = pixels(&qt_cf_dib(2, 2, &pixels_bgra, 0));
        assert_eq!((w, h), (2, 2));
        assert_eq!(px, vec![BLUE, WHITE, RED, GREEN]);
    }

    #[test]
    fn a_malformed_dib_is_refused_not_panicked_on() {
        assert!(dib_to_rgba(&[0u8; 12]).is_none(), "too short");
        let mut oversized = qt_cf_dib(2, 2, &PIXELS_2X2, 3);
        oversized[0..4].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        assert!(
            dib_to_rgba(&oversized).is_none(),
            "header beyond the buffer"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw PNG of `w`×`h` with noise so it doesn't compress to nothing.
    fn big_png(w: u32, h: u32) -> Vec<u8> {
        let mut buf = image::RgbaImage::new(w, h);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for p in buf.pixels_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let b = seed.to_le_bytes();
            *p = image::Rgba([b[0], b[1], b[2], 255]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn small_image_is_left_untouched() {
        let raw = big_png(64, 64);
        assert!(raw.len() <= TARGET_BYTES);
        assert!(normalize_image_raw(&raw).is_none(), "under budget → no-op");
    }

    #[test]
    fn oversized_dimensions_are_capped_and_shrunk() {
        // 4000×3000 noise PNG → well over 1.5 MB → must resize to <=1568px and shrink.
        let raw = big_png(4000, 3000);
        assert!(
            raw.len() > TARGET_BYTES,
            "fixture must exceed budget: {}",
            raw.len()
        );
        let (mt, out) = normalize_image_raw(&raw).expect("must normalize");
        assert!(
            out.len() < raw.len(),
            "must shrink: {} -> {}",
            raw.len(),
            out.len()
        );
        // Decode the result and check the longest edge is capped.
        let d = image::load_from_memory(&out).unwrap();
        assert!(
            d.width().max(d.height()) <= MAX_EDGE,
            "edge {} > {MAX_EDGE}",
            d.width().max(d.height())
        );
        assert!(mt == "image/png" || mt == "image/jpeg");
    }

    #[test]
    fn compressible_huge_dimension_small_byte_image_is_still_capped() {
        // A 6000×6000 solid-color PNG compresses to well under the byte budget, so a
        // byte-only gate would pass it through at 6000px — past the ~8000px provider
        // limit territory. The header dimension probe must still trigger a resize.
        let mut buf = image::RgbaImage::new(6000, 6000);
        for p in buf.pixels_mut() {
            *p = image::Rgba([10, 20, 30, 255]);
        }
        let mut raw = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut std::io::Cursor::new(&mut raw), image::ImageFormat::Png)
            .unwrap();
        assert!(
            raw.len() <= TARGET_BYTES,
            "fixture must be under byte budget: {}",
            raw.len()
        );
        let (_mt, out) = normalize_image_raw(&raw).expect("huge dimensions must normalize");
        let d = image::load_from_memory(&out).unwrap();
        assert!(
            d.width().max(d.height()) <= MAX_EDGE,
            "edge not capped: {}",
            d.width().max(d.height())
        );
    }

    #[test]
    fn malformed_bytes_fail_open() {
        let junk = vec![0u8; TARGET_BYTES + 1]; // over budget but not a decodable image
        assert!(
            normalize_image_raw(&junk).is_none(),
            "undecodable → keep original"
        );
    }

    #[test]
    fn base64_wrapper_roundtrips_and_shrinks() {
        let raw = big_png(3000, 2400);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let (mt, out_b64) = normalize_image_base64("image/png", &b64);
        assert!(out_b64.len() < b64.len(), "wire payload must shrink");
        assert!(mt == "image/png" || mt == "image/jpeg");
    }
}
