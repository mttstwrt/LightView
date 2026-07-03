use serde::Serialize;

use crate::cache::duplicates::DuplicateGroup;
use crate::AppState;

/// Hashes computed per DB-lock acquisition during a scan. Small enough that a
/// web request queued behind a batch waits milliseconds, not the whole scan.
const PHASH_BATCH: usize = 64;

#[derive(Debug, Serialize)]
pub struct FindDuplicatesResult {
    /// Number of perceptual hashes newly computed in this run.
    pub hashes_computed: usize,
    /// Groups of duplicate images (each group has 2+ items with metadata).
    pub groups: Vec<DuplicateGroup>,
}

/// Scan all thumbnails for visually similar images using perceptual hashing.
///
/// `threshold` controls sensitivity: 0 = exact duplicates only, higher values
/// catch resized/recompressed variants (recommended: 5–10).
///
/// Hashing runs in batches, releasing the cache-DB lock between them, so a
/// first scan over a large gallery doesn't block other commands (the idle
/// worker usually pre-hashes everything anyway).
pub async fn find_duplicates_impl(
    state: &AppState,
    threshold: Option<u32>,
) -> Result<FindDuplicatesResult, String> {
    let threshold = threshold.unwrap_or(8);

    let mut hashes_computed = 0;
    loop {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let computed = db.compute_phashes_batch(PHASH_BATCH).map_err(|e| e.to_string())?;
        hashes_computed += computed;
        if computed < PHASH_BATCH {
            break;
        }
    }

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    let groups = db.find_duplicates(threshold).map_err(|e| e.to_string())?;

    Ok(FindDuplicatesResult {
        hashes_computed,
        groups,
    })
}

#[tauri::command]
pub async fn find_duplicates(
    state: tauri::State<'_, AppState>,
    threshold: Option<u32>,
) -> Result<FindDuplicatesResult, String> {
    find_duplicates_impl(&state, threshold).await
}

/// Mark a set of paths as confirmed non-duplicates. Every pair within the
/// set is recorded so future `find_duplicates` runs skip the union for those
/// pairs and the group stays dismissed.
pub async fn mark_not_duplicates_impl(
    state: &AppState,
    paths: Vec<String>,
) -> Result<usize, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.mark_not_duplicates(&paths).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mark_not_duplicates(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<usize, String> {
    mark_not_duplicates_impl(&state, paths).await
}
