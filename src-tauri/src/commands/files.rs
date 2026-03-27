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

        // Try rename first (fast, same filesystem), fall back to copy+delete
        let move_result = std::fs::rename(src, &dst).or_else(|_| {
            std::fs::copy(src, &dst).and_then(|_| std::fs::remove_file(src))
        });

        match move_result {
            Ok(_) => {
                // Move companion file too
                move_companion(src, dest);
                succeeded.push(src_path.clone());
            }
            Err(e) => {
                failed.push(FileOpError {
                    path: src_path.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    // Remove moved files from the cache DB
    if !succeeded.is_empty() {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let conn = db.conn();
            for path in &succeeded {
                let _ = conn.execute("DELETE FROM media_meta WHERE path = ?1", rusqlite::params![path]);
                let _ = conn.execute("DELETE FROM thumbnails WHERE path = ?1", rusqlite::params![path]);
                let _ = conn.execute("DELETE FROM tag_index WHERE path = ?1", rusqlite::params![path]);
            }
            let _ = db.rebuild_tag_counts();
        }
    }

    Ok(FileOpResult { succeeded, failed })
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

/// Move the companion file for a media file if it exists (alongside variant).
fn move_companion(media_src: &Path, dest_dir: &Path) {
    let companion_name = format!(
        "{}{}",
        media_src.file_name().unwrap_or_default().to_string_lossy(),
        COMPANION_EXTENSION
    );
    let companion_src = media_src.with_file_name(&companion_name);
    if companion_src.exists() {
        let companion_dst = dest_dir.join(&companion_name);
        let _ = std::fs::rename(&companion_src, &companion_dst).or_else(|_| {
            std::fs::copy(&companion_src, &companion_dst)
                .and_then(|_| std::fs::remove_file(&companion_src))
        });
    }
}
