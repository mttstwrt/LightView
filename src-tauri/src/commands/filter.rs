//! Applying a filter query and returning the matching paths.
//!
//! Parse, compile to SQL, run. The result is a path list rather than full
//! rows because sorting is a separate command that takes the list as an
//! argument — keeping them apart means changing the sort does not re-run the
//! filter.
//!
//! There is deliberately no "clear the filter" command. Clearing is the absence
//! of a path list, not a different list: `get_sorted_items` with
//! `filter_paths: None` reads `media_meta` in sort order directly. A command
//! that returned every path so the caller could pass them all back was one
//! full-table read and one JSON array of the whole gallery per clear, to end up
//! at the same rows.

use crate::filter::evaluator;
use crate::filter::parser;
use crate::AppState;

/// Apply a filter expression (as a query string) and return matching paths.
#[tauri::command]
pub async fn apply_filter(
    state: tauri::State<'_, AppState>,
    query: String,
) -> Result<Vec<String>, String> {
    apply_filter_impl(&state, query).await
}

pub async fn apply_filter_impl(
    state: &AppState,
    query: String,
) -> Result<Vec<String>, String> {
    let expr = parser::parse_filter(&query).map_err(|e| e.to_string())?;

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    // Use SQL-based evaluation for performance
    let mut params = Vec::new();
    let where_clause = evaluator::to_sql(&expr, &mut params);

    // No DISTINCT: `m.path` is the primary key and tag terms compile to
    // correlated `EXISTS` subqueries rather than joins, so a row can be
    // produced at most once. Asking for it anyway buys a dedup pass over the
    // whole result for a guarantee the schema already gives.
    let sql = format!("SELECT m.path FROM media_meta m WHERE {}", where_clause);

    let mut stmt = db.conn().prepare(&sql).map_err(|e| e.to_string())?;

    // Build parameter references
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| e.to_string())?);
    }

    Ok(result)
}
