use lightview_lib::AppState;
use lightview_lib::cache::coalescer::Role;
use lightview_lib::cache::thumbnails::ThumbTier;
use lightview_lib::commands;
use lightview_lib::http_server::{self, HttpConfig};
use tauri::{AppHandle, Emitter, Manager, UriSchemeResponder};

use memmap2::Mmap;
use std::io::{Read, Seek, SeekFrom};


enum Route {
    /// A tiered thumbnail. The tier defaults to Standard if the URL is
    /// the legacy form `thumb/<path>` (without a tier segment).
    Thumb(ThumbTier),
    /// Decoded ThumbHash placeholder served as a tiny PNG.
    ThumbHash,
    /// Full-resolution media (original file).
    Media,
    Unknown,
}

/// Parse the URI into a route and the remaining path portion.
/// Tier-aware routes:
///   lightview://thumb/<path>         — standard tier (legacy)
///   lightview://thumb/s/<path>       — micro   (128)
///   lightview://thumb/m/<path>       — standard (512)
///   lightview://thumb/l/<path>       — large   (1024)
///   lightview://thumb/p/<path>       — preview (1600, for viewer)
///   lightview://thumbhash/<path>     — decoded ThumbHash → PNG
///   lightview://media/<path>         — full-res media
fn extract_route(uri: &str) -> (Route, &str) {
    for prefix_base in &[
        "lightview://",
        "lightview://localhost/",
        "http://lightview.localhost/",
    ] {
        if let Some(rest) = uri.strip_prefix(prefix_base) {
            if let Some(p) = rest.strip_prefix("thumbhash/") {
                return (Route::ThumbHash, p);
            }
            if let Some(p) = rest.strip_prefix("thumb/") {
                // Check for a tier prefix segment ("s/", "m/", "l/", "p/").
                for (seg, tier) in &[
                    ("s/", ThumbTier::Micro),
                    ("m/", ThumbTier::Standard),
                    ("l/", ThumbTier::Large),
                    ("p/", ThumbTier::Preview),
                ] {
                    if let Some(rest) = p.strip_prefix(*seg) {
                        return (Route::Thumb(*tier), rest);
                    }
                }
                // Legacy form: no tier segment → standard.
                return (Route::Thumb(ThumbTier::Standard), p);
            }
            if let Some(p) = rest.strip_prefix("media/") {
                return (Route::Media, p);
            }
        }
    }
    (Route::Unknown, "")
}

fn thumb_ok_response(data: Vec<u8>, format: &str) -> tauri::http::Response<Vec<u8>> {
    let mime = match format {
        "webp" => "image/webp",
        "png" => "image/png",
        _ => "image/jpeg",
    };
    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", mime)
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*")
        .body(data)
        .unwrap()
}

fn thumb_miss_response() -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(404)
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .body(Vec::new())
        .unwrap()
}

/// Read a cached thumbnail directly from SQLite via the read-only
/// protocol connection. Returns `None` on cache miss (caller decides
/// whether to generate) and `Some(Err)` on infrastructure errors that
/// should short-circuit with a pre-built response (e.g. 503 while no
/// gallery is open).
fn read_cached_thumbnail(
    state: &AppState,
    tier: ThumbTier,
    path: &str,
) -> Result<Option<(Vec<u8>, String)>, tauri::http::Response<Vec<u8>>> {
    let proto_db = state.thumb_protocol_db.lock().unwrap();
    let conn = match proto_db.as_ref() {
        Some(c) => c,
        None => {
            return Err(tauri::http::Response::builder()
                .status(503)
                .body(Vec::new())
                .unwrap());
        }
    };

    let sql = format!(
        "SELECT thumbnail, format FROM {} WHERE path = ?1",
        tier.table()
    );
    match conn.prepare_cached(&sql).and_then(|mut stmt| {
        stmt.query_row(rusqlite::params![path], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })
    }) {
        Ok(hit) => Ok(Some(hit)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => {
            log::warn!("thumb cache read failed for {} (tier {:?}): {}", path, tier, e);
            Ok(None)
        }
    }
}

/// Fast synchronous path for `lightview://thumb/<tier>/<path>`. Returns a
/// 200 response if the thumbnail is already cached; returns `None` on a
/// genuine miss so the caller can fall through to the async generator.
fn serve_thumbnail_fast(
    state: &AppState,
    tier: ThumbTier,
    path: &str,
) -> Option<tauri::http::Response<Vec<u8>>> {
    match read_cached_thumbnail(state, tier, path) {
        Ok(Some((data, format))) => Some(thumb_ok_response(data, &format)),
        Ok(None) => None,
        Err(resp) => Some(resp),
    }
}

/// Slow path: no cached bytes for `(path, tier)`. Generate it on the thumb
/// pool, persist it in SQLite, and return the bytes. Concurrent requests
/// for the same key coalesce through `AppState::thumb_gen_coalescer` so
/// only one decode happens per cold-gallery cell.
async fn serve_thumbnail_generate(
    app_handle: AppHandle,
    tier: ThumbTier,
    path: String,
) -> tauri::http::Response<Vec<u8>> {
    let state = app_handle.state::<AppState>();
    let key = (path.clone(), tier);

    let (role, notify) = state.thumb_gen_coalescer.acquire(key.clone());

    match role {
        Role::Generator => {
            let result =
                commands::media::generate_and_store_tier(&state, &path, tier).await;
            state.thumb_gen_coalescer.release(&key);
            match result {
                Ok((bytes, format)) => thumb_ok_response(bytes, &format),
                Err(e) => {
                    log::warn!("generate-on-miss failed for {} (tier {:?}): {}", path, tier, e);
                    thumb_miss_response()
                }
            }
        }
        Role::Waiter => {
            let listener = notify.notified();
            tokio::pin!(listener);
            // Enrol in the wake queue before re-checking the cache to avoid
            // missing a notify that races with the generator's release.
            listener.as_mut().enable();

            // If the generator completed between our cache-miss check and
            // acquiring the slot, the bytes are already in the DB.
            if let Some(resp) = serve_thumbnail_fast(&state, tier, &path) {
                return resp;
            }
            listener.await;

            match read_cached_thumbnail(&state, tier, &path) {
                Ok(Some((data, format))) => thumb_ok_response(data, &format),
                _ => thumb_miss_response(),
            }
        }
    }
}

/// Serve a decoded ThumbHash as a tiny PNG. Reads the ~25-byte blob from
/// SQLite, decodes to RGBA via the `thumbhash` crate, and encodes a PNG.
/// The PNG is cacheable in the webview because the underlying blob is
/// immutable for a given path.
fn serve_thumbhash(
    ctx: &tauri::UriSchemeContext<'_, tauri::Wry>,
    path: &str,
) -> tauri::http::Response<Vec<u8>> {
    let state = ctx.app_handle().state::<AppState>();
    let proto_db = state.thumb_protocol_db.lock().unwrap();
    let conn = match proto_db.as_ref() {
        Some(c) => c,
        None => {
            return tauri::http::Response::builder()
                .status(503)
                .body(Vec::new())
                .unwrap();
        }
    };

    let blob: Result<Option<Vec<u8>>, rusqlite::Error> = conn
        .prepare_cached("SELECT thumbhash FROM thumbnails WHERE path = ?1")
        .and_then(|mut stmt| {
            stmt.query_row(rusqlite::params![path], |row| {
                row.get::<_, Option<Vec<u8>>>(0)
            })
        });

    let Ok(Some(hash)) = blob else {
        return tauri::http::Response::builder()
            .status(404)
            .header("Cache-Control", "no-store")
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    };

    let Ok((w, h, rgba)) = thumbhash::thumb_hash_to_rgba(&hash) else {
        return tauri::http::Response::builder()
            .status(500)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    };

    // Encode to PNG so the webview can decode with createImageBitmap.
    let mut png = std::io::Cursor::new(Vec::with_capacity(2048));
    let enc = image::codecs::png::PngEncoder::new(&mut png);
    use image::ImageEncoder;
    if enc
        .write_image(&rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8)
        .is_err()
    {
        return tauri::http::Response::builder()
            .status(500)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    }

    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", "image/png")
        // ThumbHashes are immutable for a given file mtime — long cache OK.
        .header("Cache-Control", "public, max-age=31536000, immutable")
        .header("Access-Control-Allow-Origin", "*")
        .body(png.into_inner())
        .unwrap()
}

/// Serve full-resolution media. HEIC/HEIF are transcoded to JPEG.
/// Supports HTTP Range requests for chunked streaming of large files.
fn serve_full_media(
    request: &tauri::http::Request<Vec<u8>>,
    path: &str,
) -> tauri::http::Response<Vec<u8>> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // HEIC/HEIF: transcode to JPEG since browsers can't render natively.
    // Transcoded content is served in full (Range not applicable since size is unknown upfront).
    if ext == "heic" || ext == "heif" {
        let src_path = std::path::Path::new(path);
        match lightview_lib::pipeline::heic_cache::get_or_transcode(src_path) {
            Ok(jpeg_data) => {
                return tauri::http::Response::builder()
                    .status(200)
                    .header("Content-Type", "image/jpeg")
                    .header("Cache-Control", "no-cache")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(jpeg_data)
                    .unwrap();
            }
            Err(e) => {
                log::error!("HEIC transcode failed for {}: {}", path, e);
                return tauri::http::Response::builder()
                    .status(500)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap();
            }
        }
    }

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

    // Open the file and get its length
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open media file {}: {}", path, e);
            return tauri::http::Response::builder()
                .status(404)
                .header("Access-Control-Allow-Origin", "*")
                .body(Vec::new())
                .unwrap();
        }
    };

    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            log::error!("Failed to read metadata for {}: {}", path, e);
            return tauri::http::Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body(Vec::new())
                .unwrap();
        }
    };

    /// Max bytes per chunk (2 MB)
    const MAX_CHUNK: u64 = 2 * 1024 * 1024;

    // Handle Range requests for chunked streaming
    if let Some(range_header) = request
        .headers()
        .get("range")
        .and_then(|r| r.to_str().ok())
    {
        let ranges = match http_range::HttpRange::parse(range_header, len) {
            Ok(r) => r,
            Err(_) => {
                return tauri::http::Response::builder()
                    .status(416)
                    .header("Content-Range", format!("bytes */{len}"))
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap();
            }
        };

        // Only handle the first range (covers all practical browser requests)
        if let Some(range) = ranges.first() {

            let start = range.start;
            let mut end = start + range.length - 1;

            if start >= len || end >= len {
                return tauri::http::Response::builder()
                    .status(416)
                    .header("Content-Range", format!("bytes */{len}"))
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap();
            }

            // Clamp to MAX_CHUNK
            end = start + (end - start).min(MAX_CHUNK - 1);
            let nbytes = (end + 1 - start) as usize;

            let mut buf = vec![0u8; nbytes];
            if let Err(e) = file.seek(SeekFrom::Start(start)).and_then(|_| file.read_exact(&mut buf)) {
                log::error!("Failed to read range {}-{} of {}: {}", start, end, path, e);
                return tauri::http::Response::builder()
                    .status(500)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap();
            }

            return tauri::http::Response::builder()
                .status(206)
                .header("Content-Type", mime)
                .header("Accept-Ranges", "bytes")
                .header("Content-Range", format!("bytes {start}-{end}/{len}"))
                .header("Content-Length", nbytes.to_string())
                .header("Cache-Control", "no-cache")
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Expose-Headers", "content-range")
                .body(buf)
                .unwrap();
        }
    }

    // No Range header — serve the full file with status 200. Previously this
    // handler returned a truncated 206 for videos >2 MB, but WebKitGTK does not
    // issue a follow-up Range request for `<video>` after an unsolicited 206,
    // so playback stalled on the first chunk. Serving 200 + full body matches
    // what Tauri's built-in asset protocol does; the webview then issues Range
    // requests for seek/progressive playback and those are handled above.
    let resp = tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", mime)
        .header("Content-Length", len.to_string())
        .header("Accept-Ranges", "bytes")
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*");

    // SAFETY: read-only mmap, file handle kept open for the duration of the copy.
    match unsafe { Mmap::map(&file) } {
        Ok(mmap) => resp.body(mmap[..].to_vec()).unwrap(),
        Err(e) => {
            log::error!("Failed to mmap media file {}: {}", path, e);
            tauri::http::Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body(Vec::new())
                .unwrap()
        }
    }
}

fn main() {
    // WebKitGTK can crash with protocol errors on some Wayland compositors.
    // Falling back to X11 via XWayland avoids this.
    // SAFETY: Called before any threads are spawned (start of main).
    unsafe {
        if std::env::var("GDK_BACKEND").is_err() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
        // Disable DMA-BUF renderer to avoid GBM buffer allocation failures.
        if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    env_logger::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .register_asynchronous_uri_scheme_protocol(
            "lightview",
            |ctx, request, responder: UriSchemeResponder| {
                let uri = request.uri().to_string();

                // Route: lightview://thumb/<path> or lightview://media/<path>
                // On Linux: lightview://localhost/thumb/<path>
                // On Windows: http://lightview.localhost/thumb/<path>
                let (route, raw_path) = extract_route(&uri);

                // Strip query string (e.g. ?v=1 cache-buster) before decoding.
                let raw_path = raw_path.split('?').next().unwrap_or(raw_path);
                let path = percent_encoding::percent_decode_str(raw_path)
                    .decode_utf8_lossy()
                    .to_string();

                if path.is_empty() {
                    responder.respond(
                        tauri::http::Response::builder()
                            .status(400)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Vec::new())
                            .unwrap(),
                    );
                    return;
                }

                match route {
                    Route::Thumb(tier) => {
                        // Fast synchronous DB hit — respond inline, no task spawn.
                        let state = ctx.app_handle().state::<AppState>();
                        if let Some(resp) = serve_thumbnail_fast(&state, tier, &path) {
                            responder.respond(resp);
                            return;
                        }
                        // Cache miss: generate on the thumb pool and reply when done.
                        let app_handle = ctx.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            let resp = serve_thumbnail_generate(app_handle, tier, path).await;
                            responder.respond(resp);
                        });
                    }
                    Route::ThumbHash => responder.respond(serve_thumbhash(&ctx, &path)),
                    Route::Media => responder.respond(serve_full_media(&request, &path)),
                    Route::Unknown => responder.respond(
                        tauri::http::Response::builder()
                            .status(404)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Vec::new())
                            .unwrap(),
                    ),
                }
            },
        )
        .setup(|app| {
            // Start the local HTTP media server. Used by `<video>` elements
            // because WebKitGTK rejects custom URI schemes for media elements.
            // block_on is safe here: setup runs on the main thread outside the
            // tokio runtime and server startup is a few milliseconds.
            match tauri::async_runtime::block_on(
                http_server::start(HttpConfig::local_only()),
            ) {
                Ok(server) => {
                    let url = format!("http://{}", server.addr);
                    log::info!("Media server URL: {}", url);
                    let state = app.state::<AppState>();
                    let _ = state.media_server_url.set(url.clone());

                    // Inject the URL into the webview so `videoSrc()` can
                    // synthesize URLs synchronously. Tauri's eval queues the
                    // script until the page is ready, so call-order relative
                    // to page load does not matter here.
                    if let Some(window) = app.get_webview_window("main") {
                        let script = format!(
                            "window.__LV_MEDIA_URL__ = {};",
                            serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".into())
                        );
                        if let Err(e) = window.eval(&script) {
                            log::error!("Failed to inject media URL: {}", e);
                        }
                    }
                }
                Err(e) => {
                    log::error!("Failed to start HTTP media server: {}", e);
                }
            }

            // If a directory path was passed as a CLI argument, emit it to the frontend
            let args: Vec<String> = std::env::args().collect();
            if let Some(dir) = args.get(1) {
                let path = std::path::PathBuf::from(dir);
                if path.is_dir() {
                    let handle = app.handle().clone();
                    let dir = path.canonicalize()
                        .unwrap_or(path)
                        .to_string_lossy()
                        .to_string();
                    tauri::async_runtime::spawn(async move {
                        // Brief delay so the webview has time to set up its event listener
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        let _ = handle.emit("open-directory", dir);
                    });
                }
            }
            Ok(())
        })
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            // Gallery commands
            commands::gallery::open_gallery,
            commands::gallery::close_gallery,
            commands::gallery::get_gallery_info,

            // Media commands
            commands::media::get_thumbnail,
            commands::media::get_thumbnails_batch,
            commands::media::get_full_media,
            commands::media::get_media_meta,
            commands::media::regenerate_thumbnail,
            commands::media::get_cached_thumbnail_info,
            commands::media::get_all_thumbnail_tiers,
            commands::media::precache_thumbnails,
            commands::media::get_thumbhashes,
            commands::media::ensure_tier_thumbnails,

            // Tag commands
            commands::tags::get_tags,
            commands::tags::add_user_tag,
            commands::tags::remove_user_tag,
            commands::tags::set_rating,
            commands::tags::set_color_label,
            commands::tags::set_notes,
            commands::tags::add_user_tag_batch,
            commands::tags::remove_user_tag_batch,
            commands::tags::set_rating_batch,

            // Filter commands
            commands::filter::apply_filter,
            commands::filter::clear_filter,

            // Geo commands
            commands::geo::get_geo_points,
            commands::geo::get_geo_paths,

            // Autocomplete commands
            commands::autocomplete::autocomplete_tags,
            commands::autocomplete::get_recent_tags,

            // Sort commands
            commands::sort::get_sorted_items,
            commands::sort::get_timeline_index,

            // Plugin commands
            commands::plugins::list_plugins,
            commands::plugins::run_plugin,
            commands::plugins::run_plugin_batch,
            commands::plugins::cancel_plugin_batch,
            commands::plugins::install_plugin,

            // Viewer commands (GPU-accelerated transforms)
            commands::viewer::get_transformed_media,
            commands::viewer::record_view,

            // GPU capability notification
            lightview_lib::pipeline::gpu::notify_gpu_capabilities,

            // File operations (copy/move)
            commands::files::copy_files,
            commands::files::move_files,
            commands::files::trash_files,
            commands::files::copy_files_to_clipboard,

            // Duplicate detection
            commands::duplicates::find_duplicates,
            commands::duplicates::mark_not_duplicates,

            // Settings / maintenance commands
            commands::settings::get_hardware_profile,
            commands::settings::get_memory_status,
            commands::settings::get_media_server_url,
            commands::settings::reindex_gallery,
            commands::settings::rebuild_thumbnails,
            commands::settings::clear_cache,
            commands::settings::get_gallery_stats,
            commands::settings::get_debug_info,
            commands::settings::get_perf_snapshot,
            commands::settings::save_gallery_settings,
            commands::settings::load_gallery_settings,
            commands::settings::get_recent_galleries,
            commands::settings::remove_recent_gallery,
            commands::settings::open_with,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
