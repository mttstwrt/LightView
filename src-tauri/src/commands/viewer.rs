use base64::Engine;
use serde::Deserialize;

use crate::AppState;

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
#[tauri::command]
pub async fn get_transformed_media(
    state: tauri::State<'_, AppState>,
    path: String,
    transform: ImageTransform,
) -> Result<String, String> {
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

    let (rgba_data, src_w, src_h) = if ext == "heic" || ext == "heif" {
        let (rgba, w, h, _, _) = crate::pipeline::thumbnailer::decode_heic_to_rgba(std::path::Path::new(&path))
            .map_err(|e| format!("HEIC decode failed: {}", e))?;
        (rgba, w, h)
    } else {
        let img = image::load_from_memory(&data)
            .map_err(|e| format!("Image decode failed: {}", e))?;
        let (w, h) = (img.width(), img.height());
        let rgba = img.to_rgba8();
        (rgba.into_raw(), w, h)
    };

    // Compute output dimensions (account for rotation)
    let rotation_rad = transform.rotation_degrees.to_radians();
    let (dst_w, dst_h) = if (transform.rotation_degrees.abs() % 180.0 - 90.0).abs() < 1.0 {
        (src_h, src_w) // 90 or 270 degree rotation swaps dimensions
    } else {
        (src_w, src_h)
    };

    // Try GPU transform
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
        let rgba_clone = rgba_data.clone();

        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let result = pipeline_clone.transform_image(
                &rgba_clone,
                src_w,
                src_h,
                dst_w,
                dst_h,
                params,
            );
            let _ = tx.send(result);
        });

        if let Ok(Some(transformed_rgba)) = rx.await {
            // Encode transformed RGBA to JPEG
            let jpeg_data = crate::pipeline::thumbnailer::encode_rgba_to_jpeg(
                &transformed_rgba,
                dst_w,
                dst_h,
            )
            .map_err(|e| format!("JPEG encode failed: {}", e))?;

            let b64 = encode_b64(&jpeg_data);
            return Ok(format!("data:image/jpeg;base64,{}", b64));
        }
        // Fall through to CPU path if GPU transform failed
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

/// CPU fallback for image transforms.
fn apply_cpu_transforms(
    rgba: &[u8],
    width: u32,
    height: u32,
    transform: &ImageTransform,
) -> Vec<u8> {
    let pixel_count = (width * height) as usize;
    let mut output = Vec::with_capacity(pixel_count * 4);

    let rotation_rad = transform.rotation_degrees.to_radians();
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();
    let exposure_mult = 2.0f32.powf(transform.exposure);

    let (dst_w, dst_h) = if (transform.rotation_degrees.abs() % 180.0 - 90.0).abs() < 1.0 {
        (height, width)
    } else {
        (width, height)
    };

    for y in 0..dst_h {
        for x in 0..dst_w {
            // Normalized coords centered at 0.5
            let nx = (x as f32 + 0.5) / dst_w as f32 - 0.5;
            let ny = (y as f32 + 0.5) / dst_h as f32 - 0.5;

            // Rotate
            let rx = nx * cos_r - ny * sin_r + 0.5;
            let ry = nx * sin_r + ny * cos_r + 0.5;

            if rx < 0.0 || rx >= 1.0 || ry < 0.0 || ry >= 1.0 {
                output.extend_from_slice(&[0, 0, 0, 255]);
                continue;
            }

            // Sample nearest pixel
            let sx = (rx * width as f32).min(width as f32 - 1.0) as u32;
            let sy = (ry * height as f32).min(height as f32 - 1.0) as u32;
            let idx = ((sy * width + sx) * 4) as usize;

            let mut r = rgba[idx] as f32 / 255.0;
            let mut g = rgba[idx + 1] as f32 / 255.0;
            let mut b = rgba[idx + 2] as f32 / 255.0;
            let a = rgba[idx + 3];

            // Exposure
            r *= exposure_mult;
            g *= exposure_mult;
            b *= exposure_mult;

            // Saturation
            let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            r = lum + (r - lum) * transform.saturation;
            g = lum + (g - lum) * transform.saturation;
            b = lum + (b - lum) * transform.saturation;

            // Contrast
            r = (r - 0.5) * transform.contrast + 0.5;
            g = (g - 0.5) * transform.contrast + 0.5;
            b = (b - 0.5) * transform.contrast + 0.5;

            output.push((r.clamp(0.0, 1.0) * 255.0) as u8);
            output.push((g.clamp(0.0, 1.0) * 255.0) as u8);
            output.push((b.clamp(0.0, 1.0) * 255.0) as u8);
            output.push(a);
        }
    }

    output
}
