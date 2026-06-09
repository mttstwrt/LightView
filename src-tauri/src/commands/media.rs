use base64::Engine;
use serde::Serialize;
use std::path::Path;
use tauri::Emitter;

use crate::cache::thumbnails::ThumbTier;
use crate::pipeline::thumbnailer::{self, ResizeFilter, ThumbFormat, STANDARD_THUMB_SIZE};
use crate::AppState;

/// Thumbnail metadata returned over IPC. No pixel data — the frontend
/// fetches actual image bytes via the `lightview://thumb/` protocol.
#[derive(Debug, Clone, Serialize)]
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
    pub rating: Option<u8>,
    pub last_rated: Option<i64>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
}

fn encode_b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Get a single thumbnail. Checks the SQLite cache, then generates on-demand.
#[tauri::command]
pub async fn get_thumbnail(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Option<ThumbnailResult>, String> {
    let format = ThumbFormat::Jpeg;
    let filter = thumbnailer::filter_for_size(STANDARD_THUMB_SIZE);
    let thumb_size = STANDARD_THUMB_SIZE;

    // Check SQLite cache — return as-is if format matches, otherwise regenerate
    let requested_fmt = format.as_cache_str();
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

    // Generate thumbnail using tier-derived format and dimensions
    let result = dispatch_thumbnail(&state.thumb_pool, path.clone(), filter, format, thumb_size, thumb_size).await?;

    match result {
        Ok(thumb) => {
            let fmt_str = thumb.format.as_cache_str();

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

                // Extract and store video metadata (duration, dimensions, codec)
                if thumb.media_type == "video" {
                    populate_video_metadata(db.conn(), &thumb.path);
                }
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

/// Get thumbnails for a batch of paths. Serves from the SQLite cache,
/// then generates missing thumbnails.
/// Emits `thumb:streamed` events as individual thumbnails complete.
#[tauri::command]
pub async fn get_thumbnails_batch(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ThumbnailResult>, String> {
    let format = ThumbFormat::Jpeg;
    let filter = thumbnailer::filter_for_size(STANDARD_THUMB_SIZE);
    let thumb_w = STANDARD_THUMB_SIZE;
    let thumb_h = STANDARD_THUMB_SIZE;

    let mut results = Vec::with_capacity(paths.len());
    let mut uncached_paths = Vec::new();
    // Paths that have a standard thumbnail but are missing micro/thumbhash rows.
    // These need derivation from the cached standard-tier bytes.
    let mut micro_missing: Vec<String> = Vec::new();

    // Phase 1: Check SQLite cache — return as-is if format matches, otherwise regenerate
    let requested_fmt = format.as_cache_str();
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
                    // Check if micro tier is missing for this cached path
                    if !db.tier_is_cached(ThumbTier::Micro, path).unwrap_or(false) {
                        micro_missing.push(path.clone());
                    }
                } else {
                    // Format mismatch — regenerate
                    uncached_paths.push(path.clone());
                }
            } else {
                uncached_paths.push(path.clone());
            }
        }
    }

    // Derive micro + thumbhash for cached paths that are missing them
    if !micro_missing.is_empty() {
        derive_micro_for_cached(&state, &micro_missing).await;
    }

    if uncached_paths.is_empty() {
        return Ok(results);
    }

    // Phase 2: Generate missing thumbnails — GPU crop+resize with CPU fallback.
    #[cfg(feature = "gpu")]
    let gpu_generated = if let Some(ref pipeline) = state.gpu_pipeline {
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
    } else {
        None
    };

    #[cfg(not(feature = "gpu"))]
    let gpu_generated: Option<Vec<crate::pipeline::thumbnailer::ThumbResult>> = None;

    // Unpack GPU results or fall back to CPU
    let generated_thumbs = if let Some(thumbs) = gpu_generated {
        thumbs
    } else {
        // CPU fallback path — stream results as each thumbnail completes
        use futures::stream::{FuturesUnordered, StreamExt};
        let mut futures_set = FuturesUnordered::new();
        for path in uncached_paths {
            futures_set.push(dispatch_thumbnail(&state.thumb_pool, path, filter, format, thumb_w, thumb_h));
        }
        let mut thumbs = Vec::new();
        while let Some(result) = futures_set.next().await {
            if let Ok(Ok(ref thumb)) = result {
                // Emit streamed event immediately so frontend can update
                let _ = app_handle.emit("thumb:streamed", ThumbnailResult {
                    path: thumb.path.clone(),
                    width: thumb.width,
                    height: thumb.height,
                    media_type: thumb.media_type.clone(),
                    format: thumb.format.as_cache_str().to_string(),
                });
            }
            if let Ok(Ok(thumb)) = result {
                thumbs.push(thumb);
            }
        }
        thumbs
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

    for thumb in generated_thumbs {
        let fmt_str = thumb.format.as_cache_str().to_string();

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

    // -------------------------------------------------------------------
    // Derive ThumbHash + micro (T1) tier bytes in parallel on the CPU pool.
    // Done BEFORE the SQLite transaction so the inserts can include the
    // thumbhash blob and the micro row in the same commit path.
    // -------------------------------------------------------------------
    struct DerivedExtras {
        path: String,
        thumbhash: Option<Vec<u8>>,
        micro_bytes: Option<Vec<u8>>,
        micro_w: u32,
        micro_h: u32,
    }

    let derived_extras: Vec<DerivedExtras> = {
        use rayon::prelude::*;
        // Capture what the rayon closure needs, without borrowing `to_cache`.
        let inputs: Vec<(String, Vec<u8>, u32, u32)> = to_cache
            .iter()
            .map(|item| {
                (
                    item.path.clone(),
                    item.data.clone(),
                    item.width,
                    item.height,
                )
            })
            .collect();

        let (tx, rx) = tokio::sync::oneshot::channel();
        state.thumb_pool.spawn(move || {
            let out: Vec<DerivedExtras> = inputs
                .into_par_iter()
                .map(|(path, data, w, h)| {
                    let rgba = match thumbnailer::decode_thumb_bytes_to_rgba(&data) {
                        Ok(r) => r,
                        Err(_) => {
                            return DerivedExtras {
                                path,
                                thumbhash: None,
                                micro_bytes: None,
                                micro_w: 0,
                                micro_h: 0,
                            };
                        }
                    };

                    // ThumbHash — ~25-byte compact placeholder (P1).
                    let thumbhash = thumbnailer::compute_thumbhash(&rgba, w, h).ok();

                    // Micro tier — 128x128 JPEG from the standard-tier RGBA (P2).
                    let micro_target = 128u32;
                    let (micro_bytes, micro_w, micro_h) = match thumbnailer::downsample_rgba_square(
                        &rgba,
                        w,
                        h,
                        micro_target,
                    ) {
                        Ok(resized) => match thumbnailer::encode_rgba_to_jpeg_tier(
                            &resized,
                            micro_target,
                            micro_target,
                        ) {
                            Ok(jpeg) => (Some(jpeg), micro_target, micro_target),
                            Err(_) => (None, 0, 0),
                        },
                        Err(_) => (None, 0, 0),
                    };

                    DerivedExtras {
                        path,
                        thumbhash,
                        micro_bytes,
                        micro_w,
                        micro_h,
                    }
                })
                .collect();
            let _ = tx.send(out);
        });
        rx.await.unwrap_or_default()
    };

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

            // Write ThumbHash + micro tier rows in the same transaction so
            // queries never observe a split state (standard row present,
            // placeholder/micro missing).
            for extra in &derived_extras {
                if let Some(hash) = &extra.thumbhash {
                    let _ = tx.execute(
                        "UPDATE thumbnails SET thumbhash = ?1 WHERE path = ?2",
                        rusqlite::params![hash, extra.path],
                    );
                }
                if let Some(bytes) = &extra.micro_bytes {
                    // Find matching to_cache entry for media_type
                    let media_type = to_cache
                        .iter()
                        .find(|c| c.path == extra.path)
                        .map(|c| c.media_type.clone())
                        .unwrap_or_default();
                    let _ = tx.execute(
                        "INSERT OR REPLACE INTO thumbnails_micro (path, media_type, mtime, width, height, thumbnail, format)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            extra.path,
                            media_type,
                            0u64,
                            extra.micro_w,
                            extra.micro_h,
                            bytes,
                            "jpeg"
                        ],
                    );
                }
            }

            let _ = tx.commit();

            // Extract video metadata outside the transaction
            for item in &to_cache {
                if item.media_type == "video" {
                    populate_video_metadata(db.conn(), &item.path);
                }
            }
        }
    }

    Ok(results)
}

/// GPU batch pipeline:
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
) -> Option<Vec<crate::pipeline::thumbnailer::ThumbResult>> {
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
        return Some(Vec::new());
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

    // Phase 3: Encode output on CPU (rayon)
    let (tx3, rx3) = tokio::sync::oneshot::channel();
    let decoded_ref: Vec<Option<DecodedImage>> = decoded;
    pool.spawn(move || {
        let mut results = Vec::new();
        for (gpu_idx, resize_out) in resized.into_iter().enumerate() {
            let orig_idx = gpu_indices[gpu_idx];
            if let (Some(resized), Some(img)) = (resize_out, &decoded_ref[orig_idx]) {
                let encoded = match format {
                    ThumbFormat::Jpeg => encode_rgba_to_jpeg(
                        &resized.rgba_data,
                        resized.width,
                        resized.height,
                    ),
                    ThumbFormat::Webp => crate::pipeline::thumbnailer::encode_rgba_to_webp(
                        &resized.rgba_data,
                        resized.width,
                        resized.height,
                    ),
                };
                match encoded {
                    Ok(data) => {
                        results.push(ThumbResult {
                            path: img.path.clone(),
                            width: resized.width,
                            height: resized.height,
                            data,
                            media_type: img.media_type.clone(),
                            src_width: img.src_width,
                            src_height: img.src_height,
                            format,
                        });
                    }
                    Err(e) => {
                        log::warn!("Encode failed for {}: {}", img.path, e);
                    }
                }
            }
        }
        let _ = tx3.send(results);
    });

    rx3.await.ok()
}

/// Dispatch a single thumbnail generation task to the dedicated thread pool.
/// Returns a future that resolves when the pool thread finishes.
pub(crate) async fn dispatch_thumbnail(
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

/// Derive micro-tier (128px) thumbnails + thumbhash for cached standard-tier
/// paths that are missing them. Reads the standard thumbnail bytes from
/// SQLite, decodes to RGBA, downsamples, and writes the micro row + thumbhash.
async fn derive_micro_for_cached(state: &AppState, paths: &[String]) {
    // Read standard-tier bytes from DB
    let items: Vec<(String, Vec<u8>, u32, u32, String)> = {
        let db = state.cache_db.lock().await;
        let Some(db) = db.as_ref() else { return };
        paths
            .iter()
            .filter_map(|path| {
                let row = db.get_thumbnail(path).ok()??;
                Some((path.clone(), row.thumbnail, row.width, row.height, row.media_type))
            })
            .collect()
    };

    if items.is_empty() {
        return;
    }

    // Derive micro + thumbhash on the CPU pool
    struct Derived {
        path: String,
        media_type: String,
        thumbhash: Option<Vec<u8>>,
        micro_bytes: Option<Vec<u8>>,
        micro_size: u32,
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    state.thumb_pool.spawn(move || {
        use rayon::prelude::*;
        let out: Vec<Derived> = items
            .into_par_iter()
            .map(|(path, data, w, h, media_type)| {
                let rgba = match thumbnailer::decode_thumb_bytes_to_rgba(&data) {
                    Ok(r) => r,
                    Err(_) => {
                        return Derived {
                            path,
                            media_type,
                            thumbhash: None,
                            micro_bytes: None,
                            micro_size: 0,
                        };
                    }
                };

                let thumbhash = thumbnailer::compute_thumbhash(&rgba, w, h).ok();

                let micro_target = ThumbTier::Micro.target_size();
                let micro_bytes = thumbnailer::downsample_rgba_square(&rgba, w, h, micro_target)
                    .ok()
                    .and_then(|resized| {
                        thumbnailer::encode_rgba_to_jpeg_tier(&resized, micro_target, micro_target).ok()
                    });

                Derived {
                    path,
                    media_type,
                    thumbhash,
                    micro_bytes,
                    micro_size: micro_target,
                }
            })
            .collect();
        let _ = tx.send(out);
    });

    let derived = match rx.await {
        Ok(d) => d,
        Err(_) => return,
    };

    // Write to DB
    let mut db = state.cache_db.lock().await;
    let Some(db) = db.as_mut() else { return };
    let Ok(txn) = db.transaction() else { return };
    for item in &derived {
        if let Some(hash) = &item.thumbhash {
            let _ = txn.execute(
                "UPDATE thumbnails SET thumbhash = ?1 WHERE path = ?2 AND thumbhash IS NULL",
                rusqlite::params![hash, item.path],
            );
        }
        if let Some(bytes) = &item.micro_bytes {
            let _ = txn.execute(
                "INSERT OR IGNORE INTO thumbnails_micro (path, media_type, mtime, width, height, thumbnail, format)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    item.path,
                    item.media_type,
                    0u64,
                    item.micro_size,
                    item.micro_size,
                    bytes,
                    "jpeg"
                ],
            );
        }
    }
    let _ = txn.commit();
    log::info!(
        "Derived micro tier for {} cached thumbnails ({} paths checked)",
        derived.iter().filter(|d| d.micro_bytes.is_some()).count(),
        paths.len(),
    );
}

/// Regenerate a single thumbnail, bypassing all caches. Clears every tier
/// (micro/standard/large/preview) so stale rows don't survive when the
/// gallery is rendered at a different cell size, then regenerates the
/// standard tier eagerly. Emits `thumb:regenerated` so the frontend can
/// cache-bust its `<img>` URL — without that, WebKit's image cache keeps
/// serving the old bytes until the app restarts.
#[tauri::command]
pub async fn regenerate_thumbnail(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let format = ThumbFormat::Jpeg;
    let filter = thumbnailer::filter_for_size(STANDARD_THUMB_SIZE);
    let thumb_size = STANDARD_THUMB_SIZE;

    // Remove from every tier so a re-render at any cell size pulls fresh bytes.
    {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            for table in ["thumbnails", "thumbnails_micro", "thumbnails_large", "thumbnails_preview"] {
                let _ = db.conn().execute(
                    &format!("DELETE FROM {} WHERE path = ?1", table),
                    rusqlite::params![path],
                );
            }
        }
    }

    // Generate fresh
    let result = dispatch_thumbnail(&state.thumb_pool, path.clone(), filter, format, thumb_size, thumb_size).await?;

    match result {
        Ok(thumb) => {
            let fmt_str = thumb.format.as_cache_str();
            {
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
            }
            let _ = app_handle.emit("thumb:regenerated", ThumbnailResult {
                path: thumb.path,
                width: thumb.width,
                height: thumb.height,
                media_type: thumb.media_type,
                format: fmt_str.to_string(),
            });
            Ok(())
        }
        Err(e) => Err(format!("Thumbnail generation failed: {}", e)),
    }
}

/// Get the full-resolution media file as a base64-encoded data URI.
///
/// **Prefer the `lightview://media/` protocol** for loading images — it serves
/// binary data directly without the ~33% Base64 inflation overhead.  This
/// command exists as a fallback and imposes a 100 MB file-size limit to
/// prevent OOM crashes from Base64-encoding very large files.
#[tauri::command]
pub async fn get_full_media(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    /// Maximum file size allowed through this IPC path (100 MB).
    /// Base64 inflates by ~33%, so a 100 MB file becomes ~133 MB of string.
    const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

    let gallery_path = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;

    // Check file size before reading
    let file_size = std::fs::metadata(&path)
        .map(|m| m.len())
        .unwrap_or(0);
    if file_size > MAX_FILE_SIZE {
        return Err(format!(
            "File too large for IPC ({:.1} MB). Use the lightview://media/ protocol instead.",
            file_size as f64 / (1024.0 * 1024.0)
        ));
    }

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // HEIC/HEIF: transcode to JPEG since browsers can't render HEIC natively.
    // The transcode cache reads the file itself and is keyed by (path, mtime),
    // so we skip the provider read on this path.
    if ext == "heic" || ext == "heif" {
        let src_path = std::path::Path::new(&path);
        let jpeg_data = crate::pipeline::heic_cache::get_or_transcode(src_path)
            .map_err(|e| format!("HEIC transcode failed: {}", e))?;
        let b64 = encode_b64(&jpeg_data);
        return Ok(format!("data:image/jpeg;base64,{}", b64));
    }

    let reg = state.providers.read().await;
    let provider = reg.get(&gallery_path).ok_or("Provider not found")?;

    let data = provider
        .read_file(&path)
        .await
        .map_err(|e| e.to_string())?;

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
    get_media_meta_impl(&state, path).await
}

pub async fn get_media_meta_impl(
    state: &AppState,
    path: String,
) -> Result<Option<MediaMeta>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let mut stmt = db
        .conn()
        .prepare_cached(
            "SELECT path, media_type, file_size, date_taken, width, height, duration, rating, last_rated, gps_lat, gps_lon FROM media_meta WHERE path = ?1",
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
            rating: row.get(7).map_err(|e| e.to_string())?,
            last_rated: row.get(8).map_err(|e| e.to_string())?,
            gps_lat: row.get(9).map_err(|e| e.to_string())?,
            gps_lon: row.get(10).map_err(|e| e.to_string())?,
        })),
        None => Ok(None),
    }
}

/// Extract video metadata via ffprobe and store it in the media_meta table.
fn populate_video_metadata(conn: &rusqlite::Connection, path: &str) {
    match thumbnailer::probe_video_metadata(Path::new(path)) {
        Ok((w, h, duration, _codec, _has_audio, _fps)) => {
            let _ = conn.execute(
                "UPDATE media_meta SET width = ?1, height = ?2, duration = ?3 WHERE path = ?4",
                rusqlite::params![w, h, duration, path],
            );
        }
        Err(e) => {
            log::debug!("Video metadata extraction failed for {}: {}", path, e);
        }
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
    let format = ThumbFormat::Jpeg;
    let filter = thumbnailer::filter_for_size(STANDARD_THUMB_SIZE);
    let thumb_w = STANDARD_THUMB_SIZE;
    let thumb_h = STANDARD_THUMB_SIZE;

    // Filter to only paths that are uncached or have a format mismatch
    let requested_fmt = format.as_cache_str();
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
                let fmt_str = thumb.format.as_cache_str();

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

            // Extract video metadata outside the transaction
            for item in &to_cache {
                if item.1 == "video" {
                    populate_video_metadata(db.conn(), &item.0);
                }
            }
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

#[derive(Debug, Serialize)]
pub struct ThumbnailTierInfo {
    pub tier: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    pub format: String,
    pub resize_filter: Option<String>,
}

/// Get metadata about all cached thumbnail tiers for a specific file.
#[tauri::command]
pub async fn get_all_thumbnail_tiers(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Vec<ThumbnailTierInfo>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let tiers = db.get_all_tier_info(&path).map_err(|e| e.to_string())?;
    Ok(tiers
        .into_iter()
        .map(|(tier, width, height, size_bytes, format, resize_filter)| ThumbnailTierInfo {
            tier,
            width,
            height,
            size_bytes,
            format,
            resize_filter,
        })
        .collect())
}

/// Entry returned by `get_thumbhashes` — one per requested path.
/// `hash` is the base64-encoded ThumbHash blob (~32 chars for ~25 bytes),
/// or None if no hash has been computed yet. The frontend decodes these
/// into tiny bitmaps and uses them as skeleton placeholders until the
/// real thumbnail streams in.
#[derive(Debug, Clone, Serialize)]
pub struct ThumbHashResult {
    pub path: String,
    pub hash: Option<String>,
}

/// Bulk-fetch ThumbHash placeholders for a list of paths. Returned in the
/// same order as the input — missing entries become `{ path, hash: null }`.
/// This is a pure-metadata command (~25 bytes per hash); the frontend
/// decodes each hash into a 32x32 bitmap locally.
#[tauri::command]
pub async fn get_thumbhashes(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ThumbHashResult>, String> {
    get_thumbhashes_impl(&state, paths).await
}

pub async fn get_thumbhashes_impl(
    state: &AppState,
    paths: Vec<String>,
) -> Result<Vec<ThumbHashResult>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let blobs = db.get_thumbhashes(&paths).map_err(|e| e.to_string())?;
    let out = paths
        .into_iter()
        .zip(blobs.into_iter())
        .map(|(path, blob)| ThumbHashResult {
            path,
            hash: blob.map(|b| encode_b64(&b)),
        })
        .collect();
    Ok(out)
}

/// Lazily generate high-resolution tier thumbnails (L/P). This is the
/// entry point for P3 (1024 grid tier) and P5 (1600 viewer preview tier).
/// Re-decodes each source image at the tier's target size — these tiers
/// are too large to derive from the 512 px standard thumbnail.
///
/// Paths that already have a row in the tier table are skipped. Paths
/// that fail to generate are silently dropped (the frontend will fall
/// back to the next lower tier).
///
/// Returns the number of thumbnails successfully written. The frontend
/// should retry its `lightview://thumb/<tier>/<path>` URL (with a new
/// cache-buster) after this resolves.
#[tauri::command]
pub async fn ensure_tier_thumbnails(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    tier: String,
) -> Result<usize, String> {
    let tier = ThumbTier::from_segment(&tier).ok_or_else(|| format!("unknown tier: {}", tier))?;
    if matches!(tier, ThumbTier::Standard) {
        return Err("ensure_tier_thumbnails does not handle the standard tier; use get_thumbnails_batch".into());
    }

    // Skip anything already cached.
    let missing: Vec<String> = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        paths
            .into_iter()
            .filter(|p| !db.tier_is_cached(tier, p).unwrap_or(false))
            .collect()
    };

    if missing.is_empty() {
        return Ok(0);
    }

    let target = tier.target_size();
    let filter = thumbnailer::filter_for_size(target);
    // L/P tiers use lossy WebP — ~30 % smaller at equivalent quality,
    // which matters a lot at 1024/1600 px. Micro falls back to JPEG.
    let tier_format = match tier {
        ThumbTier::Large | ThumbTier::Preview => ThumbFormat::Webp,
        _ => ThumbFormat::Jpeg,
    };

    // Generate on the shared thumb pool. `generate_for_path` decodes from
    // source, center-crops (for Micro/Standard it also squares), and
    // resizes with the current filter. For Preview (1600 longest edge)
    // we accept the square crop since the viewer scales to fit anyway.
    let missing_clone = missing.clone();
    let pool = state.thumb_pool.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool.spawn(move || {
        use rayon::prelude::*;
        let out: Vec<(String, Option<crate::pipeline::thumbnailer::ThumbResult>)> = missing_clone
            .par_iter()
            .map(|path_str| {
                let p = std::path::Path::new(path_str);
                let res = crate::pipeline::thumbnailer::generate_for_path(
                    p,
                    filter,
                    tier_format,
                    target,
                    target,
                );
                (path_str.clone(), res.ok())
            })
            .collect();
        let _ = tx.send(out);
    });

    let results = rx
        .await
        .map_err(|_| "Tier thumbnail task was dropped".to_string())?;

    // Persist successful results in one transaction.
    let mut written = 0usize;
    let mut db = state.cache_db.lock().await;
    let db = db.as_mut().ok_or("No gallery open")?;
    let txn = db.transaction().map_err(|e| e.to_string())?;
    for (path, thumb) in &results {
        let Some(thumb) = thumb else { continue };
        let sql = format!(
            "INSERT OR REPLACE INTO {} (path, media_type, mtime, width, height, thumbnail, format)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            tier.table()
        );
        let fmt_str = tier_format.as_cache_str();
        if txn
            .execute(
                &sql,
                rusqlite::params![
                    path,
                    thumb.media_type,
                    0u64,
                    thumb.width,
                    thumb.height,
                    thumb.data,
                    fmt_str
                ],
            )
            .is_ok()
        {
            written += 1;
        }
    }
    txn.commit().map_err(|e| e.to_string())?;
    Ok(written)
}

/// Extras derived from a Standard-tier RGBA: compact placeholder + Micro
/// tier bytes. Mirrors the derivation step in `get_thumbnails_batch` so a
/// protocol-triggered Standard generation leaves the DB in the same shape
/// as the batch path (standard row + thumbhash + micro row all present).
struct StandardExtras {
    thumbhash: Option<Vec<u8>>,
    micro_bytes: Option<Vec<u8>>,
    micro_size: u32,
}

/// Decode the just-generated Standard-tier bytes back to RGBA, compute the
/// ThumbHash, and downsample to the 128px Micro tier. Runs on the CPU
/// thumb pool so the tokio runtime stays responsive.
async fn derive_standard_extras(
    pool: &rayon::ThreadPool,
    data: Vec<u8>,
    width: u32,
    height: u32,
) -> StandardExtras {
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool.spawn(move || {
        let extras = match thumbnailer::decode_thumb_bytes_to_rgba(&data) {
            Ok(rgba) => {
                let thumbhash = thumbnailer::compute_thumbhash(&rgba, width, height).ok();
                let micro_size = ThumbTier::Micro.target_size();
                let micro_bytes = thumbnailer::downsample_rgba_square(&rgba, width, height, micro_size)
                    .ok()
                    .and_then(|resized| {
                        thumbnailer::encode_rgba_to_jpeg_tier(&resized, micro_size, micro_size).ok()
                    });
                StandardExtras { thumbhash, micro_bytes, micro_size }
            }
            Err(_) => StandardExtras { thumbhash: None, micro_bytes: None, micro_size: 0 },
        };
        let _ = tx.send(extras);
    });
    rx.await.unwrap_or(StandardExtras { thumbhash: None, micro_bytes: None, micro_size: 0 })
}

/// Generate a single thumbnail for the requested tier, persist it, and
/// return the encoded bytes + format string. Used by the custom-protocol
/// handler to serve cache misses inline (see `serve_thumbnail_generate` in
/// `main.rs`). Mirrors the per-tier format choices of
/// `ensure_tier_thumbnails` and the Standard-tier storage of `get_thumbnail`.
///
/// When `tier == Standard`, also derives the ThumbHash placeholder and
/// Micro (128px) tier from the standard RGBA and commits all three in a
/// single transaction — keeping DB state consistent with
/// `get_thumbnails_batch`.
pub async fn generate_and_store_tier(
    state: &AppState,
    path: &str,
    tier: ThumbTier,
) -> Result<(Vec<u8>, String), String> {
    let target = tier.target_size();
    let filter = thumbnailer::filter_for_size(target);
    let tier_format = match tier {
        ThumbTier::Large | ThumbTier::Preview => ThumbFormat::Webp,
        _ => ThumbFormat::Jpeg,
    };

    let thumb = dispatch_thumbnail(
        &state.thumb_pool,
        path.to_string(),
        filter,
        tier_format,
        target,
        target,
    )
    .await?
    .map_err(|e| format!("thumb gen: {e}"))?;

    let fmt_str = tier_format.as_cache_str().to_string();
    let bytes = thumb.data.clone();

    // For Standard, derive ThumbHash + Micro before taking the DB lock so
    // everything lands in one transaction.
    let extras = if matches!(tier, ThumbTier::Standard) {
        Some(
            derive_standard_extras(
                &state.thumb_pool,
                thumb.data.clone(),
                thumb.width,
                thumb.height,
            )
            .await,
        )
    } else {
        None
    };

    let mut db = state.cache_db.lock().await;
    let db = db.as_mut().ok_or("No gallery open")?;

    if matches!(tier, ThumbTier::Standard) {
        let tx = db.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR REPLACE INTO thumbnails (path, media_type, mtime, width, height, thumbnail, format, resize_filter)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                path,
                thumb.media_type,
                0u64,
                thumb.width,
                thumb.height,
                thumb.data,
                fmt_str,
                filter.as_str(),
            ],
        )
        .map_err(|e| e.to_string())?;

        // Source dimensions — matches the batch path's media_meta update.
        let _ = tx.execute(
            "UPDATE media_meta SET width = ?1, height = ?2 WHERE path = ?3 AND width IS NULL",
            rusqlite::params![thumb.src_width, thumb.src_height, path],
        );

        if let Some(ref ex) = extras {
            if let Some(hash) = &ex.thumbhash {
                let _ = tx.execute(
                    "UPDATE thumbnails SET thumbhash = ?1 WHERE path = ?2",
                    rusqlite::params![hash, path],
                );
            }
            if let Some(micro) = &ex.micro_bytes {
                let _ = tx.execute(
                    "INSERT OR REPLACE INTO thumbnails_micro (path, media_type, mtime, width, height, thumbnail, format)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        path,
                        thumb.media_type,
                        0u64,
                        ex.micro_size,
                        ex.micro_size,
                        micro,
                        "jpeg",
                    ],
                );
            }
        }

        tx.commit().map_err(|e| e.to_string())?;

        if thumb.media_type == "video" {
            populate_video_metadata(db.conn(), path);
        }
    } else {
        let sql = format!(
            "INSERT OR REPLACE INTO {} (path, media_type, mtime, width, height, thumbnail, format)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            tier.table()
        );
        db.conn()
            .execute(
                &sql,
                rusqlite::params![
                    path,
                    thumb.media_type,
                    0u64,
                    thumb.width,
                    thumb.height,
                    thumb.data,
                    fmt_str
                ],
            )
            .map_err(|e| e.to_string())?;
    }

    Ok((bytes, fmt_str))
}
