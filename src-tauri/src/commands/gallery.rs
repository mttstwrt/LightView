use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tauri::Emitter;

use crate::cache::atlas::ThumbAtlas;
use crate::cache::db::CacheDb;
use crate::companion::schema::MediaType;
use crate::pipeline::exif;
use crate::provider::local::LocalProvider;
use crate::provider::{FileEntry, ProviderType};
use crate::util::fs_watch::FsWatcher;
use crate::AppState;
use rayon::prelude::*;

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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO media_meta (path, date_taken, file_size, media_type, width, height, duration, date_added)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5)",
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
                now,
            ])
            .map_err(|e| e.to_string())?;
        }
    }

    // Prune stale entries — files in DB but no longer on disk
    {
        let on_disk: HashSet<&str> = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.as_str())
            .collect();

        let mut sel = tx
            .prepare_cached("SELECT path FROM media_meta")
            .map_err(|e| e.to_string())?;
        let stale: Vec<String> = sel
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .filter(|p| !on_disk.contains(p.as_str()))
            .collect();

        if !stale.is_empty() {
            log::info!("Pruning {} stale media_meta entries", stale.len());
            let mut del_meta = tx
                .prepare_cached("DELETE FROM media_meta WHERE path = ?1")
                .map_err(|e| e.to_string())?;
            let mut del_thumb = tx
                .prepare_cached("DELETE FROM thumbnails WHERE path = ?1")
                .map_err(|e| e.to_string())?;
            let mut del_tag = tx
                .prepare_cached("DELETE FROM tag_index WHERE path = ?1")
                .map_err(|e| e.to_string())?;
            for path in &stale {
                let _ = del_meta.execute(rusqlite::params![path]);
                let _ = del_thumb.execute(rusqlite::params![path]);
                let _ = del_tag.execute(rusqlite::params![path]);
            }
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Read EXIF GPS for every image still missing it and write the result into
/// `media_meta`. Runs in parallel via rayon; per-file failures are silent.
/// Skipped for videos — those flow through `populate_video_metadata`/ffprobe.
fn backfill_gps_meta(db: &CacheDb) -> Result<(), String> {
    let candidates: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare_cached(
                "SELECT path FROM media_meta
                 WHERE gps_lat IS NULL AND media_type = 'image'",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    if candidates.is_empty() {
        return Ok(());
    }

    log::info!("Extracting EXIF GPS for {} images", candidates.len());

    let extracted: Vec<(String, f64, f64)> = candidates
        .par_iter()
        .filter_map(|path| {
            exif::extract_location(Path::new(path))
                .map(|loc| (path.clone(), loc.lat, loc.lon))
        })
        .collect();

    if extracted.is_empty() {
        return Ok(());
    }

    let conn = db.conn();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    {
        let mut stmt = tx
            .prepare_cached(
                "UPDATE media_meta SET gps_lat = ?1, gps_lon = ?2 WHERE path = ?3",
            )
            .map_err(|e| e.to_string())?;
        for (path, lat, lon) in &extracted {
            let _ = stmt.execute(rusqlite::params![lat, lon, path]);
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    log::info!(
        "EXIF GPS backfill: {}/{} images had coordinates",
        extracted.len(),
        candidates.len()
    );
    Ok(())
}

/// Walk all companion files in the gallery and index their tags into the
/// cache DB.  Rebuilds `tag_counts` from `tag_index` at the end so that
/// autocomplete and filter reflect every tag on disk — including those
/// written by plugins or carried over from previous sessions.
fn index_companions(db: &CacheDb, gallery_path: &str) {
    let ext = crate::companion::schema::COMPANION_EXTENSION;

    // Load the whole index_state table once so the per-companion freshness
    // check is an in-memory lookup rather than a SQL round-trip per file.
    let index_state = db.load_index_state().unwrap_or_default();

    // Gather companion files without walking the media tree through .lightview.
    // Two sources:
    //   1. the dedicated `.lightview/companions/` directory (default location,
    //      where plugin output lands) — scanned directly.
    //   2. side-by-side companions in the media tree — found by walking the
    //      gallery while pruning the entire `.lightview` subtree so we don't
    //      re-stat the cache db, atlas, thumbnails, or the companions dir.
    let mut companions: Vec<walkdir::DirEntry> = Vec::new();

    let companions_dir = Path::new(gallery_path).join(".lightview").join("companions");
    if companions_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&companions_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().to_string_lossy().ends_with(ext) {
                companions.push(entry);
            }
        }
    }

    for entry in walkdir::WalkDir::new(gallery_path)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".lightview")
        .filter_map(|e| e.ok())
    {
        if entry.path().to_string_lossy().ends_with(ext) {
            companions.push(entry);
        }
    }

    // Index everything inside a single transaction. Without this each tag
    // insert is its own implicit commit, which dominates startup after a
    // tagging run touches thousands of companions.
    let tx = match db.conn().unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            log::warn!("Failed to begin companion index transaction: {}", e);
            return;
        }
    };

    let mut indexed = 0u64;
    let mut skipped = 0u64;

    {
        let mut del_stmt = match tx.prepare_cached("DELETE FROM tag_index WHERE path = ?1") {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Failed to prepare delete statement: {}", e);
                return;
            }
        };
        let mut ins_stmt = match tx.prepare_cached(
            "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Failed to prepare insert statement: {}", e);
                return;
            }
        };
        let mut state_stmt = match tx.prepare_cached(
            "INSERT OR REPLACE INTO index_state (path, companion_mtime) VALUES (?1, ?2)",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Failed to prepare index_state statement: {}", e);
                return;
            }
        };

        for entry in &companions {
            let path = entry.path();
            let companion_str = path.to_string_lossy().to_string();
            let base = companion_str.strip_suffix(ext).unwrap_or(&companion_str);

            // Reconstruct the media file path from the companion path.
            let media_path_str = if let Some(parent) = path.parent() {
                let parent_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if parent_name == "companions" {
                    // .lightview/companions/photo.jpg.lightview.json → ../../photo.jpg
                    parent
                        .parent()
                        .and_then(|lightview_dir| lightview_dir.parent())
                        .map(|gallery_dir| {
                            let filename = Path::new(base)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(base);
                            gallery_dir.join(filename).to_string_lossy().to_string()
                        })
                        .unwrap_or_else(|| base.to_string())
                } else {
                    base.to_string()
                }
            } else {
                base.to_string()
            };

            // Skip unchanged companions — compare mtime against the cached
            // index_state loaded above.
            let companion_mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            if companion_mtime > 0 && index_state.get(&media_path_str) == Some(&companion_mtime) {
                skipped += 1;
                continue;
            }

            if let Ok(contents) = std::fs::read_to_string(path) {
                if let Ok(companion) = crate::companion::reader::parse_companion(&contents) {
                    let _ = del_stmt.execute(rusqlite::params![media_path_str]);
                    for (namespace, tag) in companion.all_tags() {
                        let _ =
                            ins_stmt.execute(rusqlite::params![media_path_str, namespace, tag]);
                    }
                    let _ = state_stmt.execute(rusqlite::params![media_path_str, companion_mtime]);
                    indexed += 1;
                }
            }
        }
    }

    if let Err(e) = tx.commit() {
        log::warn!("Failed to commit companion index transaction: {}", e);
        return;
    }

    if indexed > 0 {
        let _ = db.rebuild_tag_counts();
    }
    if indexed > 0 || skipped > 0 {
        log::info!(
            "Companion indexing: {} indexed, {} unchanged (skipped)",
            indexed,
            skipped
        );
    }
}

/// Payload emitted for `gallery:fs-changed` events.
#[derive(Debug, Clone, Serialize)]
pub struct FsChangeEvent {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Start the filesystem watcher background task.
/// Polls for notify events, debounces them, updates the DB, and emits
/// `gallery:fs-changed` to the frontend.
fn start_fs_watcher(
    app_handle: tauri::AppHandle,
    state: &AppState,
    gallery_path: &str,
) {
    // Stop any existing watcher
    state
        .fs_watch_cancel
        .store(true, std::sync::atomic::Ordering::Relaxed);
    {
        let mut w = state.fs_watcher.lock().unwrap();
        *w = None;
    }
    state
        .fs_watch_cancel
        .store(false, std::sync::atomic::Ordering::Relaxed);

    let watcher = match FsWatcher::new(Path::new(gallery_path), true) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("Failed to start filesystem watcher: {}", e);
            return;
        }
    };

    {
        let mut w = state.fs_watcher.lock().unwrap();
        *w = Some(watcher);
    }

    let cancel = Arc::clone(&state.fs_watch_cancel);
    let fs_watcher = Arc::clone(&state.fs_watcher);
    let cache_db = Arc::clone(&state.cache_db);

    tauri::async_runtime::spawn(async move {
        use notify::EventKind;
        use std::sync::atomic::Ordering;

        const POLL_MS: u64 = 300;
        const DEBOUNCE_MS: u64 = 500;

        let lightview_suffix = std::path::MAIN_SEPARATOR.to_string() + ".lightview";
        let companion_ext = crate::companion::schema::COMPANION_EXTENSION;

        let mut pending_added: HashSet<String> = HashSet::new();
        let mut pending_removed: HashSet<String> = HashSet::new();
        let mut last_event_time: Option<tokio::time::Instant> = None;

        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_MS)).await;

            // Poll events from the watcher
            let events = {
                let w = fs_watcher.lock().unwrap();
                match w.as_ref() {
                    Some(watcher) => watcher.poll_events(),
                    None => break,
                }
            };

            for event in events {
                for path in &event.paths {
                    let path_str = path.to_string_lossy().to_string();

                    // Skip .lightview directory and companion files
                    if path_str.contains(&lightview_suffix) || path_str.ends_with(companion_ext)
                    {
                        continue;
                    }

                    // Only consider files with valid media extensions
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    if MediaType::from_extension(ext).is_none() {
                        continue;
                    }

                    match &event.kind {
                        EventKind::Create(_)
                        | EventKind::Modify(notify::event::ModifyKind::Name(
                            notify::event::RenameMode::To,
                        )) => {
                            pending_removed.remove(&path_str);
                            pending_added.insert(path_str);
                            last_event_time = Some(tokio::time::Instant::now());
                        }
                        EventKind::Remove(_)
                        | EventKind::Modify(notify::event::ModifyKind::Name(
                            notify::event::RenameMode::From,
                        )) => {
                            pending_added.remove(&path_str);
                            pending_removed.insert(path_str);
                            last_event_time = Some(tokio::time::Instant::now());
                        }
                        _ => {}
                    }
                }
            }

            // Flush after debounce period
            let should_flush = match last_event_time {
                Some(t) => t.elapsed().as_millis() >= DEBOUNCE_MS as u128,
                None => false,
            };

            if !should_flush || (pending_added.is_empty() && pending_removed.is_empty()) {
                continue;
            }

            let added: Vec<String> = pending_added.drain().collect();
            let removed: Vec<String> = pending_removed.drain().collect();
            last_event_time = None;

            // Update the database
            let db = cache_db.lock().await;
            if let Some(db) = db.as_ref() {
                let conn = db.conn();

                // Insert new files
                if !added.is_empty() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;

                    for path_str in &added {
                        let p = std::path::Path::new(path_str);
                        let ext = p
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        let media_type = match MediaType::from_extension(ext) {
                            Some(mt) => mt.as_str().to_string(),
                            None => continue,
                        };
                        // Read file metadata for mtime and size
                        let (mtime, size) = match std::fs::metadata(p) {
                            Ok(meta) => {
                                let mtime = meta
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(now);
                                (mtime, meta.len() as i64)
                            }
                            Err(_) => continue, // File may have been removed already
                        };

                        let _ = conn.execute(
                            "INSERT OR IGNORE INTO media_meta (path, date_taken, file_size, media_type, date_added) VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![path_str, mtime, size, media_type, now],
                        );
                    }
                }

                // Remove deleted files
                if !removed.is_empty() {
                    for path_str in &removed {
                        let _ = conn.execute(
                            "DELETE FROM media_meta WHERE path = ?1",
                            rusqlite::params![path_str],
                        );
                        let _ = conn.execute(
                            "DELETE FROM thumbnails WHERE path = ?1",
                            rusqlite::params![path_str],
                        );
                        let _ = conn.execute(
                            "DELETE FROM tag_index WHERE path = ?1",
                            rusqlite::params![path_str],
                        );
                    }
                    let _ = db.rebuild_tag_counts();
                }
            }

            // Emit event to frontend
            let _ = app_handle.emit(
                "gallery:fs-changed",
                FsChangeEvent {
                    added,
                    removed,
                },
            );
        }

        log::info!("Filesystem watcher task exited");
    });

    log::info!("Filesystem watcher started for {}", gallery_path);
}

/// Stop the filesystem watcher and clean up.
fn stop_fs_watcher(state: &AppState) {
    state
        .fs_watch_cancel
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut w = state.fs_watcher.lock().unwrap();
    *w = None;
}

/// Open a gallery directory. Initializes the provider, cache DB,
/// and begins the background scan/thumbnail/index pipeline.
#[tauri::command]
pub async fn open_gallery(
    app_handle: tauri::AppHandle,
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

    // Companion indexing + EXIF GPS backfill are deferred to a background task
    // (see below) so the grid can render from media_meta immediately. Tags,
    // autocomplete, and map coordinates light up once that task finishes and
    // emits `gallery:tags-indexed`.

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

    // Start watching for external file changes
    start_fs_watcher(app_handle.clone(), &state, &path);

    // Index companion tags + backfill GPS in the background so the grid renders
    // immediately. The DB is now stored in `state`, so the task locks it, does
    // the heavy work, refreshes autocomplete, and notifies the frontend.
    {
        let cache_db = Arc::clone(&state.cache_db);
        let autocomplete = Arc::clone(&state.autocomplete);
        let gallery = path.clone();
        tauri::async_runtime::spawn(async move {
            let counts = {
                let db_guard = cache_db.lock().await;
                let db = match db_guard.as_ref() {
                    Some(db) => db,
                    None => return,
                };

                // Best-effort EXIF GPS backfill — only touches rows where
                // gps_lat IS NULL, so later opens skip already-extracted files.
                if let Err(e) = backfill_gps_meta(db) {
                    log::warn!("GPS backfill failed: {}", e);
                }

                // Re-index companions so tag_index/tag_counts reflect disk.
                index_companions(db, &gallery);

                // Consolidate the WAL written by the bulk indexing above.
                if let Err(e) = db.checkpoint() {
                    log::warn!("WAL checkpoint after indexing failed: {}", e);
                }

                db.query_all_tag_counts().ok()
            };

            if let Some(counts) = counts {
                if !counts.is_empty() {
                    log::info!("Loaded {} unique tags into autocomplete", counts.len());
                    autocomplete.refresh(counts).await;
                }
            }

            // Tell the frontend tags are ready so it can refresh the view.
            let _ = app_handle.emit("gallery:tags-indexed", ());
        });
    }

    Ok(GalleryOpenResult {
        path,
        total_media: media_count,
        provider_type: ProviderType::Local,
    })
}

/// Close the current gallery and release resources.
#[tauri::command]
pub async fn close_gallery(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Stop filesystem watcher first
    stop_fs_watcher(&state);

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

    // Close protocol handler DB FIRST — the read-only connection blocks
    // WAL checkpointing, so it must be dropped before we checkpoint.
    {
        let mut proto_db = state.thumb_protocol_db.lock().unwrap();
        *proto_db = None;
    }

    // Checkpoint and close cache DB
    {
        let mut db = state.cache_db.lock().await;
        if let Some(ref cache_db) = *db {
            if let Err(e) = cache_db.checkpoint() {
                log::warn!("WAL checkpoint on close failed: {}", e);
            }
        }
        *db = None;
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
    get_gallery_info_impl(&state).await
}

pub async fn get_gallery_info_impl(
    state: &AppState,
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
