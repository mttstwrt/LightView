use serde::Serialize;

use crate::hardware::HardwareProfile;
use crate::pipeline::thumbnailer::ThumbnailSettings;
use crate::{AppState, RecentGallery};

/// Debug information about which hardware optimizations are active.
#[derive(Debug, Serialize)]
pub struct DebugInfo {
    pub storage_type: String,
    pub filesystem: String,
    pub cpu_cores: usize,
    pub total_ram_mb: u64,
    pub gpu_compute: bool,
    pub supports_reflink: bool,
    pub thumbnail_threads: usize,
    pub prefetch_count: usize,
    pub lru_cache_size: usize,
    pub bc7_atlas_active: bool,
    pub thumb_format: String,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub atlas_entry_count: usize,
    pub sqlite_thumbnail_count: u64,
    pub gpu_resize_active: bool,
}

#[derive(Debug, Serialize)]
pub struct GalleryStats {
    pub total_media: u64,
    pub index_size_bytes: u64,
    pub cache_size_bytes: u64,
    pub unique_tags: u64,
}

/// Get the detected hardware profile.
#[tauri::command]
pub async fn get_hardware_profile(
    state: tauri::State<'_, AppState>,
) -> Result<HardwareProfile, String> {
    Ok((*state.hardware).clone())
}

/// Trigger a full re-index of all companion files.
#[tauri::command]
pub async fn reindex_gallery(
    state: tauri::State<'_, AppState>,
) -> Result<u64, String> {
    let gallery_path = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    // Clear existing index
    db.clear_tag_index().map_err(|e| e.to_string())?;

    // Walk the gallery and re-index all companion files
    let ext = crate::companion::schema::COMPANION_EXTENSION;
    let mut indexed = 0u64;
    for entry in walkdir::WalkDir::new(&gallery_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let path_str = path.to_string_lossy();
        if !path_str.ends_with(ext) {
            continue;
        }

        // Reconstruct the media file path.
        // Companion can be alongside (dir/photo.jpg.lightview.json → dir/photo.jpg)
        // or in .lightview/companions/ (dir/.lightview/companions/photo.jpg.lightview.json → dir/photo.jpg)
        let companion_str = path_str.to_string();
        let base = companion_str.strip_suffix(ext).unwrap_or(&companion_str);

        let media_path_str = if let Some(parent) = path.parent() {
            let parent_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if parent_name == "companions" {
                // .lightview/companions/photo.jpg.lightview.json → ../../photo.jpg
                if let Some(lightview_dir) = parent.parent() {
                    if let Some(gallery_dir) = lightview_dir.parent() {
                        let filename = std::path::Path::new(base)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(base);
                        gallery_dir.join(filename).to_string_lossy().to_string()
                    } else {
                        base.to_string()
                    }
                } else {
                    base.to_string()
                }
            } else {
                base.to_string()
            }
        } else {
            base.to_string()
        };

        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(companion) = crate::companion::reader::parse_companion(&contents) {
                let _ = db.reindex_tags_for_file(&media_path_str, &companion);
                indexed += 1;
            }
        }
    }

    // Rebuild counts
    db.rebuild_tag_counts().map_err(|e| e.to_string())?;

    // Refresh autocomplete
    if let Ok(counts) = db.query_all_tag_counts() {
        state.autocomplete.refresh(counts).await;
    }

    Ok(indexed)
}

/// Trigger a full thumbnail rebuild (clear and regenerate).
#[tauri::command]
pub async fn rebuild_thumbnails(
    state: tauri::State<'_, AppState>,
) -> Result<u64, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let cleared = db.clear_thumbnails().map_err(|e| e.to_string())?;

    // Also clear the BC7 atlas if active
    {
        let mut atlas = state.thumb_atlas.lock().await;
        if let Some(ref mut a) = *atlas {
            a.clear().map_err(|e| e.to_string())?;
        }
    }

    // TODO: Kick off background thumbnail regeneration pipeline

    Ok(cleared as u64)
}

/// Clear the entire cache database.
#[tauri::command]
pub async fn clear_cache(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let gallery_path = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;

    let cache_path = std::path::Path::new(&gallery_path)
        .join(".lightview")
        .join("cache.db");

    // Close the atlas first
    {
        let mut atlas = state.thumb_atlas.lock().await;
        *atlas = None;
        state
            .use_bc7_atlas
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    // Close the DB
    {
        let mut db = state.cache_db.lock().await;
        *db = None;
    }

    // Delete the cache file
    if cache_path.exists() {
        std::fs::remove_file(&cache_path).map_err(|e| e.to_string())?;
    }

    // Delete atlas files
    let lightview_dir = std::path::Path::new(&gallery_path).join(".lightview");
    let atlas_bin = lightview_dir.join("thumb_atlas.bin");
    let atlas_idx = lightview_dir.join("thumb_atlas.idx");
    if atlas_bin.exists() {
        let _ = std::fs::remove_file(&atlas_bin);
    }
    if atlas_idx.exists() {
        let _ = std::fs::remove_file(&atlas_idx);
    }

    // Reopen fresh
    let new_db = crate::cache::db::CacheDb::open(std::path::Path::new(&gallery_path))
        .map_err(|e| e.to_string())?;
    {
        let mut db = state.cache_db.lock().await;
        *db = Some(new_db);
    }

    Ok(())
}

/// Get debug information about which hardware optimizations are active.
#[tauri::command]
pub async fn get_debug_info(
    state: tauri::State<'_, AppState>,
) -> Result<DebugInfo, String> {
    let hw = &*state.hardware;
    let bc7_active = state
        .use_bc7_atlas
        .load(std::sync::atomic::Ordering::Relaxed);

    let atlas_entries = {
        let atlas = state.thumb_atlas.lock().await;
        atlas.as_ref().map(|a| a.len()).unwrap_or(0)
    };

    let sqlite_thumbs = {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            db.conn()
                .query_row("SELECT COUNT(*) FROM thumbnails", [], |row| row.get::<_, u64>(0))
                .unwrap_or(0)
        } else {
            0
        }
    };

    let thumb_settings = state.thumbnail_settings.read().await.clone();
    let thumb_format = format!("{:?}", thumb_settings.format).to_lowercase();

    log::info!("=== Hardware Debug Info ===");
    log::info!("  Storage:      {:?}", hw.storage_type);
    log::info!("  Filesystem:   {}", hw.filesystem);
    log::info!("  CPU cores:    {}", hw.cpu_cores);
    log::info!("  RAM:          {} MB", hw.total_ram_mb);
    log::info!("  GPU compute:  {}", hw.gpu_compute);
    log::info!("  Reflink:      {}", hw.supports_reflink);
    log::info!("  Thumb format: {}", &thumb_format);
    log::info!("  Thumb size:   {}x{}", thumb_settings.width, thumb_settings.height);
    log::info!("  BC7 atlas:    {} ({} entries)", bc7_active, atlas_entries);
    let gpu_active = state.has_gpu();

    log::info!("  SQLite cache: {} thumbnails", sqlite_thumbs);
    log::info!("  GPU pipeline: {}", gpu_active);
    log::info!("  Thumb pool:   {} threads", hw.thumbnail_threads());
    log::info!("  Prefetch:     {} images", hw.prefetch_count());
    log::info!("  LRU cache:    {} images", hw.lru_cache_size());
    log::info!("==========================");

    Ok(DebugInfo {
        storage_type: format!("{:?}", hw.storage_type),
        filesystem: hw.filesystem.clone(),
        cpu_cores: hw.cpu_cores,
        total_ram_mb: hw.total_ram_mb,
        gpu_compute: hw.gpu_compute,
        supports_reflink: hw.supports_reflink,
        thumbnail_threads: hw.thumbnail_threads(),
        prefetch_count: hw.prefetch_count(),
        lru_cache_size: hw.lru_cache_size(),
        bc7_atlas_active: bc7_active,
        thumb_format,
        thumb_width: thumb_settings.width,
        thumb_height: thumb_settings.height,
        atlas_entry_count: atlas_entries,
        sqlite_thumbnail_count: sqlite_thumbs,
        gpu_resize_active: gpu_active,
    })
}

/// Get the current thumbnail settings.
#[tauri::command]
pub async fn get_thumbnail_settings(
    state: tauri::State<'_, AppState>,
) -> Result<ThumbnailSettings, String> {
    Ok(state.thumbnail_settings.read().await.clone())
}

/// Update thumbnail settings. Returns the new settings after applying.
/// Also persists to the gallery's .lightview folder if a gallery is open.
#[tauri::command]
pub async fn update_thumbnail_settings(
    state: tauri::State<'_, AppState>,
    settings: ThumbnailSettings,
) -> Result<ThumbnailSettings, String> {
    if settings.width == 0 || settings.height == 0 {
        return Err("Thumbnail dimensions must be > 0".to_string());
    }
    if settings.width > 2048 || settings.height > 2048 {
        return Err("Thumbnail dimensions must be <= 2048".to_string());
    }

    let mut current = state.thumbnail_settings.write().await;
    *current = settings;
    log::info!(
        "Thumbnail settings updated: format={:?}, {}x{}, filter={:?}",
        current.format,
        current.width,
        current.height,
        current.resize_filter,
    );

    // Persist to gallery_meta if a gallery is open
    if let Ok(json) = serde_json::to_string(&*current) {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let _ = db.set_gallery_meta("thumbnail_settings", &json);
        }
    }

    Ok(current.clone())
}

/// Save frontend app settings to the current gallery's .lightview folder.
#[tauri::command]
pub async fn save_gallery_settings(
    state: tauri::State<'_, AppState>,
    settings_json: String,
) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.set_gallery_meta("app_settings", &settings_json)
        .map_err(|e| e.to_string())
}

/// Load frontend app settings from the current gallery's .lightview folder.
#[tauri::command]
pub async fn load_gallery_settings(
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.get_gallery_meta("app_settings")
        .map_err(|e| e.to_string())
}

/// Get gallery statistics.
#[tauri::command]
pub async fn get_gallery_stats(
    state: tauri::State<'_, AppState>,
) -> Result<GalleryStats, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let total_media: u64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM media_meta", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;

    let unique_tags: u64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tag_counts", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;

    // Estimate cache size from thumbnail count * avg size
    let cache_size: u64 = db
        .conn()
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(thumbnail)), 0) FROM thumbnails",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;

    let index_size: u64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tag_index", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;

    Ok(GalleryStats {
        total_media,
        index_size_bytes: index_size * 100, // rough estimate per row
        cache_size_bytes: cache_size,
        unique_tags,
    })
}

/// Lightweight performance snapshot for the debug overlay.
/// Reads /proc/self/io for disk bandwidth and queries cache stats.
#[derive(Debug, Serialize)]
pub struct PerfSnapshot {
    /// Bytes read from disk since process start (from /proc/self/io).
    pub disk_read_bytes: u64,
    /// Bytes written to disk since process start (from /proc/self/io).
    pub disk_write_bytes: u64,
    /// Number of cached thumbnails in SQLite.
    pub cached_thumbnails: u64,
    /// Total byte size of cached thumbnail data.
    pub cache_size_bytes: u64,
    /// Number of entries in the BC7 atlas (0 if inactive).
    pub atlas_entries: usize,
    /// Current rayon thread pool active thread count (approximate).
    pub thumb_pool_active_threads: usize,
}

/// Read /proc/self/io counters (Linux-only, returns 0 on other platforms).
fn read_proc_io() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/self/io") {
            let mut read_bytes = 0u64;
            let mut write_bytes = 0u64;
            for line in contents.lines() {
                if let Some(val) = line.strip_prefix("read_bytes: ") {
                    read_bytes = val.trim().parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("write_bytes: ") {
                    write_bytes = val.trim().parse().unwrap_or(0);
                }
            }
            return (read_bytes, write_bytes);
        }
    }
    (0, 0)
}

#[tauri::command]
pub async fn get_perf_snapshot(
    state: tauri::State<'_, AppState>,
) -> Result<PerfSnapshot, String> {
    let (disk_read_bytes, disk_write_bytes) = read_proc_io();

    let (cached_thumbnails, cache_size_bytes) = {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let count: u64 = db.conn()
                .query_row("SELECT COUNT(*) FROM thumbnails", [], |row| row.get(0))
                .unwrap_or(0);
            let size: u64 = db.conn()
                .query_row("SELECT COALESCE(SUM(LENGTH(thumbnail)), 0) FROM thumbnails", [], |row| row.get(0))
                .unwrap_or(0);
            (count, size)
        } else {
            (0, 0)
        }
    };

    let atlas_entries = {
        let atlas = state.thumb_atlas.lock().await;
        atlas.as_ref().map(|a| a.len()).unwrap_or(0)
    };

    Ok(PerfSnapshot {
        disk_read_bytes,
        disk_write_bytes,
        cached_thumbnails,
        cache_size_bytes,
        atlas_entries,
        thumb_pool_active_threads: state.hardware.thumbnail_threads(),
    })
}

/// Get the list of recently opened galleries (most recent first).
#[tauri::command]
pub async fn get_recent_galleries(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<RecentGallery>, String> {
    let recents = state.recent_galleries.lock().await;
    Ok(recents.clone())
}

/// Remove a gallery from the recent list (e.g. if the folder no longer exists).
#[tauri::command]
pub async fn remove_recent_gallery(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let mut recents = state.recent_galleries.lock().await;
    recents.retain(|r| r.path != path);
    crate::save_recent_galleries(&recents);
    Ok(())
}

/// Open a file with an external application.
/// Spawns the command as a detached process — does not wait for it to exit.
#[tauri::command]
pub async fn open_with(
    _state: tauri::State<'_, AppState>,
    command: String,
    args: Vec<String>,
) -> Result<(), String> {
    tokio::process::Command::new(&command)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch '{}': {}", command, e))?;
    Ok(())
}
