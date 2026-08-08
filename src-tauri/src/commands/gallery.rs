//! Opening, closing, and watching a gallery.
//!
//! `open_gallery_impl` is the one command with real orchestration, and its
//! ordering is load-bearing at three points:
//!
//! 1. The root is canonicalized once here, so every later path-confinement
//!    check compares against a stored value instead of canonicalizing per
//!    request.
//! 2. `adopt_gallery_root` runs **before** `populate_media_meta`. If the gallery
//!    directory moved, the cached absolute paths must be rebased first;
//!    otherwise the scan inserts bare rows under the new root that shadow the
//!    relocated history, and the accumulated ratings, tags, and thumbnails are
//!    orphaned in place.
//! 3. The read-only connection pool opens after the rebase, so it never
//!    observes mid-rebase state.
//!
//! Indexing companions, rebuilding tag counts, and starting the idle worker all
//! happen *after* the command returns — the grid renders from `media_meta`
//! alone, so there is no reason to make the user wait for them.
//!
//! The fs-watcher lives here too. `util::fs_watch` is only a transport; the
//! policy — coalescing an event storm into one refresh, distinguishing the
//! app's own `settings.toml` write from a hand edit, routing batches to both
//! the desktop event and the web client's SSE broadcast — is all in
//! `start_fs_watcher`.

use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tauri::Emitter;

use crate::cache::db::CacheDb;
use crate::companion::schema::MediaType;
use crate::pipeline::exif;
use crate::provider::local::LocalProvider;
use crate::provider::FileEntry;
use crate::util::fs_watch::FsWatcher;
use crate::AppState;
use rayon::prelude::*;

#[derive(Debug, Serialize)]
pub struct GalleryOpenResult {
    pub path: String,
    pub total_media: usize,
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
        let on_disk: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();

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
            // Every path-keyed table, not just media_meta/thumbnails/tag_index:
            // the other LOD tiers (and the GIF atlas) used to be left behind, so
            // a gallery that churned files accumulated multi-megabyte blobs for
            // paths that no longer existed and were never reachable again.
            for table in crate::cache::db::path_keyed_tables() {
                let mut del = tx
                    .prepare_cached(&format!("DELETE FROM {table} WHERE path = ?1"))
                    .map_err(|e| e.to_string())?;
                for path in &stale {
                    let _ = del.execute(rusqlite::params![path]);
                }
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
/// written by plugins or carried over from previous sessions. Also mirrors
/// each companion's rating into `media_meta` so ratings survive a gallery
/// move/copy where the cached absolute paths no longer match.
fn index_companions(db: &CacheDb, gallery_path: &str) {
    let ext = crate::companion::schema::COMPANION_EXTENSION;

    // Load the whole index_state table once so the per-companion freshness
    // check is an in-memory lookup rather than a SQL round-trip per file.
    let index_state = db.load_index_state().unwrap_or_default();

    // Gather companion files in a single walk. Companions live either
    // alongside media or in a per-directory `.lightview/companions/` tree —
    // every folder with media has its own — so the walk must descend into
    // each `.lightview` dir, but only its `companions/` subtree: the cache
    // db, atlas, and thumbnails that live next to it stay pruned.
    let mut companions: Vec<walkdir::DirEntry> = Vec::new();
    for entry in walkdir::WalkDir::new(gallery_path)
        .into_iter()
        .filter_entry(|e| {
            let inside_lightview = e
                .path()
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == ".lightview");
            !inside_lightview || e.file_name() == "companions"
        })
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
        let mut rating_stmt = match tx.prepare_cached(
            "UPDATE media_meta SET rating = ?2, last_rated = ?3 WHERE path = ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Failed to prepare rating statement: {}", e);
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
                    // Mirror the companion's rating into media_meta. The row is
                    // keyed by absolute path, so after a gallery move/copy it
                    // starts NULL even though the companion holds a rating.
                    let core = companion.meta.core.as_ref();
                    let rating = core.and_then(|c| c.rating);
                    let last_rated = core
                        .and_then(|c| c.date_rated.as_deref())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.timestamp());
                    let _ = rating_stmt
                        .execute(rusqlite::params![media_path_str, rating, last_rated]);
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

/// `gallery_meta` key recording the absolute root the cached paths are
/// keyed under, so a moved gallery can be detected and rebased on open.
const GALLERY_ROOT_KEY: &str = "gallery_root";

/// Detect a moved gallery directory and rebase the cache's absolute paths so
/// thumbnails, ratings, view history, and dedup state survive the move.
/// Runs before `populate_media_meta`, which would otherwise insert bare rows
/// under the new root and shadow the relocated history.
fn adopt_gallery_root(db: &CacheDb, root: &str, entries: &[crate::provider::FileEntry]) {
    let root = root.trim_end_matches('/');
    let stored = db
        .get_gallery_meta(GALLERY_ROOT_KEY)
        .ok()
        .flatten()
        .map(|s| s.trim_end_matches('/').to_string());

    let old_root = match stored.as_deref() {
        Some(s) if s == root => return, // unchanged — the common case
        Some(s) => Some(s.to_string()),
        // Cache predates root tracking: infer the old root from the rows.
        None => infer_old_root(db, root, entries),
    };

    if let Some(old_root) = old_root {
        match db.rebase_root(&old_root, root) {
            Ok(0) => {}
            Ok(n) => log::info!(
                "Gallery moved: rebased {} cached rows from {} to {}",
                n,
                old_root,
                root
            ),
            Err(e) => log::warn!("Cache rebase {} -> {} failed: {}", old_root, root, e),
        }
    }
    let _ = db.set_gallery_meta(GALLERY_ROOT_KEY, root);
}

/// Infer the previous gallery root of a cache that predates root tracking:
/// match a few on-disk files against cached rows by gallery-relative suffix
/// and require every sample to agree on a single foreign prefix. Returns
/// `None` (skipping the rebase — safe no-op) when there is nothing foreign
/// in the cache or the evidence is ambiguous.
fn infer_old_root(
    db: &CacheDb,
    root: &str,
    entries: &[crate::provider::FileEntry],
) -> Option<String> {
    let foreign: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM media_meta
             WHERE substr(path, 1, length(?1) + 1) != ?1 || '/'",
            rusqlite::params![root],
            |r| r.get(0),
        )
        .ok()?;
    if foreign == 0 {
        return None;
    }

    let mut candidate: Option<String> = None;
    let mut agreed = 0;
    for entry in entries {
        // Keep the leading '/' so the prefix comes out without a trailing one.
        let Some(rel) = entry.path.strip_prefix(root) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        // SQLite length()/substr() count characters, not bytes.
        let rel_chars = rel.chars().count() as i64;
        let prefixes: Vec<String> = db
            .conn()
            .prepare_cached(
                "SELECT DISTINCT substr(path, 1, length(path) - ?2) FROM media_meta
                 WHERE length(path) > ?2
                   AND substr(path, length(path) - ?2 + 1) = ?1",
            )
            .ok()?
            .query_map(rusqlite::params![rel, rel_chars], |r| r.get::<_, String>(0))
            .ok()?
            .filter_map(|r| r.ok())
            .filter(|p| p != root && !p.is_empty())
            .collect();

        match prefixes.as_slice() {
            [] => continue, // no cached history for this file under any root
            [one] => match &candidate {
                Some(c) if c != one => {
                    log::info!(
                        "Gallery root inference ambiguous ({} vs {}); skipping rebase",
                        c,
                        one
                    );
                    return None;
                }
                _ => {
                    candidate = Some(one.clone());
                    agreed += 1;
                }
            },
            _ => {
                log::info!(
                    "Gallery root inference ambiguous for {}; skipping rebase",
                    rel
                );
                return None;
            }
        }
        if agreed >= 5 {
            break;
        }
    }
    if candidate.is_some() {
        log::info!(
            "Inferred previous gallery root {} from {} matching file(s)",
            candidate.as_deref().unwrap_or(""),
            agreed
        );
    }
    candidate
}

/// Payload emitted for `gallery:fs-changed` events.
#[derive(Debug, Clone, Serialize)]
pub struct FsChangeEvent {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Insert a bare `media_meta` row for a file that just appeared on disk
/// (fs-watch add, trash restore). `date_taken` falls back to the file mtime —
/// the EXIF/GPS backfill on the next gallery open refines it. Returns `false`
/// when the path has no recognized media extension or can't be stat'd (e.g.
/// it vanished again).
pub fn insert_media_meta_row(conn: &rusqlite::Connection, path_str: &str) -> bool {
    let p = std::path::Path::new(path_str);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let media_type = match MediaType::from_extension(ext) {
        Some(mt) => mt.as_str().to_string(),
        None => return false,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
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
        Err(_) => return false,
    };
    conn.execute(
        "INSERT OR IGNORE INTO media_meta (path, date_taken, file_size, media_type, date_added) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![path_str, mtime, size, media_type, now],
    )
    .is_ok()
}

/// Start the filesystem watcher background task.
/// Polls for notify events, debounces them, updates the DB, and notifies
/// clients: the desktop webview via Tauri events (when `app_handle` is `Some`)
/// and remote web clients via the `fs_change_tx` broadcast (always). The
/// headless server has no Tauri runtime, so it passes `None` and relies solely
/// on the broadcast.
pub fn start_fs_watcher(
    app_handle: Option<tauri::AppHandle>,
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
    let last_written_settings = Arc::clone(&state.last_written_settings);
    let fs_change_tx = state.fs_change_tx.clone();

    tauri::async_runtime::spawn(async move {
        use notify::EventKind;
        use std::sync::atomic::Ordering;

        const POLL_MS: u64 = 300;
        const DEBOUNCE_MS: u64 = 500;

        let sep = std::path::MAIN_SEPARATOR;
        let lightview_suffix = sep.to_string() + ".lightview";
        // Hand-editable settings file inside `.lightview` — watched for external
        // edits and hot-reloaded (see the settings-file branch below).
        let settings_suffix = format!("{sep}.lightview{sep}settings.toml");
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

                    // Settings file lives inside .lightview but, unlike the rest
                    // of it, should hot-reload on external (hand) edits. Handle
                    // it before the blanket .lightview skip below.
                    if path_str.ends_with(&settings_suffix) {
                        if matches!(
                            event.kind,
                            EventKind::Create(_) | EventKind::Modify(_)
                        ) {
                            if let Ok(toml_str) = std::fs::read_to_string(path) {
                                let is_self_write = last_written_settings
                                    .lock()
                                    .unwrap()
                                    .as_deref()
                                    == Some(toml_str.as_str());
                                if !is_self_write {
                                    if let Ok(json) =
                                        crate::commands::settings::toml_to_json(&toml_str)
                                    {
                                        // Remember it so duplicate fs events for
                                        // this same edit don't re-emit.
                                        *last_written_settings.lock().unwrap() =
                                            Some(toml_str);
                                        if let Some(h) = &app_handle {
                                            let _ = h.emit("settings:changed", json);
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

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
                for path_str in &added {
                    insert_media_meta_row(conn, path_str);
                }

                // Remove deleted files
                if !removed.is_empty() {
                    for path_str in &removed {
                        db.remove_media_rows(path_str);
                    }
                    let _ = db.rebuild_tag_counts();
                }
            }

            // Notify the desktop webview (Tauri event) and any connected remote
            // web clients (SSE relay subscribes to `fs_change_tx`). `send`
            // errors only when there are no subscribers — expected and ignored.
            let change = FsChangeEvent { added, removed };
            if let Some(h) = &app_handle {
                let _ = h.emit("gallery:fs-changed", change.clone());
            }
            let _ = fs_change_tx.send(change);
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
/// Open a gallery without any Tauri coupling, so the headless server can reuse
/// it. Registers the provider, opens/scans the cache, populates media_meta,
/// wires up the thumbnail protocol DB and BC7 atlas, and spawns background
/// companion/GPS indexing. `on_tags_indexed` fires once that background task
/// finishes — the desktop app emits `gallery:tags-indexed`; headless passes a
/// no-op. The filesystem watcher is intentionally *not* started here: it pushes
/// changes to the webview over Tauri events, which a headless server has no
/// channel for. Callers that want it (the desktop command) start it themselves.
pub async fn open_gallery_impl(
    state: &AppState,
    path: String,
    on_tags_indexed: impl FnOnce() + Send + 'static,
) -> Result<GalleryOpenResult, String> {
    let provider = Arc::new(LocalProvider::new(&path));
    *state.provider.write().await = Some(provider.clone());

    // Open (or create) the cache database
    let cache_db = CacheDb::open(std::path::Path::new(&path))
        .map_err(|e| format!("Failed to open cache: {}", e))?;

    // Recursively scan for all media files in the directory tree
    let entries: Vec<crate::provider::FileEntry> = provider
        .list_dir_recursive(&path)
        .map_err(|e| format!("Failed to scan directory: {}", e))?;

    let media_count = entries.len();

    // If the gallery directory moved since the cache was built (new mount
    // point, renamed folder, copied to another machine), rewrite the cached
    // absolute paths before populate_media_meta inserts bare rows under the
    // new root. Must run before the read-only pool opens below so the pool
    // never sees mid-rebase state.
    adopt_gallery_root(&cache_db, &path, &entries);

    // Populate media_meta table so sorting works immediately
    populate_media_meta(&cache_db, &entries)?;

    // Companion indexing + EXIF GPS backfill are deferred to a background task
    // (see below) so the grid can render from media_meta immediately. Tags,
    // autocomplete, and map coordinates light up once that task finishes and
    // emits `gallery:tags-indexed`.

    // Open a pool of read-only connections for the thumbnail protocol handler /
    // HTTP route. SQLite WAL mode supports concurrent readers, so N connections
    // let the thumbnail hot path serve reads in parallel instead of one-at-a-
    // time through a single Mutex'd connection.
    {
        let db_path = std::path::Path::new(&path).join(".lightview").join("cache.db");
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 6);
        match crate::ThumbProtocolPool::open(&db_path, n) {
            Some(pool) => {
                let mut proto_db = state.thumb_protocol_db.write().unwrap();
                *proto_db = Some(Arc::new(pool));
                // New gallery → previous ThumbHash PNGs are no longer relevant.
                state.thumbhash_png_cache.clear();
            }
            None => {
                log::warn!("Failed to open read-only DB pool for protocol handler");
            }
        }
    }

    // Store the cache DB
    {
        let mut db = state.cache_db.lock().await;
        *db = Some(cache_db);
    }

    // Store current gallery path
    {
        let mut current = state.current_gallery.write().await;
        *current = Some(path.clone());
    }

    // Resolve the canonical root once — the HTTP routes use it for
    // path-confinement checks on every request.
    {
        let canonical = tokio::fs::canonicalize(&path).await.ok();
        if canonical.is_none() {
            log::warn!("Failed to canonicalize gallery root {path}");
        }
        let mut root = state.canonical_gallery_root.write().await;
        *root = canonical;
    }

    // Track as recently opened
    state.add_recent_gallery(&path).await;

    // Index companion tags + backfill GPS in the background so the grid renders
    // immediately. The DB is now stored in `state`, so the task locks it, does
    // the heavy work, refreshes autocomplete, and notifies the frontend.
    {
        let cache_db = Arc::clone(&state.cache_db);
        let autocomplete = Arc::clone(&state.autocomplete);
        let gallery = path.clone();
        let state_for_idle = state.clone();
        tauri::async_runtime::spawn(async move {
            let mut trash_retention_days = crate::commands::trash::DEFAULT_TRASH_RETENTION_DAYS;
            let counts = {
                let db_guard = cache_db.lock().await;
                let db = match db_guard.as_ref() {
                    Some(db) => db,
                    None => return,
                };

                trash_retention_days = db
                    .get_gallery_meta(crate::commands::trash::TRASH_RETENTION_KEY)
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(trash_retention_days);

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

            // Drop expired trash entries (filesystem-only, no DB lock needed).
            crate::commands::trash::auto_purge(&gallery, trash_retention_days);

            // Backfill missing thumbnails/phashes whenever the gallery goes
            // quiet. Started after indexing so the two don't fight over the DB.
            crate::pipeline::idle::start_idle_worker(&state_for_idle);

            // Tell the caller indexing finished (desktop refreshes the view).
            on_tags_indexed();
        });
    }

    Ok(GalleryOpenResult {
        path,
        total_media: media_count,
    })
}

/// Open a gallery (Tauri command). Thin wrapper over [`open_gallery_impl`] that
/// also starts the filesystem watcher and emits `gallery:tags-indexed` to the
/// webview once background indexing completes.
#[tauri::command]
pub async fn open_gallery(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<GalleryOpenResult, String> {
    let emit_handle = app_handle.clone();
    let result = open_gallery_impl(&state, path.clone(), move || {
        let _ = emit_handle.emit("gallery:tags-indexed", ());
    })
    .await?;

    // Start watching for external file changes. The desktop passes its
    // AppHandle so changes also surface as Tauri events for this webview.
    start_fs_watcher(Some(app_handle), &state, &path);

    Ok(result)
}

/// Close the current gallery and release resources.
#[tauri::command]
pub async fn close_gallery(state: tauri::State<'_, AppState>) -> Result<(), String> {
    close_gallery_impl(&state).await
}

/// Release everything tied to the open gallery, in an order that matters.
///
/// Also the process's shutdown path: the desktop calls it on exit and
/// `lightview-headless` calls it on SIGTERM/SIGINT, because nothing else
/// flushes the buffered tier-access marks (see below).
///
/// What an abrupt kill would and would not cost, since that decides what this
/// function has to do:
///
/// * **Companion files are already safe.** Writes go to a temp file in the
///   target directory and are renamed into place, so a reader never sees a
///   partial one. If we die between writing a companion and re-indexing it,
///   `index_state` still holds the old mtime and the next gallery open
///   re-indexes that file. Nothing to do here.
/// * **The WAL is already safe.** SQLite recovers it on the next open. The
///   checkpoint below is tidiness — a smaller `-wal` beside the database — not
///   correctness.
/// * **Buffered tier-access marks are not safe.** The thumbnail serve path
///   reads through a read-only pool and cannot write, so "this row was served"
///   accumulates in `AppState::pending_tier_accesses` and is only landed by
///   `enforce_tier_budget`, which runs when a capped tier is *written*. Exit
///   without flushing and the zoom thumbnails you were just looking at go back
///   to the eviction pass looking maximally cold. That is the one real loss,
///   and the reason this function flushes before it closes anything.
pub async fn close_gallery_impl(state: &AppState) -> Result<(), String> {
    // Stop filesystem watcher first
    stop_fs_watcher(state);

    // Retire the idle backfill worker (it exits at its next generation check).
    state
        .idle_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    // Land the buffered access marks while the writer is still open. Ordering:
    // this must happen before the connection is dropped, and it deliberately
    // does *not* evict — shutdown is the wrong moment to spend seconds in a
    // DELETE, and the next write will enforce the budget anyway.
    {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            for tier in crate::cache::thumbnails::ThumbTier::ALL {
                if !crate::cache::thumbnails::is_lru_capped(tier) {
                    continue;
                }
                let touched = state.take_tier_accesses(tier);
                if let Err(e) =
                    crate::cache::thumbnails::touch_accessed(db.conn(), tier, &touched)
                {
                    log::warn!("{} tier access flush on close failed: {e}", tier.as_segment());
                }
            }
        }
    }

    let gallery_path = {
        let current = state.current_gallery.read().await;
        current.clone()
    };

    if gallery_path.is_some() {
        *state.provider.write().await = None;
    }

    // Close protocol handler DB FIRST — the read-only connections block
    // WAL checkpointing, so they must be dropped before we checkpoint.
    {
        let mut proto_db = state.thumb_protocol_db.write().unwrap();
        *proto_db = None;
        state.thumbhash_png_cache.clear();
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
    {
        let mut root = state.canonical_gallery_root.write().await;
        *root = None;
    }

    Ok(())
}

pub async fn get_gallery_info_impl(
    state: &AppState,
) -> Result<Option<GalleryOpenResult>, String> {
    let current = state.current_gallery.read().await;
    let Some(path) = current.as_ref() else {
        return Ok(None);
    };
    if state.provider.read().await.is_none() {
        return Ok(None);
    }

    // Counted from media_meta rather than by re-scanning: the table was
    // populated from the recursive scan during open_gallery, and a re-scan here
    // would stat the whole tree on every info request.
    let db = state.cache_db.lock().await;
    let media_count = db
        .as_ref()
        .and_then(|db| {
            db.conn()
                .query_row("SELECT COUNT(*) FROM media_meta", [], |r| r.get::<_, usize>(0))
                .ok()
        })
        .unwrap_or(0);

    Ok(Some(GalleryOpenResult {
        path: path.clone(),
        total_media: media_count,
    }))
}

/// Everything the web client needs to render its first frame, in one
/// round-trip: gallery info, the gallery-wide default filter, and the sorted
/// item list (with the default filter already applied server-side). The old
/// boot was three serial invokes — on a phone that's three network RTTs
/// between opening the app and the grid having data.
#[derive(Debug, Serialize)]
pub struct BootState {
    pub gallery: Option<GalleryOpenResult>,
    /// The stored default filter, so the client can seed its FilterBar with
    /// the query that `sorted` already reflects.
    pub default_filter: Option<crate::commands::settings::DefaultFilter>,
    /// None when no gallery is open.
    pub sorted: Option<crate::commands::sort::SortedResult>,
}

pub async fn get_boot_state_impl(
    state: &AppState,
    sort_field: crate::sort::sorter::SortField,
    sort_order: crate::sort::sorter::SortOrder,
    group_by: crate::sort::grouper::GroupBy,
    sub_sort_field: Option<crate::sort::sorter::SortField>,
    sub_sort_order: Option<crate::sort::sorter::SortOrder>,
) -> Result<BootState, String> {
    let gallery = get_gallery_info_impl(state).await?;
    if gallery.is_none() {
        return Ok(BootState { gallery: None, default_filter: None, sorted: None });
    }

    // Absent gallery settings read as "no default filter", same as the
    // client-side fallback this replaces.
    let default_filter = crate::commands::settings::get_gallery_default_filter_impl(state)
        .await
        .unwrap_or(None);

    // A default filter that fails to parse/apply degrades to the unfiltered
    // list rather than failing the boot — mirrors applyDefaultFilter() in the
    // frontend, which catches and falls back to all items.
    let filter_paths = match &default_filter {
        Some(df) if df.enabled && !df.query.trim().is_empty() => {
            match crate::commands::filter::apply_filter_impl(state, df.query.trim().to_string())
                .await
            {
                Ok(paths) => Some(paths),
                Err(e) => {
                    log::warn!("Boot-state default filter failed, serving unfiltered: {e}");
                    None
                }
            }
        }
        _ => None,
    };

    let sorted = crate::commands::sort::get_sorted_items_impl(
        state,
        sort_field,
        sort_order,
        group_by,
        filter_paths,
        sub_sort_field,
        sub_sort_order,
    )
    .await?;

    Ok(BootState { gallery, default_filter, sorted: Some(sorted) })
}

/// Single-round-trip boot for the web client (also callable over Tauri IPC).
#[tauri::command]
pub async fn get_boot_state(
    state: tauri::State<'_, AppState>,
    sort_field: crate::sort::sorter::SortField,
    sort_order: crate::sort::sorter::SortOrder,
    group_by: crate::sort::grouper::GroupBy,
    sub_sort_field: Option<crate::sort::sorter::SortField>,
    sub_sort_order: Option<crate::sort::sorter::SortOrder>,
) -> Result<BootState, String> {
    get_boot_state_impl(
        &state,
        sort_field,
        sort_order,
        group_by,
        sub_sort_field,
        sub_sort_order,
    )
    .await
}
