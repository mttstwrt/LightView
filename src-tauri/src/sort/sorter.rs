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
    LastViewed,
    DateAdded,
    LastRated,
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
    pub last_viewed: Option<i64>,
    pub date_added: Option<i64>,
    pub last_rated: Option<i64>,
    /// Video duration in seconds, if known (probed lazily during thumbnailing).
    pub duration: Option<f64>,
}

impl CacheDb {
    /// Build an ORDER BY expression for a single sort field + direction.
    fn order_expr(field: &SortField, order: &str) -> String {
        match field {
            SortField::Date => format!("date_taken {} NULLS LAST", order),
            SortField::Size => format!("file_size {}", order),
            SortField::Name => format!("path {}", order),
            SortField::Rating => format!("rating {} NULLS LAST", order),
            SortField::MediaType => format!("media_type {}", order),
            SortField::LastViewed => format!("last_viewed {} NULLS LAST", order),
            SortField::DateAdded => format!("date_added {} NULLS LAST", order),
            SortField::LastRated => format!("last_rated {} NULLS LAST", order),
        }
    }

    /// Get all media items sorted by the specified field and order,
    /// with an optional secondary sort (tiebreaker within equal primary values).
    /// If `filter_paths` is Some, only return items in that set.
    pub fn get_sorted_items(
        &self,
        sort_field: &SortField,
        sort_order: &SortOrder,
        sub_sort_field: Option<&SortField>,
        sub_sort_order: Option<&SortOrder>,
        filter_paths: Option<&[String]>,
    ) -> Result<Vec<SortedItem>, CacheError> {
        let order = match sort_order {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        };

        let mut order_clause = Self::order_expr(sort_field, order);

        if let Some(sub_field) = sub_sort_field {
            let sub_order = match sub_sort_order.unwrap_or(&SortOrder::Desc) {
                SortOrder::Asc => "ASC",
                SortOrder::Desc => "DESC",
            };
            order_clause.push_str(", ");
            order_clause.push_str(&Self::order_expr(sub_field, sub_order));
        }

        let cols = "path, date_taken, file_size, media_type, rating, last_viewed, date_added, last_rated, duration";
        let sql = if filter_paths.is_some() {
            format!(
                "SELECT {} FROM media_meta
                 WHERE path IN (SELECT value FROM json_each(?1))
                 ORDER BY {}",
                cols, order_clause
            )
        } else {
            format!(
                "SELECT {} FROM media_meta ORDER BY {}",
                cols, order_clause
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

    /// Update only the rating for a file in the cache.
    /// Also sets `last_rated` to the current timestamp.
    pub fn update_rating(&self, path: &str, rating: Option<u8>) -> Result<(), CacheError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn().execute(
            "UPDATE media_meta SET rating = ?1, last_rated = ?2 WHERE path = ?3",
            rusqlite::params![rating, now, path],
        )?;
        Ok(())
    }

    /// Insert or update media metadata for a file.
    /// Preserves `date_added` on update; sets it to now on first insert.
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn().execute(
            "INSERT INTO media_meta (path, date_taken, file_size, media_type, width, height, duration, rating, date_added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(path) DO UPDATE SET
               date_taken = excluded.date_taken,
               file_size = excluded.file_size,
               media_type = excluded.media_type,
               width = excluded.width,
               height = excluded.height,
               duration = excluded.duration,
               rating = excluded.rating",
            rusqlite::params![path, date_taken, file_size, media_type, width, height, duration, rating, now],
        )?;
        Ok(())
    }

    /// Record that a media item was viewed (sets last_viewed to now).
    pub fn record_view(&self, path: &str) -> Result<(), CacheError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn().execute(
            "UPDATE media_meta SET last_viewed = ?1 WHERE path = ?2",
            rusqlite::params![now, path],
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
        last_viewed: row.get(5)?,
        date_added: row.get(6)?,
        last_rated: row.get(7)?,
        duration: row.get(8)?,
    })
}
