use serde::{Deserialize, Serialize};

use crate::cache::db::{CacheDb, CacheError};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortField {
    Date,
    Size,
    Name,
    Rating,
    MediaType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

/// A single item in the sorted results list.
#[derive(Debug, Clone, Serialize)]
pub struct SortedItem {
    pub path: String,
    pub date_taken: Option<i64>,
    pub file_size: i64,
    pub media_type: String,
    pub rating: Option<u8>,
}

impl CacheDb {
    /// Get all media items sorted by the specified field and order.
    /// If `filter_paths` is Some, only return items in that set.
    pub fn get_sorted_items(
        &self,
        sort_field: &SortField,
        sort_order: &SortOrder,
        filter_paths: Option<&[String]>,
    ) -> Result<Vec<SortedItem>, CacheError> {
        let order = match sort_order {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        };

        let order_clause = match sort_field {
            SortField::Date => format!("date_taken {} NULLS LAST", order),
            SortField::Size => format!("file_size {}", order),
            SortField::Name => format!("path {}", order),
            SortField::Rating => format!("rating {} NULLS LAST", order),
            SortField::MediaType => format!("media_type {}", order),
        };

        let sql = if filter_paths.is_some() {
            // Use a temporary approach: build IN clause
            // For large sets, the caller should use SQL-based filtering instead.
            format!(
                "SELECT path, date_taken, file_size, media_type, rating FROM media_meta 
                 WHERE path IN (SELECT value FROM json_each(?1))
                 ORDER BY {}",
                order_clause
            )
        } else {
            format!(
                "SELECT path, date_taken, file_size, media_type, rating FROM media_meta ORDER BY {}",
                order_clause
            )
        };

        let mut stmt = self.conn().prepare(&sql)?;

        let rows = if let Some(paths) = filter_paths {
            let json_array = serde_json::to_string(paths).unwrap_or_else(|_| "[]".to_string());
            stmt.query_map(rusqlite::params![json_array], map_sorted_row)?
        } else {
            stmt.query_map([], map_sorted_row)?
        };

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Insert or update media metadata for a file.
    pub fn upsert_media_meta(
        &self,
        path: &str,
        date_taken: Option<i64>,
        file_size: i64,
        media_type: &str,
        width: Option<u32>,
        height: Option<u32>,
        duration: Option<f64>,
        rating: Option<u8>,
    ) -> Result<(), CacheError> {
        self.conn().execute(
            "INSERT OR REPLACE INTO media_meta (path, date_taken, file_size, media_type, width, height, duration, rating)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![path, date_taken, file_size, media_type, width, height, duration, rating],
        )?;
        Ok(())
    }
}

fn map_sorted_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SortedItem> {
    Ok(SortedItem {
        path: row.get(0)?,
        date_taken: row.get(1)?,
        file_size: row.get(2)?,
        media_type: row.get(3)?,
        rating: row.get(4)?,
    })
}
