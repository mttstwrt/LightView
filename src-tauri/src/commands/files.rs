//! Host file operations: copy and move, with companions carried along.
//!
//! Desktop-only — absent from the `/api/invoke` allowlist, because these are
//! the commands that can write anywhere on the host's filesystem.
//!
//! A move reports its results in two buckets, and the split is what the grid
//! needs rather than a stylistic choice: files whose destination stays inside
//! the gallery come back as old→new pairs so the frontend can re-key the
//! existing cells and keep their loaded images, while files that left the
//! gallery come back as removals.

use std::path::Path;

use serde::Serialize;

use crate::companion::schema::COMPANION_EXTENSION;
use crate::AppState;

#[derive(Debug, Serialize)]
pub struct FileOpResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<FileOpError>,
}

#[derive(Debug, Serialize)]
pub struct FileOpError {
    pub path: String,
    pub error: String,
}

/// Result of a move operation. Files whose destination stays inside the current
/// gallery are reported in `moved` (old path -> new path) so the frontend can
/// re-key the existing grid cells; files that left the gallery are reported in
/// `removed` so the frontend drops them from the view.
#[derive(Debug, Serialize)]
pub struct MoveResult {
    pub moved: Vec<MovedFile>,
    pub removed: Vec<String>,
    pub failed: Vec<FileOpError>,
}

#[derive(Debug, Serialize)]
pub struct MovedFile {
    pub from: String,
    pub to: String,
}

/// Copy media files (and their companion files) to a destination directory.
#[tauri::command]
pub async fn copy_files(
    _state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    destination: String,
) -> Result<FileOpResult, String> {
    let dest = Path::new(&destination);
    if !dest.is_dir() {
        return Err(format!("Destination is not a directory: {}", destination));
    }

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for src_path in &paths {
        let src = Path::new(src_path);
        let file_name = match src.file_name() {
            Some(n) => n,
            None => {
                failed.push(FileOpError {
                    path: src_path.clone(),
                    error: "Invalid file path".to_string(),
                });
                continue;
            }
        };

        let dst = dest.join(file_name);
        match std::fs::copy(src, &dst) {
            Ok(_) => {
                // Also copy companion file if it exists
                copy_companion(src);
                succeeded.push(dst.to_string_lossy().to_string());
            }
            Err(e) => {
                failed.push(FileOpError {
                    path: src_path.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    Ok(FileOpResult { succeeded, failed })
}

/// Move media files (and their companion files) to a destination directory.
/// Removes moved files from the cache DB so they disappear from the gallery.
#[tauri::command]
pub async fn move_files(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    destination: String,
) -> Result<MoveResult, String> {
    let dest = Path::new(&destination);
    if !dest.is_dir() {
        return Err(format!("Destination is not a directory: {}", destination));
    }

    // If the destination lives inside the current gallery, the files stay in the
    // gallery — re-key their cached rows to the new path so tags, ratings, and
    // the already-generated thumbnails are preserved, and tell the frontend to
    // re-key the cells. Otherwise they've left the gallery, so drop the rows and
    // tell the frontend to remove the cells.
    let dest_in_gallery = state
        .current_gallery
        .read()
        .await
        .as_deref()
        .map(|root| dest.starts_with(root))
        .unwrap_or(false);

    // Successful moves as (old path, new path) pairs.
    let mut succeeded: Vec<(String, String)> = Vec::new();
    let mut failed = Vec::new();

    for src_path in &paths {
        let src = Path::new(src_path);
        let file_name = match src.file_name() {
            Some(n) => n,
            None => {
                failed.push(FileOpError {
                    path: src_path.clone(),
                    error: "Invalid file path".to_string(),
                });
                continue;
            }
        };

        let dst = dest.join(file_name);

        // Try rename first (fast, same filesystem), fall back to copy+delete
        let move_result = std::fs::rename(src, &dst).or_else(|_| {
            std::fs::copy(src, &dst).and_then(|_| std::fs::remove_file(src))
        });

        match move_result {
            Ok(_) => {
                // Move companion file (tags/ratings) alongside the media file.
                move_companion(src, &dst);
                succeeded.push((src_path.clone(), dst.to_string_lossy().to_string()));
            }
            Err(e) => {
                failed.push(FileOpError {
                    path: src_path.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    // Update the cache DB for the moved files.
    if !succeeded.is_empty() {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            // `dest_in_gallery` is a property of the destination, not of any
            // one file, so the choice is made once for the batch. Inside the
            // loop it also re-decided — and re-issued the SQL — per file.
            if dest_in_gallery {
                // Every path-keyed table follows the file, thumbnail tiers
                // included — see `path_keyed_tables`.
                db.rename_media_rows_batch(&succeeded);
            } else {
                let gone: Vec<&str> = succeeded.iter().map(|(old, _)| old.as_str()).collect();
                db.remove_media_rows_batch(&gone);
            }
            let _ = db.rebuild_tag_counts();
        }
    }

    let (moved, removed) = if dest_in_gallery {
        let moved = succeeded
            .into_iter()
            .map(|(from, to)| MovedFile { from, to })
            .collect();
        (moved, Vec::new())
    } else {
        let removed = succeeded.into_iter().map(|(from, _)| from).collect();
        (Vec::new(), removed)
    };

    Ok(MoveResult {
        moved,
        removed,
        failed,
    })
}

/// Copy the companion file for a media file if it exists (alongside variant).
fn copy_companion(media_src: &Path) {
    let companion_name = format!(
        "{}{}",
        media_src.file_name().unwrap_or_default().to_string_lossy(),
        COMPANION_EXTENSION
    );
    let companion_src = media_src.with_file_name(&companion_name);
    if companion_src.exists() {
        if let Some(parent) = media_src.parent() {
            let _ = std::fs::copy(
                &companion_src,
                parent.join(&companion_name),
            );
        }
    }
}

/// Move the companion file (tags, ratings, etc.) for a media file from its old
/// path to its new one. Handles both the default `.lightview/companions/`
/// layout and the alongside variant (`photo.jpg.lightview.json`).
fn move_companion(media_src: &Path, media_dst: &Path) {
    use crate::companion::reader::{companion_path, CompanionLocation};

    for location in [CompanionLocation::LightviewFolder, CompanionLocation::Alongside] {
        let companion_src = companion_path(media_src, location);
        if !companion_src.exists() {
            continue;
        }
        let companion_dst = companion_path(media_dst, location);
        if let Some(parent) = companion_dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::rename(&companion_src, &companion_dst).or_else(|_| {
            std::fs::copy(&companion_src, &companion_dst)
                .and_then(|_| std::fs::remove_file(&companion_src))
        });
    }
}

/// Place the given media file paths on the OS clipboard so a paste in the
/// system file manager copies them. Thin wrapper over [`crate::file_clipboard`].
#[tauri::command]
pub async fn copy_files_to_clipboard(paths: Vec<String>) -> Result<(), String> {
    let bufs: Vec<std::path::PathBuf> = paths.into_iter().map(std::path::PathBuf::from).collect();
    tokio::task::spawn_blocking(move || {
        let refs: Vec<&Path> = bufs.iter().map(|p| p.as_path()).collect();
        crate::file_clipboard::write_files(&refs, crate::file_clipboard::Op::Copy)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
