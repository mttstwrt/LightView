//! Sorting and grouping.
//!
//! Takes the filtered path list from `commands::filter` and returns the ordered
//! items plus their group headers in one round-trip, so the grid never renders
//! a half-updated view.

use crate::sort::grouper::{self, GroupBy, GroupHeader};
use crate::sort::sorter::{SortField, SortOrder, SortedItem};
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
