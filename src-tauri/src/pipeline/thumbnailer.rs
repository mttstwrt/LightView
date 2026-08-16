//! Decode, resize, encode: the CPU thumbnail path.
//!
//! Thumbnail generation is **decode-bound**, not resize- or encode-bound —
//! roughly 80% of the time for a 4000×3000 JPEG is the decode, and the resize
//! filter choice moves the total by under a millisecond. Every optimization
//! here is therefore about decoding fewer pixels, not about resizing faster:
//!
//! * JPEG goes through `jpeg-decoder` specifically for its DCT-scaled decode,
//!   which produces a 1/2, 1/4, or 1/8 image directly from the entropy stream.
//!   A faster per-pixel decoder that lacks scaling is a net *loss* on camera
//!   JPEGs, because it would decode sixteen times the pixels.
//! * HEIC prefers an embedded thumbnail handle over decoding the full image.
//! * Micro is derived from cached Standard bytes rather than from the original;
//!   the derivation is in `commands::media`, but this is where the primitives
//!   for it live.
//!
//! `docs/pipeline/jpeg-decode.md` has the measurements and the options that
//! were rejected.
//!
//! Source files are memory-mapped rather than read into a buffer, so a decoder
//! that only touches part of the stream only faults in that part.

use fast_image_resize as fir;
use fir::images::{Image, ImageRef};
use image::GenericImageView;
use memmap2::Mmap;
use crate::companion::schema::MediaType;
use super::video;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Standard tier thumbnail size (512px square).
pub const STANDARD_THUMB_SIZE: u32 = 512;

/// Output format for generated thumbnails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThumbFormat {
    /// JPEG encoded (compressed, 10–50 KB per thumbnail).
    #[default]
    Jpeg,
    /// Lossy WebP — ~30% smaller than JPEG at equivalent visual quality.
    /// Preferred for the L/P tiers where file size matters more than
    /// decode speed. Browsers have native WebP support.
    Webp,
}

impl ThumbFormat {
    /// Cache-row `format` column value. Must match the strings written
    /// into the `thumbnails*` tables so that lookup comparisons work.
    /// Rows written by old builds may carry format='rgba'; those never
    /// match a requested format, which triggers regeneration.
    pub fn as_cache_str(self) -> &'static str {
        match self {
            ThumbFormat::Jpeg => "jpeg",
            ThumbFormat::Webp => "webp",
        }
    }
}

/// Resize algorithm selection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResizeFilter {
    #[default]
    Nearest,
    Bilinear,
    Lanczos3,
}

impl ResizeFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            ResizeFilter::Nearest => "nearest",
            ResizeFilter::Bilinear => "bilinear",
            ResizeFilter::Lanczos3 => "lanczos3",
        }
    }

    fn to_fir_alg(self) -> fir::ResizeAlg {
        match self {
            ResizeFilter::Nearest => fir::ResizeAlg::Nearest,
            ResizeFilter::Bilinear => fir::ResizeAlg::Convolution(fir::FilterType::Bilinear),
            ResizeFilter::Lanczos3 => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        }
    }
}

/// Pick the best resize algorithm for a given target size.
/// Small thumbnails (micro/standard) use fast Bilinear — the quality
/// difference from Lanczos3 is imperceptible at <= 512px.
/// Large/Preview tiers use Lanczos3 for maximum sharpness.
pub fn filter_for_size(target: u32) -> ResizeFilter {
    if target <= STANDARD_THUMB_SIZE {
        ResizeFilter::Bilinear
    } else {
        ResizeFilter::Lanczos3
    }
}

/// Result of generating a single thumbnail.
#[derive(Debug)]
pub struct ThumbResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub media_type: String,
    /// Source image dimensions (before resize)
    pub src_width: u32,
    pub src_height: u32,
    /// Output format of `data`.
    pub format: ThumbFormat,
}

/// Error during thumbnail generation.
#[derive(Debug, thiserror::Error)]
pub enum ThumbError {
    #[error("Image decode error: {0}")]
    Decode(String),
    #[error("Encode error: {0}")]
    Encode(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Memory-map a file for zero-copy reads.
fn mmap_file(path: &Path) -> Result<Mmap, ThumbError> {
    let file = std::fs::File::open(path)?;
    // SAFETY: The file is read-only and we hold it open for the duration of the mmap.
    // The mmap is consumed before the caller returns, so no dangling references.
    unsafe { Mmap::map(&file).map_err(ThumbError::Io) }
}

/// Generate a thumbnail for a JPEG file using DCT-scaled decoding.
/// Decodes at reduced resolution (1/2, 1/4, or 1/8) then resizes to final size.
/// This is dramatically faster than full decode + resize for large images.
/// Falls back to the generic (image crate) decoder on any failure.
fn generate_jpeg_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    match generate_jpeg_thumbnail_inner(path, filter, format, thumb_w, thumb_h) {
        Ok(result) => Ok(result),
        Err(e) => {
            log::debug!(
                "JPEG DCT decode failed for {}, falling back to generic decoder: {}",
                path.display(),
                e
            );
            generate_generic_thumbnail(path, filter, format, thumb_w, thumb_h)
        }
    }
}

fn generate_jpeg_thumbnail_inner(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let mmap = mmap_file(path)?;
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&mmap[..]));

    decoder
        .read_info()
        .map_err(|e| ThumbError::Decode(e.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| ThumbError::Decode("No JPEG info".to_string()))?;
    let src_width = info.width as u32;
    let src_height = info.height as u32;

    if src_width == 0 || src_height == 0 {
        return Err(ThumbError::Decode("Zero dimension image".to_string()));
    }

    // Ask the decoder to scale down during DCT — it picks the best factor
    // (1/1, 1/2, 1/4, 1/8) that produces at least thumb size in both axes
    let target = thumb_w.max(thumb_h) as u16;
    let _scaled = decoder.scale(target, target);

    let pixels = decoder
        .decode()
        .map_err(|e| ThumbError::Decode(e.to_string()))?;
    let decoded_info = decoder
        .info()
        .ok_or_else(|| ThumbError::Decode("No JPEG info after decode".to_string()))?;
    let dw = decoded_info.width as u32;
    let dh = decoded_info.height as u32;

    if dw == 0 || dh == 0 {
        return Err(ThumbError::Decode("Zero decoded dimensions".to_string()));
    }

    // jpeg-decoder returns whatever the file's colour model is — L8, RGB24,
    // or CMYK32 — so every branch has to be handled, not just the common one.
    let rgb_buf = match decoded_info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let expected = (dw as usize) * (dh as usize) * 3;
            if pixels.len() < expected {
                return Err(ThumbError::Decode(format!(
                    "Pixel buffer too small: {} < {} ({}x{})",
                    pixels.len(), expected, dw, dh
                )));
            }
            pixels
        }
        jpeg_decoder::PixelFormat::L8 => {
            let expected = (dw as usize) * (dh as usize);
            if pixels.len() < expected {
                return Err(ThumbError::Decode(format!(
                    "L8 buffer too small: {} < {}",
                    pixels.len(), expected
                )));
            }
            let mut rgb = Vec::with_capacity(expected * 3);
            for &v in &pixels[..expected] {
                rgb.push(v);
                rgb.push(v);
                rgb.push(v);
            }
            rgb
        }
        _ => {
            // Fall back to image crate for CMYK/L16/etc
            return Err(ThumbError::Decode("Unsupported pixel format".to_string()));
        }
    };

    // Center-crop to square, then resize to target dimensions.
    let data = crop_resize_encode(
        dw, dh, SrcPixels::Rgb(&rgb_buf), thumb_w, thumb_h, format, filter,
    )?;

    Ok(ThumbResult {
        path: path.to_string_lossy().to_string(),
        width: thumb_w,
        height: thumb_h,
        data,
        media_type: "image".to_string(),
        src_width,
        src_height,
        format,
    })
}

/// Generate a thumbnail for non-JPEG formats using the image crate.
fn generate_generic_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let mmap = mmap_file(path)?;
    let img = image::load_from_memory(&mmap).map_err(|e| ThumbError::Decode(format!("{}: {}", path.display(), e)))?;

    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(ThumbError::Decode(format!("Zero dimensions: {}x{}", w, h)));
    }

    // The `image` crate can hand back either layout, so ask it for the one the
    // output format wants rather than converting a second time downstream.
    let data = match format {
        ThumbFormat::Jpeg => {
            let rgb = img.to_rgb8();
            crop_resize_encode(w, h, SrcPixels::Rgb(rgb.as_raw()), thumb_w, thumb_h, format, filter)?
        }
        ThumbFormat::Webp => {
            let rgba = img.to_rgba8();
            crop_resize_encode(w, h, SrcPixels::Rgba(rgba.as_raw()), thumb_w, thumb_h, format, filter)?
        }
    };

    Ok(ThumbResult {
        path: path.to_string_lossy().to_string(),
        width: thumb_w,
        height: thumb_h,
        data,
        media_type: "image".to_string(),
        src_width: w,
        src_height: h,
        format,
    })
}

/// Generate a thumbnail for a HEIC/HEIF/AVIF file using libheif.
///
/// Tries the embedded thumbnail first (iPhone HEICs ship a ~320px thumb that
/// decodes ~100x faster than the full image) and falls back to the primary
/// handle if no embedded thumbnail is large enough.
fn generate_heic_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let target_edge = thumb_w.max(thumb_h);
    let (rgba_buf, dw, dh, src_w, src_h) = decode_heic_to_rgba(path, target_edge)?;

    let data = crop_resize_encode(
        dw, dh, SrcPixels::Rgba(&rgba_buf), thumb_w, thumb_h, format, filter,
    )?;

    Ok(ThumbResult {
        path: path.to_string_lossy().to_string(),
        width: thumb_w,
        height: thumb_h,
        data,
        media_type: "image".to_string(),
        src_width: src_w,
        src_height: src_h,
        format,
    })
}

/// A center-crop rectangle: offset plus a square side length.
#[derive(Clone, Copy)]
struct Crop {
    x: u32,
    y: u32,
    side: u32,
}

/// Resize `src` to `(tw, th)`, optionally cropping to `crop` first.
///
/// The crop is applied *inside* fast_image_resize, so the cropped intermediate
/// buffer (a full-region alloc plus a row-by-row memcpy) is never materialized
/// — this is the hot path on every generated thumbnail. `pixel` selects the
/// layout: `U8x3` for RGB (the JPEG path), `U8x4` for RGBA (WebP).
fn resize_pixels(
    sw: u32, sh: u32, src: &[u8],
    tw: u32, th: u32,
    pixel: fir::PixelType,
    crop: Option<Crop>,
    filter: ResizeFilter,
) -> Result<Vec<u8>, ThumbError> {
    let src_image = ImageRef::new(sw, sh, src, pixel)
        .map_err(|e| ThumbError::Decode(format!("Source image error: {e}")))?;
    let mut dst_image = Image::new(tw, th, pixel);
    let mut options = fir::ResizeOptions::new().resize_alg(filter.to_fir_alg());
    if let Some(c) = crop {
        options = options.crop(c.x as f64, c.y as f64, c.side as f64, c.side as f64);
    }
    let mut resizer = fir::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|e| ThumbError::Encode(format!("Resize failed: {e}")))?;
    Ok(dst_image.into_vec())
}

/// Resize RGBA (U8x4) pixels to target dimensions, no crop.
fn resize_rgba(sw: u32, sh: u32, src: &[u8], tw: u32, th: u32, filter: ResizeFilter) -> Result<Vec<u8>, ThumbError> {
    if sw == tw && sh == th {
        return Ok(src.to_vec());
    }
    resize_pixels(sw, sh, src, tw, th, fir::PixelType::U8x4, None, filter)
}

/// Decoded source pixels, tagged with the channel layout the decoder produced.
enum SrcPixels<'a> {
    Rgb(&'a [u8]),
    Rgba(&'a [u8]),
}

/// Center-crop `(sw, sh)` to a square, resize it to `(tw, th)`, and encode it
/// in `format`.
///
/// This is the tail every generator shares. JPEG output wants RGB and WebP
/// wants RGBA, while each decoder produces whichever layout is natural for its
/// codec — so the one conversion that's actually needed happens here, and the
/// matching case costs nothing.
fn crop_resize_encode(
    sw: u32, sh: u32,
    src: SrcPixels<'_>,
    tw: u32, th: u32,
    format: ThumbFormat,
    filter: ResizeFilter,
) -> Result<Vec<u8>, ThumbError> {
    use fir::PixelType::{U8x3, U8x4};
    let crop = Some(center_crop_square(sw, sh));
    let resize = |pixels: &[u8], pixel| resize_pixels(sw, sh, pixels, tw, th, pixel, crop, filter);
    let resized = match (format, src) {
        (ThumbFormat::Jpeg, SrcPixels::Rgb(p)) => resize(p, U8x3)?,
        (ThumbFormat::Jpeg, SrcPixels::Rgba(p)) => resize(&rgba_to_rgb(p), U8x3)?,
        (ThumbFormat::Webp, SrcPixels::Rgb(p)) => resize(&rgb_to_rgba(p), U8x4)?,
        (ThumbFormat::Webp, SrcPixels::Rgba(p)) => resize(p, U8x4)?,
    };
    encode_output(&resized, tw, th, format)
}

/// Convert RGB pixel buffer to RGBA (alpha = 255). Writes into a pre-sized
/// buffer with fixed 4-byte destination chunks; the fixed stride lets the
/// compiler vectorize the copy, ~3x faster than `push`-per-byte.
pub fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let pixel_count = rgb.len() / 3;
    let mut rgba = vec![0u8; pixel_count * 4];
    for (src, dst) in rgb.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        dst[3] = 255;
    }
    rgba
}

/// Strip alpha from an RGBA buffer to produce RGB. Uses 3-byte
/// `extend_from_slice` per pixel — the compiler emits a memcpy intrinsic
/// for that, which is meaningfully faster than `push`-per-byte.
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let pixel_count = rgba.len() / 4;
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for chunk in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    rgb
}

/// Convert L8 (grayscale) pixel buffer directly to RGBA (alpha = 255). Uses a
/// pre-sized buffer with fixed 4-byte destination chunks so the compiler can
/// vectorize the broadcast, ~3x faster than `push`-per-byte (see
/// [`rgb_to_rgba`]).
fn l8_to_rgba(luma: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; luma.len() * 4];
    for (&v, dst) in luma.iter().zip(rgba.chunks_exact_mut(4)) {
        dst[0] = v;
        dst[1] = v;
        dst[2] = v;
        dst[3] = 255;
    }
    rgba
}

/// Encode resized pixel data to the requested output format.
/// JPEG expects RGB8, WebP expects RGBA8; the caller is responsible
/// for passing the correct layout via `format`.
fn encode_output(pixels: &[u8], w: u32, h: u32, format: ThumbFormat) -> Result<Vec<u8>, ThumbError> {
    match format {
        ThumbFormat::Jpeg => encode_rgb_to_jpeg(pixels, w, h),
        ThumbFormat::Webp => encode_rgba_to_webp(pixels, w, h),
    }
}

/// WebP quality for small outputs (justified base "j" tier, ~512 px). At this
/// display size the difference from a higher quality is imperceptible, so we
/// keep files small.
const WEBP_QUALITY_SMALL: f32 = 78.0;
/// WebP quality for large outputs (large/preview/justified-high tiers, shown
/// big on screen at high zoom). Q75 was visibly lossy on detailed images at
/// these sizes; ~88 is near-transparent while still well under original bytes.
const WEBP_QUALITY_LARGE: f32 = 88.0;
/// Longest-edge threshold (px) above which an output counts as "large" and
/// earns the higher WebP quality. 1024 covers the large/preview/jh tiers while
/// leaving the 512 px justified base tier on the small setting.
const WEBP_LARGE_EDGE: u32 = 1024;

/// Pick the WebP quality for an output of the given dimensions. Larger outputs
/// are shown bigger on screen, so they get the higher quality.
fn webp_quality_for(w: u32, h: u32) -> f32 {
    if w.max(h) >= WEBP_LARGE_EDGE {
        WEBP_QUALITY_LARGE
    } else {
        WEBP_QUALITY_SMALL
    }
}

/// Lossy WebP encode — ~30% smaller than JPEG at equivalent perceptual quality,
/// natively supported by WebKit. Quality scales with output size (see
/// [`webp_quality_for`]). Input must be tightly packed RGBA8 of exactly
/// `w * h * 4` bytes.
pub fn encode_rgba_to_webp(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, ThumbError> {
    let expected = (w as usize) * (h as usize) * 4;
    if rgba.len() < expected {
        return Err(ThumbError::Encode(format!(
            "RGBA buffer too small for {}x{}: {} bytes",
            w,
            h,
            rgba.len()
        )));
    }
    let encoder = webp::Encoder::from_rgba(&rgba[..expected], w, h);
    let mem = encoder.encode(webp_quality_for(w, h));
    Ok(mem.to_vec())
}

/// Decode an already-encoded thumbnail blob back to RGBA. Used by the
/// multi-tier / ThumbHash derivation pass to avoid re-decoding the source
/// file. The image crate sniffs the codec, so JPEG and WebP both work.
pub fn decode_thumb_bytes_to_rgba(data: &[u8]) -> Result<Vec<u8>, ThumbError> {
    let img = image::load_from_memory(data)
        .map_err(|e| ThumbError::Decode(format!("codec decode: {e}")))?;
    Ok(img.to_rgba8().into_raw())
}

/// Downsample an RGBA image to a target square size.
/// Used to derive micro/large/preview tiers from the standard tier,
/// and to derive the ThumbHash source buffer.
pub fn downsample_rgba_square(
    src: &[u8],
    sw: u32,
    sh: u32,
    target: u32,
) -> Result<Vec<u8>, ThumbError> {
    resize_rgba(sw, sh, src, target, target, ResizeFilter::Bilinear)
}

/// Compute a ThumbHash from RGBA pixels. Downsamples internally to ~96px
/// because the thumbhash crate is O(W*H) and the output is invariant to
/// input resolution above that. Returns the compact ~25-byte hash blob.
pub fn compute_thumbhash(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ThumbError> {
    const THUMBHASH_SRC_SIZE: u32 = 96;
    let (src, sw, sh) = if width <= THUMBHASH_SRC_SIZE && height <= THUMBHASH_SRC_SIZE {
        (rgba.to_vec(), width, height)
    } else {
        // Preserve aspect ratio: fit the longer edge into THUMBHASH_SRC_SIZE.
        let (tw, th) = if width >= height {
            let tw = THUMBHASH_SRC_SIZE;
            let th = ((height as u64) * (tw as u64) / (width as u64)).max(1) as u32;
            (tw, th)
        } else {
            let th = THUMBHASH_SRC_SIZE;
            let tw = ((width as u64) * (th as u64) / (height as u64)).max(1) as u32;
            (tw, th)
        };
        (
            resize_rgba(width, height, rgba, tw, th, ResizeFilter::Bilinear)?,
            tw,
            th,
        )
    };
    Ok(thumbhash::rgba_to_thumb_hash(sw as usize, sh as usize, &src))
}

/// Generate a thumbnail for a single image file.
/// Uses fast JPEG DCT-scaled decode for JPEGs, falls back to image crate for others.
pub fn generate_image_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" => generate_jpeg_thumbnail(path, filter, format, thumb_w, thumb_h),
        "heic" | "heif" | "avif" => generate_heic_thumbnail(path, filter, format, thumb_w, thumb_h),
        _ => generate_generic_thumbnail(path, filter, format, thumb_w, thumb_h),
    }
}

/// Route a single path to the correct generator based on file extension.
pub fn generate_for_path(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    match media_type_for_path(path) {
        Some(MediaType::Gif) => generate_image_thumbnail(path, filter, format, thumb_w, thumb_h).map(|mut r| {
            r.media_type = "gif".to_string();
            r
        }),
        Some(MediaType::Video) => generate_video_thumbnail(path, filter, format, thumb_w, thumb_h),
        _ => generate_image_thumbnail(path, filter, format, thumb_w, thumb_h),
    }
}

/// Compute aspect-preserving output dimensions that fit `(sw, sh)` into a box
/// whose longest edge is `max_edge`, never upscaling past the source. Used by
/// the justified (non-cropping) tier.
fn fit_dims(sw: u32, sh: u32, max_edge: u32) -> (u32, u32) {
    if sw == 0 || sh == 0 {
        return (max_edge.max(1), max_edge.max(1));
    }
    if sw >= sh {
        let w = max_edge.min(sw).max(1);
        let h = ((w as f64) * (sh as f64) / (sw as f64)).round().max(1.0) as u32;
        (w, h)
    } else {
        let h = max_edge.min(sh).max(1);
        let w = ((h as f64) * (sw as f64) / (sh as f64)).round().max(1.0) as u32;
        (w, h)
    }
}

/// Generate an **aspect-preserving** thumbnail (no square crop) that fits within
/// a `max_edge`×`max_edge` box. Decodes the full image via [`decode_image`] and
/// resizes the whole frame, so the stored thumbnail keeps the source's true
/// proportions. Used by the justified gallery tier.
pub fn generate_for_path_fit(
    path: &Path,
    filter: ResizeFilter,
    format: ThumbFormat,
    max_edge: u32,
) -> Result<ThumbResult, ThumbError> {
    let decoded = decode_image(path, max_edge)?;
    let (dw, dh) = (decoded.width, decoded.height);
    if dw == 0 || dh == 0 {
        return Err(ThumbError::Decode("Zero decoded dimensions".to_string()));
    }
    let (tw, th) = fit_dims(dw, dh, max_edge);
    let resized = resize_rgba(dw, dh, &decoded.rgba, tw, th, filter)?;

    let data = match format {
        ThumbFormat::Jpeg => {
            let rgb = rgba_to_rgb(&resized);
            encode_output(&rgb, tw, th, format)?
        }
        ThumbFormat::Webp => encode_output(&resized, tw, th, format)?,
    };

    Ok(ThumbResult {
        path: path.to_string_lossy().to_string(),
        width: tw,
        height: th,
        data,
        media_type: decoded.media_type,
        src_width: decoded.src_width,
        src_height: decoded.src_height,
        format,
    })
}

/// Resize already-decoded RGBA into a `max_edge` box and encode it.
///
/// The tail of [`generate_for_path_fit`] for callers that produced their own
/// pixels — a video frame lifted at a chosen timestamp, which no path-based
/// entry point can express. Never upscales, same as the path version.
pub fn fit_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    max_edge: u32,
    filter: ResizeFilter,
    format: ThumbFormat,
) -> Result<Vec<u8>, ThumbError> {
    if width == 0 || height == 0 {
        return Err(ThumbError::Decode("Zero source dimensions".to_string()));
    }
    let (tw, th) = fit_dims(width, height, max_edge);
    let resized = resize_rgba(width, height, rgba, tw, th, filter)?;
    match format {
        ThumbFormat::Jpeg => encode_output(&rgba_to_rgb(&resized), tw, th, format),
        ThumbFormat::Webp => encode_output(&resized, tw, th, format),
    }
}

/// Pixel dimensions from an image's header, without decoding it.
///
/// `None` for anything the `image` crate cannot parse from a header — HEIC,
/// AVIF, RAW, video. Callers use this to skip work that would be a no-op, so
/// "don't know" must degrade to doing the work, never to skipping it.
pub fn header_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

/// Media type inferred from a path's extension (None for non-media files).
fn media_type_for_path(path: &Path) -> Option<MediaType> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    MediaType::from_extension(ext)
}

/// Compute a center crop region to extract the largest square from an image.
fn center_crop_square(w: u32, h: u32) -> Crop {
    let side = w.min(h);
    Crop {
        x: (w - side) / 2,
        y: (h - side) / 2,
        side,
    }
}

/// Result of decoding an image without cropping — full RGBA + dimensions + crop rect.
/// Used by the GPU fused crop+resize pipeline.
pub struct DecodedImage {
    /// Full decoded RGBA pixels (not cropped).
    pub rgba: Vec<u8>,
    /// Decoded image width.
    pub width: u32,
    /// Decoded image height.
    pub height: u32,
    /// Original source dimensions (before DCT scaling).
    pub src_width: u32,
    pub src_height: u32,
    /// Center-crop rect for square extraction.
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_size: u32,
    /// Media type string.
    pub media_type: String,
    /// Original path.
    pub path: String,
}

/// Decode an image to full RGBA without cropping. Returns dimensions + crop rect
/// so the GPU can do the crop in a fused shader.
///
/// `target_edge` is the longest-edge size the caller will ultimately resize to.
/// Decoders that can scale during decode (JPEG DCT, HEIC embedded thumbnails)
/// use it to avoid decoding far more pixels than needed — without it, the JPEG
/// path silently capped every output at the 512px standard size, so the larger
/// justified/fit tiers (1280/2560) could never reach their resolution. Video
/// frames scale in ffmpeg's filter graph for the same reason. Decoders that
/// can't scale (the generic `image` crate path) decode full-size and ignore it;
/// the subsequent resize handles the downscale.
pub fn decode_image(path: &Path, target_edge: u32) -> Result<DecodedImage, ThumbError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = media_type_for_path(path)
        .map(|m| m.as_str())
        .unwrap_or("image");

    let (rgba_buf, dw, dh, src_w, src_h) = if media_type == "video" {
        decode_video_to_rgba(path, target_edge)?
    } else {
        match ext.as_str() {
            "jpg" | "jpeg" => decode_jpeg_to_rgba(path, target_edge)?,
            "heic" | "heif" | "avif" => decode_heic_to_rgba(path, target_edge)?,
            _ => decode_generic_to_rgba(path)?,
        }
    };

    let crop = center_crop_square(dw, dh);

    Ok(DecodedImage {
        rgba: rgba_buf,
        width: dw,
        height: dh,
        src_width: src_w,
        src_height: src_h,
        crop_x: crop.x,
        crop_y: crop.y,
        crop_size: crop.side,
        media_type: media_type.to_string(),
        path: path.to_string_lossy().to_string(),
    })
}

/// Decode a JPEG to RGBA pixels using DCT-scaled decoding, falling back to generic decoder.
fn decode_jpeg_to_rgba(path: &Path, target_edge: u32) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    match decode_jpeg_to_rgba_inner(path, target_edge) {
        Ok(r) => Ok(r),
        Err(_) => decode_generic_to_rgba(path),
    }
}

fn decode_jpeg_to_rgba_inner(path: &Path, target_edge: u32) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let mmap = mmap_file(path)?;
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&mmap[..]));

    decoder.read_info().map_err(|e| ThumbError::Decode(e.to_string()))?;
    let info = decoder.info().ok_or_else(|| ThumbError::Decode("No JPEG info".into()))?;
    let src_w = info.width as u32;
    let src_h = info.height as u32;
    if src_w == 0 || src_h == 0 {
        return Err(ThumbError::Decode("Zero dimension".into()));
    }

    // DCT-scale to at least the caller's longest-edge target in both axes. The
    // decoder picks the largest 1/1·1/2·1/4·1/8 factor that keeps both dims >=
    // the request, so the long edge ends up >= target_edge; the later resize
    // trims to the exact fit dimensions. Clamp to u16 (decoder API) and never 0.
    let target = target_edge.clamp(1, u16::MAX as u32) as u16;
    let _ = decoder.scale(target, target);

    let pixels = decoder.decode().map_err(|e| ThumbError::Decode(e.to_string()))?;
    let di = decoder.info().ok_or_else(|| ThumbError::Decode("No info after decode".into()))?;
    let dw = di.width as u32;
    let dh = di.height as u32;

    let rgba = match di.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let expected = (dw as usize) * (dh as usize) * 3;
            if pixels.len() < expected {
                return Err(ThumbError::Decode("Buffer too small".into()));
            }
            rgb_to_rgba(&pixels[..expected])
        }
        jpeg_decoder::PixelFormat::L8 => {
            let expected = (dw as usize) * (dh as usize);
            if pixels.len() < expected {
                return Err(ThumbError::Decode("L8 buffer too small".into()));
            }
            l8_to_rgba(&pixels[..expected])
        }
        _ => return Err(ThumbError::Decode("Unsupported pixel format".into())),
    };

    Ok((rgba, dw, dh, src_w, src_h))
}

/// Shared `libheif` instance. The library's docs note that all `LibHeif`
/// instances use shared global state, and `Drop` calls `heif_deinit` — so we
/// construct exactly one and let it live for the process lifetime. Plugin
/// discovery in `LibHeif::new` is non-trivial and was previously paid on
/// every HEIC decode call.
fn lib_heif() -> &'static libheif_rs::LibHeif {
    static LIB_HEIF: std::sync::OnceLock<libheif_rs::LibHeif> = std::sync::OnceLock::new();
    LIB_HEIF.get_or_init(libheif_rs::LibHeif::new)
}

/// Decoded HEIC pixels, tagged with their natural channel layout.
/// The transcode-to-JPEG path uses this to skip the RGBA→RGB strip when
/// the source has no alpha (the common case).
pub enum HeicPixels {
    Rgb(Vec<u8>),
    Rgba(Vec<u8>),
}

/// Result of a natural-channel HEIC decode: pixels, decoded dims, original src dims.
pub struct HeicDecode {
    pub pixels: HeicPixels,
    pub width: u32,
    pub height: u32,
    pub src_width: u32,
    pub src_height: u32,
}

/// Decode a HEIC/HEIF image to RGBA pixels, preferring an embedded thumbnail
/// that is large enough to satisfy a `target_edge`-pixel output — iPhone HEICs
/// ship a ~320px thumb that decodes ~100x faster than the full image. Falls
/// back to the primary image when no suitable embedded thumbnail exists.
///
/// Returned `src_w`/`src_h` always reflect the original primary image
/// dimensions; the first three return values describe the actually-decoded
/// pixels (which may be the embedded thumbnail).
pub fn decode_heic_to_rgba(path: &Path, target_edge: u32) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let dec = decode_heic_internal(path, Some(target_edge))?;
    Ok(into_rgba_tuple(dec))
}

/// Decode a HEIC/HEIF image into its natural channel layout — RGB if the
/// source has no alpha (the common case for camera output), RGBA otherwise.
/// Used by the transcode cache to skip a wasted alpha-strip pass before
/// JPEG encoding.
pub fn decode_heic_natural(path: &Path) -> Result<HeicDecode, ThumbError> {
    decode_heic_internal(path, None)
}

/// Decode a HEIC/HEIF image from in-memory bytes. Lets callers that
/// already have the file contents (e.g. via the provider abstraction)
/// avoid a redundant disk read, and lets remote providers (SMB/SFTP/S3)
/// participate at all.
pub fn decode_heic_natural_from_bytes(bytes: &[u8]) -> Result<HeicDecode, ThumbError> {
    let ctx = libheif_rs::HeifContext::read_from_bytes(bytes)
        .map_err(|e| ThumbError::Decode(format!("HEIC open failed: {}", e)))?;
    decode_heic_from_ctx(&ctx, None)
}

fn into_rgba_tuple(dec: HeicDecode) -> (Vec<u8>, u32, u32, u32, u32) {
    let HeicDecode { pixels, width, height, src_width, src_height } = dec;
    let rgba = match pixels {
        HeicPixels::Rgba(v) => v,
        HeicPixels::Rgb(v) => rgb_to_rgba(&v),
    };
    (rgba, width, height, src_width, src_height)
}

fn decode_heic_internal(
    path: &Path,
    target_edge: Option<u32>,
) -> Result<HeicDecode, ThumbError> {
    let ctx = libheif_rs::HeifContext::read_from_file(path.to_str().unwrap_or(""))
        .map_err(|e| ThumbError::Decode(format!("HEIC open failed: {}", e)))?;
    decode_heic_from_ctx(&ctx, target_edge)
}

fn decode_heic_from_ctx(
    ctx: &libheif_rs::HeifContext,
    target_edge: Option<u32>,
) -> Result<HeicDecode, ThumbError> {
    let primary = ctx
        .primary_image_handle()
        .map_err(|e| ThumbError::Decode(format!("HEIC handle failed: {}", e)))?;
    let src_w = primary.width();
    let src_h = primary.height();

    let handle = match target_edge.and_then(|t| pick_thumbnail_handle(&primary, t)) {
        Some(thumb) => thumb,
        None => primary,
    };

    let has_alpha = handle.has_alpha_channel();
    let chroma = if has_alpha {
        libheif_rs::RgbChroma::Rgba
    } else {
        libheif_rs::RgbChroma::Rgb
    };

    let img = lib_heif()
        .decode(&handle, libheif_rs::ColorSpace::Rgb(chroma), None)
        .map_err(|e| ThumbError::Decode(format!("HEIC decode failed: {}", e)))?;
    let plane = img
        .planes()
        .interleaved
        .ok_or_else(|| ThumbError::Decode("HEIC: no interleaved plane".to_string()))?;

    let stride = plane.stride;
    let w = img.width();
    let h = img.height();
    let bpp = if has_alpha { 4 } else { 3 } as usize;
    let row_bytes = (w as usize) * bpp;
    let total = row_bytes * (h as usize);

    // Bulk copy when libheif returned tightly packed rows (the common
    // case); fall back to row-by-row only when it added stride padding.
    let buf = if stride == row_bytes {
        plane.data[..total].to_vec()
    } else {
        let mut v = Vec::with_capacity(total);
        for row in 0..h {
            let start = (row as usize) * stride;
            v.extend_from_slice(&plane.data[start..start + row_bytes]);
        }
        v
    };

    let pixels = if has_alpha {
        HeicPixels::Rgba(buf)
    } else {
        HeicPixels::Rgb(buf)
    };

    Ok(HeicDecode {
        pixels,
        width: w,
        height: h,
        src_width: src_w,
        src_height: src_h,
    })
}

/// Pick the largest embedded thumbnail whose shorter edge is at least 75% of
/// `target_edge`. Returns `None` if no embedded thumbnail meets the bar.
///
/// Why "shorter edge": after decode we center-crop to a square of
/// `min(w, h)` then resize to `target_edge`. Allowing a modest (~33%)
/// upscale catches iPhone HEICs (240×320 embedded thumb) for 256-px grid
/// targets while keeping output quality essentially unchanged.
fn pick_thumbnail_handle(
    primary: &libheif_rs::ImageHandle,
    target_edge: u32,
) -> Option<libheif_rs::ImageHandle> {
    let n = primary.number_of_thumbnails();
    if n == 0 {
        return None;
    }
    let mut ids = vec![0 as libheif_rs::ItemId; n];
    let got = primary.thumbnail_ids(&mut ids);

    let mut best: Option<(libheif_rs::ItemId, u32)> = None;
    for &id in &ids[..got] {
        let Ok(th) = primary.thumbnail(id) else { continue };
        let short = th.width().min(th.height());
        // Require: 4 * short >= 3 * target_edge (i.e. >= 75% of target)
        if short.saturating_mul(4) < target_edge.saturating_mul(3) {
            continue;
        }
        match best {
            None => best = Some((id, short)),
            Some((_, prev)) if short > prev => best = Some((id, short)),
            _ => {}
        }
    }

    let (id, _) = best?;
    primary.thumbnail(id).ok()
}

/// Extract a frame from a video file using ffmpeg and generate a thumbnail.
/// Falls back to a grey placeholder if ffmpeg is not available or extraction fails.
fn generate_video_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    // Decode only as large as the square crop needs, the same rule the JPEG
    // decoder follows — a 4K clip is downscaled inside ffmpeg rather than piped
    // back whole.
    match video::extract_frame(path, thumb_w.max(thumb_h)) {
        Ok(frame) => {
            let data = crop_resize_encode(
                frame.width, frame.height, SrcPixels::Rgba(&frame.rgba),
                thumb_w, thumb_h, format, filter,
            )?;

            Ok(ThumbResult {
                path: path.to_string_lossy().to_string(),
                width: thumb_w,
                height: thumb_h,
                data,
                media_type: "video".to_string(),
                // The clip's true display size, not the size we decoded at —
                // this is what the grid lays the cell out from.
                src_width: frame.src_width,
                src_height: frame.src_height,
                format,
            })
        }
        Err(e) => {
            log::warn!("Video frame extraction failed for {}: {}", path.display(), e);
            generate_video_placeholder(path, format, thumb_w, thumb_h)
        }
    }
}

/// Grey placeholder fallback when ffmpeg is unavailable.
fn generate_video_placeholder(path: &Path, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let pixel_count = (thumb_w * thumb_h) as usize;
    let grey: u8 = 0x3A;

    let data = match format {
        ThumbFormat::Jpeg => encode_output(&vec![grey; pixel_count * 3], thumb_w, thumb_h, format)?,
        ThumbFormat::Webp => {
            let mut rgba = vec![255u8; pixel_count * 4];
            for pixel in rgba.chunks_exact_mut(4) {
                pixel[..3].fill(grey);
            }
            encode_output(&rgba, thumb_w, thumb_h, format)?
        }
    };

    Ok(ThumbResult {
        path: path.to_string_lossy().to_string(),
        width: thumb_w,
        height: thumb_h,
        data,
        media_type: "video".to_string(),
        src_width: 0,
        src_height: 0,
        format,
    })
}

/// Decode a video frame to RGBA pixels using ffmpeg.
/// Returns (rgba, width, height, src_width, src_height).
fn decode_video_to_rgba(path: &Path, target_edge: u32) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let frame = video::extract_frame(path, target_edge)?;
    Ok((frame.rgba, frame.width, frame.height, frame.src_width, frame.src_height))
}

/// Decode a non-JPEG image to RGBA pixels.
fn decode_generic_to_rgba(path: &Path) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let mmap = mmap_file(path)?;
    let img = image::load_from_memory(&mmap).map_err(|e| ThumbError::Decode(format!("{}: {}", path.display(), e)))?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(ThumbError::Decode(format!("Zero dimensions: {}x{}", w, h)));
    }
    let rgba = img.to_rgba8();
    Ok((rgba.into_raw(), w, h, w, h))
}

/// Encode RGBA pixels to JPEG (RGB8) output.
pub fn encode_rgba_to_jpeg(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, ThumbError> {
    let rgb = rgba_to_rgb(rgba);
    encode_rgb_to_jpeg(&rgb, w, h)
}

/// Encode tightly-packed RGB8 pixels to JPEG.
pub fn encode_rgb_to_jpeg(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, ThumbError> {
    let mut buf = std::io::Cursor::new(Vec::with_capacity(32_000));
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80);
    encoder
        .encode(rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| ThumbError::Encode(e.to_string()))?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_center_crop_landscape() {
        let c = center_crop_square(4000, 3000);
        assert_eq!((c.x, c.y, c.side), (500, 0, 3000));
    }

    #[test]
    fn test_center_crop_portrait() {
        let c = center_crop_square(3000, 4000);
        assert_eq!((c.x, c.y, c.side), (0, 500, 3000));
    }

    #[test]
    fn test_center_crop_square() {
        let c = center_crop_square(2000, 2000);
        assert_eq!((c.x, c.y, c.side), (0, 0, 2000));
    }

    #[test]
    fn test_rgb_to_rgba_sets_opaque_alpha() {
        let rgb = [1u8, 2, 3, 4, 5, 6];
        assert_eq!(rgb_to_rgba(&rgb), vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn test_l8_to_rgba_broadcasts_luma() {
        let luma = [10u8, 200];
        assert_eq!(l8_to_rgba(&luma), vec![10, 10, 10, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn test_rgb_rgba_roundtrip() {
        let rgb: Vec<u8> = (0..300).map(|i| (i % 256) as u8).collect();
        assert_eq!(rgba_to_rgb(&rgb_to_rgba(&rgb)), rgb);
    }

    /// Encode a throwaway clip with a display matrix — a stand-in for a phone
    /// `.MOV`, which is landscape on disk and portrait on screen. Returns None
    /// when ffmpeg isn't installed, so the test skips instead of failing.
    fn phone_style_clip(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        if !video::ffmpeg_available() || !video::ffprobe_available() {
            return None;
        }
        let flat = dir.join("flat.mov");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-v", "error", "-f", "lavfi",
                "-i", "testsrc=size=640x360:duration=2:rate=10",
                "-c:v", "libx264", "-pix_fmt", "yuv420p",
            ])
            .arg(&flat)
            .status()
            .ok()?
            .success();
        assert!(ok, "failed to encode test clip");

        let rotated = dir.join("IMG_0001.MOV");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-display_rotation", "90", "-i"])
            .arg(&flat)
            .args(["-c", "copy"])
            .arg(&rotated)
            .status()
            .ok()?
            .success();
        assert!(ok, "failed to remux test clip");
        Some(rotated)
    }

    #[test]
    fn a_phone_mov_thumbnails_at_its_display_orientation() {
        let dir = std::env::temp_dir().join("lightview-thumb-mov-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let Some(clip) = phone_style_clip(&dir) else {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        };

        // Square standard tier: the uppercase `.MOV` has to route to the video
        // generator, and the stored source size is what the grid lays the cell
        // out from — so it must be the portrait display size, not the landscape
        // size the container declares.
        let square = generate_for_path(&clip, ResizeFilter::Bilinear, ThumbFormat::Jpeg, 512, 512)
            .expect("square thumbnail");
        assert_eq!(square.media_type, "video");
        assert_eq!((square.width, square.height), (512, 512));
        assert_eq!((square.src_width, square.src_height), (360, 640));
        // A real frame, not the grey placeholder (which encodes to a tiny blob).
        assert!(square.data.len() > 1000, "looks like the placeholder fallback");

        // Aspect-preserving (justified) tier: same orientation, letterbox-free.
        let fit = generate_for_path_fit(&clip, ResizeFilter::Bilinear, ThumbFormat::Jpeg, 320)
            .expect("fit thumbnail");
        assert_eq!((fit.width, fit.height), (180, 320));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
