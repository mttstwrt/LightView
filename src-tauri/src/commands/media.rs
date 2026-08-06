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

/// Encode format and resize filter the Standard tier is generated with.
///
/// Four entry points (`get_thumbnail`, `get_thumbnails_batch`,
/// `precache_thumbnails_impl`, `regenerate_thumbnail_impl`) all write the same
/// tier and must agree exactly: the format string is compared against the
/// cached `format` column to decide hit-vs-regenerate, so a site that drifted
/// would make every one of its lookups miss and regenerate forever.
fn standard_tier_params() -> (ThumbFormat, ResizeFilter) {
    (
        ThumbFormat::Jpeg,
        thumbnailer::filter_for_size(STANDARD_THUMB_SIZE),
    )
}

/// Record a file's source dimensions, discovered while thumbnailing it.
///
/// Guarded by `width IS NULL` so it only ever fills a gap: a freshly indexed
/// row starts with NULL dimensions, and whichever tier happens to be generated
/// first supplies them. Idempotent, so every generation path can call it.
fn store_source_dims(conn: &rusqlite::Connection, path: &str, width: u32, height: u32) {
    // A placeholder thumbnail (no ffmpeg, unreadable file) carries no source
    // size. Writing 0×0 would leave the row *filled in* with a degenerate
    // aspect ratio the justified grid then lays out from, and the `width IS
    // NULL` guard below would stop a later real probe from correcting it.
    if width == 0 || height == 0 {
        return;
    }
    let _ = conn.execute(
        "UPDATE media_meta SET width = ?1, height = ?2 WHERE path = ?3 AND width IS NULL",
        rusqlite::params![width, height, path],
    );
}

/// Record video dimensions + duration from an ffprobe result. Unlike
/// [`store_source_dims`] this overwrites: ffprobe is authoritative for videos,
/// where the decoded frame can differ from the container's declared size.
fn store_video_meta(conn: &rusqlite::Connection, path: &str, w: u32, h: u32, duration: Option<f64>) {
    let _ = conn.execute(
        "UPDATE media_meta SET width = ?1, height = ?2, duration = ?3 WHERE path = ?4",
        rusqlite::params![w, h, duration, path],
    );
}

/// A generated Standard-tier thumbnail staged for its SQLite write. Shared by
/// the batch and precache paths, which both generate on the pool first and then
/// commit everything in one transaction.
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

impl CacheItem {
    fn new(thumb: crate::pipeline::thumbnailer::ThumbResult, filter: ResizeFilter) -> Self {
        Self {
            path: thumb.path,
            media_type: thumb.media_type,
            format: thumb.format.as_cache_str().to_string(),
            resize_filter: filter.as_str().to_string(),
            data: thumb.data,
            width: thumb.width,
            height: thumb.height,
            src_width: thumb.src_width,
            src_height: thumb.src_height,
        }
    }

    fn is_video(&self) -> bool {
        self.media_type == "video"
    }

    /// Write the standard-tier row plus the source dimensions it revealed.
    fn write(&self, conn: &rusqlite::Connection) {
        let _ = crate::cache::thumbnails::write_standard_row(
            conn, &self.path, &self.media_type, 0, self.width, self.height,
            &self.data, &self.format, &self.resize_filter,
        );
        store_source_dims(conn, &self.path, self.src_width, self.src_height);
    }
}

/// How [`store_derived_extras`] treats rows that already exist.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    /// The Standard tier was just (re)generated, so its derivations are the
    /// authoritative ones — replace whatever was there.
    Yes,
    /// Backfilling derivations from *already cached* Standard bytes. Anything
    /// already present was derived the same way, so leave it alone.
    No,
}

/// Persist the ThumbHash placeholder + Micro-tier row derived from a
/// Standard-tier thumbnail. Every Standard generation path funnels through
/// here so the DB always ends up in one shape — standard row + thumbhash +
/// micro row — rather than each site spelling out its own two statements.
fn store_derived_extras(
    conn: &rusqlite::Connection,
    path: &str,
    media_type: &str,
    extras: &DerivedExtras,
    overwrite: Overwrite,
) {
    if let Some(hash) = &extras.thumbhash {
        let sql = match overwrite {
            Overwrite::Yes => "UPDATE thumbnails SET thumbhash = ?1 WHERE path = ?2",
            Overwrite::No => {
                "UPDATE thumbnails SET thumbhash = ?1 WHERE path = ?2 AND thumbhash IS NULL"
            }
        };
        let _ = conn.execute(sql, rusqlite::params![hash, path]);
    }
    if let Some(bytes) = &extras.micro_bytes {
        match overwrite {
            Overwrite::Yes => {
                let _ = crate::cache::thumbnails::write_tier_row(
                    conn, ThumbTier::Micro, path, media_type, 0,
                    extras.micro_size, extras.micro_size, bytes, "jpeg",
                );
            }
            Overwrite::No => {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO thumbnails_micro
                         (path, media_type, mtime, width, height, thumbnail, format)
                     VALUES (?1, ?2, 0, ?3, ?4, ?5, 'jpeg')",
                    rusqlite::params![
                        path,
                        media_type,
                        extras.micro_size,
                        extras.micro_size,
                        bytes
                    ],
                );
            }
        }
    }
}

/// Get a single thumbnail. Checks the SQLite cache, then generates on-demand.
#[tauri::command]
pub async fn get_thumbnail(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Option<ThumbnailResult>, String> {
    let (format, filter) = standard_tier_params();
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
                store_source_dims(db.conn(), &thumb.path, thumb.src_width, thumb.src_height);

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
    state.touch_thumb_activity();
    let (format, filter) = standard_tier_params();
    let (thumb_w, thumb_h) = (STANDARD_THUMB_SIZE, STANDARD_THUMB_SIZE);

    let mut results = Vec::with_capacity(paths.len());
    let mut uncached_paths = Vec::new();
    // Paths that have a standard thumbnail but are missing micro/thumbhash rows.
    // These need derivation from the cached standard-tier bytes.
    let mut micro_missing: Vec<String> = Vec::new();

    // Phase 1: Check SQLite cache — return as-is if format matches, otherwise
    // regenerate. One batched IN-list query covers thumbnail info + micro
    // presence for the whole batch.
    let requested_fmt = format.as_cache_str();
    {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let info = db
            .get_thumbnail_info_batch(&paths)
            .map_err(|e| e.to_string())?;

        for path in &paths {
            match info.get(path) {
                Some((w, h, fmt, has_micro)) if fmt == requested_fmt => {
                    results.push(ThumbnailResult {
                        path: path.clone(),
                        width: *w,
                        height: *h,
                        media_type: String::new(),
                        format: fmt.clone(),
                    });
                    // Micro tier missing for this cached path — derive it below.
                    if !has_micro {
                        micro_missing.push(path.clone());
                    }
                }
                // Format mismatch or not cached — (re)generate.
                _ => uncached_paths.push(path.clone()),
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
    let mut to_cache = Vec::with_capacity(generated_thumbs.len());

    for thumb in generated_thumbs {
        results.push(ThumbnailResult {
            path: thumb.path.clone(),
            width: thumb.width,
            height: thumb.height,
            media_type: thumb.media_type.clone(),
            format: thumb.format.as_cache_str().to_string(),
        });
        to_cache.push(CacheItem::new(thumb, filter));
    }

    // -------------------------------------------------------------------
    // Derive ThumbHash + micro (T1) tier bytes in parallel on the CPU pool.
    // Done BEFORE the SQLite transaction so the inserts can include the
    // thumbhash blob and the micro row in the same commit path.
    // `to_cache` is moved into the closure and handed back — the encoded
    // blobs are large, so we avoid cloning them just for derivation.
    // -------------------------------------------------------------------
    let (to_cache, derived_extras): (Vec<CacheItem>, Vec<DerivedExtras>) = {
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.thumb_pool.spawn(move || {
            use rayon::prelude::*;
            let extras: Vec<DerivedExtras> = to_cache
                .par_iter()
                .map(|item| derive_extras_from_bytes(&item.data, item.width, item.height))
                .collect();
            let _ = tx.send((to_cache, extras));
        });
        rx.await
            .map_err(|_| "Thumbnail derivation task was dropped".to_string())?
    };

    // Batch-write thumbnails to SQLite in the generated format (single transaction)
    if !to_cache.is_empty() {
        let mut db = state.cache_db.lock().await;
        if let Some(db) = db.as_mut() {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            for item in &to_cache {
                item.write(&tx);
            }

            // Write ThumbHash + micro tier rows in the same transaction so
            // queries never observe a split state (standard row present,
            // placeholder/micro missing). `derived_extras` is index-aligned
            // with `to_cache`.
            for (item, extra) in to_cache.iter().zip(&derived_extras) {
                store_derived_extras(&tx, &item.path, &item.media_type, extra, Overwrite::Yes);
            }

            let _ = tx.commit();

            // Extract video metadata outside the transaction
            for item in to_cache.iter().filter(|i| i.is_video()) {
                populate_video_metadata(db.conn(), &item.path);
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

    // Per-image metadata carried past the GPU phase. The decoded RGBA is
    // moved into the GPU input (it can be several MB per image), so only
    // these few fields survive for phase 3.
    struct DecodedMeta {
        path: String,
        media_type: String,
        src_width: u32,
        src_height: u32,
    }

    // Phase 1: Decode on CPU (rayon). Decode only as large as the GPU output
    // needs (longest edge of the square target) so big JPEGs/HEICs DCT-scale
    // down during decode instead of decoding full-size.
    let decode_target = thumb_size_w.max(thumb_size_h);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let paths_owned: Vec<String> = paths.to_vec();
    pool.spawn(move || {
        let decoded: Vec<Option<DecodedImage>> = paths_owned
            .iter()
            .map(|p| decode_image(std::path::Path::new(p), decode_target).ok())
            .collect();
        let _ = tx.send(decoded);
    });
    let decoded = rx.await.ok()?;

    // Failed decodes are dropped here, so `gpu_inputs` and `metas` stay
    // index-aligned with each other and with the GPU output.
    let mut gpu_inputs = Vec::with_capacity(decoded.len());
    let mut metas = Vec::with_capacity(decoded.len());
    for d in decoded.into_iter().flatten() {
        gpu_inputs.push(CropResizeInput {
            rgba_data: d.rgba,
            width: d.width,
            height: d.height,
            crop_x: d.crop_x,
            crop_y: d.crop_y,
            crop_size: d.crop_size,
        });
        metas.push(DecodedMeta {
            path: d.path,
            media_type: d.media_type,
            src_width: d.src_width,
            src_height: d.src_height,
        });
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
    pool.spawn(move || {
        let mut results = Vec::new();
        for (resize_out, meta) in resized.into_iter().zip(metas) {
            let Some(resized) = resize_out else { continue };
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
                        path: meta.path,
                        width: resized.width,
                        height: resized.height,
                        data,
                        media_type: meta.media_type,
                        src_width: meta.src_width,
                        src_height: meta.src_height,
                        format,
                    });
                }
                Err(e) => {
                    log::warn!("Encode failed for {}: {}", meta.path, e);
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

/// Like [`dispatch_thumbnail`] but uses the aspect-preserving (non-cropping)
/// generator for the justified tier — fits the source into a `max_edge` box.
pub(crate) async fn dispatch_thumbnail_fit(
    pool: &rayon::ThreadPool,
    path: String,
    filter: ResizeFilter,
    format: ThumbFormat,
    max_edge: u32,
) -> Result<Result<crate::pipeline::thumbnailer::ThumbResult, crate::pipeline::thumbnailer::ThumbError>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    pool.spawn(move || {
        let p = std::path::Path::new(&path);
        let result = crate::pipeline::thumbnailer::generate_for_path_fit(p, filter, format, max_edge);
        let _ = tx.send(result);
    });

    rx.await.map_err(|_| "Thumbnail task was dropped".to_string())
}

/// Derive micro-tier (128px) thumbnails + thumbhash for cached standard-tier
/// paths that are missing them. Reads the standard thumbnail bytes from
/// SQLite, decodes to RGBA, downsamples, and writes the micro row + thumbhash.
async fn derive_micro_for_cached(state: &AppState, paths: &[String]) {
    // Read standard-tier bytes from DB — one batched query, not one per path.
    let items: Vec<(String, Vec<u8>, u32, u32, String)> = {
        let db = state.cache_db.lock().await;
        let Some(db) = db.as_ref() else { return };
        match db.get_thumbnail_blobs_batch(paths) {
            Ok(items) => items,
            Err(e) => {
                log::warn!("micro derivation blob read failed: {e}");
                return;
            }
        }
    };

    if items.is_empty() {
        return;
    }

    // Derive micro + thumbhash on the CPU pool
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.thumb_pool.spawn(move || {
        use rayon::prelude::*;
        let out: Vec<(String, String, DerivedExtras)> = items
            .into_par_iter()
            .map(|(path, data, w, h, media_type)| {
                let extras = derive_extras_from_bytes(&data, w, h);
                (path, media_type, extras)
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
    for (path, media_type, extras) in &derived {
        store_derived_extras(&txn, path, media_type, extras, Overwrite::No);
    }
    let _ = txn.commit();
    log::info!(
        "Derived micro tier for {} cached thumbnails ({} paths checked)",
        derived.iter().filter(|(_, _, e)| e.micro_bytes.is_some()).count(),
        paths.len(),
    );
}

/// Regenerate a single thumbnail, bypassing all caches. Clears every tier
/// (micro/standard/large/preview) so stale rows don't survive when the
/// gallery is rendered at a different cell size, then regenerates the
/// standard tier eagerly. Returns the fresh thumbnail info so the caller can
/// cache-bust its `<img>` URL.
pub async fn regenerate_thumbnail_impl(
    state: &AppState,
    path: String,
) -> Result<ThumbnailResult, String> {
    let (format, filter) = standard_tier_params();
    let thumb_size = STANDARD_THUMB_SIZE;

    // Remove from every tier so a re-render at any cell size pulls fresh bytes.
    {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            for tier in ThumbTier::ALL {
                let _ = db.conn().execute(
                    &format!("DELETE FROM {} WHERE path = ?1", tier.table()),
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
            Ok(ThumbnailResult {
                path: thumb.path,
                width: thumb.width,
                height: thumb.height,
                media_type: thumb.media_type,
                format: fmt_str.to_string(),
            })
        }
        Err(e) => Err(format!("Thumbnail generation failed: {}", e)),
    }
}

/// Desktop wrapper: also emits `thumb:regenerated` so the webview cache-busts
/// its `<img>` URL — without that, WebKit's image cache keeps serving the old
/// bytes until the app restarts. (The web client cache-busts from the returned
/// value instead; it has no Tauri events.)
#[tauri::command]
pub async fn regenerate_thumbnail(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let thumb = regenerate_thumbnail_impl(&state, path).await?;
    let _ = app_handle.emit("thumb:regenerated", thumb);
    Ok(())
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
        Ok((w, h, duration)) => store_video_meta(conn, path, w, h, duration),
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
    precache_thumbnails_impl(&state, paths).await
}

/// Backing implementation shared by the Tauri command and the web client's
/// `/api/invoke` bridge.
pub async fn precache_thumbnails_impl(
    state: &AppState,
    paths: Vec<String>,
) -> Result<PrecacheResult, String> {
    let (format, filter) = standard_tier_params();
    let (thumb_w, thumb_h) = (STANDARD_THUMB_SIZE, STANDARD_THUMB_SIZE);

    // Filter to only paths that are uncached or have a format mismatch. Also
    // collect cached paths whose micro row is missing (galleries thumbnailed
    // before the micro tier existed) so precache leaves the DB in the same
    // shape as batch generation: standard + thumbhash + micro all present.
    let requested_fmt = format.as_cache_str();
    let mut micro_missing: Vec<String> = Vec::new();
    let uncached: Vec<String> = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let info = db
            .get_thumbnail_info_batch(&paths)
            .map_err(|e| e.to_string())?;
        paths
            .into_iter()
            .filter(|p| match info.get(p) {
                Some((_, _, fmt, has_micro)) if fmt == requested_fmt => {
                    if !has_micro {
                        micro_missing.push(p.clone());
                    }
                    false
                }
                _ => true,
            })
            .collect()
    };

    if uncached.is_empty() && micro_missing.is_empty() {
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

    let mut failed = Vec::new();
    let mut to_cache = Vec::new();

    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(Ok(thumb)) => to_cache.push(CacheItem::new(thumb, filter)),
            _ => failed.push(uncached[i].clone()),
        }
    }
    let generated = to_cache.len();

    // Batch write to SQLite (single transaction)
    if !to_cache.is_empty() {
        let mut db = state.cache_db.lock().await;
        if let Some(db) = db.as_mut() {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            for item in &to_cache {
                item.write(&tx);
            }
            let _ = tx.commit();

            // Extract video metadata outside the transaction
            for item in to_cache.iter().filter(|i| i.is_video()) {
                populate_video_metadata(db.conn(), &item.path);
            }
        }
    }

    // Backfill micro + thumbhash for both the freshly generated rows and any
    // previously cached paths that were missing them, so a precache pass over
    // the gallery leaves the cheap "s" tier fully warm.
    let mut micro_needed = micro_missing;
    micro_needed.extend(to_cache.iter().map(|item| item.path.clone()));
    if !micro_needed.is_empty() {
        derive_micro_for_cached(state, &micro_needed).await;
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
/// Returns the number of thumbnails successfully written, plus any paths
/// evicted while making room. The frontend should retry its
/// `lightview://thumb/<tier>/<path>` URL (with a new cache-buster) after this
/// resolves, and drop the evicted paths from its "already warmed" memo so they
/// get re-warmed rather than left to the slow one-at-a-time serve path.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureTierResult {
    pub generated: usize,
    pub evicted: Vec<String>,
}

#[tauri::command]
pub async fn ensure_tier_thumbnails(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    tier: String,
) -> Result<EnsureTierResult, String> {
    ensure_tier_thumbnails_impl(&state, paths, tier).await
}

/// Backing implementation shared by the Tauri command and the web client's
/// `/api/invoke` bridge.
pub async fn ensure_tier_thumbnails_impl(
    state: &AppState,
    paths: Vec<String>,
    tier: String,
) -> Result<EnsureTierResult, String> {
    state.touch_thumb_activity();
    let tier = ThumbTier::from_segment(&tier).ok_or_else(|| format!("unknown tier: {}", tier))?;
    if matches!(tier, ThumbTier::Standard) {
        return Err("ensure_tier_thumbnails does not handle the standard tier; use get_thumbnails_batch".into());
    }

    // Skip anything already cached.
    let missing: Vec<String> = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let cached = db.tier_cached_set(tier, &paths).map_err(|e| e.to_string())?;
        paths
            .into_iter()
            .filter(|p| !cached.contains(p))
            .collect()
    };

    if missing.is_empty() {
        // Nothing to generate, but this is exactly the case where the user is
        // scrolling through already-warm cells — land those access marks so the
        // rows they're looking at read as warm at the next eviction, and so the
        // pending set doesn't grow unbounded during a long cached browse.
        let mut db = state.cache_db.lock().await;
        let evicted = match db.as_mut() {
            Some(db) => enforce_tier_budget(state, db, tier),
            None => Vec::new(),
        };
        return Ok(EnsureTierResult { generated: 0, evicted });
    }

    let target = tier.target_size();
    let filter = thumbnailer::filter_for_size(target);
    let tier_format = tier.format();

    // Generate on the shared thumb pool. `generate_for_path` decodes from
    // source, center-crops (for Micro/Standard it also squares), and
    // resizes with the current filter. For Preview (1600 longest edge)
    // we accept the square crop since the viewer scales to fit anyway.
    // The Justified tier instead uses the aspect-preserving (non-cropping)
    // path so the gallery can show true proportions.
    let missing_clone = missing.clone();
    let pool = state.thumb_pool.clone();
    let is_fit = tier.is_fit();
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool.spawn(move || {
        use rayon::prelude::*;
        let out: Vec<(String, Option<crate::pipeline::thumbnailer::ThumbResult>)> = missing_clone
            .par_iter()
            .map(|path_str| {
                let p = std::path::Path::new(path_str);
                let res = if is_fit {
                    crate::pipeline::thumbnailer::generate_for_path_fit(
                        p, filter, tier_format, target,
                    )
                } else {
                    crate::pipeline::thumbnailer::generate_for_path(
                        p, filter, tier_format, target, target,
                    )
                };
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
        if crate::cache::thumbnails::write_tier_row(
            &txn, tier, path, &thumb.media_type, 0,
            thumb.width, thumb.height, &thumb.data, tier_format.as_cache_str(),
        )
        .is_ok()
        {
            written += 1;
        }
    }
    txn.commit().map_err(|e| e.to_string())?;

    let evicted = enforce_tier_budget(state, db, tier);

    Ok(EnsureTierResult { generated: written, evicted })
}

/// Flush pending access marks and run an eviction pass for `tier`, returning
/// the evicted paths. Shared by both tier write paths so the bound is enforced
/// steadily wherever rows are added, rather than only on batch writes.
///
/// Errors are logged, not propagated: an eviction failure must not fail the
/// generation that just succeeded — the bytes are already on disk and useful.
pub(crate) fn enforce_tier_budget(
    state: &AppState,
    db: &crate::cache::db::CacheDb,
    tier: ThumbTier,
) -> Vec<String> {
    use crate::cache::thumbnails as thumbs;
    if !thumbs::is_lru_capped(tier) {
        return Vec::new();
    }

    // Serves accumulate in memory (the read path holds a read-only connection);
    // land them before deciding what's cold.
    let touched = state.take_tier_accesses(tier);
    if let Err(e) = thumbs::touch_accessed(db.conn(), tier, &touched) {
        log::warn!("{} tier access flush failed: {e}", tier.as_segment());
    }

    match thumbs::evict_tier_lru(db.conn(), tier, tier_budget_bytes(tier)) {
        Ok(evicted) => evicted,
        Err(e) => {
            log::warn!("{} tier eviction failed: {e}", tier.as_segment());
            Vec::new()
        }
    }
}

/// Disk budget for a zoomed-in justified tier.
///
/// Scaled to free disk rather than fixed: these tiers cache whatever you view
/// zoomed in, so a roomy machine should keep far more of it warm than the old
/// row caps allowed (~0.5 GiB each, which on a large library thrashed), while a
/// nearly-full disk must not be filled by a cache. The mid tier's 1280 px rows
/// are ~a quarter the bytes of high's 2560 px rows, so an equal byte budget
/// holds roughly four times as many of them — which matches usage, since mid
/// zoom covers more cells per screen.
fn tier_budget_bytes(tier: ThumbTier) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    /// Never take more than this share of what's currently free.
    const FREE_SHARE: f64 = 0.10;
    let (floor, ceiling) = match tier {
        ThumbTier::JustifiedMid | ThumbTier::JustifiedHigh => (GIB / 2, 8 * GIB),
        _ => return u64::MAX,
    };
    // Escape hatch, per tier in MiB: `LIGHTVIEW_TIER_BUDGET_MB=256`. Bypasses
    // the floor so a small NAS or a test can pin the cache well below it.
    if let Some(mb) = std::env::var("LIGHTVIEW_TIER_BUDGET_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return mb.saturating_mul(1024 * 1024);
    }
    let share = (free_disk_bytes().unwrap_or(0) as f64 * FREE_SHARE) as u64;
    share.clamp(floor, ceiling)
}

/// Free bytes on the filesystem holding the gallery cache. `None` when the
/// platform lookup finds no matching mount, in which case callers fall back to
/// the budget floor.
fn free_disk_bytes() -> Option<u64> {
    use sysinfo::Disks;
    let data = crate::util::paths::data_dir();
    let disks = Disks::new_with_refreshed_list();
    // Longest matching mount point wins — `/home` should beat `/` for a path
    // under it.
    disks
        .iter()
        .filter(|d| data.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Extras derived from a Standard-tier thumbnail: ThumbHash placeholder +
/// Micro (128px) tier bytes. Every Standard-tier generation path (batch,
/// cached backfill, protocol miss) derives these so the DB always ends up
/// in the same shape (standard row + thumbhash + micro row all present).
struct DerivedExtras {
    thumbhash: Option<Vec<u8>>,
    micro_bytes: Option<Vec<u8>>,
    micro_size: u32,
}

impl DerivedExtras {
    fn empty() -> Self {
        DerivedExtras { thumbhash: None, micro_bytes: None, micro_size: 0 }
    }
}

/// Decode Standard-tier bytes back to RGBA, compute the ThumbHash, and
/// downsample to the Micro tier. CPU-bound — call from the thumb pool.
fn derive_extras_from_bytes(data: &[u8], width: u32, height: u32) -> DerivedExtras {
    let Ok(rgba) = thumbnailer::decode_thumb_bytes_to_rgba(data) else {
        return DerivedExtras::empty();
    };
    let thumbhash = thumbnailer::compute_thumbhash(&rgba, width, height).ok();
    let micro_size = ThumbTier::Micro.target_size();
    let micro_bytes = thumbnailer::downsample_rgba_square(&rgba, width, height, micro_size)
        .ok()
        .and_then(|resized| thumbnailer::encode_rgba_to_jpeg(&resized, micro_size, micro_size).ok());
    DerivedExtras { thumbhash, micro_bytes, micro_size }
}

/// Run [`derive_extras_from_bytes`] for a single thumbnail on the CPU thumb
/// pool so the tokio runtime stays responsive.
async fn derive_standard_extras(
    pool: &rayon::ThreadPool,
    data: Vec<u8>,
    width: u32,
    height: u32,
) -> DerivedExtras {
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool.spawn(move || {
        let _ = tx.send(derive_extras_from_bytes(&data, width, height));
    });
    rx.await.unwrap_or_else(|_| DerivedExtras::empty())
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
    // Micro fast path: derive from the cached Standard-tier bytes when they
    // exist — decoding a 512px thumbnail instead of the multi-megapixel
    // original. The grids' cheap-rung look-ahead floods this tier on galleries
    // thumbnailed before the micro tier existed; paying a full source decode
    // per 128px thumb pegged the CPU during sustained scrolling. Falls back to
    // full generation when the Standard tier isn't cached yet.
    if matches!(tier, ThumbTier::Micro) {
        if let Some(hit) = derive_micro_from_standard(state, path).await? {
            return Ok(hit);
        }
    }

    let target = tier.target_size();
    let filter = thumbnailer::filter_for_size(target);
    let tier_format = tier.format();

    let thumb = if tier.is_fit() {
        dispatch_thumbnail_fit(&state.thumb_pool, path.to_string(), filter, tier_format, target)
            .await?
            .map_err(|e| format!("thumb gen: {e}"))?
    } else {
        dispatch_thumbnail(
            &state.thumb_pool,
            path.to_string(),
            filter,
            tier_format,
            target,
            target,
        )
        .await?
        .map_err(|e| format!("thumb gen: {e}"))?
    };

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

    // Videos need ffprobe for duration/exact dimensions. Probe BEFORE taking
    // the DB lock: ffprobe is a subprocess taking up to hundreds of ms, and
    // running it under the lock kept every other DB user queued for the
    // duration during sustained scroll-driven generation. Only probe when
    // duration is still missing so the (potentially 3) justified tiers don't
    // each spawn ffprobe for the same file.
    let video_meta: Option<(u32, u32, Option<f64>)> = if thumb.media_type == "video" {
        let needs_probe = {
            let db = state.cache_db.lock().await;
            let db = db.as_ref().ok_or("No gallery open")?;
            db.conn()
                .query_row(
                    "SELECT duration IS NULL FROM media_meta WHERE path = ?1",
                    rusqlite::params![path],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
        };
        if needs_probe {
            let p = path.to_string();
            tokio::task::spawn_blocking(move || {
                thumbnailer::probe_video_metadata(Path::new(&p)).ok()
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        }
    } else {
        None
    };

    let mut db = state.cache_db.lock().await;
    let db = db.as_mut().ok_or("No gallery open")?;

    if matches!(tier, ThumbTier::Standard) {
        // One transaction so a reader never sees the standard row without its
        // placeholder/micro derivations.
        let tx = db.transaction().map_err(|e| e.to_string())?;
        crate::cache::thumbnails::write_standard_row(
            &tx, path, &thumb.media_type, 0, thumb.width, thumb.height,
            &thumb.data, &fmt_str, filter.as_str(),
        )
        .map_err(|e| e.to_string())?;
        store_source_dims(&tx, path, thumb.src_width, thumb.src_height);
        if let Some(ref ex) = extras {
            store_derived_extras(&tx, path, &thumb.media_type, ex, Overwrite::Yes);
        }
        tx.commit().map_err(|e| e.to_string())?;
    } else {
        crate::cache::thumbnails::write_tier_row(
            db.conn(), tier, path, &thumb.media_type, 0,
            thumb.width, thumb.height, &thumb.data, &fmt_str,
        )
        .map_err(|e| e.to_string())?;

        // The web client's justified grid only ever requests these non-standard
        // tiers, so without this the source dimensions never reach media_meta
        // and the InfoPanel shows no dimensions for newly-added images until a
        // restart re-indexes. Idempotent: guarded by `width IS NULL`.
        store_source_dims(db.conn(), path, thumb.src_width, thumb.src_height);
    }

    // ffprobe's numbers win over the decoded frame's, for either tier shape.
    if let Some((w, h, duration)) = video_meta {
        store_video_meta(db.conn(), path, w, h, duration);
    }

    // Enforce the tier budget here too, not just on the batch path. Serving a
    // cache miss writes a row like any other, and when this path was exempt the
    // table grew unbounded between batch calls — so the next batch's single
    // eviction had to delete the entire overshoot at once, stalling every DB
    // user and dropping a large slice of the working set. Evicting from both
    // paths (past the high-water mark) keeps each pass small.
    enforce_tier_budget(state, db, tier);

    Ok((bytes, fmt_str))
}

/// Derive the Micro (128px) tier for `path` from its cached Standard-tier
/// bytes, persisting the micro row (and backfilling the ThumbHash) like
/// `derive_micro_for_cached`. Returns `Ok(None)` when the Standard tier isn't
/// cached, in which case the caller should fall back to full generation.
async fn derive_micro_from_standard(
    state: &AppState,
    path: &str,
) -> Result<Option<(Vec<u8>, String)>, String> {
    let row = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        db.get_thumbnail(path).ok().flatten()
    };
    let Some(row) = row else { return Ok(None) };

    // Decode + downsample on the CPU pool — cheap (512px source), but still
    // not for the async threads.
    let extras =
        derive_standard_extras(&state.thumb_pool, row.thumbnail, row.width, row.height).await;
    let Some(micro) = extras.micro_bytes.clone() else {
        // Undecodable standard bytes — let the caller regenerate from source.
        return Ok(None);
    };

    {
        let mut db = state.cache_db.lock().await;
        let db = db.as_mut().ok_or("No gallery open")?;
        let txn = db.transaction().map_err(|e| e.to_string())?;
        store_derived_extras(&txn, path, &row.media_type, &extras, Overwrite::No);
        txn.commit().map_err(|e| e.to_string())?;
    }

    Ok(Some((micro, "jpeg".to_string())))
}
