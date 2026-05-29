use crate::sort::grouper::{self, GroupBy, GroupHeader};
use crate::sort::sorter::{SortField, SortOrder, SortedItem};
use crate::sort::timeline::TimelineEntry;
use crate::AppState;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SortedResult {
    pub items: Vec<SortedItem>,
    pub groups: Vec<GroupHeader>,
}

/// Get sorted (and optionally grouped) items, with optional filter paths.
#[tauri::command]
pub async fn get_sorted_items(
    state: tauri::State<'_, AppState>,
    sort_field: SortField,
    sort_order: SortOrder,
    group_by: GroupBy,
    filter_paths: Option<Vec<String>>,
    sub_sort_field: Option<SortField>,
    sub_sort_order: Option<SortOrder>,
) -> Result<SortedResult, String> {
    get_sorted_items_impl(
        &state,
        sort_field,
        sort_order,
        group_by,
        filter_paths,
        sub_sort_field,
        sub_sort_order,
    )
    .await
}

pub async fn get_sorted_items_impl(
    state: &AppState,
    sort_field: SortField,
    sort_order: SortOrder,
    group_by: GroupBy,
    filter_paths: Option<Vec<String>>,
    sub_sort_field: Option<SortField>,
    sub_sort_order: Option<SortOrder>,
) -> Result<SortedResult, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let items = db
        .get_sorted_items(
            &sort_field,
            &sort_order,
            sub_sort_field.as_ref(),
            sub_sort_order.as_ref(),
            filter_paths.as_deref(),
        )
        .map_err(|e| e.to_string())?;

    let groups = grouper::compute_groups(&items, &group_by);

    Ok(SortedResult { items, groups })
}

/// Get the timeline index for scrollbar date indicators.
#[tauri::command]
pub async fn get_timeline_index(
    state: tauri::State<'_, AppState>,
    items_per_row: usize,
) -> Result<Vec<TimelineEntry>, String> {
    get_timeline_index_impl(&state, items_per_row).await
}

pub async fn get_timeline_index_impl(
    state: &AppState,
    items_per_row: usize,
) -> Result<Vec<TimelineEntry>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    db.compute_timeline(items_per_row)
        .map_err(|e| e.to_string())
}
