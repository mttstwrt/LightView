use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

use crate::cache::atlas::ThumbAtlas;
use crate::cache::db::CacheDb;
use crate::companion::schema::MediaType;
use crate::provider::local::LocalProvider;
use crate::provider::{FileEntry, ProviderType};
use crate::AppState;

#[derive(Debug, Serialize)]
pub struct GalleryOpenResult {
    pub path: String,
    pub total_media: usize,
    pub provider_type: ProviderType,
}

/// Populate the media_meta table from scanned file entries.
fn populate_media_meta(
    db: &CacheDb,
    entries: &[FileEntry],
) -> Result<(), String> {
    let conn = db.conn();
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| e.to_string())?;

    {
        let mut stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO media_meta (path, date_taken, file_size, media_type, width, height, duration)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL)",
            )
            .map_err(|e| e.to_string())?;

        for entry in entries {
            if entry.is_dir {
                continue;
            }
            let ext = Path::new(&entry.name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let media_type = match MediaType::from_extension(ext) {
                Some(mt) => mt.as_str().to_string(),
                None => continue,
            };
            stmt.execute(rusqlite::params![
                entry.path,
                entry.mtime as i64,
                entry.size as i64,
                media_type,
            ])
            .map_err(|e| e.to_string())?;
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Walk all companion files in the gallery and index their tags into the
/// cache DB.  Rebuilds `tag_counts` from `tag_index` at the end so that
/// autocomplete and filter reflect every tag on disk — including those
/// written by plugins or carried over from previous sessions.
fn index_companions(db: &CacheDb, gallery_path: &str) {
    let ext = crate::companion::schema::COMPANION_EXTENSION;
    let mut indexed = 0u64;

    for entry in walkdir::WalkDir::new(gallery_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let path_str = path.to_string_lossy();
        if !path_str.ends_with(ext) {
            continue;
        }

        // Reconstruct the media file path from the companion path.
        let companion_str = path_str.to_string();
        let base = companion_str.strip_suffix(ext).unwrap_or(&companion_str);

        let media_path_str = if let Some(parent) = path.parent() {
            let parent_name = parent
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
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

    if indexed > 0 {
        let _ = db.rebuild_tag_counts();
        log::info!("Indexed tags from {} companion files", indexed);
    }
}

/// Open a gallery directory. Initializes the provider, cache DB,
/// and begins the background scan/thumbnail/index pipeline.
#[tauri::command]
pub async fn open_gallery(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<GalleryOpenResult, String> {
    // Create the local provider
    let provider = Arc::new(LocalProvider::new(&path));

    // Register it
    {
        let mut reg = state.providers.write().await;
        reg.register(path.clone(), provider.clone());
    }

    // Open (or create) the cache database
    let cache_db = CacheDb::open(std::path::Path::new(&path))
        .map_err(|e| format!("Failed to open cache: {}", e))?;

    // Recursively scan for all media files in the directory tree
    let entries: Vec<crate::provider::FileEntry> = provider
        .list_dir_recursive(&path)
        .map_err(|e| format!("Failed to scan directory: {}", e))?;

    let media_count = entries.len();

    // Populate media_meta table so sorting works immediately
    populate_media_meta(&cache_db, &entries)?;

    // Re-index all companion files so the tag_index and tag_counts tables
    // reflect what's on disk.  This makes tags from previous sessions (and
    // from plugins) available for filtering and autocomplete immediately.
    index_companions(&cache_db, &path);

    if let Ok(counts) = cache_db.query_all_tag_counts() {
        if !counts.is_empty() {
            log::info!("Loaded {} unique tags into autocomplete", counts.len());
            state.autocomplete.refresh(counts).await;
        }
    }

    // Restore thumbnail settings from gallery_meta if present
    if let Ok(Some(json)) = cache_db.get_gallery_meta("thumbnail_settings") {
        if let Ok(thumb_settings) = serde_json::from_str::<crate::pipeline::thumbnailer::ThumbnailSettings>(&json) {
            log::info!(
                "Restored thumbnail settings: format={:?}, {}x{}, filter={:?}",
                thumb_settings.format,
                thumb_settings.width,
                thumb_settings.height,
                thumb_settings.resize_filter,
            );
            let mut ts = state.thumbnail_settings.write().await;
            *ts = thumb_settings;
        }
    }

    // Open a second read-only connection for the thumbnail protocol handler.
    // SQLite WAL mode supports concurrent readers.
    {
        let db_path = std::path::Path::new(&path).join(".lightview").join("cache.db");
        match rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(conn) => {
                let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
                let _ = conn.execute_batch("PRAGMA cache_size=-16000;"); // 16MB read cache
                let mut proto_db = state.thumb_protocol_db.lock().unwrap();
                *proto_db = Some(conn);
            }
            Err(e) => {
                log::warn!("Failed to open read-only DB for protocol handler: {}", e);
            }
        }
    }

    // Store the cache DB
    {
        let mut db = state.cache_db.lock().await;
        *db = Some(cache_db);
    }

    // Initialize BC7 atlas if hardware supports it
    let lightview_dir = std::path::Path::new(&path).join(".lightview");
    if state.should_use_bc7() {
        match ThumbAtlas::open(&lightview_dir) {
            Ok(atlas) => {
                log::info!(
                    "BC7 atlas opened with {} thumbnails",
                    atlas.len()
                );
                let mut a = state.thumb_atlas.lock().await;
                *a = Some(atlas);
                state
                    .use_bc7_atlas
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                log::warn!("Failed to open BC7 atlas, falling back to JPEG: {}", e);
                state
                    .use_bc7_atlas
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    } else {
        state
            .use_bc7_atlas
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    // Store current gallery path
    {
        let mut current = state.current_gallery.write().await;
        *current = Some(path.clone());
    }

    // Track as recently opened
    state.add_recent_gallery(&path).await;

    Ok(GalleryOpenResult {
        path,
        total_media: media_count,
        provider_type: ProviderType::Local,
    })
}

/// Close the current gallery and release resources.
#[tauri::command]
pub async fn close_gallery(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let gallery_path = {
        let current = state.current_gallery.read().await;
        current.clone()
    };

    if let Some(path) = gallery_path {
        // Remove provider
        let mut reg = state.providers.write().await;
        reg.remove(&path);
    }

    // Flush and close BC7 atlas
    {
        let mut atlas = state.thumb_atlas.lock().await;
        if let Some(ref mut a) = *atlas {
            let _ = a.sync();
        }
        *atlas = None;
        state
            .use_bc7_atlas
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    // Close cache DB
    {
        let mut db = state.cache_db.lock().await;
        *db = None;
    }

    // Close protocol handler DB
    {
        let mut proto_db = state.thumb_protocol_db.lock().unwrap();
        *proto_db = None;
    }

    // Clear current gallery
    {
        let mut current = state.current_gallery.write().await;
        *current = None;
    }

    Ok(())
}

/// Get information about the currently open gallery.
#[tauri::command]
pub async fn get_gallery_info(
    state: tauri::State<'_, AppState>,
) -> Result<Option<GalleryOpenResult>, String> {
    let current = state.current_gallery.read().await;
    match current.as_ref() {
        Some(path) => {
            let reg = state.providers.read().await;
            match reg.get(path) {
                Some(_provider) => {
                    // Use the media_meta table for count — it was populated
                    // from the recursive scan during open_gallery.
                    let db = state.cache_db.lock().await;
                    let media_count = if let Some(db) = db.as_ref() {
                        db.conn()
                            .query_row("SELECT COUNT(*) FROM media_meta", [], |r| r.get::<_, usize>(0))
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    Ok(Some(GalleryOpenResult {
                        path: path.clone(),
                        total_media: media_count,
                        provider_type: _provider.provider_type(),
                    }))
                }
                None => Ok(None),
            }
        }
        None => Ok(None),
    }
}
