use lightview_lib::AppState;
use lightview_lib::cache::thumbnails::ThumbTier;
use lightview_lib::commands;
use lightview_lib::http_server::{self, HttpConfig};
use lightview_lib::thumb_serve::{self, ThumbOutcome, ThumbhashOutcome};
use tauri::{Emitter, Manager, UriSchemeResponder};


enum Route {
    /// A tiered thumbnail. The tier defaults to Standard if the URL is
    /// the legacy form `thumb/<path>` (without a tier segment).
    Thumb(ThumbTier),
    /// Decoded ThumbHash placeholder served as a tiny PNG.
    ThumbHash,
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
///
/// Full-resolution media is served by the local axum HTTP server (see
/// `http_server/routes.rs`) — one streaming code path for both `<img>`
/// and `<video>` elements.
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
                    ("jh/", ThumbTier::JustifiedHigh),
                    ("jm/", ThumbTier::JustifiedMid),
                    ("j/", ThumbTier::Justified),
                ] {
                    if let Some(rest) = p.strip_prefix(*seg) {
                        return (Route::Thumb(*tier), rest);
                    }
                }
                // Legacy form: no tier segment → standard.
                return (Route::Thumb(ThumbTier::Standard), p);
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
        // Cacheable: the frontend bumps a ?v= cache-buster whenever a
        // thumbnail is (re)generated and bumps an epoch on full rebuilds,
        // so within a session a URL's bytes never change. max-age bounds
        // cross-session staleness for plain (un-versioned) URLs.
        .header("Cache-Control", "public, max-age=3600")
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

fn thumb_status_response(status: u16) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .body(Vec::new())
        .unwrap()
}

/// No cached bytes for `(path, tier)` on first read. Delegates to the shared
/// generate-with-coalescing helper and wraps the outcome in a Tauri response.
async fn serve_thumbnail_generate(
    state: &AppState,
    tier: ThumbTier,
    path: String,
) -> tauri::http::Response<Vec<u8>> {
    match thumb_serve::get_or_generate(state, tier, path).await {
        ThumbOutcome::Hit { data, format } => thumb_ok_response(data, &format),
        ThumbOutcome::NoGallery => thumb_status_response(503),
        ThumbOutcome::Miss => thumb_miss_response(),
    }
}

/// Serve a decoded ThumbHash as a tiny PNG. The PNG is cacheable in the
/// webview because the underlying blob is immutable for a given path.
fn serve_thumbhash(state: &AppState, path: &str) -> tauri::http::Response<Vec<u8>> {
    match thumb_serve::render_thumbhash_png(state, path) {
        ThumbhashOutcome::Png(png) => tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", "image/png")
            // ThumbHashes are immutable for a given file mtime — long cache OK.
            .header("Cache-Control", "public, max-age=31536000, immutable")
            .header("Access-Control-Allow-Origin", "*")
            .body(png)
            .unwrap(),
        ThumbhashOutcome::NoGallery => thumb_status_response(503),
        ThumbhashOutcome::Miss => thumb_status_response(404),
        ThumbhashOutcome::Error => thumb_status_response(500),
    }
}

fn main() {
    // Rendering prefs are read here, before GTK/WebKit init. An explicit
    // environment variable always wins; otherwise the persisted RenderConfig
    // applies; otherwise the built-in defaults. Changing the config needs a
    // restart (these env vars only take effect at process start).
    let render_cfg = lightview_lib::load_render_config();
    // SAFETY: Called before any threads are spawned (start of main).
    unsafe {
        // WebKitGTK can crash with protocol errors on some Wayland compositors;
        // x11 (XWayland) is the safe default.
        if std::env::var("GDK_BACKEND").is_err() {
            let backend = render_cfg.gtk_backend.as_deref().unwrap_or("x11");
            std::env::set_var("GDK_BACKEND", backend);
        }
        // DMA-BUF/GPU compositing is disabled by default to avoid GBM buffer
        // allocation failures on some systems. Opting in via the config can
        // dramatically speed up image upload + scrolling where it's stable.
        if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            let disable = match render_cfg.gpu_acceleration {
                Some(true) => "0",
                Some(false) | None => "1",
            };
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", disable);
        }
    }

    env_logger::init();

    // TEMP perf instrumentation: record the main (GTK) thread so the thumbnail
    // handler logs can show whether it runs on it. Remove once measured.
    log::info!("[perf] main/GTK thread id = {:?}", std::thread::current().id());

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .register_asynchronous_uri_scheme_protocol(
            "lightview",
            |ctx, request, responder: UriSchemeResponder| {
                let uri = request.uri().to_string();

                // Route: lightview://thumb/<path> or lightview://thumbhash/<path>
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

                // TEMP perf instrumentation: which thread does WebKitGTK invoke
                // this handler on? If it matches the main/GTK thread logged at
                // startup, the inline-read fix was relevant; if not, the handler
                // was already off-thread. Remove once measured.
                log::debug!(
                    "[perf] scheme handler entry thread={:?} uri={}",
                    std::thread::current().id(),
                    uri
                );

                match route {
                    Route::Thumb(tier) => {
                        // Always answer off the GTK main thread. The DB read
                        // (and any generate-on-miss) runs on the async runtime /
                        // blocking pool so resource loads never block scrolling
                        // or painting. AppState is a cheap Arc-bundle clone.
                        let state_owned: AppState =
                            (*ctx.app_handle().state::<AppState>()).clone();
                        // The desktop user is browsing — hold the idle worker off.
                        state_owned.touch_thumb_activity();
                        // TEMP perf instrumentation.
                        let log_path = path.clone();
                        tauri::async_runtime::spawn(async move {
                            let t0 = std::time::Instant::now();
                            let resp =
                                serve_thumbnail_generate(&state_owned, tier, path).await;
                            log::debug!(
                                "[perf] thumb served thread={:?} elapsed={:?} status={} path={}",
                                std::thread::current().id(),
                                t0.elapsed(),
                                resp.status(),
                                log_path
                            );
                            responder.respond(resp);
                        });
                    }
                    Route::ThumbHash => {
                        // ThumbHash decode + PNG encode is CPU work — run it on
                        // the blocking pool, not the GTK main thread.
                        let state_owned: AppState =
                            (*ctx.app_handle().state::<AppState>()).clone();
                        tauri::async_runtime::spawn(async move {
                            let resp = tokio::task::spawn_blocking(move || {
                                serve_thumbhash(&state_owned, &path)
                            })
                            .await
                            .unwrap_or_else(|_| thumb_status_response(500));
                            responder.respond(resp);
                        });
                    }
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
            let app_state: AppState = (*app.state::<AppState>()).clone();
            match tauri::async_runtime::block_on(
                http_server::start(HttpConfig::local_only(), app_state.clone()),
            ) {
                Ok(server) => {
                    let url = format!("http://{}", server.addr);
                    log::info!("Media server URL: {}", url);
                    let _ = app_state.media_server_url.set(url.clone());

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
            commands::plugins::apply_plugin_tags,

            // Viewer commands (GPU-accelerated transforms)
            commands::viewer::get_transformed_media,
            commands::viewer::record_view,

            // File operations (copy/move)
            commands::files::copy_files,
            commands::files::move_files,
            commands::files::copy_files_to_clipboard,

            // App-managed trash
            commands::trash::trash_files,
            commands::trash::list_trash,
            commands::trash::restore_trash,
            commands::trash::purge_trash,

            // Duplicate detection
            commands::duplicates::find_duplicates,
            commands::duplicates::mark_not_duplicates,
            commands::duplicates::get_merge_candidates,
            commands::duplicates::merge_duplicates,

            // Settings / maintenance commands
            commands::settings::get_hardware_profile,
            commands::settings::get_memory_status,
            commands::settings::get_media_server_url,
            commands::settings::enable_remote_access,
            commands::settings::disable_remote_access,
            commands::settings::get_remote_access_info,
            commands::settings::get_remote_auth_state,
            commands::settings::generate_pairing_code,
            commands::settings::revoke_remote_device,
            commands::settings::delete_remote_device,
            commands::settings::set_remote_password,
            commands::settings::clear_remote_password,
            commands::settings::set_remote_inactivity,
            commands::settings::get_upload_config,
            commands::settings::set_upload_config,
            commands::settings::get_server_capabilities,
            commands::settings::get_remote_delete_config,
            commands::settings::set_remote_delete_config,
            commands::settings::reindex_gallery,
            commands::settings::rebuild_thumbnails,
            commands::settings::clear_cache,
            commands::settings::get_gallery_stats,
            commands::settings::get_debug_info,
            commands::settings::get_perf_snapshot,
            commands::settings::save_gallery_settings,
            commands::settings::load_gallery_settings,
            commands::settings::get_gallery_default_filter,
            commands::settings::get_recent_galleries,
            commands::settings::remove_recent_gallery,
            commands::settings::get_render_config,
            commands::settings::set_render_config,
            commands::settings::open_with,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
