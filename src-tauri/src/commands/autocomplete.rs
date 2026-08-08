//! Tag suggestions for the filter bar.
//!
//! A thin pass-through to the in-memory autocomplete engine — no database
//! access, which is why it can be called on every keystroke.

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
