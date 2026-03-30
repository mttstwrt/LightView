use lightview_lib::AppState;
use lightview_lib::commands;
use tauri::{Emitter, Manager};

/// Create a BMP image from raw RGBA pixels.
/// Uses top-down row order (negative height) to avoid row flipping.
/// Swaps R↔B channels since BMP uses BGRA.
fn rgba_to_bmp(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixel_data_size = (width * height * 4) as usize;
    let file_size = 54 + pixel_data_size;
    let mut bmp = Vec::with_capacity(file_size);

    // File header (14 bytes)
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp.extend_from_slice(&[0u8; 4]); // reserved
    bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset

    // BITMAPINFOHEADER (40 bytes)
    bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
    bmp.extend_from_slice(&(width as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(height as i32)).to_le_bytes()); // negative = top-down
    bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
    bmp.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    bmp.extend_from_slice(&0u32.to_le_bytes()); // compression (BI_RGB)
    bmp.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    bmp.extend_from_slice(&[0u8; 16]); // ppm x, ppm y, colors used, colors important

    // Pixel data: RGBA → BGRA
    for chunk in rgba.chunks_exact(4) {
        bmp.push(chunk[2]); // B
        bmp.push(chunk[1]); // G
        bmp.push(chunk[0]); // R
        bmp.push(chunk[3]); // A
    }

    bmp
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

            // Expected: lightview://thumb/<url-encoded-path>
            // On Linux: lightview://localhost/thumb/<path>
            // On Windows: http://lightview.localhost/thumb/<path>
            let path_part = uri
                .strip_prefix("lightview://thumb/")
                .or_else(|| uri.strip_prefix("lightview://localhost/thumb/"))
                .or_else(|| uri.strip_prefix("http://lightview.localhost/thumb/"))
                .unwrap_or("");
            // Strip query string (e.g. ?v=1 cache-buster) before decoding.
            // Safe before percent-decode: literal '?' in paths is encoded as %3F.
            let path_part = path_part.split('?').next().unwrap_or(path_part);
            let path = percent_encoding::percent_decode_str(path_part)
                .decode_utf8_lossy()
                .to_string();

            if path.is_empty() {
                return tauri::http::Response::builder()
                    .status(400)
                    .body(Vec::new())
                    .unwrap();
            }

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
                        let bmp = rgba_to_bmp(&data, width, height);
                        tauri::http::Response::builder()
                            .status(200)
                            .header("Content-Type", "image/bmp")
                            .header("Cache-Control", "no-cache")
                            .body(bmp)
                            .unwrap()
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
