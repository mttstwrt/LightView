use crate::autocomplete::engine::TagSuggestion;
use crate::AppState;

/// Get tag autocomplete suggestions for the filter bar.
#[tauri::command]
pub async fn autocomplete_tags(
    state: tauri::State<'_, AppState>,
    query: String,
    namespace: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<TagSuggestion>, String> {
    autocomplete_tags_impl(&state, query, namespace, limit).await
}

pub async fn autocomplete_tags_impl(
    state: &AppState,
    query: String,
    namespace: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<TagSuggestion>, String> {
    let limit = limit.unwrap_or(15);
    Ok(state
        .autocomplete
        .query(&query, namespace.as_deref(), limit)
        .await)
}

/// Get recently used filter tags (stored in settings, not in autocomplete engine).
#[tauri::command]
pub async fn get_recent_tags(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<String>, String> {
    get_recent_tags_impl(&state).await
}

pub async fn get_recent_tags_impl(
    state: &AppState,
) -> Result<Vec<String>, String> {
    // TODO: Store recent tags in gallery_meta or a separate settings store
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        if let Ok(Some(json)) = db.get_gallery_meta("recent_filter_tags") {
            if let Ok(tags) = serde_json::from_str::<Vec<String>>(&json) {
                return Ok(tags);
            }
        }
    }
    Ok(Vec::new())
}
