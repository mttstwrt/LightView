use fast_image_resize as fir;
use fir::images::{Image, ImageRef};
use image::GenericImageView;
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Default thumbnail dimensions.
pub const DEFAULT_THUMB_WIDTH: u32 = 400;
pub const DEFAULT_THUMB_HEIGHT: u32 = 400;

/// Output format for generated thumbnails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThumbFormat {
    /// JPEG encoded (compressed, 10–50 KB per thumbnail).
    #[default]
    Jpeg,
    /// Raw RGBA8 pixels (internal use for GPU/BC7 atlas pipeline only).
    Rgba,
}

/// User-configurable thumbnail settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailSettings {
    /// Output format for cached/served thumbnails.
    pub format: ThumbFormat,
    /// Thumbnail width in pixels.
    pub width: u32,
    /// Thumbnail height in pixels.
    pub height: u32,
    /// Resize algorithm.
    pub resize_filter: ResizeFilter,
}

impl Default for ThumbnailSettings {
    fn default() -> Self {
        Self {
            format: ThumbFormat::Jpeg,
            width: DEFAULT_THUMB_WIDTH,
            height: DEFAULT_THUMB_HEIGHT,
            resize_filter: ResizeFilter::default(),
        }
    }
}

/// Resize algorithm selection, exposed to the frontend.
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
    #[error("Stale generation (cancelled)")]
    Cancelled,
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

    // Read header to get source dimensions
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

    // Convert to RGB if needed (jpeg-decoder may return L8, RGB, or CMYK)
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
    let (cx, cy, side) = center_crop_square(dw, dh);

    let final_data = match format {
        ThumbFormat::Jpeg => {
            let cropped = crop_rgb(&rgb_buf, dw, cx, cy, side, side);
            resize_rgb(side, side, &cropped, thumb_w, thumb_h, filter)?
        }
        ThumbFormat::Rgba => {
            let rgba_src = rgb_to_rgba(&rgb_buf);
            let cropped = crop_rgba(&rgba_src, dw, cx, cy, side, side);
            resize_rgba(side, side, &cropped, thumb_w, thumb_h, filter)?
        }
    };

    let data = encode_output(&final_data, thumb_w, thumb_h, format)?;

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
    let (cx, cy, side) = center_crop_square(w, h);

    let final_data = match format {
        ThumbFormat::Jpeg => {
            let rgb = img.to_rgb8();
            let src_buf = rgb.as_raw();
            let cropped = crop_rgb(src_buf, w, cx, cy, side, side);
            resize_rgb(side, side, &cropped, thumb_w, thumb_h, filter)?
        }
        ThumbFormat::Rgba => {
            let rgba = img.to_rgba8();
            let src_buf = rgba.as_raw();
            let cropped = crop_rgba(src_buf, w, cx, cy, side, side);
            resize_rgba(side, side, &cropped, thumb_w, thumb_h, filter)?
        }
    };

    let data = encode_output(&final_data, thumb_w, thumb_h, format)?;

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

/// Generate a thumbnail for a HEIC/HEIF file using libheif.
fn generate_heic_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let (rgba_buf, _dw, _dh, src_w, src_h) = decode_heic_to_rgba(path)?;
    let dw = _dw;
    let dh = _dh;
    let (cx, cy, side) = center_crop_square(dw, dh);

    let final_data = match format {
        ThumbFormat::Jpeg => {
            // Convert RGBA→RGB for JPEG path
            let mut rgb = Vec::with_capacity((dw * dh * 3) as usize);
            for chunk in rgba_buf.chunks_exact(4) {
                rgb.push(chunk[0]);
                rgb.push(chunk[1]);
                rgb.push(chunk[2]);
            }
            let cropped = crop_rgb(&rgb, dw, cx, cy, side, side);
            resize_rgb(side, side, &cropped, thumb_w, thumb_h, filter)?
        }
        ThumbFormat::Rgba => {
            let cropped = crop_rgba(&rgba_buf, dw, cx, cy, side, side);
            resize_rgba(side, side, &cropped, thumb_w, thumb_h, filter)?
        }
    };

    let data = encode_output(&final_data, thumb_w, thumb_h, format)?;

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

/// Resize RGB (U8x3) pixels to target dimensions.
fn resize_rgb(sw: u32, sh: u32, src: &[u8], tw: u32, th: u32, filter: ResizeFilter) -> Result<Vec<u8>, ThumbError> {
    if sw == tw && sh == th {
        return Ok(src.to_vec());
    }
    let src_image = ImageRef::new(sw, sh, src, fir::PixelType::U8x3)
        .map_err(|e| ThumbError::Decode(format!("Source image error: {e}")))?;
    let mut dst_image = Image::new(tw, th, fir::PixelType::U8x3);
    let options = fir::ResizeOptions::new().resize_alg(filter.to_fir_alg());
    let mut resizer = fir::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|e| ThumbError::Encode(format!("Resize failed: {e}")))?;
    Ok(dst_image.into_vec())
}

/// Resize RGBA (U8x4) pixels to target dimensions.
fn resize_rgba(sw: u32, sh: u32, src: &[u8], tw: u32, th: u32, filter: ResizeFilter) -> Result<Vec<u8>, ThumbError> {
    if sw == tw && sh == th {
        return Ok(src.to_vec());
    }
    let src_image = ImageRef::new(sw, sh, src, fir::PixelType::U8x4)
        .map_err(|e| ThumbError::Decode(format!("Source image error: {e}")))?;
    let mut dst_image = Image::new(tw, th, fir::PixelType::U8x4);
    let options = fir::ResizeOptions::new().resize_alg(filter.to_fir_alg());
    let mut resizer = fir::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|e| ThumbError::Encode(format!("Resize failed: {e}")))?;
    Ok(dst_image.into_vec())
}

/// Crop an RGB (U8x3) buffer to a sub-region.
fn crop_rgb(src: &[u8], src_w: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for row in y..y + h {
        let start = ((row * src_w + x) * 3) as usize;
        let end = start + (w * 3) as usize;
        out.extend_from_slice(&src[start..end]);
    }
    out
}

/// Crop an RGBA (U8x4) buffer to a sub-region.
fn crop_rgba(src: &[u8], src_w: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let start = ((row * src_w + x) * 4) as usize;
        let end = start + (w * 4) as usize;
        out.extend_from_slice(&src[start..end]);
    }
    out
}

/// Convert RGB pixel buffer to RGBA (alpha = 255).
fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let pixel_count = rgb.len() / 3;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for chunk in rgb.chunks_exact(3) {
        rgba.push(chunk[0]);
        rgba.push(chunk[1]);
        rgba.push(chunk[2]);
        rgba.push(255);
    }
    rgba
}

/// Convert L8 (grayscale) pixel buffer directly to RGBA (alpha = 255).
fn l8_to_rgba(luma: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(luma.len() * 4);
    for &v in luma {
        rgba.push(v);
        rgba.push(v);
        rgba.push(v);
        rgba.push(255);
    }
    rgba
}

/// Encode resized pixel data to the requested output format.
fn encode_output(pixels: &[u8], w: u32, h: u32, format: ThumbFormat) -> Result<Vec<u8>, ThumbError> {
    match format {
        ThumbFormat::Jpeg => {
            let mut jpeg_buf = std::io::Cursor::new(Vec::with_capacity(32_000));
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 80);
            encoder
                .encode(pixels, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| ThumbError::Encode(e.to_string()))?;
            Ok(jpeg_buf.into_inner())
        }
        ThumbFormat::Rgba => {
            // Raw RGBA pixels — caller will BC7-encode.
            Ok(pixels.to_vec())
        }
    }
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
        "heic" | "heif" => generate_heic_thumbnail(path, filter, format, thumb_w, thumb_h),
        _ => generate_generic_thumbnail(path, filter, format, thumb_w, thumb_h),
    }
}

/// Route a single path to the correct generator based on file extension.
pub fn generate_for_path(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "gif" => generate_image_thumbnail(path, filter, format, thumb_w, thumb_h).map(|mut r| {
            r.media_type = "gif".to_string();
            r
        }),
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv" => {
            generate_video_thumbnail(path, filter, format, thumb_w, thumb_h)
        }
        _ => generate_image_thumbnail(path, filter, format, thumb_w, thumb_h),
    }
}

/// Generate thumbnails for a batch of paths in parallel using rayon.
pub fn generate_batch_parallel(paths: &[String], filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Vec<(String, Result<ThumbResult, ThumbError>)> {
    paths
        .par_iter()
        .map(|path_str| {
            let path = Path::new(path_str);
            let result = generate_for_path(path, filter, format, thumb_w, thumb_h);
            (path_str.clone(), result)
        })
        .collect()
}

/// Generate thumbnails for a batch of image paths in parallel.
/// Respects the generation counter — if it changes, workers stop.
pub fn generate_batch(
    paths: &[String],
    filter: ResizeFilter,
    format: ThumbFormat,
    thumb_w: u32,
    thumb_h: u32,
    generation: Arc<AtomicU64>,
    expected_gen: u64,
) -> Vec<Result<ThumbResult, ThumbError>> {
    paths
        .par_iter()
        .map(|path_str| {
            if generation.load(Ordering::Relaxed) != expected_gen {
                return Err(ThumbError::Cancelled);
            }
            let path = Path::new(path_str);
            generate_for_path(path, filter, format, thumb_w, thumb_h)
        })
        .collect()
}

/// Compute a center crop region to extract the largest square from an image.
/// Returns (x_offset, y_offset, side_length).
pub fn compute_crop_rect(w: u32, h: u32) -> (u32, u32, u32) {
    let side = w.min(h);
    let x = (w - side) / 2;
    let y = (h - side) / 2;
    (x, y, side)
}

// Private alias for backward compatibility within this module.
fn center_crop_square(w: u32, h: u32) -> (u32, u32, u32) {
    compute_crop_rect(w, h)
}

// ---------------------------------------------------------------------------
// GPU batch helpers — decode + crop on CPU, resize on GPU, encode on CPU
// ---------------------------------------------------------------------------

/// Result of the decode + crop phase (before resize).
pub struct DecodedCrop {
    /// Center-cropped RGBA pixels.
    pub rgba: Vec<u8>,
    /// Crop dimensions (square).
    pub crop_side: u32,
    /// Original source dimensions.
    pub src_width: u32,
    pub src_height: u32,
    /// Media type string.
    pub media_type: String,
    /// Original path.
    pub path: String,
}

/// Decode an image and center-crop to a square. Returns RGBA pixels ready for resize.
pub fn decode_and_crop(path: &Path) -> Result<DecodedCrop, ThumbError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "gif" => "gif",
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv" => "video",
        _ => "image",
    };

    // Decode to RGBA
    let (rgba_buf, dw, dh, src_w, src_h) = match ext.as_str() {
        "jpg" | "jpeg" => decode_jpeg_to_rgba(path)?,
        "heic" | "heif" => decode_heic_to_rgba(path)?,
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv" => {
            decode_video_to_rgba(path)?
        }
        _ => decode_generic_to_rgba(path)?,
    };

    let (cx, cy, side) = center_crop_square(dw, dh);
    let cropped = crop_rgba(&rgba_buf, dw, cx, cy, side, side);

    Ok(DecodedCrop {
        rgba: cropped,
        crop_side: side,
        src_width: src_w,
        src_height: src_h,
        media_type: media_type.to_string(),
        path: path.to_string_lossy().to_string(),
    })
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
pub fn decode_image(path: &Path) -> Result<DecodedImage, ThumbError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "gif" => "gif",
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv" => "video",
        _ => "image",
    };

    let (rgba_buf, dw, dh, src_w, src_h) = match ext.as_str() {
        "jpg" | "jpeg" => decode_jpeg_to_rgba(path)?,
        "heic" | "heif" => decode_heic_to_rgba(path)?,
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv" => {
            decode_video_to_rgba(path)?
        }
        _ => decode_generic_to_rgba(path)?,
    };

    let (cx, cy, side) = compute_crop_rect(dw, dh);

    Ok(DecodedImage {
        rgba: rgba_buf,
        width: dw,
        height: dh,
        src_width: src_w,
        src_height: src_h,
        crop_x: cx,
        crop_y: cy,
        crop_size: side,
        media_type: media_type.to_string(),
        path: path.to_string_lossy().to_string(),
    })
}

/// Decode a JPEG to RGBA pixels using DCT-scaled decoding, falling back to generic decoder.
fn decode_jpeg_to_rgba(path: &Path) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    match decode_jpeg_to_rgba_inner(path) {
        Ok(r) => Ok(r),
        Err(_) => decode_generic_to_rgba(path),
    }
}

fn decode_jpeg_to_rgba_inner(path: &Path) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let mmap = mmap_file(path)?;
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&mmap[..]));

    decoder.read_info().map_err(|e| ThumbError::Decode(e.to_string()))?;
    let info = decoder.info().ok_or_else(|| ThumbError::Decode("No JPEG info".into()))?;
    let src_w = info.width as u32;
    let src_h = info.height as u32;
    if src_w == 0 || src_h == 0 {
        return Err(ThumbError::Decode("Zero dimension".into()));
    }

    let target = DEFAULT_THUMB_WIDTH.max(DEFAULT_THUMB_HEIGHT) as u16;
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

/// Decode a HEIC/HEIF image to RGBA pixels using libheif.
pub fn decode_heic_to_rgba(path: &Path) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let lib_heif = libheif_rs::LibHeif::new();
    let ctx = libheif_rs::HeifContext::read_from_file(path.to_str().unwrap_or(""))
        .map_err(|e| ThumbError::Decode(format!("HEIC open failed: {}", e)))?;
    let handle = ctx
        .primary_image_handle()
        .map_err(|e| ThumbError::Decode(format!("HEIC handle failed: {}", e)))?;
    let src_w = handle.width();
    let src_h = handle.height();
    let img = lib_heif
        .decode(&handle, libheif_rs::ColorSpace::Rgb(libheif_rs::RgbChroma::Rgba), None)
        .map_err(|e| ThumbError::Decode(format!("HEIC decode failed: {}", e)))?;
    let plane = img
        .planes()
        .interleaved
        .ok_or_else(|| ThumbError::Decode("HEIC: no interleaved plane".to_string()))?;

    let stride = plane.stride;
    let w = img.width();
    let h = img.height();

    // Copy row-by-row to strip any padding from the stride
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (row as usize) * stride;
        let end = start + (w as usize) * 4;
        rgba.extend_from_slice(&plane.data[start..end]);
    }

    Ok((rgba, w, h, src_w, src_h))
}

/// Extract a frame from a video file using ffmpeg and generate a thumbnail.
/// Falls back to a grey placeholder if ffmpeg is not available or extraction fails.
fn generate_video_thumbnail(path: &Path, filter: ResizeFilter, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    match extract_video_frame(path) {
        Ok((rgba, w, h)) => {
            let (cx, cy, side) = center_crop_square(w, h);

            let final_data = match format {
                ThumbFormat::Jpeg => {
                    // Convert RGBA→RGB for JPEG
                    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
                    for chunk in rgba.chunks_exact(4) {
                        rgb.push(chunk[0]);
                        rgb.push(chunk[1]);
                        rgb.push(chunk[2]);
                    }
                    let cropped = crop_rgb(&rgb, w, cx, cy, side, side);
                    resize_rgb(side, side, &cropped, thumb_w, thumb_h, filter)?
                }
                ThumbFormat::Rgba => {
                    let cropped = crop_rgba(&rgba, w, cx, cy, side, side);
                    resize_rgba(side, side, &cropped, thumb_w, thumb_h, filter)?
                }
            };

            let data = encode_output(&final_data, thumb_w, thumb_h, format)?;

            Ok(ThumbResult {
                path: path.to_string_lossy().to_string(),
                width: thumb_w,
                height: thumb_h,
                data,
                media_type: "video".to_string(),
                src_width: w,
                src_height: h,
                format,
            })
        }
        Err(e) => {
            log::warn!("Video frame extraction failed for {}: {}", path.display(), e);
            generate_video_placeholder(path, format, thumb_w, thumb_h)
        }
    }
}

/// Extract a single frame from a video using ffmpeg.
/// Seeks to 1 second (or start for short videos) and outputs raw RGBA pixels.
fn extract_video_frame(path: &Path) -> Result<(Vec<u8>, u32, u32), ThumbError> {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-ss", "1",           // seek to 1s (fast keyframe seek before input)
            "-i",
        ])
        .arg(path.as_os_str())
        .args([
            "-frames:v", "1",     // extract one frame
            "-f", "rawvideo",
            "-pix_fmt", "rgba",
            "-v", "error",
            "pipe:1",             // output to stdout
        ])
        .output()
        .map_err(|e| ThumbError::Decode(format!("ffmpeg not found or failed to run: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ThumbError::Decode(format!("ffmpeg failed: {}", stderr.trim())));
    }

    // We need dimensions — probe them separately
    let (w, h) = probe_video_dimensions(path)?;
    let expected = (w as usize) * (h as usize) * 4;

    if output.stdout.len() != expected {
        return Err(ThumbError::Decode(format!(
            "ffmpeg output size mismatch: got {} bytes, expected {} ({}x{}x4)",
            output.stdout.len(), expected, w, h
        )));
    }

    Ok((output.stdout, w, h))
}

/// Get video dimensions using ffprobe.
fn probe_video_dimensions(path: &Path) -> Result<(u32, u32), ThumbError> {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-of", "csv=p=0:s=x",
        ])
        .arg(path.as_os_str())
        .output()
        .map_err(|e| ThumbError::Decode(format!("ffprobe not found: {}", e)))?;

    if !output.status.success() {
        return Err(ThumbError::Decode("ffprobe failed".to_string()));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    let parts: Vec<&str> = text.split('x').collect();
    if parts.len() != 2 {
        return Err(ThumbError::Decode(format!("ffprobe unexpected output: {}", text)));
    }
    let w: u32 = parts[0].parse().map_err(|_| ThumbError::Decode("bad width".to_string()))?;
    let h: u32 = parts[1].parse().map_err(|_| ThumbError::Decode("bad height".to_string()))?;
    Ok((w, h))
}

/// Probe video metadata: (width, height, duration_seconds, codec, has_audio, fps).
pub fn probe_video_metadata(path: &Path) -> Result<(u32, u32, Option<f64>, Option<String>, bool, Option<f64>), ThumbError> {
    // Video stream info
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height,codec_name,r_frame_rate,duration",
            "-show_entries", "format=duration",
            "-of", "json",
        ])
        .arg(path.as_os_str())
        .output()
        .map_err(|e| ThumbError::Decode(format!("ffprobe not found: {}", e)))?;

    if !output.status.success() {
        return Err(ThumbError::Decode("ffprobe failed".to_string()));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| ThumbError::Decode(format!("ffprobe json parse error: {}", e)))?;

    let stream = json.get("streams").and_then(|s| s.get(0));
    let format = json.get("format");

    let w = stream
        .and_then(|s| s.get("width"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let h = stream
        .and_then(|s| s.get("height"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    // Duration: prefer stream duration, fall back to format duration
    let duration = stream
        .and_then(|s| s.get("duration"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            format
                .and_then(|f| f.get("duration"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
        });

    let codec = stream
        .and_then(|s| s.get("codec_name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Parse fps from r_frame_rate (e.g. "30000/1001")
    let fps = stream
        .and_then(|s| s.get("r_frame_rate"))
        .and_then(|v| v.as_str())
        .and_then(|s| {
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() == 2 {
                let num: f64 = parts[0].parse().ok()?;
                let den: f64 = parts[1].parse().ok()?;
                if den > 0.0 { Some(num / den) } else { None }
            } else {
                s.parse::<f64>().ok()
            }
        });

    // Check for audio stream
    let audio_output = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "a:0",
            "-show_entries", "stream=codec_name",
            "-of", "csv=p=0",
        ])
        .arg(path.as_os_str())
        .output()
        .ok();

    let has_audio = audio_output
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    Ok((w, h, duration, codec, has_audio, fps))
}

/// Grey placeholder fallback when ffmpeg is unavailable.
fn generate_video_placeholder(path: &Path, format: ThumbFormat, thumb_w: u32, thumb_h: u32) -> Result<ThumbResult, ThumbError> {
    let pixel_count = (thumb_w * thumb_h) as usize;
    let grey: u8 = 0x3A;

    let data = match format {
        ThumbFormat::Rgba => {
            let mut buf = vec![0u8; pixel_count * 4];
            for pixel in buf.chunks_exact_mut(4) {
                pixel[0] = grey;
                pixel[1] = grey;
                pixel[2] = grey;
                pixel[3] = 255;
            }
            buf
        }
        ThumbFormat::Jpeg => {
            let rgb = vec![grey; pixel_count * 3];
            let mut cursor = std::io::Cursor::new(Vec::with_capacity(4096));
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 80);
            encoder
                .encode(&rgb, thumb_w, thumb_h, image::ExtendedColorType::Rgb8)
                .map_err(|e| ThumbError::Encode(e.to_string()))?;
            cursor.into_inner()
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
fn decode_video_to_rgba(path: &Path) -> Result<(Vec<u8>, u32, u32, u32, u32), ThumbError> {
    let (rgba, w, h) = extract_video_frame(path)?;
    Ok((rgba, w, h, w, h))
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
    // Convert RGBA → RGB for JPEG encoding
    let pixel_count = (w * h) as usize;
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for chunk in rgba.chunks_exact(4) {
        rgb.push(chunk[0]);
        rgb.push(chunk[1]);
        rgb.push(chunk[2]);
    }
    let mut buf = std::io::Cursor::new(Vec::with_capacity(32_000));
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80);
    encoder
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| ThumbError::Encode(e.to_string()))?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_center_crop_landscape() {
        let (x, y, side) = center_crop_square(4000, 3000);
        assert_eq!(side, 3000);
        assert_eq!(x, 500);
        assert_eq!(y, 0);
    }

    #[test]
    fn test_center_crop_portrait() {
        let (x, y, side) = center_crop_square(3000, 4000);
        assert_eq!(side, 3000);
        assert_eq!(x, 0);
        assert_eq!(y, 500);
    }

    #[test]
    fn test_center_crop_square() {
        let (x, y, side) = center_crop_square(2000, 2000);
        assert_eq!(side, 2000);
        assert_eq!(x, 0);
        assert_eq!(y, 0);
    }
}
