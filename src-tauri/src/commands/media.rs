use base64::Engine;
use serde::Serialize;

use crate::pipeline::thumbnailer::{ResizeFilter, ThumbFormat};
use crate::AppState;

/// Thumbnail metadata returned over IPC. No pixel data — the frontend
/// fetches actual image bytes via the `lightview://thumb/` protocol.
#[derive(Debug, Serialize)]
pub struct ThumbnailResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub media_type: String,
    pub format: String,
}

#[derive(Debug, Serialize)]
pub struct MediaMeta {
    pub path: String,
    pub media_type: String,
    pub file_size: u64,
    pub date_taken: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
}

fn encode_b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Get a single thumbnail. Checks BC7 atlas first (if active), then SQLite cache,
/// then generates on-demand.
#[tauri::command]
pub async fn get_thumbnail(
    state: tauri::State<'_, AppState>,
    path: String,
    resize_filter: Option<ResizeFilter>,
) -> Result<Option<ThumbnailResult>, String> {
    let settings = state.thumbnail_settings.read().await.clone();
    let filter = resize_filter.unwrap_or(settings.resize_filter);
    let use_atlas = state
        .use_bc7_atlas
        .load(std::sync::atomic::Ordering::Relaxed);

    // Check SQLite cache — return as-is if format matches, otherwise regenerate
    let requested_fmt = match settings.format {
        ThumbFormat::Rgba => "rgba",
        ThumbFormat::Jpeg => "jpeg",
    };
    {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;

        if let Ok(Some(cached)) = db.get_thumbnail(&path) {
            if cached.format == requested_fmt {
                return Ok(Some(ThumbnailResult {
                    path: cached.path,
                    width: cached.width,
                    height: cached.height,
                    media_type: cached.media_type,
                    format: cached.format,
                }));
            }
            // Format mismatch — fall through to regenerate
        }
    }
    // Lock released — generate without holding the mutex

    // Generate thumbnail using configured format and dimensions
    let format = settings.format;
    let (thumb_w, thumb_h) = (settings.width, settings.height);
    let result = dispatch_thumbnail(&state.thumb_pool, path.clone(), filter, format, thumb_w, thumb_h).await?;

    match result {
        Ok(thumb) => {
            // If atlas is active, store BC7 in atlas
            if use_atlas && thumb.format == ThumbFormat::Rgba {
                let mut atlas = state.thumb_atlas.lock().await;
                if let Some(atlas) = atlas.as_mut() {
                    let _ = atlas.upsert(&thumb.path, &thumb.data, thumb.width, thumb.height, 0);
                    let _ = atlas.remap();
                }
            }

            let fmt_str = match thumb.format {
                ThumbFormat::Rgba => "rgba",
                ThumbFormat::Jpeg => "jpeg",
            };

            // Cache in SQLite in the generated format
            let db = state.cache_db.lock().await;
            if let Some(db) = db.as_ref() {
                let _ = db.upsert_thumbnail(
                    &thumb.path,
                    &thumb.media_type,
                    0,
                    thumb.width,
                    thumb.height,
                    &thumb.data,
                    fmt_str,
                    filter.as_str(),
                );
                let _ = db.conn().execute(
                    "UPDATE media_meta SET width = ?1, height = ?2 WHERE path = ?3 AND width IS NULL",
                    rusqlite::params![thumb.src_width, thumb.src_height, thumb.path],
                );
            }

            Ok(Some(ThumbnailResult {
                path: thumb.path.clone(),
                width: thumb.width,
                height: thumb.height,
                media_type: thumb.media_type.clone(),
                format: fmt_str.to_string(),
            }))
        }
        Err(e) => {
            log::warn!("Thumbnail generation failed for {}: {}", path, e);
            Ok(None)
        }
    }
}

/// Get thumbnails for a batch of paths. Uses BC7 atlas when available,
/// falls back to SQLite cache, then generates missing thumbnails.
#[tauri::command]
pub async fn get_thumbnails_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    resize_filter: Option<ResizeFilter>,
) -> Result<Vec<ThumbnailResult>, String> {
    let settings = state.thumbnail_settings.read().await.clone();
    let filter = resize_filter.unwrap_or(settings.resize_filter);
    let use_atlas = state
        .use_bc7_atlas
        .load(std::sync::atomic::Ordering::Relaxed);
    let format = settings.format;
    let (thumb_w, thumb_h) = (settings.width, settings.height);

    let mut results = Vec::with_capacity(paths.len());
    let mut uncached_paths = Vec::new();

    // Phase 1: Check SQLite cache — return as-is if format matches, otherwise regenerate
    let requested_fmt = match format {
        ThumbFormat::Rgba => "rgba",
        ThumbFormat::Jpeg => "jpeg",
    };
    {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;

        for path in &paths {
            if let Ok(Some(info)) = db.get_thumbnail_info(path) {
                let (w, h, _sz, ref fmt, _) = info;
                if fmt == requested_fmt {
                    results.push(ThumbnailResult {
                        path: path.clone(),
                        width: w,
                        height: h,
                        media_type: String::new(),
                        format: fmt.clone(),
                    });
                } else {
                    // Format mismatch — regenerate
                    uncached_paths.push(path.clone());
                }
            } else {
                uncached_paths.push(path.clone());
            }
        }
    }

    if uncached_paths.is_empty() {
        return Ok(results);
    }

    // Phase 2: Generate missing thumbnails
    // Try full GPU pipeline (crop+resize+BC7) when atlas is active,
    // otherwise use GPU crop+resize only, with CPU fallback.
    #[cfg(feature = "gpu")]
    let gpu_generated = if let Some(ref pipeline) = state.gpu_pipeline {
        if use_atlas {
            // Full chained pipeline: crop+resize → BC7 encode on GPU (zero intermediate readback)
            generate_batch_gpu_full(
                &state.thumb_pool,
                pipeline,
                &uncached_paths,
                filter,
                format,
                thumb_w,
                thumb_h,
            )
            .await
        } else {
            // Crop+resize on GPU, encode on CPU
            generate_batch_gpu(
                &state.thumb_pool,
                pipeline,
                &uncached_paths,
                filter,
                format,
                thumb_w,
                thumb_h,
            )
            .await
        }
    } else {
        None
    };

    #[cfg(not(feature = "gpu"))]
    let gpu_generated: Option<GpuBatchResult> = None;

    // Unpack GPU results or fall back to CPU
    let (generated_thumbs, gpu_bc7_data) = if let Some(gpu_result) = gpu_generated {
        (gpu_result.thumbs, gpu_result.bc7_items)
    } else {
        // CPU fallback path
        let mut handles = Vec::with_capacity(uncached_paths.len());
        for path in uncached_paths {
            handles.push(dispatch_thumbnail(&state.thumb_pool, path, filter, format, thumb_w, thumb_h));
        }
        let cpu_results = futures::future::join_all(handles).await;
        let thumbs: Vec<_> = cpu_results
            .into_iter()
            .filter_map(|r| r.ok().and_then(|r| r.ok()))
            .collect();
        (thumbs, Vec::new())
    };

    // Phase 3: Cache results and build response in the requested format
    struct CacheItem {
        path: String,
        media_type: String,
        data: Vec<u8>,
        format: String,
        resize_filter: String,
        width: u32,
        height: u32,
        src_width: u32,
        src_height: u32,
    }
    let mut to_cache = Vec::new();
    let mut rgba_to_atlas: Vec<(String, Vec<u8>, u32, u32, u64)> = Vec::new();

    for thumb in generated_thumbs {
        let fmt_str = match thumb.format {
            ThumbFormat::Rgba => "rgba".to_string(),
            ThumbFormat::Jpeg => "jpeg".to_string(),
        };
        let needs_atlas = thumb.format == ThumbFormat::Rgba && use_atlas && gpu_bc7_data.is_empty();

        // Queue RGBA for CPU BC7 encoding if atlas is active and no GPU BC7 was produced
        if needs_atlas {
            rgba_to_atlas.push((
                thumb.path.clone(),
                thumb.data.clone(),
                thumb.width,
                thumb.height,
                0,
            ));
        }

        to_cache.push(CacheItem {
            path: thumb.path.clone(),
            media_type: thumb.media_type.clone(),
            data: thumb.data,
            format: fmt_str.clone(),
            resize_filter: filter.as_str().to_string(),
            width: thumb.width,
            height: thumb.height,
            src_width: thumb.src_width,
            src_height: thumb.src_height,
        });

        results.push(ThumbnailResult {
            path: thumb.path,
            width: thumb.width,
            height: thumb.height,
            media_type: thumb.media_type,
            format: fmt_str,
        });
    }

    // Batch-write GPU-encoded BC7 directly to atlas (zero CPU BC7 encoding)
    if !gpu_bc7_data.is_empty() {
        let mut atlas = state.thumb_atlas.lock().await;
        if let Some(atlas) = atlas.as_mut() {
            let _ = atlas.upsert_bc7_raw_batch(&gpu_bc7_data);
        }
    }

    // Batch-write CPU-encoded RGBA to BC7 atlas (fallback when no GPU BC7)
    if !rgba_to_atlas.is_empty() {
        let mut atlas = state.thumb_atlas.lock().await;
        if let Some(atlas) = atlas.as_mut() {
            let _ = atlas.upsert_batch(&rgba_to_atlas);
        }
    }

    // Batch-write thumbnails to SQLite in the generated format (single transaction)
    if !to_cache.is_empty() {
        let mut db = state.cache_db.lock().await;
        if let Some(db) = db.as_mut() {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            for item in &to_cache {
                let _ = tx.execute(
                    "INSERT OR REPLACE INTO thumbnails (path, media_type, mtime, width, height, thumbnail, format, resize_filter)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![item.path, item.media_type, 0u64, item.width, item.height, item.data, item.format, item.resize_filter],
                );
                let _ = tx.execute(
                    "UPDATE media_meta SET width = ?1, height = ?2 WHERE path = ?3 AND width IS NULL",
                    rusqlite::params![item.src_width, item.src_height, item.path],
                );
            }
            let _ = tx.commit();
        }
    }

    Ok(results)
}

/// Result from GPU batch thumbnail generation.
#[cfg(feature = "gpu")]
struct GpuBatchResult {
    /// Generated thumbnails (with JPEG data for IPC).
    thumbs: Vec<crate::pipeline::thumbnailer::ThumbResult>,
    /// Pre-encoded BC7 data for atlas storage: (path, bc7_data, w, h, mtime).
    bc7_items: Vec<(String, Vec<u8>, u32, u32, u64)>,
}

#[cfg(not(feature = "gpu"))]
struct GpuBatchResult {
    thumbs: Vec<crate::pipeline::thumbnailer::ThumbResult>,
    bc7_items: Vec<(String, Vec<u8>, u32, u32, u64)>,
}

/// GPU batch pipeline (crop+resize only, no BC7):
/// 1. Decode on CPU (rayon) — no CPU crop
/// 2. Fused crop+resize on GPU
/// 3. Encode to JPEG on CPU (rayon)
#[cfg(feature = "gpu")]
async fn generate_batch_gpu(
    pool: &rayon::ThreadPool,
    pipeline: &std::sync::Arc<crate::pipeline::gpu_pipeline::GpuPipeline>,
    paths: &[String],
    filter: ResizeFilter,
    format: ThumbFormat,
    thumb_size_w: u32,
    thumb_size_h: u32,
) -> Option<GpuBatchResult> {
    use crate::pipeline::gpu_pipeline::CropResizeInput;
    use crate::pipeline::thumbnailer::{
        decode_image, encode_rgba_to_jpeg, DecodedImage, ThumbFormat, ThumbResult,
    };

    // Phase 1: Decode on CPU (rayon)
    let (tx, rx) = tokio::sync::oneshot::channel();
    let paths_owned: Vec<String> = paths.to_vec();
    pool.spawn(move || {
        let decoded: Vec<Option<DecodedImage>> = paths_owned
            .iter()
            .map(|p| decode_image(std::path::Path::new(p)).ok())
            .collect();
        let _ = tx.send(decoded);
    });
    let decoded = rx.await.ok()?;

    let mut gpu_inputs = Vec::with_capacity(decoded.len());
    let mut gpu_indices = Vec::with_capacity(decoded.len());
    for (i, img) in decoded.iter().enumerate() {
        if let Some(d) = img {
            gpu_inputs.push(CropResizeInput {
                rgba_data: d.rgba.clone(),
                width: d.width,
                height: d.height,
                crop_x: d.crop_x,
                crop_y: d.crop_y,
                crop_size: d.crop_size,
            });
            gpu_indices.push(i);
        }
    }

    if gpu_inputs.is_empty() {
        return Some(GpuBatchResult {
            thumbs: Vec::new(),
            bc7_items: Vec::new(),
        });
    }

    // Phase 2: GPU fused crop+resize
    let bilinear = !matches!(filter, ResizeFilter::Nearest);
    let pipeline_clone = pipeline.clone();
    let (tx2, rx2) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let resized =
            pipeline_clone.crop_resize_batch(&gpu_inputs, thumb_size_w, thumb_size_h, bilinear);
        let _ = tx2.send(resized);
    });
    let resized = rx2.await.ok()?;

    // Phase 3: Encode output on CPU (rayon) — JPEG or raw RGBA depending on format
    let (tx3, rx3) = tokio::sync::oneshot::channel();
    let decoded_ref: Vec<Option<DecodedImage>> = decoded;
    pool.spawn(move || {
        let mut results = Vec::new();
        for (gpu_idx, resize_out) in resized.into_iter().enumerate() {
            let orig_idx = gpu_indices[gpu_idx];
            if let (Some(resized), Some(img)) = (resize_out, &decoded_ref[orig_idx]) {
                match format {
                    ThumbFormat::Jpeg => {
                        match encode_rgba_to_jpeg(&resized.rgba_data, resized.width, resized.height) {
                            Ok(jpeg_data) => {
                                results.push(ThumbResult {
                                    path: img.path.clone(),
                                    width: resized.width,
                                    height: resized.height,
                                    data: jpeg_data,
                                    media_type: img.media_type.clone(),
                                    src_width: img.src_width,
                                    src_height: img.src_height,
                                    format: ThumbFormat::Jpeg,
                                });
                            }
                            Err(e) => {
                                log::warn!("JPEG encode failed for {}: {}", img.path, e);
                            }
                        }
                    }
                    ThumbFormat::Rgba => {
                        results.push(ThumbResult {
                            path: img.path.clone(),
                            width: resized.width,
                            height: resized.height,
                            data: resized.rgba_data,
                            media_type: img.media_type.clone(),
                            src_width: img.src_width,
                            src_height: img.src_height,
                            format: ThumbFormat::Rgba,
                        });
                    }
                }
            }
        }
        let _ = tx3.send(results);
    });

    let thumbs = rx3.await.ok()?;
    Some(GpuBatchResult {
        thumbs,
        bc7_items: Vec::new(),
    })
}

/// Full GPU chained pipeline (crop+resize → BC7 encode, zero intermediate readback):
/// 1. Decode on CPU (rayon) — no CPU crop
/// 2. Fused crop+resize on GPU → BC7 encode on GPU (single submission)
/// 3. Readback BC7 + RGBA, encode JPEG on CPU (rayon)
#[cfg(feature = "gpu")]
async fn generate_batch_gpu_full(
    pool: &rayon::ThreadPool,
    pipeline: &std::sync::Arc<crate::pipeline::gpu_pipeline::GpuPipeline>,
    paths: &[String],
    filter: ResizeFilter,
    format: ThumbFormat,
    thumb_size_w: u32,
    thumb_size_h: u32,
) -> Option<GpuBatchResult> {
    use crate::pipeline::gpu_pipeline::CropResizeInput;
    use crate::pipeline::thumbnailer::{
        decode_image, encode_rgba_to_jpeg, DecodedImage, ThumbFormat, ThumbResult,
    };

    // Phase 1: Decode on CPU (rayon)
    let (tx, rx) = tokio::sync::oneshot::channel();
    let paths_owned: Vec<String> = paths.to_vec();
    pool.spawn(move || {
        let decoded: Vec<Option<DecodedImage>> = paths_owned
            .iter()
            .map(|p| decode_image(std::path::Path::new(p)).ok())
            .collect();
        let _ = tx.send(decoded);
    });
    let decoded = rx.await.ok()?;

    let mut gpu_inputs = Vec::with_capacity(decoded.len());
    let mut gpu_indices = Vec::with_capacity(decoded.len());
    for (i, img) in decoded.iter().enumerate() {
        if let Some(d) = img {
            gpu_inputs.push(CropResizeInput {
                rgba_data: d.rgba.clone(),
                width: d.width,
                height: d.height,
                crop_x: d.crop_x,
                crop_y: d.crop_y,
                crop_size: d.crop_size,
            });
            gpu_indices.push(i);
        }
    }

    if gpu_inputs.is_empty() {
        return Some(GpuBatchResult {
            thumbs: Vec::new(),
            bc7_items: Vec::new(),
        });
    }

    // Phase 2: Full GPU pipeline — crop+resize → BC7 encode in one submission
    // Also readback RGBA for JPEG encoding (needed for IPC + SQLite cache)
    let bilinear = !matches!(filter, ResizeFilter::Nearest);
    let pipeline_clone = pipeline.clone();
    let (tx2, rx2) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let gpu_results = pipeline_clone.generate_thumbnails_batch(
            &gpu_inputs,
            thumb_size_w,
            thumb_size_h,
            bilinear,
            true, // need RGBA readback for JPEG encoding
        );
        let _ = tx2.send(gpu_results);
    });
    let gpu_results = rx2.await.ok()?;

    // Phase 3: Encode JPEG on CPU (rayon) and collect BC7 for atlas
    let (tx3, rx3) = tokio::sync::oneshot::channel();
    let decoded_ref: Vec<Option<DecodedImage>> = decoded;
    pool.spawn(move || {
        let mut thumbs = Vec::new();
        let mut bc7_items = Vec::new();

        for (gpu_idx, gpu_out) in gpu_results.into_iter().enumerate() {
            let orig_idx = gpu_indices[gpu_idx];
            if let (Some(result), Some(img)) = (gpu_out, &decoded_ref[orig_idx]) {
                // Store BC7 data for atlas
                bc7_items.push((
                    img.path.clone(),
                    result.bc7_data,
                    result.width,
                    result.height,
                    0u64,
                ));

                // Encode output from RGBA readback for IPC
                match format {
                    ThumbFormat::Jpeg => {
                        match encode_rgba_to_jpeg(&result.rgba_data, result.width, result.height) {
                            Ok(jpeg_data) => {
                                thumbs.push(ThumbResult {
                                    path: img.path.clone(),
                                    width: result.width,
                                    height: result.height,
                                    data: jpeg_data,
                                    media_type: img.media_type.clone(),
                                    src_width: img.src_width,
                                    src_height: img.src_height,
                                    format: ThumbFormat::Jpeg,
                                });
                            }
                            Err(e) => {
                                log::warn!("JPEG encode failed for {}: {}", img.path, e);
                            }
                        }
                    }
                    ThumbFormat::Rgba => {
                        thumbs.push(ThumbResult {
                            path: img.path.clone(),
                            width: result.width,
                            height: result.height,
                            data: result.rgba_data,
                            media_type: img.media_type.clone(),
                            src_width: img.src_width,
                            src_height: img.src_height,
                            format: ThumbFormat::Rgba,
                        });
                    }
                }
            }
        }
        let _ = tx3.send((thumbs, bc7_items));
    });

    let (thumbs, bc7_items) = rx3.await.ok()?;
    Some(GpuBatchResult { thumbs, bc7_items })
}

/// Dispatch a single thumbnail generation task to the dedicated thread pool.
/// Returns a future that resolves when the pool thread finishes.
async fn dispatch_thumbnail(
    pool: &rayon::ThreadPool,
    path: String,
    filter: ResizeFilter,
    format: ThumbFormat,
    thumb_w: u32,
    thumb_h: u32,
) -> Result<Result<crate::pipeline::thumbnailer::ThumbResult, crate::pipeline::thumbnailer::ThumbError>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    pool.spawn(move || {
        let p = std::path::Path::new(&path);
        let result = crate::pipeline::thumbnailer::generate_for_path(p, filter, format, thumb_w, thumb_h);
        let _ = tx.send(result);
    });

    rx.await.map_err(|_| "Thumbnail task was dropped".to_string())
}

/// Regenerate a single thumbnail, bypassing all caches.
#[tauri::command]
pub async fn regenerate_thumbnail(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let settings = state.thumbnail_settings.read().await.clone();
    let filter = settings.resize_filter;
    let format = settings.format;
    let (thumb_w, thumb_h) = (settings.width, settings.height);

    // Remove from SQLite cache
    {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let _ = db.conn().execute(
                "DELETE FROM thumbnails WHERE path = ?1",
                rusqlite::params![path],
            );
        }
    }

    // Generate fresh
    let result = dispatch_thumbnail(&state.thumb_pool, path.clone(), filter, format, thumb_w, thumb_h).await?;

    match result {
        Ok(thumb) => {
            let use_atlas = state.use_bc7_atlas.load(std::sync::atomic::Ordering::Relaxed);
            if use_atlas && thumb.format == ThumbFormat::Rgba {
                let mut atlas = state.thumb_atlas.lock().await;
                if let Some(atlas) = atlas.as_mut() {
                    let _ = atlas.upsert(&thumb.path, &thumb.data, thumb.width, thumb.height, 0);
                    let _ = atlas.remap();
                }
            }
            let fmt_str = match thumb.format {
                ThumbFormat::Rgba => "rgba",
                ThumbFormat::Jpeg => "jpeg",
            };
            let db = state.cache_db.lock().await;
            if let Some(db) = db.as_ref() {
                let _ = db.upsert_thumbnail(
                    &thumb.path,
                    &thumb.media_type,
                    0,
                    thumb.width,
                    thumb.height,
                    &thumb.data,
                    fmt_str,
                    filter.as_str(),
                );
            }
            Ok(())
        }
        Err(e) => Err(format!("Thumbnail generation failed: {}", e)),
    }
}

/// Get the full-resolution media file as a base64-encoded data URI.
#[tauri::command]
pub async fn get_full_media(
    state: tauri::State<'_, AppState>,
    path: String,
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

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "tiff" | "tif" => "image/tiff",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    };

    let b64 = encode_b64(&data);
    Ok(format!("data:{};base64,{}", mime, b64))
}

/// Get metadata for a media file from the media_meta cache table.
#[tauri::command]
pub async fn get_media_meta(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Option<MediaMeta>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let mut stmt = db
        .conn()
        .prepare_cached(
            "SELECT path, media_type, file_size, date_taken, width, height, duration FROM media_meta WHERE path = ?1",
        )
        .map_err(|e| e.to_string())?;

    let mut rows = stmt.query(rusqlite::params![path]).map_err(|e| e.to_string())?;

    match rows.next().map_err(|e| e.to_string())? {
        Some(row) => Ok(Some(MediaMeta {
            path: row.get(0).map_err(|e| e.to_string())?,
            media_type: row.get(1).map_err(|e| e.to_string())?,
            file_size: row.get(2).map_err(|e| e.to_string())?,
            date_taken: row.get(3).map_err(|e| e.to_string())?,
            width: row.get(4).map_err(|e| e.to_string())?,
            height: row.get(5).map_err(|e| e.to_string())?,
            duration_seconds: row.get(6).map_err(|e| e.to_string())?,
        })),
        None => Ok(None),
    }
}

/// Result of background precaching — no blob data returned over IPC.
#[derive(Debug, Serialize)]
pub struct PrecacheResult {
    /// Number of thumbnails successfully generated and cached.
    pub generated: usize,
    /// Paths that failed generation (caller can retry later).
    pub failed: Vec<String>,
}

/// Pre-generate and cache thumbnails without returning blob data.
/// Designed for background prefetch: generates missing thumbnails so they're
/// available instantly from cache when the user scrolls to them.
#[tauri::command]
pub async fn precache_thumbnails(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<PrecacheResult, String> {
    let settings = state.thumbnail_settings.read().await.clone();
    let filter = settings.resize_filter;
    let format = settings.format;
    let (thumb_w, thumb_h) = (settings.width, settings.height);
    let use_atlas = state
        .use_bc7_atlas
        .load(std::sync::atomic::Ordering::Relaxed);

    // Filter to only paths that are uncached or have a format mismatch
    let requested_fmt = match format {
        ThumbFormat::Rgba => "rgba",
        ThumbFormat::Jpeg => "jpeg",
    };
    let uncached: Vec<String> = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        paths
            .into_iter()
            .filter(|p| {
                match db.get_thumbnail_info(p) {
                    Ok(Some((_w, _h, _sz, ref fmt, _))) => fmt != requested_fmt,
                    _ => true,
                }
            })
            .collect()
    };

    if uncached.is_empty() {
        return Ok(PrecacheResult {
            generated: 0,
            failed: Vec::new(),
        });
    }

    // Generate on the thumbnail thread pool
    let mut handles = Vec::with_capacity(uncached.len());
    for path in &uncached {
        handles.push(dispatch_thumbnail(
            &state.thumb_pool,
            path.clone(),
            filter,
            format,
            thumb_w,
            thumb_h,
        ));
    }
    let results = futures::future::join_all(handles).await;

    let mut generated = 0usize;
    let mut failed = Vec::new();
    let mut to_cache = Vec::new();

    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(Ok(thumb)) => {
                let fmt_str = match thumb.format {
                    ThumbFormat::Rgba => "rgba",
                    ThumbFormat::Jpeg => "jpeg",
                };

                if use_atlas && thumb.format == ThumbFormat::Rgba {
                    let mut atlas = state.thumb_atlas.lock().await;
                    if let Some(atlas) = atlas.as_mut() {
                        let _ = atlas.upsert(
                            &thumb.path,
                            &thumb.data,
                            thumb.width,
                            thumb.height,
                            0,
                        );
                    }
                }

                to_cache.push((
                    thumb.path,
                    thumb.media_type,
                    thumb.width,
                    thumb.height,
                    thumb.data,
                    fmt_str.to_string(),
                    thumb.src_width,
                    thumb.src_height,
                ));
                generated += 1;
            }
            _ => {
                failed.push(uncached[i].clone());
            }
        }
    }

    // Batch write to SQLite (single transaction)
    if !to_cache.is_empty() {
        let mut db = state.cache_db.lock().await;
        if let Some(db) = db.as_mut() {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            for item in &to_cache {
                let _ = tx.execute(
                    "INSERT OR REPLACE INTO thumbnails (path, media_type, mtime, width, height, thumbnail, format, resize_filter)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![item.0, item.1, 0u64, item.2, item.3, item.4, item.5, filter.as_str()],
                );
                let _ = tx.execute(
                    "UPDATE media_meta SET width = ?1, height = ?2 WHERE path = ?3 AND width IS NULL",
                    rusqlite::params![item.6, item.7, item.0],
                );
            }
            let _ = tx.commit();
        }
    }

    Ok(PrecacheResult { generated, failed })
}

#[derive(Debug, Serialize)]
pub struct CachedThumbnailInfo {
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    pub format: String,
    pub resize_filter: String,
}

/// Get metadata about the cached thumbnail for a specific file (without reading the blob).
#[tauri::command]
pub async fn get_cached_thumbnail_info(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Option<CachedThumbnailInfo>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    match db.get_thumbnail_info(&path) {
        Ok(Some((width, height, size_bytes, format, resize_filter))) => Ok(Some(CachedThumbnailInfo {
            width,
            height,
            size_bytes,
            format,
            resize_filter,
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
