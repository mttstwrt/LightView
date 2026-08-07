//! Sort fields and the `SELECT ... ORDER BY` that serves the grid.
//!
//! One query returns everything the grid needs for first paint, including the
//! ~25-byte ThumbHash blob joined from `thumbnails`. Inlining that is what lets
//! the remote web client paint a recognisable blurry grid before any thumbnail
//! request goes out, instead of a round-trip per cell.
//!
//! A filtered path list is bound as a single JSON parameter and expanded with
//! `json_each`, not spliced into a generated `IN (?, ?, …)`. One prepared
//! statement then serves every filter size, so the statement cache stays warm
//! instead of being invalidated by each new result count.

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
    /// Source media dimensions, if indexed. Drive aspect-ratio layout
    /// (e.g. the justified gallery view) without an extra round-trip.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Base64 of the ~25-byte ThumbHash placeholder, when a thumbnail has
    /// been generated. Inlined here so the grid can paint every cell as a
    /// blurry placeholder from the items payload alone — no per-cell
    /// round-trip before first paint (crucial on the remote web client).
    pub thumbhash: Option<String>,
}

impl CacheDb {
    /// Build an ORDER BY expression for a single sort field + direction.
    ///
    /// Every column is qualified with the `m` alias of the query below. Sorting
    /// reads only `media_meta`, but the query joins `thumbnails` for the inlined
    /// ThumbHash — and that table has `path` and `media_type` columns of its
    /// own, so the bare names made SQLite reject the whole statement with
    /// "ambiguous column name". That broke sorting by Name or Media Type
    /// outright, and any other sort that used one of them as its tiebreaker.
    /// Qualifying all of them keeps the expression correct no matter which
    /// columns a future join brings into scope.
    fn order_expr(field: &SortField, order: &str) -> String {
        match field {
            SortField::Date => format!("m.date_taken {} NULLS LAST", order),
            SortField::Size => format!("m.file_size {}", order),
            SortField::Name => format!("m.path {}", order),
            SortField::Rating => format!("m.rating {} NULLS LAST", order),
            SortField::MediaType => format!("m.media_type {}", order),
            SortField::LastViewed => format!("m.last_viewed {} NULLS LAST", order),
            SortField::DateAdded => format!("m.date_added {} NULLS LAST", order),
            SortField::LastRated => format!("m.last_rated {} NULLS LAST", order),
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

        let cols = "m.path, m.date_taken, m.file_size, m.media_type, m.rating, m.last_viewed, m.date_added, m.last_rated, m.duration, m.width, m.height, t.thumbhash";
        let sql = if filter_paths.is_some() {
            format!(
                "SELECT {} FROM media_meta m
                 LEFT JOIN thumbnails t ON t.path = m.path
                 WHERE m.path IN (SELECT value FROM json_each(?1))
                 ORDER BY {}",
                cols, order_clause
            )
        } else {
            format!(
                "SELECT {} FROM media_meta m
                 LEFT JOIN thumbnails t ON t.path = m.path
                 ORDER BY {}",
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
    use base64::Engine;
    let thumbhash: Option<Vec<u8>> = row.get(11)?;
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
        width: row.get(9)?,
        height: row.get(10)?,
        thumbhash: thumbhash.map(|h| base64::engine::general_purpose::STANDARD.encode(h)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FIELDS: [SortField; 8] = [
        SortField::Date,
        SortField::Size,
        SortField::Name,
        SortField::Rating,
        SortField::MediaType,
        SortField::LastViewed,
        SortField::DateAdded,
        SortField::LastRated,
    ];

    /// A gallery with a thumbnail row present, so the query's LEFT JOIN really
    /// pulls `thumbnails` (and its own `path`/`media_type` columns) into scope.
    fn open_db() -> (tempfile::TempDir, CacheDb) {
        let tmp = tempfile::tempdir().unwrap();
        let db = CacheDb::open(tmp.path()).unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO media_meta (path, date_taken, file_size, media_type, rating) VALUES
                     ('/g/c.jpg', 300, 30, 'image', 5),
                     ('/g/a.mp4', 100, 10, 'video', 1),
                     ('/g/b.jpg', 200, 20, 'image', 3);
                 INSERT INTO thumbnails (path, media_type, mtime, width, height, thumbnail)
                     VALUES ('/g/a.mp4', 'video', 1, 8, 8, x'00');",
            )
            .unwrap();
        (tmp, db)
    }

    /// Every sort field has to survive the join to `thumbnails`. `path` and
    /// `media_type` exist on both tables, so an unqualified ORDER BY made
    /// SQLite reject the statement outright — sorting by Name or Media Type
    /// failed with "ambiguous column name" rather than returning anything, as
    /// did any sort using one of them as its tiebreaker.
    #[test]
    fn every_sort_field_builds_a_runnable_query() {
        let (_tmp, db) = open_db();
        let filter = vec!["/g/a.mp4".to_string(), "/g/b.jpg".to_string()];

        for field in &ALL_FIELDS {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                // Unfiltered, no sub-sort.
                let all = db
                    .get_sorted_items(field, &order, None, None, None)
                    .unwrap_or_else(|e| panic!("{:?} {:?}: {}", field, order, e));
                assert_eq!(all.len(), 3);

                // Filtered — the other SQL branch.
                let some = db
                    .get_sorted_items(field, &order, None, None, Some(&filter))
                    .unwrap_or_else(|e| panic!("{:?} {:?} filtered: {}", field, order, e));
                assert_eq!(some.len(), 2);

                // And behind every other field as a tiebreaker, since the
                // sub-sort goes through the same expression builder.
                for sub in &ALL_FIELDS {
                    db.get_sorted_items(field, &order, Some(sub), Some(&SortOrder::Asc), None)
                        .unwrap_or_else(|e| panic!("{:?} then {:?}: {}", field, sub, e));
                }
            }
        }
    }

    #[test]
    fn name_sort_orders_by_path() {
        let (_tmp, db) = open_db();

        let asc = db
            .get_sorted_items(&SortField::Name, &SortOrder::Asc, None, None, None)
            .expect("name asc");
        let paths: Vec<&str> = asc.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["/g/a.mp4", "/g/b.jpg", "/g/c.jpg"]);

        let desc = db
            .get_sorted_items(&SortField::Name, &SortOrder::Desc, None, None, None)
            .expect("name desc");
        let paths: Vec<&str> = desc.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["/g/c.jpg", "/g/b.jpg", "/g/a.mp4"]);
    }

    /// The join still has to deliver the ThumbHash it was added for.
    #[test]
    fn the_thumbnail_join_still_supplies_thumbhashes() {
        let (_tmp, db) = open_db();
        db.conn()
            .execute(
                "UPDATE thumbnails SET thumbhash = x'0102' WHERE path = '/g/a.mp4'",
                [],
            )
            .unwrap();

        let items = db
            .get_sorted_items(&SortField::Name, &SortOrder::Asc, None, None, None)
            .expect("name asc");
        assert_eq!(items[0].path, "/g/a.mp4");
        assert!(items[0].thumbhash.is_some(), "joined thumbhash missing");
        assert!(items[1].thumbhash.is_none(), "unrelated row got a thumbhash");
    }
}
