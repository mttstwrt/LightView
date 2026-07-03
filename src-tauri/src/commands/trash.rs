//! App-managed trash.
//!
//! Deleting media moves it into `<gallery>/.lightview/trash/` instead of the
//! OS trash, so deletes are restorable from any client — including the LAN web
//! UI, where the OS trash of the *server* would be unreachable — and entries
//! auto-purge after a retention period (purge runs on gallery open).
//!
//! Each trash entry is a self-describing directory; nothing about it lives in
//! the cache DB, so entries survive a cache rebuild and are portable across
//! machines that mount the gallery at different absolute paths:
//!
//! ```text
//! .lightview/trash/<epoch_ms>_<seq>/
//!     <original filename>                 the media file
//!     companion.lightview_folder.json     companion, per original location
//!     companion.alongside.json            (either, both, or neither)
//!     meta.json                           original path (gallery-relative), timestamps
//! ```
//!
//! The trash dir sits under `.lightview`, which every walk already skips
//! (media scan prunes dot-dirs, companion indexing prunes `.lightview`,
//! fs-watch skips paths containing `/.lightview`), so trashed files never
//! reappear in the index.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::commands::files::{FileOpError, FileOpResult};
use crate::commands::gallery::{insert_media_meta_row, FsChangeEvent};
use crate::companion::reader::{companion_path, read_companion, CompanionLocation};
use crate::AppState;

/// `gallery_meta` key for the retention period. `0` disables auto-purge.
pub const TRASH_RETENTION_KEY: &str = "trash.retention_days";
pub const DEFAULT_TRASH_RETENTION_DAYS: u32 = 30;

const META_FILE: &str = "meta.json";
/// Companion file names inside an entry, one per original storage location.
const COMPANION_ENTRIES: [(&str, CompanionLocation); 2] = [
    ("companion.lightview_folder.json", CompanionLocation::LightviewFolder),
    ("companion.alongside.json", CompanionLocation::Alongside),
];

/// Sidecar describing one trash entry (`meta.json`).
#[derive(Debug, Serialize, Deserialize)]
struct TrashMeta {
    /// Gallery-relative path with `/` separators, portable across mounts.
    original_path: String,
    file_name: String,
    /// Unix seconds.
    deleted_at: i64,
}

/// One entry as listed to the frontend.
#[derive(Debug, Serialize)]
pub struct TrashEntry {
    pub id: String,
    pub original_path: String,
    pub file_name: String,
    pub deleted_at: i64,
    pub size: u64,
}

pub fn trash_root(gallery: &str) -> PathBuf {
    Path::new(gallery).join(".lightview").join("trash")
}

/// Entry ids are `<epoch_ms>_<seq>` — digits and underscores only. Anything
/// else (separators, `..`) is rejected so a remote client can't escape the
/// trash dir.
fn valid_entry_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '_')
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Rename, falling back to copy+delete for cross-filesystem moves.
fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::rename(src, dst).or_else(|_| {
        std::fs::copy(src, dst).and_then(|_| std::fs::remove_file(src))
    })
}

/// Turn a stored `/`-separated relative path back into an absolute path under
/// the current gallery root.
fn resolve_original(gallery: &str, original_path: &str) -> PathBuf {
    let mut p = PathBuf::from(gallery);
    for part in original_path.split('/') {
        p.push(part);
    }
    p
}

/// Create a fresh entry dir, bumping the sequence suffix on same-millisecond
/// collisions.
fn create_entry_dir(root: &Path) -> std::io::Result<PathBuf> {
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut seq = 0u32;
    loop {
        let candidate = root.join(format!("{epoch_ms}_{seq}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => seq += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Move one media file (and its companions) into a new trash entry.
fn trash_one(gallery: &str, root: &Path, src_path: &str) -> Result<(), String> {
    let src = Path::new(src_path);
    let rel = src
        .strip_prefix(gallery)
        .map_err(|_| "File is outside the current gallery".to_string())?;
    let file_name = src
        .file_name()
        .ok_or_else(|| "Invalid file path".to_string())?
        .to_string_lossy()
        .to_string();

    let entry_dir = create_entry_dir(root).map_err(|e| e.to_string())?;

    if let Err(e) = move_file(src, &entry_dir.join(&file_name)) {
        let _ = std::fs::remove_dir_all(&entry_dir);
        return Err(e.to_string());
    }

    for (entry_name, location) in COMPANION_ENTRIES {
        let companion_src = companion_path(src, location);
        if companion_src.exists() {
            let _ = move_file(&companion_src, &entry_dir.join(entry_name));
        }
    }

    let meta = TrashMeta {
        original_path: rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        file_name,
        deleted_at: now_secs(),
    };
    let meta_json = serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?;
    std::fs::write(entry_dir.join(META_FILE), meta_json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Move media files (and their companion files) into the gallery trash.
/// Removes trashed files from the cache DB so they disappear from the gallery.
pub async fn trash_files_impl(
    state: &AppState,
    paths: Vec<String>,
) -> Result<FileOpResult, String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    let root = trash_root(&gallery);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for src_path in paths {
        match trash_one(&gallery, &root, &src_path) {
            Ok(()) => succeeded.push(src_path),
            Err(error) => failed.push(FileOpError { path: src_path, error }),
        }
    }

    if !succeeded.is_empty() {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            for path in &succeeded {
                db.remove_media_rows(path);
            }
            let _ = db.rebuild_tag_counts();
        }
    }

    Ok(FileOpResult { succeeded, failed })
}

#[tauri::command]
pub async fn trash_files(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<FileOpResult, String> {
    trash_files_impl(&state, paths).await
}

/// List trash entries, newest first. Malformed entries are skipped.
pub async fn list_trash_impl(state: &AppState) -> Result<Vec<TrashEntry>, String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    let root = trash_root(&gallery);
    let mut entries = Vec::new();

    let dir = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return Ok(entries), // no trash dir yet
    };

    for dirent in dir.flatten() {
        let entry_dir = dirent.path();
        if !entry_dir.is_dir() {
            continue;
        }
        let id = dirent.file_name().to_string_lossy().to_string();
        let meta: TrashMeta = match std::fs::read_to_string(entry_dir.join(META_FILE))
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Skipping malformed trash entry {id}: {e}");
                continue;
            }
        };
        let size = std::fs::metadata(entry_dir.join(&meta.file_name))
            .map(|m| m.len())
            .unwrap_or(0);
        entries.push(TrashEntry {
            id,
            original_path: meta.original_path,
            file_name: meta.file_name,
            deleted_at: meta.deleted_at,
            size,
        });
    }

    entries.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    Ok(entries)
}

#[tauri::command]
pub async fn list_trash(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TrashEntry>, String> {
    list_trash_impl(&state).await
}

/// Move one entry back to its original path. Fails (without overwriting) when
/// the target already exists.
fn restore_one(gallery: &str, root: &Path, id: &str) -> Result<String, String> {
    if !valid_entry_id(id) {
        return Err("Invalid trash entry id".to_string());
    }
    let entry_dir = root.join(id);
    let meta: TrashMeta = std::fs::read_to_string(entry_dir.join(META_FILE))
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))?;

    let dest = resolve_original(gallery, &meta.original_path);
    if dest.exists() {
        return Err(format!(
            "A file already exists at {}",
            dest.to_string_lossy()
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    move_file(&entry_dir.join(&meta.file_name), &dest).map_err(|e| e.to_string())?;

    for (entry_name, location) in COMPANION_ENTRIES {
        let companion_src = entry_dir.join(entry_name);
        if companion_src.exists() {
            let companion_dst = companion_path(&dest, location);
            if let Some(parent) = companion_dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = move_file(&companion_src, &companion_dst);
        }
    }

    let _ = std::fs::remove_dir_all(&entry_dir);
    Ok(dest.to_string_lossy().to_string())
}

/// Restore trash entries to their original paths. Successful restores are
/// re-inserted into the cache DB and broadcast as an fs-change so every
/// connected client (desktop grid, web SSE) refreshes. The fs-watcher may see
/// the rename too — its insert is `OR IGNORE`, so the overlap is harmless.
pub async fn restore_trash_impl(
    state: &AppState,
    ids: Vec<String>,
) -> Result<FileOpResult, String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    let root = trash_root(&gallery);

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for id in ids {
        match restore_one(&gallery, &root, &id) {
            Ok(path) => succeeded.push(path),
            Err(error) => failed.push(FileOpError { path: id, error }),
        }
    }

    if !succeeded.is_empty() {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            for path in &succeeded {
                insert_media_meta_row(db.conn(), path);
                if let Ok(companion) = read_companion(Path::new(path)) {
                    let _ = db.reindex_tags_for_file(path, &companion);
                }
            }
            let _ = db.rebuild_tag_counts();
        }
        let _ = state.fs_change_tx.send(FsChangeEvent {
            added: succeeded.clone(),
            removed: Vec::new(),
        });
    }

    Ok(FileOpResult { succeeded, failed })
}

#[tauri::command]
pub async fn restore_trash(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    ids: Vec<String>,
) -> Result<FileOpResult, String> {
    let result = restore_trash_impl(&state, ids).await?;
    if !result.succeeded.is_empty() {
        let _ = app_handle.emit(
            "gallery:fs-changed",
            FsChangeEvent {
                added: result.succeeded.clone(),
                removed: Vec::new(),
            },
        );
    }
    Ok(result)
}

/// Permanently delete trash entries and return how many were removed.
/// `ids = None` selects every entry; `older_than_days` additionally filters by
/// deletion age. `(None, None)` empties the trash.
pub async fn purge_trash_impl(
    state: &AppState,
    ids: Option<Vec<String>>,
    older_than_days: Option<u32>,
) -> Result<usize, String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    if let Some(ids) = &ids {
        if ids.iter().any(|id| !valid_entry_id(id)) {
            return Err("Invalid trash entry id".to_string());
        }
    }
    Ok(purge_entries(&gallery, ids.as_deref(), older_than_days))
}

#[tauri::command]
pub async fn purge_trash(
    state: tauri::State<'_, AppState>,
    ids: Option<Vec<String>>,
    older_than_days: Option<u32>,
) -> Result<usize, String> {
    purge_trash_impl(&state, ids, older_than_days).await
}

/// Filesystem-level purge shared by the command and the on-open auto-purge.
fn purge_entries(gallery: &str, ids: Option<&[String]>, older_than_days: Option<u32>) -> usize {
    let root = trash_root(gallery);
    let cutoff = older_than_days.map(|days| now_secs() - i64::from(days) * 86_400);
    let mut purged = 0;

    let dir = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return 0,
    };

    for dirent in dir.flatten() {
        let entry_dir = dirent.path();
        if !entry_dir.is_dir() {
            continue;
        }
        let id = dirent.file_name().to_string_lossy().to_string();
        if let Some(ids) = ids {
            if !ids.iter().any(|i| *i == id) {
                continue;
            }
        }
        if let Some(cutoff) = cutoff {
            let deleted_at = std::fs::read_to_string(entry_dir.join(META_FILE))
                .ok()
                .and_then(|s| serde_json::from_str::<TrashMeta>(&s).ok())
                .map(|m| m.deleted_at);
            // Entries without readable metadata are only purged explicitly.
            match deleted_at {
                Some(t) if t < cutoff => {}
                _ => continue,
            }
        }
        if std::fs::remove_dir_all(&entry_dir).is_ok() {
            purged += 1;
        }
    }

    purged
}

/// Purge entries older than the gallery's retention period. Called from the
/// post-open background task; `retention_days == 0` disables auto-purge.
pub fn auto_purge(gallery: &str, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    let purged = purge_entries(gallery, None, Some(retention_days));
    if purged > 0 {
        log::info!("Trash auto-purge removed {purged} entries older than {retention_days} days");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_media(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"jpegbytes").unwrap();
        p
    }

    #[test]
    fn trash_one_creates_self_describing_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let gallery = tmp.path().to_string_lossy().to_string();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let media = write_media(&tmp.path().join("sub"), "photo.jpg");

        // Companion in the default per-directory layout.
        let companion = companion_path(&media, CompanionLocation::LightviewFolder);
        std::fs::create_dir_all(companion.parent().unwrap()).unwrap();
        std::fs::write(&companion, b"{}").unwrap();

        let root = trash_root(&gallery);
        std::fs::create_dir_all(&root).unwrap();
        trash_one(&gallery, &root, &media.to_string_lossy()).unwrap();

        assert!(!media.exists());
        assert!(!companion.exists());

        let entry = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap();
        let meta: TrashMeta = serde_json::from_str(
            &std::fs::read_to_string(entry.path().join(META_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(meta.original_path, "sub/photo.jpg");
        assert_eq!(meta.file_name, "photo.jpg");
        assert!(entry.path().join("photo.jpg").exists());
        assert!(entry.path().join("companion.lightview_folder.json").exists());
    }

    #[test]
    fn restore_round_trips_and_refuses_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let gallery = tmp.path().to_string_lossy().to_string();
        let media = write_media(tmp.path(), "photo.jpg");

        let root = trash_root(&gallery);
        std::fs::create_dir_all(&root).unwrap();
        trash_one(&gallery, &root, &media.to_string_lossy()).unwrap();
        let id = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .to_string();

        let restored = restore_one(&gallery, &root, &id).unwrap();
        assert_eq!(restored, media.to_string_lossy());
        assert!(media.exists());
        assert!(!root.join(&id).exists());

        // Trash again, drop a conflicting file at the original path, restore fails.
        trash_one(&gallery, &root, &media.to_string_lossy()).unwrap();
        let id2 = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .to_string();
        write_media(tmp.path(), "photo.jpg");
        let err = restore_one(&gallery, &root, &id2).unwrap_err();
        assert!(err.contains("already exists"));
        assert!(root.join(&id2).join("photo.jpg").exists());
    }

    #[test]
    fn purge_respects_age_cutoff_and_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let gallery = tmp.path().to_string_lossy().to_string();
        let root = trash_root(&gallery);
        std::fs::create_dir_all(&root).unwrap();

        for (id, age_days) in [("100_0", 40i64), ("200_0", 5)] {
            let dir = root.join(id);
            std::fs::create_dir(&dir).unwrap();
            let meta = TrashMeta {
                original_path: format!("{id}.jpg"),
                file_name: format!("{id}.jpg"),
                deleted_at: now_secs() - age_days * 86_400,
            };
            std::fs::write(
                dir.join(META_FILE),
                serde_json::to_string(&meta).unwrap(),
            )
            .unwrap();
        }

        // Age cutoff: only the 40-day-old entry goes.
        assert_eq!(purge_entries(&gallery, None, Some(30)), 1);
        assert!(!root.join("100_0").exists());
        assert!(root.join("200_0").exists());

        // Explicit id purge ignores age.
        assert_eq!(purge_entries(&gallery, Some(&["200_0".to_string()]), None), 1);
        assert!(!root.join("200_0").exists());
    }

    #[test]
    fn entry_ids_are_validated() {
        assert!(valid_entry_id("1730000000000_0"));
        assert!(!valid_entry_id("../escape"));
        assert!(!valid_entry_id(""));
        assert!(!valid_entry_id("abc/def"));
    }
}
