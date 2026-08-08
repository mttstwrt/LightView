//! View history.
//!
//! This module used to also hold `get_transformed_media` — a viewer-side
//! rotate/exposure/colour render with a GPU path and a CPU fallback. No UI ever
//! called it, so it went along with `gpu_pipeline::transform_image` and its
//! shader. If viewer adjustments come back, they belong here, and the deleted
//! version is in the history.

use crate::AppState;

/// Record that a media item was viewed (updates last_viewed timestamp).
#[tauri::command]
pub async fn record_view(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    record_view_impl(&state, path).await
}

/// Shared by the Tauri command and the web client's `/api/invoke` bridge —
/// a phone browsing the gallery remotely feeds the same "Recently Viewed"
/// history the desktop app does.
pub async fn record_view_impl(state: &AppState, path: String) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.record_view(&path).map_err(|e| e.to_string())
}
