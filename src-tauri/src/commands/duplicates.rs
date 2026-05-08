use serde::Serialize;

use crate::cache::duplicates::DuplicateGroup;
use crate::AppState;

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
#[tauri::command]
pub async fn find_duplicates(
    state: tauri::State<'_, AppState>,
    threshold: Option<u32>,
) -> Result<FindDuplicatesResult, String> {
    let threshold = threshold.unwrap_or(8);

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    // Compute hashes for any thumbnails that don't have one yet
    let hashes_computed = db.compute_phashes().map_err(|e| e.to_string())?;

    // Find groups within the hamming distance threshold
    let groups = db.find_duplicates(threshold).map_err(|e| e.to_string())?;

    Ok(FindDuplicatesResult {
        hashes_computed,
        groups,
    })
}

/// Mark a set of paths as confirmed non-duplicates. Every pair within the
/// set is recorded so future `find_duplicates` runs skip the union for those
/// pairs and the group stays dismissed.
#[tauri::command]
pub async fn mark_not_duplicates(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<usize, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.mark_not_duplicates(&paths).map_err(|e| e.to_string())
}
