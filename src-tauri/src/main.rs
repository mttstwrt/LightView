use lightview_lib::AppState;
use lightview_lib::commands;
use tauri::{Emitter, Manager};


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
            if format == "rgba" {
                // Transcode RGBA→JPEG on-the-fly instead of serving raw pixels
                match lightview_lib::pipeline::thumbnailer::encode_rgba_to_jpeg(&data, width, height) {
                    Ok(jpeg) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "image/jpeg")
                        .header("Cache-Control", "no-cache")
                        .body(jpeg)
                        .unwrap(),
                    Err(_) => tauri::http::Response::builder()
                        .status(500)
                        .body(Vec::new())
                        .unwrap(),
                }
            } else {
                tauri::http::Response::builder()
                    .status(200)
                    .header("Content-Type", "image/jpeg")
                    .header("Cache-Control", "no-cache")
                    .body(data)
                    .unwrap()
            }
        }
        Err(_) => {
            // Not cached yet — 404, frontend queues for generation
            tauri::http::Response::builder()
                .status(404)
                .header("Cache-Control", "no-store")
                .body(Vec::new())
                .unwrap()
        }
    }
}

/// Serve full-resolution media directly as binary. HEIC/HEIF are transcoded to JPEG.
fn serve_full_media(
    path: &str,
) -> tauri::http::Response<Vec<u8>> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // HEIC/HEIF: transcode to JPEG since browsers can't render natively
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
                            .body(jpeg_data)
                            .unwrap();
                    }
                    Err(e) => {
                        log::error!("HEIC→JPEG encode failed for {}: {}", path, e);
                        return tauri::http::Response::builder()
                            .status(500)
                            .body(Vec::new())
                            .unwrap();
                    }
                }
            }
            Err(e) => {
                log::error!("HEIC decode failed for {}: {}", path, e);
                return tauri::http::Response::builder()
                    .status(500)
                    .body(Vec::new())
                    .unwrap();
            }
        }
    }

    // All other formats: read and serve directly
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

    match std::fs::read(path) {
        Ok(data) => tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", mime)
            .header("Cache-Control", "no-cache")
            .body(data)
            .unwrap(),
        Err(e) => {
            log::error!("Failed to read media file {}: {}", path, e);
            tauri::http::Response::builder()
                .status(404)
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
                    .body(Vec::new())
                    .unwrap();
            }

            match route {
                Route::Thumb => serve_thumbnail(&ctx, &path),
                Route::Media => serve_full_media(&path),
                Route::Unknown => tauri::http::Response::builder()
                    .status(404)
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

            // GPU capability notification
            lightview_lib::pipeline::gpu::notify_gpu_capabilities,

            // File operations (copy/move)
            commands::files::copy_files,
            commands::files::move_files,

            // Duplicate detection
            commands::duplicates::find_duplicates,

            // Settings / maintenance commands
            commands::settings::get_hardware_profile,
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
