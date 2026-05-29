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

    let sql = format!(
        "SELECT DISTINCT m.path FROM media_meta m WHERE {}",
        where_clause
    );

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

/// Clear the active filter (returns all media paths).
#[tauri::command]
pub async fn clear_filter(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<String>, String> {
    clear_filter_impl(&state).await
}

pub async fn clear_filter_impl(
    state: &AppState,
) -> Result<Vec<String>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let mut stmt = db
        .conn()
        .prepare("SELECT path FROM media_meta ORDER BY date_taken DESC")
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| e.to_string())?);
    }

    Ok(result)
}
