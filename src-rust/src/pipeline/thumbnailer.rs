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

/// The edge below which a cheaper resize filter is indistinguishable.
///
/// There is no `ThumbFormat` any more: every cached thumbnail and every
/// `?fit=` response is WebP. The enum existed so the square tiers could be
/// JPEG and the fit tiers WebP; one family means one encoder, and an encoder
/// chosen per call is a second way a thumbnail can be wrong.
const SMALL_OUTPUT_EDGE: u32 = 512;

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

/// Pick the resize algorithm for a target edge.
///
/// The two small tiers use Bilinear: the quality difference from Lanczos3 is
/// imperceptible at 512px or less, and this path is decode-bound anyway. The
/// two large ones are shown big on screen and earn the sharper filter.
pub fn filter_for_size(target: u32) -> ResizeFilter {
    if target <= SMALL_OUTPUT_EDGE {
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
    /// The resized RGBA pixels `data` was encoded from.
    ///
    /// Carried out rather than dropped so the ThumbHash can be computed from
    /// the pixels that are already in hand. The alternative — deriving it later
    /// from the stored bytes — means a second decode, and it means a freshly
    /// opened gallery paints with no placeholders at all until the idle worker
    /// catches up, which is the one case the ThumbHash exists for.
    pub rgba: Vec<u8>,
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





/// Resize RGBA pixels to target dimensions.
///
/// There is no crop parameter any more, and no pixel-layout parameter either.
/// Both existed for the square tiers, whose whole family this design deletes:
/// one grid, aspect-preserving, so every output is a fit — and one encoder, so
/// every buffer is RGBA.
fn resize_rgba(
    sw: u32,
    sh: u32,
    src: &[u8],
    tw: u32,
    th: u32,
    filter: ResizeFilter,
) -> Result<Vec<u8>, ThumbError> {
    if sw == tw && sh == th {
        return Ok(src.to_vec());
    }
    let pixel = fir::PixelType::U8x4;
    let src_image = ImageRef::new(sw, sh, src, pixel)
        .map_err(|e| ThumbError::Decode(format!("Source image error: {e}")))?;
    let mut dst_image = Image::new(tw, th, pixel);
    let options = fir::ResizeOptions::new().resize_alg(filter.to_fir_alg());
    let mut resizer = fir::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|e| ThumbError::Encode(format!("Resize failed: {e}")))?;
    Ok(dst_image.into_vec())
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
pub fn decode_thumb_bytes_to_rgba(data: &[u8]) -> Result<(Vec<u8>, u32, u32), ThumbError> {
    let img = image::load_from_memory(data)
        .map_err(|e| ThumbError::Decode(format!("codec decode: {e}")))?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok((rgba.into_raw(), w, h))
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
    max_edge: u32,
) -> Result<ThumbResult, ThumbError> {
    let decoded = match decode_image(path, max_edge) {
        Ok(d) => d,
        // `ffmpeg` is a runtime dependency, not a build one, so a deployment
        // without it must show clips as a grey cell rather than as a hole the
        // grid re-requests on every scroll pass. Stills have no such fallback:
        // a still that will not decode is a broken file, and inventing a cell
        // for it would hide that.
        Err(e) if media_type_for_path(path) == Some(MediaType::Video) => {
            log::warn!("video decode failed for {}, using a placeholder: {e}", path.display());
            video_placeholder(path, max_edge)?
        }
        Err(e) => return Err(e),
    };
    let (dw, dh) = (decoded.width, decoded.height);
    if dw == 0 || dh == 0 {
        return Err(ThumbError::Decode("Zero decoded dimensions".to_string()));
    }
    let (tw, th) = fit_dims(dw, dh, max_edge);
    let resized = resize_rgba(dw, dh, &decoded.rgba, tw, th, filter)?;

    Ok(ThumbResult {
        path: path.to_string_lossy().to_string(),
        width: tw,
        height: th,
        data: encode_rgba_to_webp(&resized, tw, th)?,
        media_type: decoded.media_type,
        src_width: decoded.src_width,
        src_height: decoded.src_height,
        rgba: resized,
    })
}

/// A neutral grey frame standing in for a clip that could not be decoded.
///
/// **`src_width` and `src_height` stay zero**, and the caller must not store
/// them. A `0x0` write both fills the `width IS NULL` gap that guards the
/// column — so the real dimensions are never probed again — and hands the grid
/// a degenerate aspect ratio, which lays the cell out with no height at all.
fn video_placeholder(path: &Path, max_edge: u32) -> Result<DecodedImage, ThumbError> {
    // 16:9 at the requested box, which is the common case and keeps the cell a
    // plausible shape until a real frame replaces it.
    let w = max_edge.max(16);
    let h = (w * 9 / 16).max(9);
    Ok(DecodedImage {
        rgba: vec![0x30; (w as usize) * (h as usize) * 4],
        width: w,
        height: h,
        src_width: 0,
        src_height: 0,
        media_type: "video".to_string(),
        path: path.to_string_lossy().to_string(),
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
) -> Result<Vec<u8>, ThumbError> {
    if width == 0 || height == 0 {
        return Err(ThumbError::Decode("Zero source dimensions".to_string()));
    }
    let (tw, th) = fit_dims(width, height, max_edge);
    let resized = resize_rgba(width, height, rgba, tw, th, filter)?;
    encode_rgba_to_webp(&resized, tw, th)
}

/// Strip alpha from an RGBA buffer to produce RGB. Uses 3-byte
/// `extend_from_slice` per pixel — the compiler emits a memcpy intrinsic for
/// that, which is meaningfully faster than `push`-per-byte.
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let pixel_count = rgba.len() / 4;
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for chunk in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    rgb
}

/// Encode RGBA pixels to JPEG.
///
/// **The one place JPEG is still written, and it is not a thumbnail.** Serving
/// a full-resolution `.heic` original has to transcode it — no browser renders
/// HEIC — and at 12 megapixels libwebp's encoder is materially slower than
/// libjpeg on the N100 this design has to serve from. Every *cached* tier is
/// WebP through the one render path; this is a viewer request for an original.
pub fn encode_rgba_to_jpeg(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, ThumbError> {
    encode_rgb_to_jpeg(&rgba_to_rgb(rgba), w, h)
}

/// Encode tightly-packed RGB8 pixels to JPEG. See [`encode_rgba_to_jpeg`] for
/// why this exists at all.
pub fn encode_rgb_to_jpeg(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, ThumbError> {
    let mut buf = std::io::Cursor::new(Vec::with_capacity(32_000));
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80);
    encoder
        .encode(rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| ThumbError::Encode(e.to_string()))?;
    Ok(buf.into_inner())
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


/// Decoded source pixels and what the decoder learned on the way.
///
/// No crop rectangle. It existed for the square tiers and for the GPU's fused
/// crop+resize, and both leave with the square grid.
pub struct DecodedImage {
    /// Full decoded RGBA pixels.
    pub rgba: Vec<u8>,
    /// Decoded dimensions, which a scaling decoder may have reduced.
    pub width: u32,
    pub height: u32,
    /// Original source dimensions, before any decode-time scaling. This is what
    /// the grid lays a cell out from, so for a rotated video it must be the
    /// *display* orientation.
    pub src_width: u32,
    pub src_height: u32,
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

    Ok(DecodedImage {
        rgba: rgba_buf,
        width: dw,
        height: dh,
        src_width: src_w,
        src_height: src_h,
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



#[cfg(test)]
mod tests {
    use super::*;

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

        // The uppercase `.MOV` has to route to the video decoder, and the
        // stored source size is what the grid lays the cell out from — so it
        // must be the portrait *display* size, not the landscape size the
        // container declares. A mismatch here is what makes .MOV thumbnails
        // come back sideways.
        let fit = generate_for_path_fit(&clip, ResizeFilter::Bilinear, 320)
            .expect("fit thumbnail");
        assert_eq!(fit.media_type, "video");
        assert_eq!((fit.width, fit.height), (180, 320));
        assert_eq!((fit.src_width, fit.src_height), (360, 640));
        // A real frame, not the grey placeholder (which encodes to a tiny blob).
        assert!(fit.data.len() > 1000, "looks like the placeholder fallback");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
