use base64::Engine;
use rayon::prelude::*;
use serde::Deserialize;

use crate::AppState;

/// Record that a media item was viewed (updates last_viewed timestamp).
#[tauri::command]
pub async fn record_view(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    record_view_impl(&state, path).await
}

/// Shared by the Tauri command and the web client's `/api/invoke` bridge —
/// a phone browsing the gallery remotely feeds the same "Recently Viewed"
/// history the desktop app does.
pub async fn record_view_impl(state: &AppState, path: String) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.record_view(&path).map_err(|e| e.to_string())
}

/// Image transform parameters for GPU-accelerated viewer adjustments.
#[derive(Debug, Deserialize)]
pub struct ImageTransform {
    /// Rotation in degrees (0, 90, 180, 270, or arbitrary).
    #[serde(default)]
    pub rotation_degrees: f32,
    /// Exposure adjustment in EV stops (default 0.0).
    #[serde(default)]
    pub exposure: f32,
    /// Saturation multiplier (default 1.0).
    #[serde(default = "default_one")]
    pub saturation: f32,
    /// Contrast multiplier (default 1.0).
    #[serde(default = "default_one")]
    pub contrast: f32,
}

fn default_one() -> f32 {
    1.0
}

fn encode_b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Get a full-resolution image with GPU-applied transforms (rotation, exposure, etc.).
/// Returns a JPEG data URI. Falls back to CPU if GPU is unavailable.
///
/// Images with more than `MAX_PIXELS` pixels are downsampled before
/// transforms to prevent OOM from the decoded RGBA buffer.
#[tauri::command]
pub async fn get_transformed_media(
    state: tauri::State<'_, AppState>,
    path: String,
    transform: ImageTransform,
) -> Result<String, String> {
    /// Maximum decoded pixel count before we downsample (50 megapixels).
    /// 50 MP × 4 bytes/pixel = 200 MB of RGBA — a safe upper bound.
    const MAX_PIXELS: u64 = 50_000_000;

    let gallery_path = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;

    let reg = state.providers.read().await;
    let provider = reg.get(&gallery_path).ok_or("Provider not found")?;

    let data = provider
        .read_file(&path)
        .await
        .map_err(|e| e.to_string())?;

    // Decode image to RGBA (HEIC uses libheif, others use image crate)
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let (mut rgba_data, mut src_w, mut src_h) = if ext == "heic" || ext == "heif" {
        let dec = crate::pipeline::thumbnailer::decode_heic_natural_from_bytes(&data)
            .map_err(|e| format!("HEIC decode failed: {}", e))?;
        let rgba = match dec.pixels {
            crate::pipeline::thumbnailer::HeicPixels::Rgba(v) => v,
            crate::pipeline::thumbnailer::HeicPixels::Rgb(v) => {
                crate::pipeline::thumbnailer::rgb_to_rgba(&v)
            }
        };
        (rgba, dec.width, dec.height)
    } else {
        let img = image::load_from_memory(&data)
            .map_err(|e| format!("Image decode failed: {}", e))?;
        let (w, h) = (img.width(), img.height());
        let rgba = img.to_rgba8();
        (rgba.into_raw(), w, h)
    };

    // Drop the encoded source data now — we only need RGBA from here on.
    drop(data);

    // Downsample if the decoded image exceeds the pixel budget
    let pixel_count = src_w as u64 * src_h as u64;
    if pixel_count > MAX_PIXELS {
        let scale = (MAX_PIXELS as f64 / pixel_count as f64).sqrt();
        let new_w = ((src_w as f64 * scale) as u32).max(1);
        let new_h = ((src_h as f64 * scale) as u32).max(1);
        log::info!(
            "Downsampling {}×{} → {}×{} for transform (exceeded {} MP limit)",
            src_w, src_h, new_w, new_h, MAX_PIXELS / 1_000_000
        );
        let src_img = image::RgbaImage::from_raw(src_w, src_h, rgba_data)
            .ok_or("Failed to create image buffer for downsampling")?;
        let resized = image::imageops::resize(
            &src_img,
            new_w,
            new_h,
            image::imageops::FilterType::Lanczos3,
        );
        src_w = new_w;
        src_h = new_h;
        rgba_data = resized.into_raw();
    }

    // Compute output dimensions (account for rotation)
    let rotation_rad = transform.rotation_degrees.to_radians();
    let (dst_w, dst_h) = if (transform.rotation_degrees.abs() % 180.0 - 90.0).abs() < 1.0 {
        (src_h, src_w) // 90 or 270 degree rotation swaps dimensions
    } else {
        (src_w, src_h)
    };

    // Try GPU transform — pass owned data to avoid a redundant clone
    #[cfg(feature = "gpu")]
    if let Some(ref pipeline) = state.gpu_pipeline {
        use crate::pipeline::gpu_pipeline::TransformParams;

        let params = TransformParams {
            src_width: src_w,
            src_height: src_h,
            dst_width: dst_w,
            dst_height: dst_h,
            rotation_cos: rotation_rad.cos(),
            rotation_sin: rotation_rad.sin(),
            exposure: transform.exposure,
            saturation: transform.saturation,
            contrast: transform.contrast,
            _pad0: 0.0,
            _pad1: 0.0,
            _pad2: 0.0,
        };

        let pipeline_clone = pipeline.clone();

        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let result = pipeline_clone.transform_image(
                &rgba_data,
                src_w,
                src_h,
                dst_w,
                dst_h,
                params,
            );
            let _ = tx.send(result);
        });

        if let Ok(Some(transformed_rgba)) = rx.await {
            let jpeg_data = crate::pipeline::thumbnailer::encode_rgba_to_jpeg(
                &transformed_rgba,
                dst_w,
                dst_h,
            )
            .map_err(|e| format!("JPEG encode failed: {}", e))?;

            let b64 = encode_b64(&jpeg_data);
            return Ok(format!("data:image/jpeg;base64,{}", b64));
        }
        // Fall through to CPU path if GPU transform failed.
        // Note: rgba_data was moved into the closure above, so we need to
        // return an error rather than silently fall through with no data.
        return Err("GPU transform failed and RGBA data was consumed".to_string());
    }

    // CPU fallback: apply transforms on CPU
    let transformed = apply_cpu_transforms(&rgba_data, src_w, src_h, &transform);
    let (final_w, final_h) = (dst_w, dst_h);

    let jpeg_data = crate::pipeline::thumbnailer::encode_rgba_to_jpeg(
        &transformed,
        final_w,
        final_h,
    )
    .map_err(|e| format!("JPEG encode failed: {}", e))?;

    let b64 = encode_b64(&jpeg_data);
    Ok(format!("data:image/jpeg;base64,{}", b64))
}

/// CPU fallback for image transforms. Row-parallel over rayon's global pool —
/// each output row samples independently from `rgba`, so no synchronization
/// is needed beyond the disjoint mutable row borrows.
fn apply_cpu_transforms(
    rgba: &[u8],
    width: u32,
    height: u32,
    transform: &ImageTransform,
) -> Vec<u8> {
    let rotation_rad = transform.rotation_degrees.to_radians();
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();
    let exposure_mult = 2.0f32.powf(transform.exposure);
    let saturation = transform.saturation;
    let contrast = transform.contrast;

    let (dst_w, dst_h) = if (transform.rotation_degrees.abs() % 180.0 - 90.0).abs() < 1.0 {
        (height, width)
    } else {
        (width, height)
    };

    let row_bytes = (dst_w as usize) * 4;
    let mut output = vec![0u8; row_bytes * (dst_h as usize)];

    let dst_w_f = dst_w as f32;
    let dst_h_f = dst_h as f32;
    let src_w_f = width as f32;
    let src_h_f = height as f32;
    let src_w_max = src_w_f - 1.0;
    let src_h_max = src_h_f - 1.0;

    output
        .par_chunks_exact_mut(row_bytes)
        .enumerate()
        .for_each(|(y, row)| {
            let ny = (y as f32 + 0.5) / dst_h_f - 0.5;
            for x in 0..dst_w {
                let nx = (x as f32 + 0.5) / dst_w_f - 0.5;

                let rx = nx * cos_r - ny * sin_r + 0.5;
                let ry = nx * sin_r + ny * cos_r + 0.5;

                let out = &mut row[(x as usize) * 4..(x as usize) * 4 + 4];

                if rx < 0.0 || rx >= 1.0 || ry < 0.0 || ry >= 1.0 {
                    out[0] = 0;
                    out[1] = 0;
                    out[2] = 0;
                    out[3] = 255;
                    continue;
                }

                let sx = (rx * src_w_f).min(src_w_max) as u32;
                let sy = (ry * src_h_f).min(src_h_max) as u32;
                let idx = ((sy * width + sx) * 4) as usize;

                let mut r = rgba[idx] as f32 / 255.0;
                let mut g = rgba[idx + 1] as f32 / 255.0;
                let mut b = rgba[idx + 2] as f32 / 255.0;
                let a = rgba[idx + 3];

                r *= exposure_mult;
                g *= exposure_mult;
                b *= exposure_mult;

                let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                r = lum + (r - lum) * saturation;
                g = lum + (g - lum) * saturation;
                b = lum + (b - lum) * saturation;

                r = (r - 0.5) * contrast + 0.5;
                g = (g - 0.5) * contrast + 0.5;
                b = (b - 0.5) * contrast + 0.5;

                out[0] = (r.clamp(0.0, 1.0) * 255.0) as u8;
                out[1] = (g.clamp(0.0, 1.0) * 255.0) as u8;
                out[2] = (b.clamp(0.0, 1.0) * 255.0) as u8;
                out[3] = a;
            }
        });

    output
}
