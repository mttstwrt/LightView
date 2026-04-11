use lightview_lib::AppState;
use lightview_lib::commands;
use tauri::{Emitter, Manager};

use memmap2::Mmap;
use std::io::{Read, Seek, SeekFrom};


enum Route {
    Thumb,
    Media,
    Unknown,
}

/// Parse the URI into a route and the remaining path portion.
fn extract_route(uri: &str) -> (Route, &str) {
    // Try each prefix variant (cross-platform)
    for prefix_base in &[
        "lightview://",
        "lightview://localhost/",
        "http://lightview.localhost/",
    ] {
        if let Some(rest) = uri.strip_prefix(prefix_base) {
            if let Some(p) = rest.strip_prefix("thumb/") {
                return (Route::Thumb, p);
            }
            if let Some(p) = rest.strip_prefix("media/") {
                return (Route::Media, p);
            }
        }
    }
    (Route::Unknown, "")
}

/// Serve a cached thumbnail from SQLite.
fn serve_thumbnail(
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

    let result: Result<(Vec<u8>, u32, u32, String), rusqlite::Error> = conn
        .prepare_cached(
            "SELECT thumbnail, width, height, format FROM thumbnails WHERE path = ?1",
        )
        .and_then(|mut stmt| {
            stmt.query_row(rusqlite::params![path], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
        });

    match result {
        Ok((data, width, height, format)) => {
            tauri::http::Response::builder()
                .status(200)
                .header("Content-Type", "image/jpeg")
                .header("Cache-Control", "no-cache")
                .header("Access-Control-Allow-Origin", "*")
                .body(data)
                .unwrap()
        }
        Err(_) => {
            // Not cached yet — 404, frontend queues for generation
            tauri::http::Response::builder()
                .status(404)
                .header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", "*")
                .body(Vec::new())
                .unwrap()
        }
    }
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
        match lightview_lib::pipeline::thumbnailer::decode_heic_to_rgba(src_path) {
            Ok((rgba, w, h, _, _)) => {
                match lightview_lib::pipeline::thumbnailer::encode_rgba_to_jpeg(&rgba, w, h) {
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
                        log::error!("HEIC→JPEG encode failed for {}: {}", path, e);
                        return tauri::http::Response::builder()
                            .status(500)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Vec::new())
                            .unwrap();
                    }
                }
            }
            Err(e) => {
                log::error!("HEIC decode failed for {}: {}", path, e);
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

    // No Range header — serve the full file.
    // For video files, serve just the first chunk (2 MB) as a 206 response to
    // avoid loading a multi-GB file entirely into memory.  The <video> element
    // will follow up with Range requests for subsequent chunks.
    let is_video = matches!(ext.as_str(), "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv");

    if is_video && len > MAX_CHUNK {
        let nbytes = MAX_CHUNK as usize;
        let mut buf = vec![0u8; nbytes];
        if let Err(e) = file.read_exact(&mut buf) {
            log::error!("Failed to read first chunk of {}: {}", path, e);
            return tauri::http::Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body(Vec::new())
                .unwrap();
        }
        let end = MAX_CHUNK - 1;
        return tauri::http::Response::builder()
            .status(206)
            .header("Content-Type", mime)
            .header("Accept-Ranges", "bytes")
            .header("Content-Range", format!("bytes 0-{end}/{len}"))
            .header("Content-Length", nbytes.to_string())
            .header("Cache-Control", "no-cache")
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Expose-Headers", "content-range")
            .body(buf)
            .unwrap();
    }

    // Small file — serve the full content via mmap.
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
        .register_uri_scheme_protocol("lightview", |ctx, request| {
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
                return tauri::http::Response::builder()
                    .status(400)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap();
            }

            match route {
                Route::Thumb => serve_thumbnail(&ctx, &path),
                Route::Media => serve_full_media(&request, &path),
                Route::Unknown => tauri::http::Response::builder()
                    .status(404)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap(),
            }
        })
        .setup(|app| {
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
            commands::media::precache_thumbnails,

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

            // Duplicate detection
            commands::duplicates::find_duplicates,

            // Settings / maintenance commands
            commands::settings::get_hardware_profile,
            commands::settings::get_memory_status,
            commands::settings::reindex_gallery,
            commands::settings::rebuild_thumbnails,
            commands::settings::clear_cache,
            commands::settings::get_gallery_stats,
            commands::settings::get_debug_info,
            commands::settings::get_perf_snapshot,
            commands::settings::get_thumbnail_settings,
            commands::settings::update_thumbnail_settings,
            commands::settings::save_gallery_settings,
            commands::settings::load_gallery_settings,
            commands::settings::get_recent_galleries,
            commands::settings::remove_recent_gallery,
            commands::settings::open_with,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
